//! Recovery of deleted records: rows SQLite freed but didn't overwrite, in
//! freeblocks and unallocated space of table pages, on freelist pages, and
//! in older versions of pages that a write-ahead log keeps.
//!
//! A candidate must read as a record (a header whose serial types account
//! for its bytes, plausible values) and fit a table: as many values as its
//! columns (or as few as live rows store, for columns added later), each
//! of a type SQLite could store there (NULL for the rowid alias, no numbers
//! in a `TEXT` column). Copies of live rows aren't reported.

mod carve;
mod dedupe;
mod fit;
mod layout;
mod survey;

use carve::{PageRef, Scanner};
use dedupe::{deduplicate, Seen};
use fit::Shape;
use survey::{Survey, TRUNK_HEADER_SIZE};

use crate::btree::PAGE_NUMBER_SIZE;
use crate::record::Value;
use crate::schema::{SchemaEntry, Table, TableKind};
use crate::wal::FrameState;
use crate::Database;

/// What recovery found.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovered {
    /// Deleted records in the database as it reads now (the log's committed
    /// pages applied): in freeblocks and unallocated space of table pages,
    /// and on freelist pages.
    pub records: Vec<RecoveredRecord>,
    /// Records in older versions of pages: log frames superseded by a later
    /// commit, never committed, or left from before the log restarted, and
    /// the database file's own copies of pages the log replaces. Rows
    /// deleted or changed since, or never committed.
    pub older_versions: Vec<RecoveredRecord>,
    /// Tables no longer in the schema, read from its deleted rows; records
    /// are matched with them too.
    pub dropped_tables: Vec<Table>,
    /// Damage met: freeblock chains and freelists that loop or leave the
    /// page or the file, layouts that don't fit their page. Damage in live
    /// b-trees is reported by [`Database::rows`] instead.
    pub problems: Vec<String>,
}

/// A record recovered, and where and how.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredRecord {
    /// The table it fits best; `None` for a cell an older or free leaf page
    /// still points to that fits no table (its values are then as stored).
    pub table: Option<String>,
    /// Its rowid, when the cell's start survived (never for a record whose
    /// first bytes a freeblock header overwrote).
    pub rowid: Option<i64>,
    /// One value per column of its table, as [`Database::rows`] gives them
    /// (the rowid alias holds the rowid); `None` where the value was lost:
    /// overwritten, or after the point where a truncated record stops.
    pub values: Vec<Option<Value>>,
    /// The page it was found on (for a log frame, the page it is a version
    /// of).
    pub page: u32,
    /// Where its cell starts on that page, from the page's first byte.
    pub offset: usize,
    /// What the page is, or which older version of it.
    pub page_state: PageState,
    /// Where on the page.
    pub area: Area,
    /// How much of the cell was read as stored.
    pub evidence: Evidence,
    /// How sure the match with its table is.
    pub confidence: Confidence,
    /// Values its record stores (fewer than the table's columns when it was
    /// written before columns were added).
    pub stored_columns: usize,
    /// Other tables it fits as well as its own.
    pub also_fits: Vec<String>,
    /// Whether its payload spilled to overflow pages that are gone or no
    /// longer consistent: the values after the local bytes are lost, the
    /// one they cut kept as far as it goes.
    pub truncated: bool,
}

/// The page a record was found on, or the older version of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PageState {
    /// A page of a table's b-tree in use.
    InUse,
    /// A freelist trunk page: its first bytes list free pages.
    FreelistTrunk,
    /// A freelist leaf page, left as it was when freed.
    FreelistLeaf,
    /// A log frame that a later committed frame for the same page replaced.
    Superseded {
        /// The frame's position in the log, counting from 0.
        frame: usize,
    },
    /// A valid log frame after the last commit: a transaction that never
    /// committed (rolled back, or still open when the log was copied).
    Uncommitted {
        /// The frame's position in the log, counting from 0.
        frame: usize,
    },
    /// A log frame past the end of the valid log: left from before the log
    /// restarted (other salts), or damaged.
    Invalid {
        /// The frame's position in the log, counting from 0.
        frame: usize,
    },
    /// The database file's own copy of a page that a committed log frame
    /// replaces.
    ReplacedInFile,
}

impl PageState {
    /// Whether it is an older version of a page, not the page as the
    /// database reads now.
    #[must_use]
    pub fn is_older_version(self) -> bool {
        !matches!(self, Self::InUse | Self::FreelistTrunk | Self::FreelistLeaf)
    }

    fn of_frame(state: FrameState, frame: usize) -> Option<Self> {
        match state {
            FrameState::Applied => None,
            FrameState::Superseded => Some(Self::Superseded { frame }),
            FrameState::Uncommitted => Some(Self::Uncommitted { frame }),
            FrameState::Invalid => Some(Self::Invalid { frame }),
        }
    }
}

/// Where on its page a record was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    /// A cell the page's cell pointer array lists (on an older version of a
    /// page, or a freelist leaf page still laid out as a table leaf).
    Cell,
    /// A freeblock: space freed inside the cell content area.
    Freeblock,
    /// Between the cell pointer array and the cell content area: cells
    /// freed at the content area's start, and leftovers of earlier layouts.
    Unallocated,
    /// A freelist page that no longer reads as a table page, searched
    /// whole (a trunk page after its list of leaves).
    FreePage,
}

/// How much of a record's cell was read as stored, most first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Evidence {
    /// A whole cell a page's cell pointer array points to.
    CellPointer,
    /// A whole cell: its payload size agrees with its record, its rowid is
    /// read.
    Cell,
    /// An intact record header, its payload size and rowid overwritten.
    Record,
    /// A record whose header size (and perhaps first serial type) was
    /// overwritten, rebuilt from its table's shape: a lost first value is
    /// NULL for the rowid alias, else inferred from where the cell ends.
    Shape,
}

/// How sure the match of a record with its table is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    /// Values of unusual types for their columns, a single value, or a
    /// damaged record that fits several tables.
    Low,
    /// A damaged record, or a whole cell that fits several tables.
    Medium,
    /// A whole cell whose every value has its column's usual type, fitting
    /// one table only.
    High,
}

impl Database<'_> {
    /// Recover deleted records from free space, the freelist and, when a
    /// log was given, older versions of pages. Records are matched with the
    /// rowid tables of the schema, the schema table itself, and tables
    /// dropped from it whose deleted schema rows are recovered.
    ///
    /// Every page is searched at most once per source and every chain
    /// followed at most once per page, so recovery ends, and memory stays
    /// proportional to the database. A database written with
    /// `secure_delete` on has its freed space zeroed: nothing is recovered
    /// from it.
    #[must_use]
    pub fn recover(&self) -> Recovered {
        let mut shapes: Vec<Shape> = std::iter::once(&self.schema_table)
            .chain(
                self.tables
                    .iter()
                    .filter(|table| table.kind == TableKind::Rowid),
            )
            .filter_map(Shape::new)
            .collect();
        let survey = Survey::new(self, &mut shapes);
        let (mut records, mut older_versions, mut problems) = self.search(&survey, &shapes);
        let dropped_tables = self.dropped_tables(records.iter().chain(&older_versions));
        if !dropped_tables.is_empty() {
            shapes.extend(dropped_tables.iter().filter_map(Shape::new));
            (records, older_versions, problems) = self.search(&survey, &shapes);
        }
        problems.splice(0..0, survey.problems.iter().cloned());
        Recovered {
            records,
            older_versions,
            dropped_tables,
            problems,
        }
    }

    /// Search every source for records of `shapes`: current records, older
    /// versions, problems.
    fn search(
        &self,
        survey: &Survey,
        shapes: &[Shape],
    ) -> (Vec<RecoveredRecord>, Vec<RecoveredRecord>, Vec<String>) {
        let mut scanner =
            Scanner::new(&self.pages, &survey.uses, shapes, self.header.text_encoding);
        self.search_current(&mut scanner, survey);
        let current = std::mem::take(&mut scanner.found);
        self.search_older(&mut scanner);
        let older = std::mem::take(&mut scanner.found);
        let finish = |found: Vec<carve::Found>, seen: Seen| {
            let records = found.into_iter().map(|found| found.record).collect();
            let mut records = deduplicate(records, seen, shapes);
            records.sort_by_key(|record| (record.page_state, record.page, record.offset));
            records
        };
        (
            finish(current, survey.live.clone()),
            finish(older, survey.live.clone()),
            scanner.problems,
        )
    }

    /// Table pages in use, then the freelist.
    fn search_current(&self, scanner: &mut Scanner, survey: &Survey) {
        let page = |number: u32, state: PageState| {
            let bytes = self.pages.get(number).ok()?;
            Some(PageRef {
                number,
                bytes,
                state,
            })
        };
        for page in survey
            .table_pages
            .iter()
            .filter_map(|&number| page(number, PageState::InUse))
        {
            scanner.scan_tree_page(&page);
        }
        for &(number, leaves) in &survey.trunks {
            if let Some(page) = page(number, PageState::FreelistTrunk) {
                let after_list = TRUNK_HEADER_SIZE + leaves * PAGE_NUMBER_SIZE;
                scanner.scan_free_page(&page, after_list);
            }
        }
        for page in survey
            .free_leaves
            .iter()
            .filter_map(|&number| page(number, PageState::FreelistLeaf))
        {
            if !scanner.scan_tree_page(&page) {
                scanner.scan_free_page(&page, 0);
            }
        }
    }

    /// Every log frame but the applied ones, then the file's copies of the
    /// pages the log replaces: table pages only.
    fn search_older(&self, scanner: &mut Scanner) {
        let usable = self.pages.usable_size();
        for frame in &self.log_frames {
            let Some(state) = PageState::of_frame(frame.state, frame.index) else {
                continue;
            };
            if frame.page_number != 0 {
                scanner.scan_tree_page(&PageRef {
                    number: frame.page_number,
                    bytes: &frame.page[..usable],
                    state,
                });
            }
        }
        for (number, bytes) in self.pages.replaced_file_pages() {
            scanner.scan_tree_page(&PageRef {
                number,
                bytes,
                state: PageState::ReplacedInFile,
            });
        }
    }

    /// Tables that deleted schema rows declare and the schema no longer
    /// does, each once.
    fn dropped_tables<'r>(&self, records: impl Iterator<Item = &'r RecoveredRecord>) -> Vec<Table> {
        let mut dropped: Vec<Table> = Vec::new();
        for entry in records.filter_map(dropped_schema_entry) {
            let known = |name: &str| {
                self.table(name).is_some()
                    || dropped
                        .iter()
                        .any(|table| table.name.eq_ignore_ascii_case(name))
            };
            if known(&entry.name) {
                continue;
            }
            let (table, problem) = Table::from_entry(&entry);
            if problem.is_none() && table.kind == TableKind::Rowid {
                dropped.push(table);
            }
        }
        dropped
    }
}

/// The schema entry a recovered schema table row declares, if it is a
/// whole one declaring a table.
fn dropped_schema_entry(record: &RecoveredRecord) -> Option<SchemaEntry> {
    if record.table.as_deref() != Some(crate::schema::SCHEMA_TABLE_NAME) || record.truncated {
        return None;
    }
    let text = |index: usize| match record.values.get(index) {
        Some(Some(Value::Text(text))) => Some(text.clone()),
        _ => None,
    };
    let root_page = match record.values.get(3) {
        Some(Some(Value::Integer(page))) => u32::try_from(*page).ok()?,
        _ => return None,
    };
    let entry = SchemaEntry {
        kind: text(0)?,
        name: text(1)?,
        table_name: text(2)?,
        root_page,
        sql: Some(text(4)?),
    };
    entry.is_table().then_some(entry)
}
