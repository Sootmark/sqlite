//! The schema table (`sqlite_schema`, also called `sqlite_master`) on page
//! 1, and the tables it declares.

use crate::record::Value;
use crate::rows::Row;
use crate::sql::{parse_create_table, Definition};

/// The schema table's root page.
pub(crate) const SCHEMA_ROOT_PAGE: u32 = 1;
/// The names the schema table answers to.
const SCHEMA_TABLE_NAMES: [&str; 2] = ["sqlite_schema", "sqlite_master"];

/// One row of the schema table: a table, index, view or trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    /// `type`: `table`, `index`, `view` or `trigger`.
    pub kind: String,
    /// `name`: the object's name.
    pub name: String,
    /// `tbl_name`: the table an index or trigger belongs to (for a table or
    /// view, its own name).
    pub table_name: String,
    /// `rootpage`: the b-tree's root page; 0 for views, triggers and
    /// virtual tables.
    pub root_page: u32,
    /// `sql`: the `CREATE` statement, normalized by SQLite; `None` for the
    /// indexes behind `UNIQUE` and `PRIMARY KEY` constraints.
    pub sql: Option<String>,
}

impl SchemaEntry {
    /// Whether it declares a table.
    #[must_use]
    pub fn is_table(&self) -> bool {
        self.kind == "table"
    }

    /// Whether it declares an index.
    #[must_use]
    pub fn is_index(&self) -> bool {
        self.kind == "index"
    }

    /// The entry a schema table row describes; values of the wrong type
    /// read as empty.
    pub(crate) fn from_row(row: &Row) -> Self {
        let text = |index: usize| {
            row.values
                .get(index)
                .and_then(Value::as_text)
                .map(str::to_owned)
        };
        let root_page = row
            .values
            .get(3)
            .and_then(Value::as_integer)
            .and_then(|page| u32::try_from(page).ok())
            .unwrap_or_default();
        Self {
            kind: text(0).unwrap_or_default(),
            name: text(1).unwrap_or_default(),
            table_name: text(2).unwrap_or_default(),
            root_page,
            sql: text(4),
        }
    }
}

/// How a table stores its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// A table b-tree keyed by rowid.
    Rowid,
    /// `WITHOUT ROWID`: an index b-tree keyed by the primary key.
    WithoutRowid,
    /// A virtual table: rows come from a module (its shadow tables, if any,
    /// are ordinary tables of their own).
    Virtual,
}

/// A table, as the schema declares it.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    /// Its name.
    pub name: String,
    /// Its b-tree's root page (0 for a virtual table).
    pub root_page: u32,
    /// The `CREATE TABLE` statement.
    pub sql: Option<String>,
    /// How it stores its rows.
    pub kind: TableKind,
    /// Its columns in declared order; empty when the statement couldn't be
    /// read (the rows' values then come unnamed, as stored).
    pub columns: Vec<Column>,
}

impl Table {
    /// The table a schema entry declares, and a problem when its statement
    /// can't be read.
    pub(crate) fn from_entry(entry: &SchemaEntry) -> (Self, Option<String>) {
        let definition = entry.sql.as_deref().and_then(parse_create_table);
        let problem = definition.is_none().then(|| {
            format!(
                "table {}: CREATE TABLE statement not understood",
                entry.name
            )
        });
        let (kind, columns) = match definition {
            Some(Definition::Virtual) => (TableKind::Virtual, Vec::new()),
            Some(Definition::Table {
                columns,
                without_rowid: true,
            }) => (TableKind::WithoutRowid, columns),
            Some(Definition::Table { columns, .. }) => (TableKind::Rowid, columns),
            None => (TableKind::Rowid, Vec::new()),
        };
        let table = Self {
            name: entry.name.clone(),
            root_page: entry.root_page,
            sql: entry.sql.clone(),
            kind,
            columns,
        };
        (table, problem)
    }

    /// The schema table itself, which has no entry of its own.
    pub(crate) fn schema() -> Self {
        let column = |name: &str, declared_type: &str| Column {
            name: name.to_owned(),
            declared_type: declared_type.to_owned(),
            affinity: Affinity::of_declared_type(declared_type),
            rowid_alias: false,
            default: Value::Null,
            generated: None,
        };
        Self {
            name: SCHEMA_TABLE_NAMES[0].to_owned(),
            root_page: SCHEMA_ROOT_PAGE,
            sql: None,
            kind: TableKind::Rowid,
            columns: vec![
                column("type", "text"),
                column("name", "text"),
                column("tbl_name", "text"),
                column("rootpage", "integer"),
                column("sql", "text"),
            ],
        }
    }

    /// Whether `name` is one of the schema table's names.
    pub(crate) fn is_schema_name(name: &str) -> bool {
        SCHEMA_TABLE_NAMES
            .iter()
            .any(|schema| schema.eq_ignore_ascii_case(name))
    }

    /// The names of its columns, in order.
    #[must_use]
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }
}

/// A column of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Its name, unquoted.
    pub name: String,
    /// Its declared type, words separated by single spaces (`VARCHAR(10)`,
    /// `UNSIGNED BIG INT`); empty when none.
    pub declared_type: String,
    /// The type affinity the declared type gives it.
    pub affinity: Affinity,
    /// Whether it is an `INTEGER PRIMARY KEY`, which aliases the rowid: the
    /// record stores NULL for it and the row's value is its rowid.
    pub rowid_alias: bool,
    /// What a row stored before the column was added (`ALTER TABLE … ADD
    /// COLUMN … DEFAULT`) reads as: the default when it is a literal, else
    /// NULL.
    pub default: Value,
    /// Whether it is generated from other columns, and how.
    pub generated: Option<Generated>,
}

/// A generated column's storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generated {
    /// Computed on write and stored in the record.
    Stored,
    /// Computed on read and not stored: this reader, which evaluates no
    /// SQL, gives NULL.
    Virtual,
}

/// The type affinity a column's declared type gives it (SQLite's rules, in
/// order: `INT`; `CHAR`, `CLOB` or `TEXT`; `BLOB` or no type; `REAL`,
/// `FLOA` or `DOUB`; else numeric).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Declared types containing `INT`.
    Integer,
    /// `CHAR`, `CLOB`, `TEXT`.
    Text,
    /// `BLOB`, or no type.
    Blob,
    /// `REAL`, `FLOA`, `DOUB`. Integers stored in such a column read as
    /// reals, as SQLite reads them.
    Real,
    /// Anything else.
    Numeric,
}

impl Affinity {
    /// The affinity of a declared type.
    #[must_use]
    pub fn of_declared_type(declared_type: &str) -> Self {
        let upper = declared_type.to_ascii_uppercase();
        let has = |words: &[&str]| words.iter().any(|word| upper.contains(word));
        if has(&["INT"]) {
            Self::Integer
        } else if has(&["CHAR", "CLOB", "TEXT"]) {
            Self::Text
        } else if upper.is_empty() || has(&["BLOB"]) {
            Self::Blob
        } else if has(&["REAL", "FLOA", "DOUB"]) {
            Self::Real
        } else {
            Self::Numeric
        }
    }
}
