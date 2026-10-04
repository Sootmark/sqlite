//! Damage is reported, never a panic, and every walk ends: unusable
//! headers, cycles in b-trees, overflow chains and freelists, pages out of
//! range, truncated files, and (property tests) arbitrary bytes and real
//! files damaged anywhere, read and recovered from.

mod support;

use sqlite::{Database, Error};

const PAGE_SIZE: usize = 512;
/// The page size of the recovery fixtures (SQLite's default).
const RECOVERY_PAGE_SIZE: usize = 4096;
/// The b-tree page header's right-most child pointer (interior pages).
const RIGHT_MOST_POINTER: usize = 8;

fn pages_db() -> Vec<u8> {
    support::fixture("pages.db")
}

/// The file offset of a table's root page.
fn root_offset(file: &[u8], table: &str) -> usize {
    let db = Database::open(file).unwrap();
    (db.table(table).unwrap().root_page as usize - 1) * PAGE_SIZE
}

fn set_u32(file: &mut [u8], at: usize, value: u32) {
    file[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

fn has_problem(walk: &support::Walk, text: &str) -> bool {
    walk.problems.iter().any(|problem| problem.contains(text))
}

#[test]
fn unusable_headers_are_errors() {
    let file = support::fixture("types.db");
    assert_eq!(Database::open(&file[..99]).err(), Some(Error::TooShort(99)));
    let mut damaged = file.clone();
    damaged[0] = b's';
    assert_eq!(Database::open(&damaged).err(), Some(Error::NotSqlite));
    let mut damaged = file.clone();
    damaged[16..18].copy_from_slice(&1000u16.to_be_bytes());
    assert_eq!(Database::open(&damaged).err(), Some(Error::PageSize(1000)));
    let mut damaged = file;
    damaged[20] = 33; // 512 - 33 = 479 usable bytes, one short
    assert_eq!(
        Database::open(&damaged).err(),
        Some(Error::ReservedBytes(33))
    );
}

#[test]
fn a_b_tree_cycle_is_reported_and_ends() {
    let mut file = pages_db();
    let root = root_offset(&file, "events");
    let root_page = (root / PAGE_SIZE + 1) as u32;
    set_u32(&mut file, root + RIGHT_MOST_POINTER, root_page);
    let walk = support::walk(&Database::open(&file).unwrap());
    assert!(
        has_problem(&walk, &format!("page {root_page} reached twice")),
        "{:?}",
        walk.problems
    );
}

#[test]
fn a_page_out_of_range_is_reported() {
    let mut file = pages_db();
    let root = root_offset(&file, "events");
    set_u32(&mut file, root + RIGHT_MOST_POINTER, 100_000);
    let walk = support::walk(&Database::open(&file).unwrap());
    assert!(
        has_problem(&walk, "page 100000 is outside the database's"),
        "{:?}",
        walk.problems
    );
}

#[test]
fn an_overflow_cycle_is_reported_and_ends() {
    let mut file = pages_db();
    // An overflow page of the long note: its content continues the text.
    let page = file
        .chunks(PAGE_SIZE)
        .position(|page| page[4..].starts_with(b"overflow overflow"))
        .unwrap();
    set_u32(&mut file, page * PAGE_SIZE, page as u32 + 1);
    let walk = support::walk(&Database::open(&file).unwrap());
    assert!(
        has_problem(&walk, "overflow chain loops back"),
        "{:?}",
        walk.problems
    );
}

#[test]
fn a_truncated_file_reads_what_is_left() {
    let file = pages_db();
    let full = support::walk(&Database::open(&file).unwrap());
    let half = &file[..file.len() / PAGE_SIZE / 2 * PAGE_SIZE + 100];
    let walk = support::walk(&Database::open(half).unwrap());
    assert!(has_problem(&walk, "100 bytes after the last whole page"));
    assert!(has_problem(&walk, "(truncated)"));
    let (read, all) = (
        walk.rows + walk.index_entries,
        full.rows + full.index_entries,
    );
    assert!(read > 0 && read < all, "{read} of {all}");
}

/// A freelist trunk page that names itself as the next trunk: reported,
/// and the walk ends; the records on the freelist are still recovered.
#[test]
fn a_freelist_cycle_is_reported_and_ends() {
    let mut file = support::fixture("recovery/deleted.db");
    let healthy = Database::open(&file).unwrap().recover().records.len();
    let trunk = u32::from_be_bytes(file[32..36].try_into().unwrap());
    let trunk_offset = (trunk as usize - 1) * RECOVERY_PAGE_SIZE;
    set_u32(&mut file, trunk_offset, trunk);
    let recovered = Database::open(&file).unwrap().recover();
    let expected = format!("freelist: page {trunk} is already used");
    assert!(
        recovered
            .problems
            .iter()
            .any(|problem| problem.contains(&expected)),
        "{:?}",
        recovered.problems
    );
    assert_eq!(recovered.records.len(), healthy);
}

/// A freeblock that points back at itself: reported, and the chain ends.
#[test]
fn a_freeblock_cycle_is_reported_and_ends() {
    let mut file = support::fixture("recovery/deleted.db");
    let db = Database::open(&file).unwrap();
    let page = db
        .recover()
        .records
        .iter()
        .find(|record| record.area == sqlite::Area::Freeblock)
        .unwrap()
        .page as usize;
    let header = (page - 1) * RECOVERY_PAGE_SIZE;
    let first = u16::from_be_bytes([file[header + 1], file[header + 2]]);
    let at = header + usize::from(first);
    file[at..at + 2].copy_from_slice(&first.to_be_bytes());
    let recovered = Database::open(&file).unwrap().recover();
    assert!(
        recovered
            .problems
            .iter()
            .any(|problem| problem.starts_with(&format!("page {page}: freeblock at {first}"))),
        "{:?}",
        recovered.problems
    );
}

/// A header page count that a legacy writer may have left stale (the change
/// counter doesn't match the version-valid-for number) gives way to the
/// file's size.
#[test]
fn a_stale_page_count_gives_way_to_the_file_size() {
    let mut file = pages_db();
    set_u32(&mut file, 28, 3);
    file[24] ^= 0xff;
    let db = Database::open(&file).unwrap();
    assert_eq!(db.page_count as usize, file.len() / PAGE_SIZE);
    assert!(support::walk(&db).problems.is_empty());
}

mod properties {
    use proptest::prelude::*;
    use sqlite::Database;

    /// At most every page holds a cell per two bytes, in every tree; a
    /// recovered record takes at least four bytes of the input.
    fn plausible(walk: &super::support::Walk, db: &Database, input: usize) -> bool {
        let trees = db.schema.len() + 1;
        let most = db.page_count as usize * (db.header.page_size as usize / 2) * trees;
        walk.rows + walk.index_entries <= most && walk.recovered <= input / 4
    }

    fn damage(mut file: Vec<u8>, flips: &[(usize, u8)], cut: usize) -> Vec<u8> {
        for &(at, byte) in flips {
            let len = file.len();
            file[at % len] = byte;
        }
        file.truncate(cut);
        file
    }

    proptest! {
        /// Any bytes: read or refused, never a panic.
        #[test]
        fn arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..4_096)) {
            if let Ok(db) = Database::open(&data) {
                super::support::walk(&db);
            }
        }

        /// Arbitrary pages behind a real header.
        #[test]
        fn arbitrary_pages(pages in proptest::collection::vec(any::<u8>(), 0..8_192)) {
            let mut file = super::support::fixture("types.db")[..100].to_vec();
            file.extend(pages);
            let db = Database::open(&file).unwrap();
            let walk = super::support::walk(&db);
            prop_assert!(plausible(&walk, &db, file.len()));
        }

        /// Real files damaged anywhere and cut anywhere.
        #[test]
        fn damaged_files(
            fixture in prop::sample::select(vec!["types.db", "pages.db", "utf16be.db", "reserved.db", "autovacuum.db"]),
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..20),
            cut in 0usize..45_000,
        ) {
            let file = damage(super::support::fixture(fixture), &flips, cut);
            if let Ok(db) = Database::open(&file) {
                let walk = super::support::walk(&db);
                prop_assert!(plausible(&walk, &db, file.len()));
            }
        }

        /// Databases with deleted records damaged anywhere and cut
        /// anywhere: freeblock chains, freelists, freed cells and their
        /// overflow chains.
        #[test]
        fn damaged_deletions(
            fixture in prop::sample::select(vec!["recovery/deleted.db", "recovery/chromium.db", "recovery/churn.db"]),
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..40),
            cut in 0usize..110_000,
        ) {
            let file = damage(super::support::fixture(fixture), &flips, cut);
            if let Ok(db) = Database::open(&file) {
                let walk = super::support::walk(&db);
                prop_assert!(plausible(&walk, &db, file.len()));
            }
        }

        /// A real log damaged anywhere and cut anywhere.
        #[test]
        fn damaged_logs(
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..20),
            cut in 0usize..20_000,
        ) {
            let file = super::support::fixture("wal.db");
            let wal = damage(super::support::fixture("wal.db-wal"), &flips, cut);
            if let Ok(db) = Database::open_with_wal(&file, &wal) {
                let walk = super::support::walk(&db);
                prop_assert!(plausible(&walk, &db, file.len() + wal.len()));
            }
        }

        /// A log holding older versions of pages with deleted rows, damaged
        /// anywhere and cut anywhere: every frame is searched, whatever its
        /// state.
        #[test]
        fn damaged_logs_with_older_versions(
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..40),
            cut in 0usize..13_000,
        ) {
            let file = super::support::fixture("recovery/recovery-wal.db");
            let wal = damage(super::support::fixture("recovery/recovery-wal.db-wal"), &flips, cut);
            if let Ok(db) = Database::open_with_wal(&file, &wal) {
                let walk = super::support::walk(&db);
                prop_assert!(plausible(&walk, &db, file.len() + wal.len()));
            }
        }
    }
}
