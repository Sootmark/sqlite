//! What the database uses now: the pages of every b-tree and overflow
//! chain, the rows of every table (so copies of them aren't reported as
//! deleted), and the freelist.

use std::collections::{HashMap, HashSet};

use super::dedupe::Seen;
use super::fit::Shape;
use crate::btree::PAGE_NUMBER_SIZE;
use crate::bytes::u32_at;
use crate::rows::Rows;
use crate::Database;

/// Bytes of a freelist trunk page before its leaf page numbers: the next
/// trunk page, then the count of leaves.
pub(super) const TRUNK_HEADER_SIZE: usize = 8;

/// What a page is used for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Use {
    /// A page of a table or index b-tree.
    Tree,
    /// An overflow page of a live row or entry.
    Overflow,
    /// A freelist trunk page.
    FreelistTrunk,
    /// A freelist leaf page: free, and left as it was.
    FreelistLeaf,
}

/// The database's live content and free pages.
pub(super) struct Survey {
    pub(super) uses: HashMap<u32, Use>,
    /// Pages of the rowid tables' b-trees (the schema table's included), in
    /// page order: where freeblocks and unallocated space are.
    pub(super) table_pages: Vec<u32>,
    /// Freelist trunk pages, each with the count of leaf numbers it holds.
    pub(super) trunks: Vec<(u32, usize)>,
    /// Freelist leaf pages, in freelist order.
    pub(super) free_leaves: Vec<u32>,
    /// Every live row, for telling copies of them from deleted records.
    pub(super) live: Seen,
    pub(super) problems: Vec<String>,
}

impl Survey {
    /// Walk every tree of `db`; note in `shapes` the stored value counts
    /// live rows show.
    pub(super) fn new(db: &Database, shapes: &mut [Shape]) -> Self {
        let mut survey = Self {
            uses: HashMap::new(),
            table_pages: Vec::new(),
            trunks: Vec::new(),
            free_leaves: Vec::new(),
            live: Seen::default(),
            problems: Vec::new(),
        };
        let mut table_pages = HashSet::new();
        for shape in shapes.iter_mut() {
            if let Ok(rows) = db.rows(&shape.table.name) {
                survey.walk_rows(rows, shape, &mut table_pages);
            }
        }
        for entry in &db.schema {
            if let Ok(mut entries) = db.index_entries(&entry.name) {
                entries.by_ref().for_each(drop);
                survey.mark(
                    entries.cursor().tree_pages(),
                    entries.cursor().overflow_pages(),
                );
            }
        }
        survey.table_pages = table_pages.into_iter().collect();
        survey.table_pages.sort_unstable();
        survey.walk_freelist(db);
        survey
    }

    fn walk_rows(&mut self, mut rows: Rows, shape: &mut Shape, table_pages: &mut HashSet<u32>) {
        while let Some((row, stored_count)) = rows.next_counted() {
            shape.note_stored_count(stored_count);
            self.live.insert_row(shape, row.rowid, &row.values);
        }
        let cursor = rows.cursor();
        table_pages.extend(cursor.tree_pages());
        self.mark(cursor.tree_pages(), cursor.overflow_pages());
    }

    fn mark(&mut self, tree_pages: &HashSet<u32>, overflow_pages: &HashSet<u32>) {
        self.uses
            .extend(tree_pages.iter().map(|&page| (page, Use::Tree)));
        self.uses
            .extend(overflow_pages.iter().map(|&page| (page, Use::Overflow)));
    }

    /// Follow the freelist from the header's first trunk page, each page at
    /// most once, so the walk ends.
    fn walk_freelist(&mut self, db: &Database) {
        let mut next = db.header.freelist_trunk;
        while next != 0 {
            if !self.claim(next, Use::FreelistTrunk) {
                break;
            }
            let page = match db.pages.get(next) {
                Ok(page) => page,
                Err(problem) => {
                    self.problems.push(format!("freelist: {problem}"));
                    break;
                }
            };
            let count = self.trunk_leaves(next, page, db.page_count);
            self.trunks.push((next, count));
            next = u32_at(page, 0).unwrap_or_default();
        }
    }

    /// Note the leaves trunk page `number` lists; how many numbers it holds.
    fn trunk_leaves(&mut self, number: u32, page: &[u8], page_count: u32) -> usize {
        let declared = u32_at(page, PAGE_NUMBER_SIZE).map_or(0, |count| count as usize);
        let room = (page.len() - TRUNK_HEADER_SIZE) / PAGE_NUMBER_SIZE;
        if declared > room {
            self.problems.push(format!(
                "freelist trunk page {number}: {declared} leaves declared, room for {room}"
            ));
        }
        let count = declared.min(room);
        for index in 0..count {
            let leaf = u32_at(page, TRUNK_HEADER_SIZE + index * PAGE_NUMBER_SIZE).unwrap_or(0);
            if leaf == 0 || leaf > page_count {
                self.problems.push(format!(
                    "freelist trunk page {number}: leaf page {leaf} is outside the database"
                ));
            } else if self.claim(leaf, Use::FreelistLeaf) {
                self.free_leaves.push(leaf);
            }
        }
        count
    }

    /// Mark page `number` as on the freelist, unless something else claims
    /// it already (a cycle, or a page both free and in use).
    fn claim(&mut self, number: u32, free_use: Use) -> bool {
        if let Some(already) = self.uses.get(&number) {
            self.problems.push(format!(
                "freelist: page {number} is already used ({already:?})"
            ));
            return false;
        }
        self.uses.insert(number, free_use);
        true
    }
}
