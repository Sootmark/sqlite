//! Rows of a table and entries of an index, read one at a time as the
//! b-tree walk reaches them.

use crate::btree::{Cursor, Entry, TreeKind};
use crate::header::TextEncoding;
use crate::pager::Pages;
use crate::record::{self, Value};
use crate::schema::{Affinity, Column, Generated, Table};

/// A row of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Its rowid, the table b-tree's key.
    pub rowid: i64,
    /// The leaf page holding it.
    pub page: u32,
    /// One value per column, in declared order (an `INTEGER PRIMARY KEY`
    /// column holds the rowid). Values the record holds beyond the declared
    /// columns follow them, and are reported. When the table's statement
    /// couldn't be read, the record's values as stored.
    pub values: Vec<Value>,
}

/// The rows of a table, in rowid order.
///
/// Damage met on the way (unreadable pages, cut cells, broken overflow
/// chains, damaged records) is skipped and listed in [`Rows::problems`];
/// the rows after it still read.
pub struct Rows<'a> {
    cursor: Cursor<'a>,
    columns: &'a [Column],
    encoding: TextEncoding,
}

impl<'a> Rows<'a> {
    pub(crate) fn new(pages: &'a Pages<'a>, table: &'a Table, encoding: TextEncoding) -> Self {
        Self {
            cursor: Cursor::new(pages, table.root_page, TreeKind::Table),
            columns: &table.columns,
            encoding,
        }
    }

    /// What went wrong so far, in the order met.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.cursor.problems
    }

    /// Take the problems met so far.
    pub(crate) fn take_problems(&mut self) -> Vec<String> {
        std::mem::take(&mut self.cursor.problems)
    }

    fn row(&mut self, entry: &Entry) -> Row {
        let rowid = entry.rowid.unwrap_or_default();
        let (stored, problem) = record::decode(&entry.payload, self.encoding);
        if let Some(problem) = problem {
            self.cursor
                .problems
                .push(format!("page {}: row {rowid}: {problem}", entry.page));
        }
        let values = if self.columns.is_empty() {
            stored
        } else {
            self.by_column(stored, rowid, entry.page)
        };
        Row {
            rowid,
            page: entry.page,
            values,
        }
    }

    /// The stored values placed in their columns: virtual generated columns
    /// aren't stored, columns added after the row was written take their
    /// default, the rowid alias takes the rowid.
    fn by_column(&mut self, stored: Vec<Value>, rowid: i64, page: u32) -> Vec<Value> {
        let mut stored = stored.into_iter();
        let mut values: Vec<Value> = self
            .columns
            .iter()
            .map(|column| {
                let value = if column.generated == Some(Generated::Virtual) {
                    Value::Null
                } else {
                    stored.next().unwrap_or_else(|| column.default.clone())
                };
                if column.rowid_alias {
                    Value::Integer(rowid)
                } else {
                    with_affinity(column.affinity, value)
                }
            })
            .collect();
        let extra = stored.len();
        if extra > 0 {
            self.cursor.problems.push(format!(
                "page {page}: row {rowid}: {extra} values beyond the declared columns"
            ));
            values.extend(stored);
        }
        values
    }
}

impl Iterator for Rows<'_> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        let entry = self.cursor.next_entry()?;
        Some(self.row(&entry))
    }
}

/// A value as a column of `affinity` reads it: SQLite may store a real
/// with no fractional part as an integer in a REAL column, and reads it
/// back as a real.
fn with_affinity(affinity: Affinity, value: Value) -> Value {
    match (affinity, value) {
        (Affinity::Real, Value::Integer(integer)) => Value::Real(integer as f64),
        (_, value) => value,
    }
}

/// An entry of an index b-tree.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    /// The page holding it (interior pages hold entries too).
    pub page: u32,
    /// The key record's values: for an index, the indexed columns then the
    /// rowid; for a `WITHOUT ROWID` table, the primary key columns then the
    /// others.
    pub values: Vec<Value>,
}

/// The entries of an index b-tree, in key order, with
/// [`IndexEntries::problems`] as for [`Rows`].
pub struct IndexEntries<'a> {
    cursor: Cursor<'a>,
    encoding: TextEncoding,
}

impl<'a> IndexEntries<'a> {
    pub(crate) fn new(pages: &'a Pages<'a>, root_page: u32, encoding: TextEncoding) -> Self {
        Self {
            cursor: Cursor::new(pages, root_page, TreeKind::Index),
            encoding,
        }
    }

    /// What went wrong so far, in the order met.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.cursor.problems
    }
}

impl Iterator for IndexEntries<'_> {
    type Item = IndexEntry;

    fn next(&mut self) -> Option<IndexEntry> {
        let entry = self.cursor.next_entry()?;
        let (values, problem) = record::decode(&entry.payload, self.encoding);
        if let Some(problem) = problem {
            self.cursor
                .problems
                .push(format!("page {}: index entry: {problem}", entry.page));
        }
        Some(IndexEntry {
            page: entry.page,
            values,
        })
    }
}
