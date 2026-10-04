//! Finding records in the bytes of a page: whole cells where a page points
//! to them or where their payload size and rowid still agree with their
//! record, and freed cells whose first bytes a freeblock header overwrote,
//! rebuilt from the shape of the tables they could belong to.
//!
//! A table leaf cell is a payload size varint, a rowid varint, then the
//! record (a header size varint, serial types, values), cut after the local
//! bytes by an overflow page number when it spills. Freeing a cell inside
//! the content area writes a four-byte freeblock header over its start, so
//! the record header starts at byte 2, 3 or later of the freed cell
//! depending on the two varints' widths: from byte 4 on, it is intact.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::fit::{infer_serial, number_sizes, Agreement, Shape, Slot, Strictness};
use super::layout::{PageLayout, FREEBLOCK_HEADER_SIZE};
use super::survey::Use;
use super::{Area, Confidence, Evidence, PageState, RecoveredRecord};
use crate::btree::{local_size, TreeKind, PAGE_NUMBER_SIZE};
use crate::bytes::{u32_at, varint_at};
use crate::header::{TextEncoding, HEADER_SIZE};
use crate::pager::Pages;
use crate::record::{strict_header, SerialType, Value};

/// The most bytes a cell's payload size and rowid varints take before its
/// record: four for payloads under 2^28 bytes (SQLite's limit is 10^9),
/// nine for any rowid.
const MAX_CELL_PREFIX: usize = 13;
/// Largest values of one- and two-byte varints.
const MAX_ONE_BYTE_VARINT: u64 = 0x7f;
const MAX_TWO_BYTE_VARINT: u64 = 0x3fff;
/// Bytes of a text or blob whose serial type takes one varint byte at most.
const MAX_ONE_BYTE_SERIAL_SIZE: u64 = 57;

/// A page being searched.
pub(super) struct PageRef<'p> {
    pub(super) number: u32,
    /// Its usable bytes.
    pub(super) bytes: &'p [u8],
    pub(super) state: PageState,
}

impl PageRef<'_> {
    /// Where its b-tree header starts: after the database header on page 1.
    fn header_start(&self) -> usize {
        if self.number == 1 {
            HEADER_SIZE
        } else {
            0
        }
    }
}

/// How the start of a freed cell was damaged, as tried.
#[derive(Debug, Clone, Copy)]
enum Damage {
    /// The record starts intact this many bytes into the cell; the payload
    /// size and rowid before it are lost (both varints together took at
    /// least four bytes).
    RecordAt(usize),
    /// The record starts at byte 3 and only its header size is lost.
    HeaderSize,
    /// The record starts at byte 2: its header size and its first serial
    /// type, `width` bytes, are lost (one-byte payload size and rowid, so
    /// the payload is under 128 bytes).
    FirstSerialType { width: usize },
}

impl Damage {
    /// Every way a freed cell's start can be damaged.
    fn all() -> impl Iterator<Item = Self> {
        (FREEBLOCK_HEADER_SIZE..=MAX_CELL_PREFIX)
            .map(Self::RecordAt)
            .chain([
                Self::HeaderSize,
                Self::FirstSerialType { width: 1 },
                Self::FirstSerialType { width: 2 },
            ])
    }
}

/// How the first value's serial type was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstValue {
    /// Read from the record.
    Read,
    /// Lost, but the rowid alias: NULL.
    RowidAlias,
    /// Lost; it takes the bytes up to where the freeblock (or the next
    /// cell) starts, which is right unless a later cell took the freed
    /// cell's tail.
    EndsAtLimit,
    /// Lost; a number of one of the widths numbers take, kept only when a
    /// cell starts right after it.
    NumberWidth,
}

/// A record located on a page, before its values are read.
#[derive(Debug, Clone)]
struct Layout {
    cell_start: usize,
    payload_start: usize,
    /// The record header's size, read or implied.
    header_len: usize,
    slots: Vec<Slot>,
    rowid: Option<i64>,
    evidence: Evidence,
    first: FirstValue,
}

impl Layout {
    fn payload_size(&self) -> u64 {
        self.header_len as u64 + self.slots.iter().map(|slot| slot.size).sum::<u64>()
    }

    fn strictness(&self) -> Strictness {
        match self.evidence {
            Evidence::CellPointer => Strictness::Pointed,
            Evidence::Cell => Strictness::Whole,
            Evidence::Record | Evidence::Shape => Strictness::Damaged,
        }
    }
}

/// A record's values, read from the page (and its overflow pages).
struct Body {
    /// Where the cell's bytes on the page end.
    end: usize,
    /// The values as stored, `None` where lost.
    values: Vec<Option<Value>>,
    truncated: bool,
}

/// How good a reading is, best greatest: every value read of its
/// column's usual type, all columns stored, the evidence, the values read,
/// ending where the space does.
type Rank = (bool, bool, u8, usize, bool);

/// A record that could be a row of one table (or of none, for a cell a
/// page points to that fits no table).
struct Candidate {
    shape: Option<usize>,
    agreement: Option<Agreement>,
    layout: Layout,
    body: Body,
}

impl Candidate {
    /// How well it fits; the better fit wins.
    fn rank(&self, limit: usize) -> Rank {
        let agreement = self.agreement.unwrap_or(Agreement {
            unexpected: usize::MAX,
            read_values: 0,
            read_bytes: 0,
            exact: false,
        });
        (
            agreement.unexpected == 0,
            agreement.exact,
            evidence_strength(self.layout.evidence),
            agreement.read_values,
            self.body.end == limit,
        )
    }

    /// How well it fits its table, whatever the evidence: equal for tables
    /// it fits as well.
    fn fit(&self) -> Option<(bool, bool, usize)> {
        self.agreement.map(|agreement| {
            (
                agreement.unexpected == 0,
                agreement.exact,
                agreement.read_values,
            )
        })
    }
}

fn evidence_strength(evidence: Evidence) -> u8 {
    match evidence {
        Evidence::Shape => 0,
        Evidence::Record => 1,
        Evidence::Cell => 2,
        Evidence::CellPointer => 3,
    }
}

/// A record found, where its bytes end on the page, and how good a
/// reading it is.
pub(super) struct Found {
    pub(super) record: RecoveredRecord,
    end: usize,
    rank: Rank,
    /// Bytes of values read rather than inferred.
    read_bytes: u64,
    /// Whether its first value's size is a guess that only a cell right
    /// after it, or the end of the space, confirms.
    guessed: bool,
}

/// Positions of readings along a run of damaged cells: where a reading
/// starts, and which of the readings there.
type Path = Vec<(usize, usize)>;

/// The run of readings from `anchor` that ends exactly at `limit`, reading
/// the most bytes, if any does.
fn exact_path(readings: &HashMap<usize, Vec<Found>>, anchor: usize, limit: usize) -> Option<Path> {
    // From the last position back: the best way from each to `limit`, as
    // bytes read and the reading taken first.
    let mut positions: Vec<usize> = readings.keys().copied().collect();
    positions.sort_unstable_by(|a, b| b.cmp(a));
    let mut best: HashMap<usize, (u64, usize)> = HashMap::new();
    for at in positions {
        let ways = readings[&at]
            .iter()
            .enumerate()
            .filter_map(|(choice, found)| {
                let rest = if found.end == limit {
                    Some(0)
                } else {
                    best.get(&found.end).map(|&(bytes, _)| bytes)
                };
                rest.map(|rest| (rest + found.read_bytes, choice))
            });
        if let Some(way) = ways.max_by_key(|&(bytes, choice)| (bytes, std::cmp::Reverse(choice))) {
            best.insert(at, way);
        }
    }
    let mut path = Vec::new();
    let mut at = anchor;
    while at != limit {
        let &(_, choice) = best.get(&at)?;
        path.push((at, choice));
        at = readings[&at][choice].end;
    }
    Some(path)
}

/// The run of best readings from `anchor`, a guessed one only where a
/// reading follows it.
fn greedy_path(readings: &HashMap<usize, Vec<Found>>, anchor: usize, limit: usize) -> Path {
    let mut path = Vec::new();
    let mut at = anchor;
    while let Some(found) = readings.get(&at) {
        let settled = |found: &Found| {
            !found.guessed
                || found.end == limit
                || readings
                    .get(&found.end)
                    .is_some_and(|next| !next.is_empty())
        };
        let Some((choice, found)) = found
            .iter()
            .enumerate()
            .filter(|(_, found)| settled(found))
            .max_by_key(|(choice, found)| (found.rank, std::cmp::Reverse(*choice)))
        else {
            break;
        };
        path.push((at, choice));
        at = found.end;
    }
    path
}

/// Searches pages for records of a set of tables.
pub(super) struct Scanner<'s, 'a> {
    pages: &'s Pages<'a>,
    uses: &'s HashMap<u32, Use>,
    shapes: &'s [Shape],
    encoding: TextEncoding,
    usable: usize,
    /// The most values any table stores: longer headers aren't records.
    max_columns: usize,
    pub(super) found: Vec<Found>,
    pub(super) problems: Vec<String>,
}

impl<'s, 'a> Scanner<'s, 'a> {
    pub(super) fn new(
        pages: &'s Pages<'a>,
        uses: &'s HashMap<u32, Use>,
        shapes: &'s [Shape],
        encoding: TextEncoding,
    ) -> Self {
        Self {
            pages,
            uses,
            shapes,
            encoding,
            usable: pages.usable_size(),
            max_columns: shapes.iter().map(Shape::stored_len).max().unwrap_or(0),
            found: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Search a page that reads as a table b-tree page (else nothing): the
    /// cells it lists unless they are live, its freeblocks and its
    /// unallocated space. Damage to its layout is reported for pages in use.
    /// Returns whether it read as one.
    pub(super) fn scan_tree_page(&mut self, page: &PageRef) -> bool {
        let mut problems = Vec::new();
        let layout = PageLayout::read(page.bytes, page.header_start(), &mut problems);
        if page.state == PageState::InUse {
            let number = page.number;
            self.problems.extend(
                problems
                    .into_iter()
                    .map(|problem| format!("page {number}: {problem}")),
            );
        }
        let Some(layout) = layout else {
            return false;
        };
        if layout.leaf {
            if page.state != PageState::InUse {
                for &offset in &layout.cells {
                    self.pointed_cell(page, offset);
                }
            }
            for &(start, end) in &layout.freeblocks {
                self.carve(page, start, end, Area::Freeblock, true);
            }
        }
        if let Some((start, end)) = layout.unallocated {
            self.carve(page, start, end, Area::Unallocated, false);
        }
        true
    }

    /// Search a free page, from `start`, for whole cells.
    pub(super) fn scan_free_page(&mut self, page: &PageRef, start: usize) {
        self.carve(page, start, page.bytes.len(), Area::FreePage, false);
    }

    /// The cell a page's pointer array points to: certainly a record, kept
    /// even when it fits no table.
    fn pointed_cell(&mut self, page: &PageRef, offset: usize) {
        let limit = page.bytes.len();
        if let Some(layout) = self.cell_at(page, offset, limit, Evidence::CellPointer) {
            if let Some(found) = self.resolve_cell(page, layout, limit, Area::Cell) {
                self.found.push(found);
            }
        }
    }

    /// Search bytes `start..end` of a page: for whole cells anywhere, then
    /// for damaged cells where one must start: at `start` when it is a
    /// freeblock's (`damaged_start`), and right after every cell found
    /// (freed cells lie next to each other).
    fn carve(&mut self, page: &PageRef, start: usize, end: usize, area: Area, damaged_start: bool) {
        let whole = self.whole_cells(page, start, end, area);
        let starts: Vec<usize> = whole.iter().map(|found| found.record.offset).collect();
        let anchors = damaged_start
            .then_some(start)
            .into_iter()
            .chain(whole.iter().map(|found| found.end));
        for anchor in anchors.collect::<Vec<_>>() {
            let limit = starts.iter().copied().find(|&s| s >= anchor).unwrap_or(end);
            self.damaged_run(page, anchor, limit, area);
        }
        self.found.extend(whole);
    }

    /// Whole cells in `start..end`, found byte by byte, none overlapping.
    fn whole_cells(&self, page: &PageRef, start: usize, end: usize, area: Area) -> Vec<Found> {
        let mut found = Vec::new();
        let mut at = start;
        while at < end {
            let cell = self
                .cell_at(page, at, end, Evidence::Cell)
                .and_then(|layout| self.resolve_cell(page, layout, end, area));
            match cell {
                Some(cell) => {
                    at = cell.end;
                    found.push(cell);
                }
                None => at += 1,
            }
        }
        found
    }

    /// Damaged cells one after the other from `anchor`, before `limit`:
    /// the readings that lead exactly to `limit` when some do (freed cells
    /// fill their freeblock), else the best reading at each step.
    fn damaged_run(&mut self, page: &PageRef, anchor: usize, limit: usize, area: Area) {
        let mut readings = self.readings_from(page, anchor, limit, area);
        let path = exact_path(&readings, anchor, limit)
            .unwrap_or_else(|| greedy_path(&readings, anchor, limit));
        // Positions along a path only increase: each is taken once.
        for (at, choice) in path {
            let found = readings
                .remove(&at)
                .and_then(|found| found.into_iter().nth(choice));
            self.found.extend(found);
        }
    }

    /// The readings of damaged cells at `anchor` and wherever one of them
    /// ends, each position once: for each, the best reading per end.
    fn readings_from(
        &self,
        page: &PageRef,
        anchor: usize,
        limit: usize,
        area: Area,
    ) -> HashMap<usize, Vec<Found>> {
        let mut readings = HashMap::new();
        let mut pending = vec![anchor];
        while let Some(at) = pending.pop() {
            if readings.contains_key(&at) || at + FREEBLOCK_HEADER_SIZE >= limit {
                continue;
            }
            let found = self.damaged_readings(page, at, limit, area);
            pending.extend(found.iter().map(|found| found.end));
            readings.insert(at, found);
        }
        readings
    }

    /// A whole cell at `at`, ending by `limit`: its payload size agrees
    /// with its record header. Carved cells need a positive rowid.
    fn cell_at(
        &self,
        page: &PageRef,
        at: usize,
        limit: usize,
        evidence: Evidence,
    ) -> Option<Layout> {
        let bytes = page.bytes.get(..limit)?;
        let (payload_size, size_length) = varint_at(bytes, at)?;
        let (rowid, rowid_length) = varint_at(bytes, at + size_length)?;
        let rowid = rowid as i64;
        if evidence == Evidence::Cell && rowid <= 0 {
            return None;
        }
        let payload_start = at + size_length + rowid_length;
        let (serials, header_len) = strict_header(bytes.get(payload_start..)?, self.max_columns)?;
        let layout = Layout {
            cell_start: at,
            payload_start,
            header_len,
            slots: serials.into_iter().map(Slot::read).collect(),
            rowid: Some(rowid),
            evidence,
            first: FirstValue::Read,
        };
        (layout.payload_size() == payload_size).then_some(layout)
    }

    /// The best table for a whole cell; a cell a page points to is kept
    /// unmatched when none fits.
    fn resolve_cell(
        &self,
        page: &PageRef,
        layout: Layout,
        limit: usize,
        area: Area,
    ) -> Option<Found> {
        let pointed = layout.evidence == Evidence::CellPointer;
        let mut candidates: Vec<Candidate> = (0..self.shapes.len())
            .filter_map(|shape| self.candidate(page, layout.clone(), limit, Some(shape)))
            .collect();
        if candidates.is_empty() && pointed {
            candidates.extend(self.candidate(page, layout, limit, None));
        }
        self.choose(page, area, candidates, limit)
    }

    /// The readings of a damaged cell at `at`, over every table and every
    /// way its start can be damaged: the best for each place it ends.
    fn damaged_readings(&self, page: &PageRef, at: usize, limit: usize, area: Area) -> Vec<Found> {
        let mut by_end: BTreeMap<usize, Vec<Candidate>> = BTreeMap::new();
        for (index, shape) in self.shapes.iter().enumerate() {
            for damage in Damage::all() {
                for layout in damaged_layouts(page, at, limit, shape, damage, area) {
                    if let Some(candidate) = self.candidate(page, layout, limit, Some(index)) {
                        by_end
                            .entry(candidate.body.end)
                            .or_default()
                            .push(candidate);
                    }
                }
            }
        }
        by_end
            .into_values()
            .filter_map(|candidates| self.choose(page, area, candidates, limit))
            .collect()
    }

    /// `layout` as a row of `shapes[shape]` (or of no table), its values
    /// read, if it fits and reads as plausible values.
    fn candidate(
        &self,
        page: &PageRef,
        layout: Layout,
        limit: usize,
        shape: Option<usize>,
    ) -> Option<Candidate> {
        let agreement = match shape {
            Some(index) => Some(self.shapes[index].agree(&layout.slots, layout.strictness())?),
            None => None,
        };
        let body = self.body(page, &layout, limit)?;
        Some(Candidate {
            shape,
            agreement,
            layout,
            body,
        })
    }

    /// The values of the record `layout` locates: its local bytes, then its
    /// overflow chain while the chain's pages are consistent.
    fn body(&self, page: &PageRef, layout: &Layout, limit: usize) -> Option<Body> {
        let payload_size = layout.payload_size();
        let local = local_size(TreeKind::Table, payload_size, self.usable);
        let local_end = layout.payload_start.checked_add(local)?;
        let spills = payload_size > local as u64;
        let end = local_end + if spills { PAGE_NUMBER_SIZE } else { 0 };
        if end > limit || layout.header_len > local {
            return None;
        }
        let mut payload = page.bytes[layout.payload_start..local_end].to_vec();
        let mut truncated = false;
        if spills {
            let first = u32_at(page.bytes, local_end)?;
            truncated = !self.follow_overflow(first, payload_size, &mut payload, page.state);
        }
        let strict = layout.evidence != Evidence::CellPointer;
        let (values, cut) = read_values(&payload, layout, self.encoding, strict)?;
        Some(Body {
            end,
            values,
            truncated: truncated || cut,
        })
    }

    /// Append the overflow chain from page `first` to `payload` while each
    /// page may still hold it: a freelist leaf (left as it was freed), or,
    /// for a record from an older version of a page, an overflow page in
    /// use (the row may be live still). Whether the whole payload was read,
    /// ending on a page that points nowhere.
    fn follow_overflow(
        &self,
        first: u32,
        payload_size: u64,
        payload: &mut Vec<u8>,
        state: PageState,
    ) -> bool {
        let mut chain = HashSet::new();
        let mut next = first;
        while (payload.len() as u64) < payload_size {
            let consistent = match self.uses.get(&next) {
                Some(Use::FreelistLeaf) => true,
                Some(Use::Overflow) => state.is_older_version(),
                _ => false,
            };
            if !consistent || !chain.insert(next) {
                return false;
            }
            let Ok(page) = self.pages.get(next) else {
                return false;
            };
            let wanted = payload_size - payload.len() as u64;
            let content = &page[PAGE_NUMBER_SIZE..];
            let take = content
                .len()
                .min(usize::try_from(wanted).unwrap_or(usize::MAX));
            payload.extend_from_slice(&content[..take]);
            next = u32_at(page, 0).unwrap_or_default();
        }
        next == 0
    }

    /// The best of `candidates`, as a record found on `page`.
    fn choose(
        &self,
        page: &PageRef,
        area: Area,
        candidates: Vec<Candidate>,
        limit: usize,
    ) -> Option<Found> {
        let best = candidates
            .iter()
            .enumerate()
            .max_by_key(|(index, candidate)| (candidate.rank(limit), std::cmp::Reverse(*index)))?
            .0;
        let fit = candidates[best].fit();
        let mut also_fits: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.shape != candidates[best].shape && candidate.fit() == fit)
            .filter_map(|candidate| {
                candidate
                    .shape
                    .map(|index| self.shapes[index].name().to_owned())
            })
            .collect();
        also_fits.sort();
        also_fits.dedup();
        let candidate = candidates.into_iter().nth(best)?;
        Some(self.found(page, area, candidate, also_fits, limit))
    }

    fn found(
        &self,
        page: &PageRef,
        area: Area,
        candidate: Candidate,
        also_fits: Vec<String>,
        limit: usize,
    ) -> Found {
        let rank = candidate.rank(limit);
        let Candidate {
            shape,
            agreement,
            layout,
            body,
        } = candidate;
        let shape = shape.map(|index| &self.shapes[index]);
        let confidence = confidence(&layout, agreement, !also_fits.is_empty());
        let stored_columns = layout.slots.len();
        let values = match shape {
            Some(shape) => shape.align(body.values, layout.rowid),
            None => body.values,
        };
        Found {
            record: RecoveredRecord {
                table: shape.map(|shape| shape.name().to_owned()),
                rowid: layout.rowid,
                values,
                page: page.number,
                offset: layout.cell_start,
                page_state: page.state,
                area,
                evidence: layout.evidence,
                confidence,
                stored_columns,
                also_fits,
                truncated: body.truncated,
            },
            end: body.end,
            rank,
            read_bytes: agreement.map_or(0, |agreement| agreement.read_bytes),
            guessed: layout.first == FirstValue::NumberWidth,
        }
    }
}

/// How sure the match is: whole cells whose every value has its column's
/// usual type, matching one table only, are sure; damaged or ambiguous
/// ones less; unusual types, a single value, or a first value sized by
/// where its freeblock ends, least.
fn confidence(layout: &Layout, agreement: Option<Agreement>, ambiguous: bool) -> Confidence {
    let Some(agreement) = agreement else {
        return Confidence::Low;
    };
    let whole = matches!(layout.evidence, Evidence::Cell | Evidence::CellPointer);
    let sized_by_limit = layout.first == FirstValue::EndsAtLimit;
    if agreement.unexpected > 0
        || agreement.read_values < 2
        || sized_by_limit
        || (ambiguous && !whole)
    {
        Confidence::Low
    } else if ambiguous || !whole {
        Confidence::Medium
    } else {
        Confidence::High
    }
}

/// The layouts a damaged cell at `at` (ending by `limit`) could have as a
/// row of `shape`.
fn damaged_layouts(
    page: &PageRef,
    at: usize,
    limit: usize,
    shape: &Shape,
    damage: Damage,
    area: Area,
) -> Vec<Layout> {
    let Some(bytes) = page.bytes.get(..limit) else {
        return Vec::new();
    };
    match damage {
        Damage::RecordAt(offset) => record_at(bytes, at, offset, shape).into_iter().collect(),
        Damage::HeaderSize => lost_header_size(bytes, at, shape),
        Damage::FirstSerialType { width } => {
            // Only a freeblock's end is where a freed cell ended.
            let ends_at_limit = area == Area::Freeblock;
            lost_first_serial_type(bytes, at, shape, width, ends_at_limit)
        }
    }
}

/// A damaged cell whose record starts intact `offset` bytes in: its
/// header size agrees with its serial types, and the bytes between the
/// overwritten four and the record are a plausible rowid varint's tail.
fn record_at(bytes: &[u8], at: usize, offset: usize, shape: &Shape) -> Option<Layout> {
    let payload_start = at + offset;
    let (serials, header_len) = strict_header(bytes.get(payload_start..)?, shape.stored_len())?;
    let layout = Layout {
        cell_start: at,
        payload_start,
        header_len,
        slots: serials.into_iter().map(Slot::read).collect(),
        rowid: None,
        evidence: Evidence::Record,
        first: FirstValue::Read,
    };
    let size_length = varint_length(layout.payload_size());
    let rowid_length = offset.checked_sub(size_length)?;
    let rowid_tail = &bytes[at + FREEBLOCK_HEADER_SIZE.max(size_length)..payload_start];
    plausible_varint_tail(rowid_length, rowid_tail).then_some(layout)
}

/// Whether `tail` can end a varint of `length` bytes: every byte but the
/// last has its high bit set, the last (unless the ninth) has it clear.
fn plausible_varint_tail(length: usize, tail: &[u8]) -> bool {
    if !(1..=9).contains(&length) {
        return false;
    }
    match tail.split_last() {
        None => true,
        Some((last, rest)) => {
            rest.iter().all(|byte| byte & 0x80 != 0) && (length == 9 || last & 0x80 == 0)
        }
    }
}

/// Damaged cells whose record starts at byte 3, header size lost: the
/// serial types read from byte 4, as many as `shape` allows.
fn lost_header_size(bytes: &[u8], at: usize, shape: &Shape) -> Vec<Layout> {
    let payload_start = at + 3;
    let serials = serial_types(bytes, at + 4, shape.stored_len());
    shape
        .counts()
        .filter_map(|count| {
            let (_, types_end) = *serials.get(count.checked_sub(1)?)?;
            let layout = Layout {
                cell_start: at,
                payload_start,
                header_len: types_end - payload_start,
                slots: serials[..count]
                    .iter()
                    .map(|&(serial, _)| Slot::read(serial))
                    .collect(),
                rowid: None,
                evidence: Evidence::Shape,
                first: FirstValue::Read,
            };
            // A one-byte header size; payload size and rowid in three bytes.
            let fits = layout.header_len as u64 <= MAX_ONE_BYTE_VARINT
                && layout.payload_size() <= MAX_TWO_BYTE_VARINT;
            fits.then_some(layout)
        })
        .collect()
}

/// Damaged cells whose record starts at byte 2, header size and first
/// serial type (`width` bytes) lost: the other serial types read after it.
/// The first value is NULL when it is the rowid alias; otherwise it takes
/// the bytes left before the limit when `ends_at_limit`, or is guessed as a
/// number of each width numbers take.
fn lost_first_serial_type(
    bytes: &[u8],
    at: usize,
    shape: &Shape,
    width: usize,
    ends_at_limit: bool,
) -> Vec<Layout> {
    let payload_start = at + 2;
    let types_start = payload_start + 1 + width;
    let serials = serial_types(bytes, types_start, shape.stored_len() - 1);
    let mut layouts = Vec::new();
    for count in shape.counts() {
        let Some(rest) = count.checked_sub(1).and_then(|rest| serials.get(..rest)) else {
            continue;
        };
        let types_end = rest.last().map_or(types_start, |&(_, end)| end);
        let rest_slots = rest.iter().map(|&(serial, _)| Slot::read(serial));
        for (first, kind) in first_slots(bytes.len(), types_end, rest, shape, width, ends_at_limit)
        {
            let layout = Layout {
                cell_start: at,
                payload_start,
                header_len: types_end - payload_start,
                slots: std::iter::once(first).chain(rest_slots.clone()).collect(),
                rowid: None,
                evidence: Evidence::Shape,
                first: kind,
            };
            // One-byte payload size and rowid: a payload under 128 bytes.
            if layout.payload_size() <= MAX_ONE_BYTE_VARINT {
                layouts.push(layout);
            }
        }
    }
    layouts
}

/// The readings of a lost first value of `shape` whose serial type took
/// `width` bytes, before serial types ending at `types_end` and values
/// `rest`, in a cell that ends by `limit`.
fn first_slots(
    limit: usize,
    types_end: usize,
    rest: &[(SerialType, usize)],
    shape: &Shape,
    width: usize,
    ends_at_limit: bool,
) -> Vec<(Slot, FirstValue)> {
    let column = shape.first_column();
    let lost = |size: u64| Slot {
        serial: infer_serial(column, size),
        size,
        read: false,
    };
    if column.rowid_alias {
        let null = Slot {
            serial: Some(SerialType::Null),
            size: 0,
            read: false,
        };
        return if width == 1 {
            vec![(null, FirstValue::RowidAlias)]
        } else {
            Vec::new()
        };
    }
    let fits_width = |size: u64| (size <= MAX_ONE_BYTE_SERIAL_SIZE) == (width == 1);
    // Numbers first: of two equal readings, the earlier wins.
    let mut slots: Vec<(Slot, FirstValue)> = Vec::new();
    if width == 1 {
        slots.extend(
            number_sizes(column)
                .iter()
                .map(|&size| (lost(size), FirstValue::NumberWidth)),
        );
    }
    if ends_at_limit {
        let rest_size: u64 = rest.iter().map(|(serial, _)| serial.size()).sum();
        let room = limit.saturating_sub(types_end) as u64;
        if let Some(size) = room.checked_sub(rest_size).filter(|&size| fits_width(size)) {
            slots.push((lost(size), FirstValue::EndsAtLimit));
        }
    }
    slots
}

/// Up to `most` serial types from `at`, each with where it ends; fewer
/// when the bytes run out or a type is reserved.
fn serial_types(bytes: &[u8], mut at: usize, most: usize) -> Vec<(SerialType, usize)> {
    let mut serials = Vec::new();
    while serials.len() < most {
        let Some((raw, length)) = varint_at(bytes, at) else {
            break;
        };
        let serial = SerialType::from_raw(raw);
        if matches!(serial, SerialType::Reserved(_)) {
            break;
        }
        at += length;
        serials.push((serial, at));
    }
    serials
}

/// Bytes the varint of `value` takes: seven bits in each of the first
/// eight, all of the ninth.
fn varint_length(value: u64) -> usize {
    (1..=8)
        .find(|&length| value < 1u64 << (7 * length))
        .unwrap_or(9)
}

/// The record's values from its payload, `None` where lost, and whether the
/// payload ran out first (the value it cut is kept as far as it goes).
/// `None` when a value can't be what a database stores: text that isn't
/// valid in its encoding, a NaN real; for a carved record (`strict`), text
/// holding control characters other than tabs and line breaks too.
fn read_values(
    payload: &[u8],
    layout: &Layout,
    encoding: TextEncoding,
    strict: bool,
) -> Option<(Vec<Option<Value>>, bool)> {
    let mut at = layout.header_len;
    let mut values = Vec::with_capacity(layout.slots.len());
    let mut cut = false;
    for slot in &layout.slots {
        if cut {
            values.push(None);
            continue;
        }
        let end = usize::try_from(slot.size)
            .ok()
            .and_then(|size| at.checked_add(size));
        let whole = end.and_then(|end| payload.get(at..end));
        let bytes = whole.unwrap_or_else(|| payload.get(at..).unwrap_or_default());
        cut = whole.is_none();
        let value = match slot.serial {
            // A number cut short is lost.
            Some(serial) if cut && !matches!(serial, SerialType::Text(_) | SerialType::Blob(_)) => {
                None
            }
            Some(serial) => Some(plausible_value(serial, bytes, cut, encoding, strict)?),
            None => None,
        };
        values.push(value);
        at = end.unwrap_or(payload.len());
    }
    Some((values, cut))
}

/// The value `serial` stores in `bytes` (a prefix of text or a blob when
/// `cut`), if plausible.
fn plausible_value(
    serial: SerialType,
    bytes: &[u8],
    cut: bool,
    encoding: TextEncoding,
    strict: bool,
) -> Option<Value> {
    let unusual = |c: char| c.is_control() && !matches!(c, '\t' | '\n' | '\r');
    match serial {
        SerialType::Text(_) => {
            let text = valid_text(bytes, cut, encoding)?;
            (!strict || !text.chars().any(unusual)).then_some(Value::Text(text))
        }
        SerialType::Blob(_) => Some(Value::Blob(bytes.to_vec())),
        _ => match serial.value(bytes, encoding) {
            Value::Real(real) if real.is_nan() => None,
            value => Some(value),
        },
    }
}

/// `bytes` as text in `encoding`, if valid there; when `cut`, a sequence
/// the cut split at the end is dropped.
fn valid_text(bytes: &[u8], cut: bool, encoding: TextEncoding) -> Option<String> {
    let unit: fn([u8; 2]) -> u16 = match encoding {
        TextEncoding::Utf8 => {
            return match std::str::from_utf8(bytes) {
                Ok(text) => Some(text.to_owned()),
                Err(error) if cut && error.error_len().is_none() => {
                    Some(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned())
                }
                Err(_) => None,
            };
        }
        TextEncoding::Utf16Le => u16::from_le_bytes,
        TextEncoding::Utf16Be => u16::from_be_bytes,
    };
    if bytes.len() % 2 == 1 && !cut {
        return None;
    }
    let mut units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| unit([pair[0], pair[1]]))
        .collect();
    if cut
        && units
            .last()
            .is_some_and(|&last| (0xd800..0xdc00).contains(&last))
    {
        units.pop();
    }
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_lengths() {
        assert_eq!(varint_length(0), 1);
        assert_eq!(varint_length(127), 1);
        assert_eq!(varint_length(128), 2);
        assert_eq!(varint_length(16_383), 2);
        assert_eq!(varint_length(16_384), 3);
        assert_eq!(varint_length((1 << 56) - 1), 8);
        assert_eq!(varint_length(1 << 56), 9);
        assert_eq!(varint_length(u64::MAX), 9);
    }

    #[test]
    fn rowid_tails() {
        // A two-byte rowid whose first byte was overwritten.
        assert!(plausible_varint_tail(2, &[0x05]));
        assert!(!plausible_varint_tail(2, &[0x85]));
        // Three bytes, the last two visible.
        assert!(plausible_varint_tail(3, &[0x81, 0x05]));
        assert!(!plausible_varint_tail(3, &[0x01, 0x05]));
        assert!(!plausible_varint_tail(0, &[]));
    }

    #[test]
    fn text_must_be_valid_unless_cut_mid_character() {
        let check = |bytes: &[u8], cut| valid_text(bytes, cut, TextEncoding::Utf8);
        assert_eq!(check("é".as_bytes(), false).as_deref(), Some("é"));
        assert_eq!(check(&"é".as_bytes()[..1], false), None);
        assert_eq!(check(&[b'a', 0xc3], true).as_deref(), Some("a"));
        assert_eq!(check(&[0xff, b'a'], true), None);
        let utf16 = |bytes: &[u8], cut| valid_text(bytes, cut, TextEncoding::Utf16Le);
        assert_eq!(utf16(&[b'a', 0, 0x3d, 0xd8], true).as_deref(), Some("a"));
        assert_eq!(utf16(&[b'a', 0, 0x3d, 0xd8], false), None);
    }
}
