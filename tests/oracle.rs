//! Every fixture against sqlite3's own reading of it (`tests/oracle/`,
//! written by `tests/fixtures/gen.sh`): the schema, every row of every
//! table (rowid, column names, values) and every entry of every index, in
//! order.

mod support;

use sqlite::{Database, Generated, Table, Value};
use support::json;

/// The fixtures, and the oracle directory each is compared with.
const FIXTURES: [&str; 8] = [
    "types.db",
    "pages.db",
    "utf16le.db",
    "utf16be.db",
    "page65536.db",
    "reserved.db",
    "autovacuum.db",
    "wal.db-file-only",
];

#[test]
fn every_fixture_matches_sqlite3() {
    for name in FIXTURES {
        let file = support::fixture(name.trim_end_matches("-file-only"));
        let db = Database::open(&file).unwrap();
        check(name, &db);
    }
}

/// The write-ahead log's committed frames are applied: the rows round two
/// changed only in the log read as sqlite3 reads them.
#[test]
fn wal_fixture_with_its_log_matches_sqlite3() {
    let (file, wal) = (support::fixture("wal.db"), support::fixture("wal.db-wal"));
    let db = Database::open_with_wal(&file, &wal).unwrap();
    check("wal.db", &db);
}

/// Compare `db` with every oracle file of `fixture`.
fn check(fixture: &str, db: &Database) {
    assert_eq!(db.problems, Vec::<String>::new(), "{fixture}");
    let mut checked = 0;
    for (object, rows) in support::oracle(fixture) {
        checked += rows.len();
        if object == "sqlite_schema" {
            check_schema(fixture, db, &rows);
        } else if let Some(table) = db
            .table(&object)
            .filter(|t| t.kind == sqlite::TableKind::Rowid)
        {
            check_table(fixture, db, table, &rows);
        } else {
            check_index(fixture, db, &object, &rows);
        }
    }
    assert!(checked > 0, "{fixture}: no oracle rows");
}

fn check_schema(fixture: &str, db: &Database, oracle: &[json::Row]) {
    let ours: Vec<Vec<Value>> = db
        .schema
        .iter()
        .map(|entry| {
            vec![
                Value::Text(entry.kind.clone()),
                Value::Text(entry.name.clone()),
                Value::Text(entry.table_name.clone()),
                Value::Integer(entry.root_page.into()),
                entry.sql.clone().map_or(Value::Null, Value::Text),
            ]
        })
        .collect();
    assert_eq!(ours, values(oracle), "{fixture}: sqlite_schema");
}

fn check_table(fixture: &str, db: &Database, table: &Table, oracle: &[json::Row]) {
    let context = format!("{fixture}: {}", table.name);
    let mut rows = db.rows(&table.name).unwrap();
    let ours: Vec<_> = rows.by_ref().collect();
    assert_eq!(rows.problems(), &[] as &[String], "{context}");
    assert_eq!(ours.len(), oracle.len(), "{context}: rows");
    for (row, expected) in ours.iter().zip(oracle) {
        let (rowid, columns) = expected.split_first().unwrap();
        assert_eq!(
            rowid,
            &("rowid".to_owned(), json::Value::Integer(row.rowid)),
            "{context}"
        );
        let names: Vec<_> = columns.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(table.column_names(), names, "{context}: columns");
        for ((column, value), (_, expected)) in table.columns.iter().zip(&row.values).zip(columns) {
            // A virtual generated column is computed, not stored.
            if column.generated != Some(Generated::Virtual) {
                let expected = support::oracle_value(expected);
                assert_eq!(
                    value, &expected,
                    "{context}: row {}, {}",
                    row.rowid, column.name
                );
            }
        }
    }
}

fn check_index(fixture: &str, db: &Database, name: &str, oracle: &[json::Row]) {
    let mut entries = db.index_entries(name).unwrap();
    let ours: Vec<_> = entries.by_ref().map(|entry| entry.values).collect();
    assert_eq!(entries.problems(), &[] as &[String], "{fixture}: {name}");
    assert_eq!(ours, values(oracle), "{fixture}: {name}");
}

fn values(oracle: &[json::Row]) -> Vec<Vec<Value>> {
    oracle
        .iter()
        .map(|row| {
            row.iter()
                .map(|(_, value)| support::oracle_value(value))
                .collect()
        })
        .collect()
}
