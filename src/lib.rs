//! SQLite database files, read without SQLite: the header, the schema, the
//! rows of every table and the entries of every index, and the committed
//! frames of a write-ahead log, read from the
//! [file format specification](https://www.sqlite.org/fileformat2.html).
//!
//! Read-only and dependency-free, for evidence: nothing is written, no SQL
//! is run, and the database is borrowed, not copied.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file = std::fs::read("History")?;
//! let wal = std::fs::read("History-wal").unwrap_or_default();
//! let db = sqlite::Database::open_with_wal(&file, &wal)?;
//! for table in &db.tables {
//!     println!("{} (root page {}): {:?}", table.name, table.root_page, table.column_names());
//! }
//! let mut rows = db.rows("urls")?;
//! for row in &mut rows {
//!     println!("{} {:?}", row.rowid, row.values);
//! }
//! for problem in db.problems.iter().chain(rows.problems()) {
//!     eprintln!("{problem}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Damage is reported, never a panic: only a file that isn't a database, or
//! whose header can't be used (page size, reserved bytes), is an error.
//! Pages out of range or past the end of a truncated file, cycles in
//! b-trees and overflow chains, cells and records that don't fit are listed
//! in `problems` and skipped. Every page is visited at most once per walk,
//! so every walk ends and memory stays proportional to the input.

mod btree;
mod bytes;
mod header;
mod pager;
mod record;
mod rows;
mod schema;
mod sql;
mod wal;

use std::collections::HashMap;

pub use header::{Header, TextEncoding};
pub use record::Value;
pub use rows::{IndexEntries, IndexEntry, Row, Rows};
pub use schema::{Affinity, Column, Generated, SchemaEntry, Table, TableKind};
pub use wal::WalSummary;

use pager::Pages;
use wal::Wal;

/// This crate's version, for records of what parsed them.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why a database, or a table or index in it, can't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Shorter than the 100-byte header (this many bytes).
    TooShort(usize),
    /// No "SQLite format 3" magic string.
    NotSqlite,
    /// The page size field isn't a power of two from 512 to 32768, or 1.
    PageSize(u16),
    /// So many reserved bytes per page that fewer than 480 are usable.
    ReservedBytes(u8),
    /// No table of this name.
    NoSuchTable(String),
    /// No index (or `WITHOUT ROWID` table) of this name.
    NoSuchIndex(String),
    /// A `WITHOUT ROWID` table, whose rows aren't read as rows yet (its
    /// entries are, with [`Database::index_entries`]).
    WithoutRowid(String),
    /// A virtual table, whose rows a module provides.
    VirtualTable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort(size) => write!(f, "{size} bytes, too short for a database header"),
            Self::NotSqlite => f.write_str("not an SQLite database (no magic string)"),
            Self::PageSize(raw) => write!(f, "invalid page size field {raw}"),
            Self::ReservedBytes(reserved) => {
                write!(
                    f,
                    "{reserved} reserved bytes per page leave under 480 usable"
                )
            }
            Self::NoSuchTable(name) => write!(f, "no table named {name}"),
            Self::NoSuchIndex(name) => write!(f, "no index named {name}"),
            Self::WithoutRowid(name) => {
                write!(f, "{name} is a WITHOUT ROWID table; read its index entries")
            }
            Self::VirtualTable(name) => write!(f, "{name} is a virtual table, with no b-tree"),
        }
    }
}

impl std::error::Error for Error {}

/// An open database: its header and schema, read on opening; rows are
/// read on demand.
pub struct Database<'a> {
    /// The database header (from page 1 as the log left it, if it did).
    pub header: Header,
    /// Pages in the database: the last commit's count when a log committed,
    /// else the header's when valid, else the file's whole pages.
    pub page_count: u32,
    /// Every row of the schema table, in rowid order.
    pub schema: Vec<SchemaEntry>,
    /// The tables the schema declares, with their columns.
    pub tables: Vec<Table>,
    /// What the write-ahead log held, when one was given and usable.
    pub wal: Option<WalSummary>,
    /// Damage met opening the database and reading its schema.
    pub problems: Vec<String>,
    pages: Pages<'a>,
    schema_table: Table,
}

impl<'a> Database<'a> {
    /// Open a database file as it is on disk.
    ///
    /// # Errors
    /// When it isn't an SQLite database or its header can't be used.
    pub fn open(file: &'a [u8]) -> Result<Self, Error> {
        let mut problems = Vec::new();
        let header = Header::parse(file, &mut problems)?;
        Ok(Self::assemble(file, header, None, problems))
    }

    /// Open a database file with its write-ahead log (the `-wal` file
    /// beside it), as SQLite would see them: each page as the last
    /// committed frame for it left it, else as in the file. Frames after
    /// the last commit, or after the first frame whose salts or checksum
    /// fail, are not applied; [`Database::wal`] counts them. An empty log
    /// is no log; one that can't be used is reported and ignored.
    ///
    /// # Errors
    /// When page 1 (from the log, else the file) isn't an SQLite database
    /// or its header can't be used.
    pub fn open_with_wal(file: &'a [u8], wal: &'a [u8]) -> Result<Self, Error> {
        let mut problems = Vec::new();
        let mut wal = Wal::parse(wal, &mut problems);
        let page_one = wal
            .as_ref()
            .and_then(|wal| wal.pages.get(&1).copied())
            .unwrap_or(file);
        let header = Header::parse(page_one, &mut problems)?;
        if let Some(log) = wal.as_ref() {
            if log.summary.page_size != header.page_size {
                problems.push(format!(
                    "WAL ignored: its page size {} is not the database's {}",
                    log.summary.page_size, header.page_size
                ));
                wal = None;
            }
        }
        Ok(Self::assemble(file, header, wal, problems))
    }

    fn assemble(
        file: &'a [u8],
        header: Header,
        wal: Option<Wal<'a>>,
        mut problems: Vec<String>,
    ) -> Self {
        let page_count = page_count(file.len(), &header, wal.as_ref(), &mut problems);
        let (summary, wal_pages) = match wal {
            Some(wal) => (Some(wal.summary), wal.pages),
            None => (None, HashMap::new()),
        };
        let pages = Pages::new(
            file,
            wal_pages,
            header.page_size,
            header.usable_size(),
            page_count,
        );
        let schema_table = Table::schema();
        let schema = read_schema(&pages, &schema_table, header.text_encoding, &mut problems);
        let tables = schema
            .iter()
            .filter(|entry| entry.is_table())
            .map(|entry| {
                let (table, problem) = Table::from_entry(entry);
                problems.extend(problem);
                table
            })
            .collect();
        Self {
            header,
            page_count,
            schema,
            tables,
            wal: summary,
            problems,
            pages,
            schema_table,
        }
    }

    /// The table named `name` (ignoring ASCII case, as SQL does).
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(name))
    }

    /// The rows of table `name` (or of the schema table, as `sqlite_schema`
    /// or `sqlite_master`), in rowid order, read as the iterator advances.
    ///
    /// # Errors
    /// When there is no such table, or it is a `WITHOUT ROWID` or virtual
    /// table.
    pub fn rows(&self, name: &str) -> Result<Rows<'_>, Error> {
        let table = if Table::is_schema_name(name) {
            &self.schema_table
        } else {
            self.table(name)
                .ok_or_else(|| Error::NoSuchTable(name.to_owned()))?
        };
        match table.kind {
            TableKind::Rowid => Ok(Rows::new(&self.pages, table, self.header.text_encoding)),
            TableKind::WithoutRowid => Err(Error::WithoutRowid(table.name.clone())),
            TableKind::Virtual => Err(Error::VirtualTable(table.name.clone())),
        }
    }

    /// The entries of index `name`, or of `WITHOUT ROWID` table `name`, in
    /// key order, read as the iterator advances.
    ///
    /// # Errors
    /// When there is no index or `WITHOUT ROWID` table of that name.
    pub fn index_entries(&self, name: &str) -> Result<IndexEntries<'_>, Error> {
        let is_index_tree = |entry: &&SchemaEntry| {
            entry.name.eq_ignore_ascii_case(name)
                && (entry.is_index()
                    || self
                        .table(&entry.name)
                        .is_some_and(|table| table.kind == TableKind::WithoutRowid))
        };
        let entry = self
            .schema
            .iter()
            .find(is_index_tree)
            .ok_or_else(|| Error::NoSuchIndex(name.to_owned()))?;
        Ok(IndexEntries::new(
            &self.pages,
            entry.root_page,
            self.header.text_encoding,
        ))
    }
}

/// How many pages the database has, reporting a file shorter than that.
fn page_count(
    file_size: usize,
    header: &Header,
    wal: Option<&Wal>,
    problems: &mut Vec<String>,
) -> u32 {
    let page_size = header.page_size as usize;
    let whole_pages = u32::try_from(file_size / page_size).unwrap_or(u32::MAX);
    let extra = file_size % page_size;
    if extra > 0 {
        problems.push(format!("{extra} bytes after the last whole page"));
    }
    if let Some(committed) = wal
        .map(|wal| wal.summary.database_pages)
        .filter(|&pages| pages > 0)
    {
        return committed;
    }
    match header.valid_page_count() {
        Some(count) => {
            if count > whole_pages {
                problems.push(format!(
                    "the header counts {count} pages, the file holds {whole_pages} (truncated)"
                ));
            }
            count
        }
        None => whole_pages,
    }
}

/// The schema table's rows, as entries.
fn read_schema(
    pages: &Pages,
    schema_table: &Table,
    encoding: TextEncoding,
    problems: &mut Vec<String>,
) -> Vec<SchemaEntry> {
    let mut rows = Rows::new(pages, schema_table, encoding);
    let schema = rows
        .by_ref()
        .map(|row| SchemaEntry::from_row(&row))
        .collect();
    problems.extend(rows.take_problems());
    schema
}
