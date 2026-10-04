//! Telling a recovered record from a live row, and from another find of
//! the same record: keys over what is known of its values.
//!
//! A record whose first bytes were overwritten has lost its rowid, and
//! sometimes its first value; it is the same as a row when every value it
//! still has is equal. Each record therefore gets the key for what it
//! knows, and each row or kept record adds the keys of everything weaker,
//! so a lesser copy is recognized whichever is met first.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use super::fit::Shape;
use super::RecoveredRecord;
use crate::record::Value;

/// How much of a record is known, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Known {
    /// The rowid and every value.
    Everything,
    /// Every value but the rowid (and so the rowid alias column).
    AllButRowid,
    /// Every value but the rowid and the first stored value.
    AllButRowidAndFirst,
    /// Less, or cut short: only an identical record is the same.
    Exactly,
}

/// Records and rows seen so far, by key.
#[derive(Debug, Clone, Default)]
pub(super) struct Seen {
    keys: HashSet<u64>,
}

impl Seen {
    /// Note a live row: whatever is lost of a copy of it, the copy is it.
    pub(super) fn insert_row(&mut self, shape: &Shape, rowid: i64, values: &[Value]) {
        let values: Vec<Option<Value>> = values.iter().cloned().map(Some).collect();
        for known in [
            Known::Everything,
            Known::AllButRowid,
            Known::AllButRowidAndFirst,
        ] {
            self.keys
                .insert(key(shape.name(), known, Some(rowid), &values, Some(shape)));
        }
    }

    /// Whether `record` is new (not a row or a record seen before), noting
    /// it if it is. `shape` is its table's.
    pub(super) fn admit(&mut self, record: &RecoveredRecord, shape: Option<&Shape>) -> bool {
        let table = record.table.as_deref().unwrap_or_default();
        let known = knowledge(record, shape);
        if !self
            .keys
            .insert(key(table, known, record.rowid, &record.values, shape))
        {
            return false;
        }
        let weaker = [Known::AllButRowid, Known::AllButRowidAndFirst];
        for weaker in weaker.into_iter().filter(|&weaker| weaker > known) {
            self.keys
                .insert(key(table, weaker, record.rowid, &record.values, shape));
        }
        true
    }
}

/// Keep the records that are new, the most complete copy of each.
pub(super) fn deduplicate(
    mut records: Vec<RecoveredRecord>,
    mut seen: Seen,
    shapes: &[Shape],
) -> Vec<RecoveredRecord> {
    let shape_of = |record: &RecoveredRecord| {
        record
            .table
            .as_deref()
            .and_then(|name| shapes.iter().find(|shape| shape.name() == name))
    };
    records.sort_by_key(|record| knowledge(record, shape_of(record)));
    records.retain(|record| seen.admit(record, shape_of(record)));
    records
}

/// What is known of `record`.
fn knowledge(record: &RecoveredRecord, shape: Option<&Shape>) -> Known {
    let Some(shape) = shape.filter(|_| !record.truncated) else {
        return Known::Exactly;
    };
    let known_but = |without_first: bool| {
        record
            .values
            .iter()
            .enumerate()
            .all(|(index, value)| value.is_some() || shape.is_unkeyed(index, without_first))
    };
    if record.rowid.is_some() && record.values.iter().all(Option::is_some) {
        Known::Everything
    } else if known_but(false) {
        Known::AllButRowid
    } else if known_but(true) {
        Known::AllButRowidAndFirst
    } else {
        Known::Exactly
    }
}

/// The key of a record of `table` with `values`, over what `known` says.
fn key(
    table: &str,
    known: Known,
    rowid: Option<i64>,
    values: &[Option<Value>],
    shape: Option<&Shape>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    table.hash(&mut hasher);
    known.hash(&mut hasher);
    if matches!(known, Known::Everything | Known::Exactly) {
        rowid.hash(&mut hasher);
    }
    for (index, value) in values.iter().enumerate() {
        let unkeyed =
            |without_first| shape.is_some_and(|shape| shape.is_unkeyed(index, without_first));
        let skip = match known {
            Known::Everything | Known::Exactly => false,
            Known::AllButRowid => unkeyed(false),
            Known::AllButRowidAndFirst => unkeyed(true),
        };
        if !skip {
            hash_value(value.as_ref(), &mut hasher);
        }
    }
    hasher.finish()
}

fn hash_value(value: Option<&Value>, hasher: &mut DefaultHasher) {
    match value {
        None => 0u8.hash(hasher),
        Some(Value::Null) => 1u8.hash(hasher),
        Some(Value::Integer(integer)) => (2u8, integer).hash(hasher),
        Some(Value::Real(real)) => (3u8, real.to_bits()).hash(hasher),
        Some(Value::Text(text)) => (4u8, text).hash(hasher),
        Some(Value::Blob(blob)) => (5u8, blob).hash(hasher),
    }
}
