//! Recovery of deleted records against what the fixtures' generator deleted
//! (`tests/oracle/recovery/`, written by `tests/fixtures/recovery/gen.sh`):
//! every deleted row whose record SQLite left on disk is recovered, in its
//! table, with its values; nothing else is reported (no live row, no
//! garbage); a database written with `secure_delete` on yields nothing.
//!
//! "Left on disk" is decided independently of the crate: the row's record
//! is encoded here as SQLite writes it, and its bytes after the first two
//! (which a freeblock header may overwrite, with the cell's payload size
//! and rowid before them) are searched for in the database and its log.
//! Run with `--nocapture` for each fixture's precision and recall.

mod support;

use sqlite::{Area, Database, Evidence, PageState, RecoveredRecord, Table, Value};
use support::json;

/// Bytes of a record's start that a freeblock header can overwrite once
/// the cell's payload size and rowid (at least a byte each) are counted.
const OVERWRITTEN_RECORD_BYTES: usize = 2;

/// A row the generator deleted (or dropped, or never committed).
#[derive(Debug)]
struct Deleted {
    table: String,
    kind: String,
    rowid: i64,
    values: Vec<Value>,
}

/// The rows deleted from a fixture, from every oracle file of it.
fn deleted_rows(fixture: &str) -> Vec<Deleted> {
    support::oracle(&format!("recovery/{fixture}"))
        .into_iter()
        .flat_map(|(_, rows)| rows)
        .map(|row| {
            let text = |name: &str| match &row.iter().find(|(key, _)| key == name).unwrap().1 {
                json::Value::Text(text) => text.clone(),
                other => panic!("{name}: {other:?}"),
            };
            let rowid = match row.iter().find(|(key, _)| key == "rowid").unwrap().1 {
                json::Value::Integer(rowid) => rowid,
                ref other => panic!("rowid: {other:?}"),
            };
            Deleted {
                table: text("table"),
                kind: text("kind"),
                rowid,
                values: row
                    .iter()
                    .filter(|(key, _)| key.starts_with('c'))
                    .map(|(_, value)| support::oracle_value(value))
                    .collect(),
            }
        })
        .collect()
}

/// How a fixture's recovery measured against its oracle.
#[derive(Debug, Default)]
struct Score {
    /// Deleted rows whose record is still on disk.
    expected: usize,
    /// Of those, the ones recovered, and the others.
    recovered: usize,
    missed: Vec<String>,
    /// Deleted rows recovered though not found on disk whole (partly
    /// overwritten, truncated overflow).
    beyond_expected: usize,
    /// Records reported, and those matching no deleted row.
    reported: usize,
    unmatched: Vec<String>,
    /// Values reported as lost in matched records.
    lost_values: usize,
    /// Matched records found in freeblocks (whose rowid is lost).
    from_freeblocks: usize,
}

impl Score {
    fn precision(&self) -> f64 {
        ratio(self.reported - self.unmatched.len(), self.reported)
    }

    fn recall(&self) -> f64 {
        ratio(self.recovered, self.expected)
    }
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        1.0
    } else {
        part as f64 / whole as f64
    }
}

/// Recover from a fixture (with its log, if any) and score the result.
fn score(fixture: &str, wal: Option<&str>) -> (Score, sqlite::Recovered) {
    let file = support::fixture(&format!("recovery/{fixture}"));
    let log = wal.map_or_else(Vec::new, |wal| support::fixture(&format!("recovery/{wal}")));
    let db = Database::open_with_wal(&file, &log).unwrap();
    let recovered = db.recover();
    let deleted = deleted_rows(fixture);
    let tables: Vec<&Table> = db.tables.iter().chain(&recovered.dropped_tables).collect();
    let usable = db.header.usable_size() as usize;
    let disk = [file.as_slice(), log.as_slice()];
    let all: Vec<&RecoveredRecord> = recovered
        .records
        .iter()
        .chain(&recovered.older_versions)
        .collect();
    let mut score = Score {
        reported: all.len(),
        ..Score::default()
    };
    for row in &deleted {
        let on_disk = is_on_disk(row, &tables, usable, &disk);
        let found = all.iter().any(|record| matches(record, row));
        score.expected += usize::from(on_disk);
        score.recovered += usize::from(on_disk && found);
        score.beyond_expected += usize::from(!on_disk && found);
        if on_disk && !found {
            score.missed.push(format!("{row:?}"));
        }
    }
    for record in &all {
        if deleted.iter().any(|row| matches(record, row)) {
            score.lost_values += record.values.iter().filter(|value| value.is_none()).count();
            score.from_freeblocks += usize::from(record.area == Area::Freeblock);
        } else {
            score.unmatched.push(format!("{record:?}"));
        }
    }
    report(fixture, &score);
    (score, recovered)
}

fn report(fixture: &str, score: &Score) {
    println!(
        "{fixture}: recall {:.3} ({} of {} deleted rows left on disk, {} more partly), \
         precision {:.3} ({} of {} records match a deleted row), \
         {} from freeblocks, {} values lost",
        score.recall(),
        score.recovered,
        score.expected,
        score.beyond_expected,
        score.precision(),
        score.reported - score.unmatched.len(),
        score.reported,
        score.from_freeblocks,
        score.lost_values,
    );
    for missed in &score.missed {
        println!("  missed: {missed}");
    }
    for unmatched in &score.unmatched {
        println!("  unmatched: {unmatched}");
    }
}

/// Whether `record` is `row`: the same table, the same rowid when it has
/// one, and every value it still has equal (a value a truncation cut,
/// a prefix).
fn matches(record: &RecoveredRecord, row: &Deleted) -> bool {
    record.table.as_deref() == Some(row.table.as_str())
        && record.rowid.map_or(true, |rowid| rowid == row.rowid)
        && record.values.len() == row.values.len()
        && record
            .values
            .iter()
            .zip(&row.values)
            .all(|(ours, theirs)| match ours {
                None => true,
                Some(ours) if record.truncated => is_prefix(ours, theirs),
                Some(ours) => same(ours, theirs),
            })
}

/// Equal, reals to the bit.
fn same(ours: &Value, theirs: &Value) -> bool {
    match (ours, theirs) {
        (Value::Real(a), Value::Real(b)) => a.to_bits() == b.to_bits(),
        _ => ours == theirs,
    }
}

fn is_prefix(ours: &Value, theirs: &Value) -> bool {
    match (ours, theirs) {
        (Value::Text(a), Value::Text(b)) => b.starts_with(a.as_str()),
        (Value::Blob(a), Value::Blob(b)) => b.starts_with(a),
        _ => same(ours, theirs),
    }
}

/// Whether the record of `row`, as SQLite encodes it, is in `disk` from
/// its third byte to the end of its local part.
fn is_on_disk(row: &Deleted, tables: &[&Table], usable: usize, disk: &[&[u8]]) -> bool {
    let record = match tables.iter().find(|table| table.name == row.table) {
        Some(table) => encode(&row.values, |index| table.columns[index].rowid_alias),
        None => encode(&row.values, |_| false), // the schema table
    };
    let local = local_size(record.len(), usable);
    let needle = &record[OVERWRITTEN_RECORD_BYTES..local];
    disk.iter()
        .any(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
}

/// Bytes of a `payload`-byte table cell payload kept on its page.
fn local_size(payload: usize, usable: usize) -> usize {
    let most = usable - 35;
    let least = (usable - 12) * 32 / 255 - 23;
    if payload <= most {
        return payload;
    }
    let kept = least + (payload - least) % (usable - 4);
    if kept <= most {
        kept
    } else {
        least
    }
}

/// A record as SQLite writes it (file format 4): the rowid alias as NULL,
/// integers in the fewest bytes, 0 and 1 in none.
fn encode(values: &[Value], is_alias: impl Fn(usize) -> bool) -> Vec<u8> {
    let mut types = Vec::new();
    let mut body = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let value = if is_alias(index) { &Value::Null } else { value };
        let (serial, bytes) = serial(value);
        types.extend(varint(serial));
        body.extend(bytes);
    }
    // The header size counts its own varint.
    let mut size = types.len() + 1;
    while varint(size as u64).len() + types.len() != size {
        size += 1;
    }
    let mut record = varint(size as u64);
    record.extend(types);
    record.extend(body);
    record
}

fn serial(value: &Value) -> (u64, Vec<u8>) {
    match value {
        Value::Null => (0, Vec::new()),
        Value::Integer(0) => (8, Vec::new()),
        Value::Integer(1) => (9, Vec::new()),
        Value::Integer(integer) => {
            let (serial, width) = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 6), (6, 8)]
                .into_iter()
                .find(|&(_, width)| {
                    width == 8
                        || (-(1i64 << (8 * width - 1))..1i64 << (8 * width - 1)).contains(integer)
                })
                .unwrap();
            (serial, integer.to_be_bytes()[8 - width..].to_vec())
        }
        Value::Real(real) => (7, real.to_be_bytes().to_vec()),
        Value::Text(text) => (13 + 2 * text.len() as u64, text.as_bytes().to_vec()),
        Value::Blob(blob) => (12 + 2 * blob.len() as u64, blob.clone()),
    }
}

fn varint(mut value: u64) -> Vec<u8> {
    if value >> 56 != 0 {
        let mut bytes = vec![value as u8];
        value >>= 8;
        for _ in 0..8 {
            bytes.push((value & 0x7f) as u8 | 0x80);
            value >>= 7;
        }
        bytes.reverse();
        return bytes;
    }
    let mut bytes = vec![(value & 0x7f) as u8];
    value >>= 7;
    while value > 0 {
        bytes.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    bytes.reverse();
    bytes
}

fn assert_perfect(fixture: &str, score: &Score) {
    assert!(score.expected > 0, "{fixture}: nothing to recover");
    assert_eq!(
        score.unmatched,
        Vec::<String>::new(),
        "{fixture}: reported, not deleted"
    );
    assert_eq!(
        score.missed,
        Vec::<String>::new(),
        "{fixture}: deleted, not recovered"
    );
}

#[test]
fn deleted_rows_dropped_tables_and_overflow() {
    let (score, recovered) = score("deleted.db", None);
    assert_perfect("deleted.db", &score);
    assert_eq!(recovered.problems, Vec::<String>::new());
    assert!(recovered.older_versions.is_empty());
    // The dropped table, read back from its deleted schema row; its rows,
    // whole, from the freelist leaf pages that were its leaves.
    let names: Vec<&str> = recovered
        .dropped_tables
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["scratch"]);
    let scratch: Vec<_> = recovered
        .records
        .iter()
        .filter(|record| record.table.as_deref() == Some("scratch"))
        .collect();
    assert_eq!(scratch.len(), 300);
    assert!(scratch.iter().all(|record| record.rowid.is_some()));
    // Records in freeblocks have lost their rowid; whole cells keep theirs.
    for record in &recovered.records {
        assert_eq!(
            record.rowid.is_some(),
            matches!(record.evidence, Evidence::Cell | Evidence::CellPointer),
            "{record:?}"
        );
    }
    // The long document's local part, its overflow chain broken where its
    // first page became a freelist trunk.
    let document = recovered
        .records
        .iter()
        .find(|record| record.table.as_deref() == Some("documents"))
        .unwrap();
    assert!(document.truncated, "{document:?}");
    assert_eq!(document.values[1], Some(Value::Text("long two".to_owned())));
}

#[test]
fn cleared_browser_history() {
    let (score, recovered) = score("chromium.db", None);
    assert_perfect("chromium.db", &score);
    for table in ["visits", "urls", "keyword_search_terms"] {
        assert!(
            recovered
                .records
                .iter()
                .any(|record| record.table.as_deref() == Some(table)),
            "{table}"
        );
    }
}

/// Rounds of inserts reusing space that deletions freed, a layout no one
/// planned. Two deleted records on disk are missed: in unallocated space,
/// each under a freeblock header from another layout of the page that
/// starts a byte into the cell, where no neighbouring cell leads the
/// search. One whole cell is reported with a wrong last value: later
/// writes overwrote the last bytes of its real, and a damaged tail still
/// reads as a value.
#[test]
fn deletions_and_reuse() {
    let (score, recovered) = score("churn.db", None);
    assert_eq!(recovered.problems, Vec::<String>::new());
    assert!(score.expected > 150, "{score:?}");
    assert!(score.missed.len() <= 2, "{:#?}", score.missed);
    assert!(score.unmatched.len() <= 1, "{:#?}", score.unmatched);
}

/// Rows deleted after a checkpoint: whole, with their rowids, in the frame
/// the deleting commit superseded (and damaged in the current page's
/// freeblocks); rows never committed, in the uncommitted frame only.
#[test]
fn older_versions_in_the_log() {
    let (score, recovered) = score("recovery-wal.db", Some("recovery-wal.db-wal"));
    assert_perfect("recovery-wal.db", &score);
    let deleted = deleted_rows("recovery-wal.db");
    for row in &deleted {
        let older: Vec<_> = recovered
            .older_versions
            .iter()
            .filter(|record| matches(record, row) && record.rowid == Some(row.rowid))
            .collect();
        let expected_state = |state: PageState| match row.kind.as_str() {
            "deleted" => matches!(
                state,
                PageState::Superseded { .. } | PageState::ReplacedInFile
            ),
            _ => matches!(state, PageState::Uncommitted { .. }),
        };
        if !older.is_empty() {
            assert!(
                older.iter().all(|record| expected_state(record.page_state)),
                "{older:?}"
            );
        }
    }
    let deleted_in_frames = deleted
        .iter()
        .filter(|row| row.kind == "deleted")
        .filter(|row| {
            recovered
                .older_versions
                .iter()
                .any(|record| matches(record, row) && record.rowid == Some(row.rowid))
        })
        .count();
    assert_eq!(deleted_in_frames, 7);
    assert!(recovered
        .records
        .iter()
        .all(|record| !record.page_state.is_older_version()));
    assert!(recovered
        .older_versions
        .iter()
        .all(|record| record.page_state.is_older_version()));
}

/// The reader's own log fixture (`tests/fixtures/gen.sh`), made for
/// reading, not recovery: its older frames still hold the row round two
/// deleted, the body round two replaced, and round three's rows that never
/// committed.
#[test]
fn the_reader_log_fixture_keeps_older_versions() {
    let (file, wal) = (support::fixture("wal.db"), support::fixture("wal.db-wal"));
    let recovered = Database::open_with_wal(&file, &wal).unwrap().recover();
    assert_eq!(recovered.records, []);
    let body = |record: &RecoveredRecord| match &record.values[1] {
        Some(Value::Text(text)) => text.clone(),
        other => panic!("{other:?}"),
    };
    let older = &recovered.older_versions;
    assert!(older.iter().any(|record| record.rowid == Some(4)
        && body(record).starts_with("round one, note 4: ")
        && matches!(record.page_state, PageState::Superseded { .. })));
    assert!(older
        .iter()
        .any(|record| record.rowid == Some(3) && body(record).starts_with("round one, note 3: ")));
    let never_committed: Vec<_> = older
        .iter()
        .filter(|record| body(record).starts_with("round three, never committed"))
        .collect();
    assert!(!never_committed.is_empty());
    assert!(never_committed
        .iter()
        .all(|record| matches!(record.page_state, PageState::Uncommitted { .. })));
    assert_eq!(older.len(), never_committed.len() + 2, "{older:#?}");
}

/// With `secure_delete` on, SQLite zeroes what it frees: nothing to
/// recover, and nothing made up. (`pages.db` among the reader's fixtures
/// was written so too.)
#[test]
fn secure_delete_yields_nothing() {
    for fixture in ["recovery/secure.db", "pages.db"] {
        let file = support::fixture(fixture);
        let recovered = Database::open(&file).unwrap().recover();
        assert_eq!(recovered.records, [], "{fixture}");
        assert_eq!(recovered.older_versions, [], "{fixture}");
    }
}

/// No fixture of the reader (no deletions, or zeroed ones) yields a record
/// that is a live row.
#[test]
fn live_rows_are_never_reported() {
    let fixtures = [
        ("types.db", None),
        ("utf16le.db", None),
        ("utf16be.db", None),
        ("reserved.db", None),
        ("autovacuum.db", None),
        ("wal.db", Some("wal.db-wal")),
    ];
    for (fixture, wal) in fixtures {
        let file = support::fixture(fixture);
        let log = wal.map_or_else(Vec::new, support::fixture);
        let db = Database::open_with_wal(&file, &log).unwrap();
        let recovered = db.recover();
        for record in &recovered.records {
            let table = record.table.as_deref().unwrap_or_default();
            let live = db
                .rows(table)
                .map(Iterator::collect::<Vec<_>>)
                .unwrap_or_default();
            let is_live =
                live.iter().any(|row| {
                    record.rowid.map_or(true, |rowid| rowid == row.rowid)
                        && record.values.iter().zip(&row.values).all(|(ours, theirs)| {
                            ours.as_ref().map_or(true, |ours| same(ours, theirs))
                        })
                });
            assert!(!is_live, "{fixture}: live row reported: {record:?}");
        }
    }
}

#[test]
fn the_test_encoder_writes_records_as_sqlite_does() {
    // id INTEGER PRIMARY KEY, then 'hi', 300, 1, NULL: header 6 bytes.
    let values = [
        Value::Integer(5),
        Value::Text("hi".to_owned()),
        Value::Integer(300),
        Value::Integer(1),
        Value::Null,
    ];
    let record = encode(&values, |index| index == 0);
    assert_eq!(record, [6, 0, 17, 2, 9, 0, b'h', b'i', 0x01, 0x2c]);
    assert_eq!(varint(128), [0x81, 0x00]);
    assert_eq!(varint(u64::MAX), [0xff; 9]);
}
