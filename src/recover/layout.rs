//! Where deleted content can be on a table b-tree page: the cells its
//! pointer array lists, its chain of freeblocks, and the unallocated gap
//! between the pointer array and the cell content area.

use crate::btree::{
    CELL_POINTER_SIZE, INTERIOR_HEADER_SIZE, LEAF_HEADER_SIZE, TABLE_INTERIOR, TABLE_LEAF,
};
use crate::bytes::{u16_at, u8_at};

/// Bytes of a freeblock's header: the next freeblock's offset, then its
/// size. They overwrite the first four bytes of the cell freed.
pub(super) const FREEBLOCK_HEADER_SIZE: usize = 4;
/// The cell content area offset field's stand-in for 65536.
const CONTENT_START_65536: usize = 65_536;

/// A table b-tree page's parts.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PageLayout {
    /// Whether it is a leaf (whose cells are records), not an interior page.
    pub(super) leaf: bool,
    /// Where the cells its pointer array lists start.
    pub(super) cells: Vec<usize>,
    /// Freeblocks, as start and end offsets, in page order (leaves only).
    pub(super) freeblocks: Vec<(usize, usize)>,
    /// The gap between the cell pointer array and the cell content area.
    pub(super) unallocated: Option<(usize, usize)>,
}

impl PageLayout {
    /// The layout of a page whose b-tree header starts at `header_start`,
    /// if it reads as a table b-tree page. Damage (pointers and freeblocks
    /// off the page or out of order) is reported and left out.
    pub(super) fn read(
        bytes: &[u8],
        header_start: usize,
        problems: &mut Vec<String>,
    ) -> Option<Self> {
        let leaf = match u8_at(bytes, header_start)? {
            TABLE_LEAF => true,
            TABLE_INTERIOR => false,
            _ => return None,
        };
        let header_size = if leaf {
            LEAF_HEADER_SIZE
        } else {
            INTERIOR_HEADER_SIZE
        };
        let pointers = header_start + header_size;
        let declared = usize::from(u16_at(bytes, header_start + 3)?);
        let room = bytes.len().saturating_sub(pointers) / CELL_POINTER_SIZE;
        if declared > room {
            problems.push(format!(
                "{declared} cells declared, room for {room} pointers"
            ));
        }
        let pointers_end = pointers + declared.min(room) * CELL_POINTER_SIZE;
        let cells = (pointers..pointers_end)
            .step_by(CELL_POINTER_SIZE)
            .filter_map(|at| u16_at(bytes, at).map(usize::from))
            .filter(|&offset| offset >= pointers_end && offset < bytes.len())
            .collect();
        let content_start = match u16_at(bytes, header_start + 5)? {
            0 => CONTENT_START_65536,
            offset => usize::from(offset),
        };
        let unallocated = if (pointers_end..=bytes.len()).contains(&content_start) {
            (pointers_end < content_start).then_some((pointers_end, content_start))
        } else {
            problems.push(format!(
                "cell content area starts off the page ({content_start})"
            ));
            None
        };
        let freeblocks = if leaf {
            let first = usize::from(u16_at(bytes, header_start + 1)?);
            freeblocks(bytes, first, pointers_end, problems)
        } else {
            Vec::new()
        };
        Some(Self {
            leaf,
            cells,
            freeblocks,
            unallocated,
        })
    }
}

/// The chain of freeblocks from offset `first`, each after the one before
/// it (so the walk ends), none before `floor` or past the page.
fn freeblocks(
    bytes: &[u8],
    first: usize,
    floor: usize,
    problems: &mut Vec<String>,
) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut at = first;
    let mut floor = floor;
    while at != 0 {
        let size = u16_at(bytes, at + 2).map_or(0, usize::from);
        let end = at + size;
        if at < floor || size < FREEBLOCK_HEADER_SIZE || end > bytes.len() {
            problems.push(format!(
                "freeblock at {at} ({size} bytes) is out of order or off the page"
            ));
            break;
        }
        blocks.push((at, end));
        floor = end;
        at = u16_at(bytes, at).map_or(0, usize::from);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 512-byte table leaf: two cells, a freeblock chain of two, and
    /// content from offset 300.
    fn leaf() -> Vec<u8> {
        let mut page = vec![0u8; 512];
        page[0] = TABLE_LEAF;
        page[1..3].copy_from_slice(&320u16.to_be_bytes());
        page[3..5].copy_from_slice(&2u16.to_be_bytes());
        page[5..7].copy_from_slice(&300u16.to_be_bytes());
        page[8..10].copy_from_slice(&400u16.to_be_bytes());
        page[10..12].copy_from_slice(&300u16.to_be_bytes());
        page[320..322].copy_from_slice(&350u16.to_be_bytes());
        page[322..324].copy_from_slice(&10u16.to_be_bytes());
        page[352..354].copy_from_slice(&20u16.to_be_bytes());
        page
    }

    #[test]
    fn cells_freeblocks_and_the_gap() {
        let mut problems = Vec::new();
        let layout = PageLayout::read(&leaf(), 0, &mut problems).unwrap();
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(
            layout,
            PageLayout {
                leaf: true,
                cells: vec![400, 300],
                freeblocks: vec![(320, 330), (350, 370)],
                unallocated: Some((12, 300)),
            }
        );
    }

    #[test]
    fn a_freeblock_chain_that_goes_back_ends() {
        let mut page = leaf();
        page[350..352].copy_from_slice(&320u16.to_be_bytes());
        let mut problems = Vec::new();
        let layout = PageLayout::read(&page, 0, &mut problems).unwrap();
        assert_eq!(layout.freeblocks, [(320, 330), (350, 370)]);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[test]
    fn other_pages_have_no_layout() {
        let mut page = leaf();
        page[0] = 0x0a;
        assert_eq!(PageLayout::read(&page, 0, &mut Vec::new()), None);
    }
}
