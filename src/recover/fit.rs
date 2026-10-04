//! Matching a record with the tables it could belong to: the number of
//! values it stores, and whether each value's type is one SQLite could have
//! stored in that column.

use std::collections::BTreeSet;

use crate::record::{SerialType, Value};
use crate::rows::with_affinity;
use crate::schema::{Affinity, Column, Generated, Table};

/// Bytes of values a damaged record must have read to be kept.
const MIN_DAMAGED_READ_BYTES: u64 = 4;

/// How much a record must show to be matched with a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Strictness {
    /// A cell a page points to, certainly a record: any count of stored
    /// values up to the columns'.
    Pointed,
    /// A whole cell carved from free space: the full count of stored
    /// values, or one live rows show.
    Whole,
    /// A cell whose start was overwritten: as `Whole`, and every value read
    /// of its column's usual type, at least a few bytes of them.
    Damaged,
}

/// One value of a record, as read or reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Slot {
    /// Its serial type; `None` when it was lost and can't be inferred.
    pub(super) serial: Option<SerialType>,
    /// Bytes it takes in the record's body.
    pub(super) size: u64,
    /// Whether its serial type was read from the record, rather than
    /// inferred from the table it is matched with.
    pub(super) read: bool,
}

impl Slot {
    /// A value whose serial type was read from the record.
    pub(super) fn read(serial: SerialType) -> Self {
        Self {
            serial: Some(serial),
            size: serial.size(),
            read: true,
        }
    }

    /// Whether it holds something other than NULL, as read.
    fn read_non_null(self) -> bool {
        self.read && !matches!(self.serial, Some(SerialType::Null))
    }
}

/// How well a record's values agree with a table's columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Agreement {
    /// Values read whose type the column allows but doesn't lead to (an
    /// integer in a column without a type, text in an `INTEGER` column).
    pub(super) unexpected: usize,
    /// Values read that aren't NULL.
    pub(super) read_values: usize,
    /// Bytes those values take.
    pub(super) read_bytes: u64,
    /// Whether the record stores every column.
    pub(super) exact: bool,
}

/// What SQLite stores a value as, whatever its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl Storage {
    fn of(serial: SerialType) -> Self {
        match serial {
            SerialType::Null | SerialType::Reserved(_) => Self::Null,
            SerialType::Integer(_) | SerialType::Constant(_) => Self::Integer,
            SerialType::Real => Self::Real,
            SerialType::Text(_) => Self::Text,
            SerialType::Blob(_) => Self::Blob,
        }
    }

    /// Whether a column of `affinity` can hold it: a `TEXT` column turns
    /// numbers into text before storing them; the others keep what can't
    /// be converted.
    fn possible_in(self, affinity: Affinity) -> bool {
        !(affinity == Affinity::Text && matches!(self, Self::Integer | Self::Real))
    }

    /// Whether it is what `column` usually holds: blobs only where declared
    /// (a column without a type takes anything, but rarely blobs).
    fn expected_in(self, column: &Column) -> bool {
        match (column.affinity, self) {
            (Affinity::Blob, Self::Blob) => !column.declared_type.is_empty(),
            (_, Self::Null)
            | (Affinity::Blob, _)
            | (Affinity::Text, Self::Text)
            | (Affinity::Integer, Self::Integer)
            | (Affinity::Real | Affinity::Numeric, Self::Integer | Self::Real) => true,
            _ => false,
        }
    }
}

/// A table that records are matched with.
#[derive(Debug, Clone)]
pub(super) struct Shape {
    pub(super) table: Table,
    /// Indexes of the columns a record stores (all but virtual generated
    /// ones), in order.
    stored: Vec<usize>,
    /// Fewer stored values than columns that live rows show (columns added
    /// later with `ALTER TABLE`), which carved records may have too.
    fewer: BTreeSet<usize>,
}

impl Shape {
    /// The shape of a rowid table whose columns are known.
    pub(super) fn new(table: &Table) -> Option<Self> {
        let stored: Vec<usize> = (0..table.columns.len())
            .filter(|&index| table.columns[index].generated != Some(Generated::Virtual))
            .collect();
        (!stored.is_empty()).then(|| Self {
            table: table.clone(),
            stored,
            fewer: BTreeSet::new(),
        })
    }

    /// Note that a live row stores `count` values.
    pub(super) fn note_stored_count(&mut self, count: usize) {
        if count > 0 && count < self.stored.len() {
            self.fewer.insert(count);
        }
    }

    pub(super) fn name(&self) -> &str {
        &self.table.name
    }

    /// Values a full record stores.
    pub(super) fn stored_len(&self) -> usize {
        self.stored.len()
    }

    /// The counts of stored values a carved record may have, most first.
    pub(super) fn counts(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.stored.len()).chain(self.fewer.iter().rev().copied())
    }

    /// The column the record's first value belongs to.
    pub(super) fn first_column(&self) -> &Column {
        &self.table.columns[self.stored[0]]
    }

    /// Whether value `index` is excluded from a dedupe key that leaves out
    /// the rowid (`alias`) and, optionally, the first stored value. Values
    /// past the columns (which a damaged live row may hold) are kept.
    pub(super) fn is_unkeyed(&self, index: usize, without_first: bool) -> bool {
        let is_alias = self
            .table
            .columns
            .get(index)
            .is_some_and(|column| column.rowid_alias);
        is_alias || (without_first && index == self.stored[0])
    }

    /// How `slots` agree with the columns, or `None` when they can't be a
    /// row of this table, or say too little to tell.
    pub(super) fn agree(&self, slots: &[Slot], strictness: Strictness) -> Option<Agreement> {
        let count = slots.len();
        let relaxed = strictness == Strictness::Pointed;
        let count_allowed =
            count == self.stored.len() || (relaxed && count > 0) || self.fewer.contains(&count);
        if count > self.stored.len() || !count_allowed {
            return None;
        }
        let mut agreement = Agreement {
            unexpected: 0,
            read_values: 0,
            read_bytes: 0,
            exact: count == self.stored.len(),
        };
        for (slot, &index) in slots.iter().zip(&self.stored) {
            let column = &self.table.columns[index];
            let storage = slot.serial.map(Storage::of);
            if column.rowid_alias {
                // The record stores NULL for the rowid alias.
                if storage.is_some_and(|storage| storage != Storage::Null) {
                    return None;
                }
                continue;
            }
            if let Some(storage) = storage {
                if !storage.possible_in(column.affinity) {
                    return None;
                }
                if slot.read && !storage.expected_in(column) {
                    agreement.unexpected += 1;
                }
            }
            if slot.read_non_null() {
                agreement.read_values += 1;
                agreement.read_bytes += slot.size;
            }
        }
        let enough = match strictness {
            Strictness::Pointed | Strictness::Whole => agreement.read_values > 0,
            // A few bytes of a freed cell parse as anything: a damaged
            // record needs every value usual and some bytes of them.
            Strictness::Damaged => {
                agreement.unexpected == 0 && agreement.read_bytes >= MIN_DAMAGED_READ_BYTES
            }
        };
        enough.then_some(agreement)
    }

    /// The record's stored values placed in the table's columns, as
    /// [`crate::Rows`] places a live row's: virtual generated columns are
    /// NULL, columns past the stored ones take their default, the rowid
    /// alias takes the rowid (unknown when the rowid is), integers in
    /// `REAL` columns read as reals.
    pub(super) fn align(
        &self,
        stored: Vec<Option<Value>>,
        rowid: Option<i64>,
    ) -> Vec<Option<Value>> {
        let mut stored = stored.into_iter();
        self.table
            .columns
            .iter()
            .map(|column| {
                if column.generated == Some(Generated::Virtual) {
                    return Some(Value::Null);
                }
                let value = stored
                    .next()
                    .unwrap_or_else(|| Some(column.default.clone()));
                if column.rowid_alias {
                    rowid.map(Value::Integer)
                } else {
                    value.map(|value| with_affinity(column.affinity, value))
                }
            })
            .collect()
    }
}

/// The serial type a lost first value most likely had, given its size and
/// its column, when that is clear: `TEXT` columns hold text; `INTEGER`
/// columns integers of the sizes integers take; `REAL` columns 8-byte
/// reals. `None` when it could be several things (a value of no bytes is
/// NULL, 0 or 1).
pub(super) fn infer_serial(column: &Column, size: u64) -> Option<SerialType> {
    match (column.affinity, size) {
        (_, 0) => None,
        (Affinity::Text, _) => Some(SerialType::Text(size)),
        (Affinity::Integer, 1..=4 | 6 | 8) => Some(SerialType::Integer(size as usize)),
        (Affinity::Real, 8) => Some(SerialType::Real),
        _ => None,
    }
}

/// The sizes a lost first value of `column` could have if it is a number
/// (none unless its column leads to numbers): NULL, 0 and 1 take none,
/// integers 1 to 4, 6 or 8 bytes, reals 8.
pub(super) fn number_sizes(column: &Column) -> &'static [u64] {
    match column.affinity {
        Affinity::Integer | Affinity::Real | Affinity::Numeric => &[0, 1, 2, 3, 4, 6, 8],
        Affinity::Text | Affinity::Blob => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaEntry;

    fn shape(sql: &str) -> Shape {
        let entry = SchemaEntry {
            kind: "table".to_owned(),
            name: "t".to_owned(),
            table_name: "t".to_owned(),
            root_page: 2,
            sql: Some(sql.to_owned()),
        };
        Shape::new(&Table::from_entry(&entry).0).unwrap()
    }

    fn slots(types: &[SerialType]) -> Vec<Slot> {
        types.iter().copied().map(Slot::read).collect()
    }

    #[test]
    fn a_text_column_never_holds_numbers() {
        let shape = shape("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
        let null = SerialType::Null;
        assert!(shape
            .agree(&slots(&[null, SerialType::Text(3)]), Strictness::Whole)
            .is_some());
        assert!(shape
            .agree(&slots(&[null, SerialType::Integer(1)]), Strictness::Whole)
            .is_none());
    }

    #[test]
    fn the_rowid_alias_is_stored_as_null() {
        let shape = shape("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
        let text = SerialType::Text(3);
        assert!(shape
            .agree(&slots(&[SerialType::Integer(1), text]), Strictness::Whole)
            .is_none());
    }

    #[test]
    fn fewer_values_only_when_live_rows_show_them_or_relaxed() {
        let mut shape = shape("CREATE TABLE t (a TEXT, b INTEGER, c TEXT)");
        let two = slots(&[SerialType::Text(2), SerialType::Integer(1)]);
        assert!(shape.agree(&two, Strictness::Whole).is_none());
        assert!(shape.agree(&two, Strictness::Pointed).is_some());
        shape.note_stored_count(2);
        let agreement = shape.agree(&two, Strictness::Whole).unwrap();
        assert!(!agreement.exact);
    }

    #[test]
    fn a_record_of_nulls_matches_nothing() {
        let shape = shape("CREATE TABLE t (a TEXT, b INTEGER)");
        assert!(shape
            .agree(
                &slots(&[SerialType::Null, SerialType::Null]),
                Strictness::Whole
            )
            .is_none());
    }

    #[test]
    fn unexpected_types_are_counted() {
        let shape = shape("CREATE TABLE t (a INTEGER, b TEXT)");
        let slots = slots(&[SerialType::Text(4), SerialType::Text(1)]);
        let agreement = shape.agree(&slots, Strictness::Whole).unwrap();
        assert_eq!(agreement.unexpected, 1);
        assert_eq!(agreement.read_values, 2);
        // Not enough for a damaged record.
        assert!(shape.agree(&slots, Strictness::Damaged).is_none());
    }

    #[test]
    fn a_damaged_record_needs_a_few_bytes() {
        let shape = shape("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT)");
        let null = SerialType::Null;
        let one = slots(&[null, SerialType::Constant(1), null]);
        assert!(shape.agree(&one, Strictness::Whole).is_some());
        assert!(shape.agree(&one, Strictness::Damaged).is_none());
        let text = slots(&[null, SerialType::Constant(1), SerialType::Text(4)]);
        assert!(shape.agree(&text, Strictness::Damaged).is_some());
    }

    #[test]
    fn blobs_are_unusual_in_a_column_without_a_type() {
        let untyped = shape("CREATE TABLE t (a TEXT, b)");
        let typed = shape("CREATE TABLE t (a TEXT, b BLOB)");
        let slots = slots(&[SerialType::Text(4), SerialType::Blob(4)]);
        assert_eq!(
            untyped.agree(&slots, Strictness::Whole).unwrap().unexpected,
            1
        );
        assert_eq!(
            typed.agree(&slots, Strictness::Whole).unwrap().unexpected,
            0
        );
    }
}
