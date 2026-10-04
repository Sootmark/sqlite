# sqlite

SQLite database files, read without SQLite: the header, the schema, the rows of every table, the entries of every index, and the committed frames of a write-ahead log, read from SQLite's [file format specification](https://www.sqlite.org/fileformat2.html). Read-only, no dependencies, and the file is borrowed, not copied.

```toml
[dependencies]
sootmark-sqlite = "0.1"
```

```rust
let file = std::fs::read("History")?;
let wal = std::fs::read("History-wal").unwrap_or_default();
let db = sqlite::Database::open_with_wal(&file, &wal)?;
for table in &db.tables {
    println!("{} (root page {}): {:?}", table.name, table.root_page, table.column_names());
}
let mut rows = db.rows("urls")?;
for row in &mut rows {
    println!("{} {:?}", row.rowid, row.values);
}
for problem in db.problems.iter().chain(rows.problems()) {
    eprintln!("{problem}");
}
```

## What you get

- `Database::open(bytes)`: the header (page size, 65536 included; reserved bytes per page; text encoding; page count; freelist trunk and count; schema cookie and format; user version, application id, the SQLite version that last wrote it, …), every row of the schema table (`sqlite_schema`, also called `sqlite_master`), and the tables it declares.
- `Database::open_with_wal(bytes, wal)`: the same as SQLite sees it with its `-wal` file, which often holds the most recent changes: each page from the last valid frame for it at or before the last commit frame. Frames are valid while their salts match the log header's and the running checksum holds (either byte order); the log ends at the first that fails. `db.wal` counts the frames: whole, valid, committed (applied), so the uncommitted ones and those left over from before a checkpoint show.
- `db.tables`: name, root page, `CREATE TABLE` statement, and columns read from it: names unquoted (`"…"`, `[…]`, `` `…` ``, `'…'`), declared type and its affinity, the `INTEGER PRIMARY KEY` that aliases the rowid (with SQLite's quirks: `INT` doesn't, `PRIMARY KEY DESC` on the column doesn't, `PRIMARY KEY (x DESC)` does), literal defaults, generated columns.
- `db.rows(table)`: an iterator over the rows in rowid order, each with its rowid, the leaf page holding it, and one `Value` (`Null`, `Integer`, `Real`, `Text`, `Blob`) per column. The rowid alias column holds the rowid; columns added after a row was written hold their default; integers stored in a `REAL` column read as reals; text is decoded from UTF-8, UTF-16le or UTF-16be. Rows are read as the iterator advances, never all at once.
- `db.index_entries(name)`: the entries of an index (indexed columns then rowid) or of a `WITHOUT ROWID` table (primary key columns first), in key order, interior pages included.
- B-tree pages of the four kinds, page 1's 100-byte offset, cell pointer arrays, varints, every serial type of the record format, and overflow chains with the specification's exact local payload sizes (X = U − 35 on table leaves, (U − 12) × 64 / 255 − 23 on index pages, M and K as given).

## Damage is contained

Evidence is hostile. Only a file that isn't a database, or whose header can't be used (a page size that isn't a power of two from 512 to 65536, reserved bytes that leave under 480 usable), is an error. The rest is listed in `problems` and skipped: pages out of range or past the end of a truncated file, b-tree and overflow chains that loop or cross (every page is visited at most once per walk), pages of the wrong kind, cell pointers off the page, cells and records cut short (the values before the damage are kept), impossible sizes, a header page count a legacy writer left stale (the file's size is used, as SQLite does), a damaged or foreign log (ignored, as SQLite ignores it). Nothing is allocated beyond what the input holds, and every walk ends.

## Not yet

- Recovery of deleted records from freeblocks, freelist pages and unallocated space.
- `WITHOUT ROWID` tables as rows with named columns (their entries are read, primary key first).
- Rollback journals (`-journal`), and the `-shm` WAL index (not needed: the log is read directly).
- Virtual generated columns and non-literal defaults read as NULL: no SQL is evaluated.

## How it's checked

| Check | Result |
|---|---|
| Databases made by the `sqlite3` shell 3.46.1 from synthetic SQL (`tests/fixtures/gen.sh`), against `sqlite3 -json` output of every table, index and the schema (`tests/oracle/`, values typed so reals compare to the bit): serial types 0 to 9 (every integer width), empty and non-empty text and blobs, quoted names, constraints, rowid aliases and non-aliases, `ALTER TABLE … ADD COLUMN` defaults, generated columns, a `WITHOUT ROWID` table; 300 rows on 512-byte pages with interior table and index pages, overflow chains, payloads either side of every spill threshold, deleted rows; UTF-16le and UTF-16be with text outside the BMP; 65536-byte pages; 32 reserved bytes per page (480 usable); auto-vacuum pointer map pages | 9 databases, 1,173 rows and entries: all match |
| A write-ahead log copied while a writer was mid-transaction: one committed transaction after a checkpoint restarted the log, the spilled frames of one that never committed, earlier frames under old salts | matches sqlite3 reading the same pair; the file alone matches sqlite3 reading the file alone; the same log re-checksummed big-endian, as a big-endian machine writes it, reads the same |
| Logs damaged in a committed frame, in the header, cut mid-frame, or not logs at all; databases with b-tree and overflow cycles, pages out of range, cut in half, a stale page count | each reported, every walk ends |
| Property tests: arbitrary bytes, arbitrary pages behind a real header, real databases and the log damaged and cut anywhere | no panic, row counts bounded by the input (256 cases each per run) |

## Licence

MIT or Apache-2.0, at your option. The test databases are made by the scripts in this repository from synthetic data.
