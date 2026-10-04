//! B-tree pages, cells and overflow chains, walked in key order one cell at
//! a time.
//!
//! Damage never stops the walk: a page that can't be read, isn't the kind
//! its tree needs, or was already visited (a cycle, or a page claimed twice)
//! is reported and skipped, and so is a cell that doesn't fit its page.
//! Every page is visited at most once per walk, so a walk ends, and memory
//! stays proportional to the file.

use std::collections::HashSet;

use crate::bytes::{u16_at, u32_at, u8_at, varint_at};
use crate::header::HEADER_SIZE;
use crate::pager::Pages;

/// Page type flags.
const INDEX_INTERIOR: u8 = 0x02;
pub(crate) const TABLE_INTERIOR: u8 = 0x05;
const INDEX_LEAF: u8 = 0x0a;
pub(crate) const TABLE_LEAF: u8 = 0x0d;
/// B-tree page header sizes.
pub(crate) const LEAF_HEADER_SIZE: usize = 8;
pub(crate) const INTERIOR_HEADER_SIZE: usize = 12;
/// Bytes of a cell pointer, a child pointer, and an overflow pointer.
pub(crate) const CELL_POINTER_SIZE: usize = 2;
pub(crate) const PAGE_NUMBER_SIZE: usize = 4;

/// Which of the two b-tree families a tree belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeKind {
    /// Keyed by rowid, data in the leaves.
    Table,
    /// Keyed by a record, which is the whole entry.
    Index,
}

impl TreeKind {
    /// Most payload kept on the b-tree page before spilling to overflow
    /// pages, for usable size `u`.
    fn max_local(self, u: usize) -> usize {
        match self {
            Self::Table => u - 35,
            Self::Index => (u - 12) * 64 / 255 - 23,
        }
    }
}

/// Least payload kept on the b-tree page once it spills.
fn min_local(u: usize) -> usize {
    (u - 12) * 32 / 255 - 23
}

/// Bytes of a `payload_size`-byte payload stored on the b-tree page itself,
/// the rest going to overflow pages.
pub(crate) fn local_size(kind: TreeKind, payload_size: u64, u: usize) -> usize {
    let max = kind.max_local(u);
    if payload_size <= max as u64 {
        return payload_size as usize;
    }
    let min = min_local(u);
    let surplus = min as u64 + (payload_size - min as u64) % (u as u64 - 4);
    if surplus <= max as u64 {
        surplus as usize
    } else {
        min
    }
}

/// A page's role in its tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageType {
    kind: TreeKind,
    leaf: bool,
}

impl PageType {
    fn from_flag(flag: u8) -> Option<Self> {
        let (kind, leaf) = match flag {
            INDEX_INTERIOR => (TreeKind::Index, false),
            TABLE_INTERIOR => (TreeKind::Table, false),
            INDEX_LEAF => (TreeKind::Index, true),
            TABLE_LEAF => (TreeKind::Table, true),
            _ => return None,
        };
        Some(Self { kind, leaf })
    }

    fn header_size(self) -> usize {
        if self.leaf {
            LEAF_HEADER_SIZE
        } else {
            INTERIOR_HEADER_SIZE
        }
    }
}

/// An entry found in a tree: a table row's payload with its rowid, or an
/// index entry's key.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The page holding the cell.
    pub(crate) page: u32,
    /// The rowid (table trees only).
    pub(crate) rowid: Option<i64>,
    /// The whole payload, overflow included (cut short if the chain is
    /// damaged).
    pub(crate) payload: Vec<u8>,
}

/// A b-tree page being walked.
struct Node<'a> {
    number: u32,
    bytes: &'a [u8],
    page_type: PageType,
    /// Where the b-tree page header starts: 100 on page 1, else 0.
    header_start: usize,
    cells: usize,
    /// Steps taken through this page. On a leaf, step i is cell i. On an
    /// interior page, step 2i descends into cell i's child (or, at i =
    /// cells, the right-most child) and step 2i + 1 is cell i itself.
    step: usize,
}

/// What the next step through a page does.
enum Step {
    /// Descend into the left child of this cell (the right-most child when
    /// it is one past the last cell).
    Descend(usize),
    /// Yield this cell's entry.
    Cell(usize),
    /// Pass a table interior cell, whose key the leaves already hold.
    Skip,
}

impl Node<'_> {
    fn cell_pointer_array(&self) -> usize {
        self.header_start + self.page_type.header_size()
    }

    fn steps(&self) -> usize {
        if self.page_type.leaf {
            self.cells
        } else {
            2 * self.cells + 1
        }
    }

    /// Where cell `index` starts, if its pointer lands on the page.
    fn cell_offset(&self, index: usize) -> Result<usize, String> {
        let at = self.cell_pointer_array() + index * CELL_POINTER_SIZE;
        let offset = u16_at(self.bytes, at).map_or(0, usize::from);
        if offset < self.cell_pointer_array() || offset >= self.bytes.len() {
            return Err(format!(
                "page {}: cell {index} points outside the page (offset {offset})",
                self.number
            ));
        }
        Ok(offset)
    }

    /// Take the next step through the page; `None` once it is done.
    fn next_step(&mut self) -> Option<Step> {
        if self.step == self.steps() {
            return None;
        }
        let step = self.step;
        self.step += 1;
        Some(match (self.page_type.leaf, step % 2 == 0) {
            (true, _) => Step::Cell(step),
            (false, true) => Step::Descend(step / 2),
            (false, false) if self.page_type.kind == TreeKind::Table => Step::Skip,
            (false, false) => Step::Cell(step / 2),
        })
    }

    /// The left child page of `cell`, or the right-most child when `cell`
    /// is one past the last.
    fn child(&self, cell: usize) -> Result<u32, String> {
        let at = if cell == self.cells {
            self.header_start + LEAF_HEADER_SIZE
        } else {
            self.cell_offset(cell)?
        };
        u32_at(self.bytes, at)
            .ok_or_else(|| format!("page {}: child pointer of cell {cell} cut off", self.number))
    }
}

/// A walk through one b-tree, in key order, yielding its entries.
pub(crate) struct Cursor<'a> {
    pages: &'a Pages<'a>,
    kind: TreeKind,
    stack: Vec<Node<'a>>,
    visited: HashSet<u32>,
    overflow_pages: HashSet<u32>,
    /// What went wrong on the way, in the order met.
    pub(crate) problems: Vec<String>,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(pages: &'a Pages<'a>, root: u32, kind: TreeKind) -> Self {
        let mut cursor = Self {
            pages,
            kind,
            stack: Vec::new(),
            visited: HashSet::new(),
            overflow_pages: HashSet::new(),
            problems: Vec::new(),
        };
        cursor.descend(root);
        cursor
    }

    /// The tree's pages walked so far.
    pub(crate) fn tree_pages(&self) -> &HashSet<u32> {
        &self.visited
    }

    /// The overflow pages read so far.
    pub(crate) fn overflow_pages(&self) -> &HashSet<u32> {
        &self.overflow_pages
    }

    /// Push page `number`, if it can be read as a page of this tree.
    fn descend(&mut self, number: u32) {
        match self.node(number) {
            Ok(node) => self.stack.push(node),
            Err(problem) => self.problems.push(problem),
        }
    }

    fn node(&mut self, number: u32) -> Result<Node<'a>, String> {
        if !self.visited.insert(number) {
            return Err(format!(
                "page {number} reached twice in one b-tree (a cycle?)"
            ));
        }
        let bytes = self.pages.get(number)?;
        let header_start = if number == 1 { HEADER_SIZE } else { 0 };
        let flag = u8_at(bytes, header_start).unwrap_or_default();
        let page_type = PageType::from_flag(flag)
            .filter(|page_type| page_type.kind == self.kind)
            .ok_or_else(|| {
                format!(
                    "page {number}: type 0x{flag:02x} is not a {:?} b-tree page",
                    self.kind
                )
            })?;
        let mut node = Node {
            number,
            bytes,
            page_type,
            header_start,
            cells: 0,
            step: 0,
        };
        node.cells = self.cell_count(&node);
        Ok(node)
    }

    /// The page's cell count, cut to the pointers that fit on the page.
    fn cell_count(&mut self, node: &Node) -> usize {
        let declared = u16_at(node.bytes, node.header_start + 3).map_or(0, usize::from);
        let room = node.bytes.len().saturating_sub(node.cell_pointer_array()) / CELL_POINTER_SIZE;
        if declared > room {
            self.problems.push(format!(
                "page {}: {declared} cells declared, room for {room} pointers",
                node.number
            ));
        }
        declared.min(room)
    }

    /// The next entry in key order, or `None` at the end of the tree.
    pub(crate) fn next_entry(&mut self) -> Option<Entry> {
        while let Some(node) = self.stack.last_mut() {
            match node.next_step() {
                None => {
                    self.stack.pop();
                }
                Some(Step::Descend(cell)) => match node.child(cell) {
                    Ok(child) => self.descend(child),
                    Err(problem) => self.problems.push(problem),
                },
                Some(Step::Skip) => {}
                Some(Step::Cell(cell)) => {
                    if let Some(entry) = self.entry(cell) {
                        return Some(entry);
                    }
                }
            }
        }
        None
    }

    /// The entry in cell `index` of the page on top of the stack.
    fn entry(&mut self, index: usize) -> Option<Entry> {
        let node = self.stack.last()?;
        let (number, bytes, leaf) = (node.number, node.bytes, node.page_type.leaf);
        let cell = match node.cell_offset(index) {
            Ok(offset) => &bytes[offset..],
            Err(problem) => {
                self.problems.push(problem);
                return None;
            }
        };
        // Interior index cells start with their left child pointer.
        let skip = if leaf { 0 } else { PAGE_NUMBER_SIZE };
        let Some((rowid, payload)) = self.cell(cell, skip) else {
            self.problems
                .push(format!("page {number}: cell {index} cut off"));
            return None;
        };
        Some(Entry {
            page: number,
            rowid,
            payload,
        })
    }

    /// A cell's rowid (table leaves) and whole payload, from `cell`, the
    /// bytes from the cell's start to the end of the page.
    fn cell(&mut self, cell: &[u8], mut at: usize) -> Option<(Option<i64>, Vec<u8>)> {
        let (payload_size, length) = varint_at(cell, at)?;
        at += length;
        let rowid = if self.kind == TreeKind::Table {
            let (rowid, length) = varint_at(cell, at)?;
            at += length;
            Some(rowid as i64)
        } else {
            None
        };
        let usable = self.pages.usable_size();
        let local = local_size(self.kind, payload_size, usable);
        let local_bytes = cell.get(at..at + local)?;
        let mut payload = local_bytes.to_vec();
        if payload_size > local as u64 {
            let first = u32_at(cell, at + local)?;
            self.overflow(first, payload_size, &mut payload);
        }
        Some((rowid, payload))
    }

    /// Append the overflow chain from page `first` to `payload`, until it
    /// holds `payload_size` bytes or the chain breaks.
    fn overflow(&mut self, first: u32, payload_size: u64, payload: &mut Vec<u8>) {
        let mut chain = HashSet::new();
        let mut next = first;
        while (payload.len() as u64) < payload_size {
            if !chain.insert(next) {
                self.problems
                    .push(format!("overflow chain loops back to page {next}"));
                return;
            }
            let page = match self.pages.get(next) {
                Ok(page) => page,
                Err(problem) => {
                    self.problems
                        .push(format!("overflow chain broken: {problem}"));
                    return;
                }
            };
            self.overflow_pages.insert(next);
            let wanted = payload_size - payload.len() as u64;
            let content = &page[PAGE_NUMBER_SIZE..];
            let take = content
                .len()
                .min(usize::try_from(wanted).unwrap_or(usize::MAX));
            payload.extend_from_slice(&content[..take]);
            next = u32_at(page, 0).unwrap_or_default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Usable size of a 4096-byte page without reserved bytes.
    const U: usize = 4096;

    #[test]
    fn small_payloads_stay_on_the_page() {
        assert_eq!(local_size(TreeKind::Table, 4061, U), 4061);
        assert_eq!(local_size(TreeKind::Index, 1002, U), 1002);
    }

    #[test]
    fn large_payloads_keep_k_or_m_bytes() {
        // X = U - 35 = 4061 (index: 1002); M = (U - 12) * 32 / 255 - 23 =
        // 489; K = M + (P - M) % (U - 4).
        assert_eq!(
            local_size(TreeKind::Table, 5000, U),
            489 + (5000 - 489) % 4092
        );
        assert_eq!(local_size(TreeKind::Table, 489 + 4092 + 100, U), 589);
        // K over X: only M stays.
        assert_eq!(local_size(TreeKind::Table, 4062, U), 489);
        assert_eq!(local_size(TreeKind::Index, 1003, U), 489);
    }
}
