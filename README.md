# sqlite

SQLite database files, read without SQLite: the header, the schema, the rows of every table, the entries of every index, the committed frames of a write-ahead log, and the records of deleted rows that SQLite freed but didn't overwrite, read from SQLite's [file format specification](https://www.sqlite.org/fileformat2.html). Read-only, no dependencies, and the file is borrowed, not copied.

```toml
[dependencies]
sootmark-sqlite = "0.2"
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
let recovered = db.recover();
for record in recovered.records.iter().chain(&recovered.older_versions) {
    println!("{:?} {:?} page {} {:?} {:?}: {:?}", record.table, record.rowid, record.page,
        record.page_state, record.confidence, record.values);
}
```

## What you get

- `Database::open(bytes)`: the header (page size, 65536 included; reserved bytes per page; text encoding; page count; freelist trunk and count; schema cookie and format; user version, application id, the SQLite version that last wrote it, …), every row of the schema table (`sqlite_schema`, also called `sqlite_master`), and the tables it declares.
- `Database::open_with_wal(bytes, wal)`: the same as SQLite sees it with its `-wal` file, which often holds the most recent changes: each page from the last valid frame for it at or before the last commit frame. Frames are valid while their salts match the log header's and the running checksum holds (either byte order); the log ends at the first that fails. `db.wal` counts the frames: whole, valid, committed (applied), so the uncommitted ones and those left over from before a checkpoint show.
- `db.image()`: the database as one file, the log's committed pages in place of the file's (what a checkpoint would write), for tools that take one file.
- `db.tables`: name, root page, `CREATE TABLE` statement, and columns read from it: names unquoted (`"…"`, `[…]`, `` `…` ``, `'…'`), declared type and its affinity, the `INTEGER PRIMARY KEY` that aliases the rowid (with SQLite's quirks: `INT` doesn't, `PRIMARY KEY DESC` on the column doesn't, `PRIMARY KEY (x DESC)` does), literal defaults, generated columns.
- `db.rows(table)`: an iterator over the rows in rowid order, each with its rowid, the leaf page holding it, and one `Value` (`Null`, `Integer`, `Real`, `Text`, `Blob`) per column. The rowid alias column holds the rowid; columns added after a row was written hold their default; integers stored in a `REAL` column read as reals; text is decoded from UTF-8, UTF-16le or UTF-16be. Rows are read as the iterator advances, never all at once.
- `db.index_entries(name)`: the entries of an index (indexed columns then rowid) or of a `WITHOUT ROWID` table (primary key columns first), in key order, interior pages included.
- `db.recover()`: records of deleted rows, matched with the tables they fit, each with where it was found (page, offset, source) and how (see below).
- B-tree pages of the four kinds, page 1's 100-byte offset, cell pointer arrays, varints, every serial type of the record format, and overflow chains with the specification's exact local payload sizes (X = U − 35 on table leaves, (U − 12) × 64 / 255 − 23 on index pages, M and K as given).

## Deleted records

`db.recover()` searches where SQLite leaves freed content, and returns `records` (the database as it reads now) and `older_versions` (pages the write-ahead log keeps older copies of) apart:

| Source | What is there | What comes back |
|---|---|---|
| Freeblocks of table leaf pages | Cells freed inside the content area. SQLite writes a 4-byte freeblock header (next freeblock, size) over each freed cell's start: its payload size and rowid varints, and, when those took fewer than 4 bytes, the record's header size and perhaps its first serial type. | The record, without its rowid. Its start is rebuilt from each table's shape: an intact record header past byte 4, a lost header size (implied by the serial types that follow), or a lost first serial type (NULL for an `INTEGER PRIMARY KEY` column; a number of one of the widths numbers take, confirmed by the next cell starting right after it; else the bytes up to where the freeblock ends, reported as low confidence). Adjacent freed cells are read as a run, the readings that tile the freeblock exactly preferred. |
| Unallocated space of table pages (between the cell pointer array and the content area) | Cells freed at the start of the content area, which SQLite frees by moving the area's start, writing nothing; leftovers of earlier layouts (a leaf that became an interior page keeps its old cells there). | Whole cells, rowid included, found wherever their payload size and rowid agree with their record; damaged cells right after them. |
| Freelist pages | Pages freed whole: a dropped table's, emptied pages, overflow pages. Leaf pages are left as they were freed; a trunk page's first bytes list leaf page numbers. | Leaf pages still laid out as table leaves: every cell their pointer array lists, plus their freeblocks and unallocated space. Other free pages and trunks (after their list): whole cells wherever they are. |
| Dropped tables | The schema row of a dropped table is a deleted record of `sqlite_schema` too. | Its `CREATE TABLE` read back (`dropped_tables`), and records matched with it as with any table. |
| Write-ahead log | Frames a later commit superseded, frames of transactions that never committed, frames left past the valid log from before it restarted, and the database file's own copy of every page a committed frame replaces. | Every cell of those older page versions (rows deleted or changed since, or never committed), and their freeblocks and unallocated space, in `older_versions` with the frame's position and state. |

A candidate must read as a record (a header whose serial types account for its bytes, valid text, no NaN) and fit a table: as many values as its columns (or as few as live rows store, for columns added later), each of a type SQLite could store in that column (NULL for the rowid alias, no numbers in a `TEXT` column). A damaged record also needs every value of its column's usual type and at least 4 bytes of values. Each record says which table it fits best, which others fit as well, its evidence (cell pointer, whole cell, intact record header, rebuilt from the table's shape), a confidence, how many values it stores, and its values per column (`None` where lost). Copies of live rows, and further copies of a record already found, are left out; a damaged copy matches on what it still has.

Deleted records whose payload spilled to overflow pages keep their local part; the chain is followed only while its pages are freelist leaves (or, for older page versions, overflow pages in use), never twice, and ending where the payload does; otherwise the record is marked `truncated`, its last value cut where the bytes stop. Freeing the first overflow page of a chain often makes it a freelist trunk, which overwrites its start: such chains stop there.

Not recovered: cells freed in a database written with `secure_delete` on (SQLite zeroes them; Debian's sqlite3 is built with it on by default); bytes since overwritten (a damaged tail can still read as plausible values); deleted entries of index b-trees and of `WITHOUT ROWID` tables; records of tables whose schema is gone and whose schema row is gone too (cells a free or older page points to are still returned, unmatched, as stored); freed cells in unallocated space that no neighbouring cell leads to, whose start was overwritten; rowids of cells whose start was overwritten; pages neither in a tree nor on the freelist; rollback journals.

## Damage is contained

Evidence is hostile. Only a file that isn't a database, or whose header can't be used (a page size that isn't a power of two from 512 to 65536, reserved bytes that leave under 480 usable), is an error. The rest is listed in `problems` and skipped: pages out of range or past the end of a truncated file, b-tree, overflow and freelist chains that loop or cross (every page is visited at most once per walk), freeblock chains out of order, pages of the wrong kind, cell pointers off the page, cells and records cut short (the values before the damage are kept), impossible sizes, a header page count a legacy writer left stale (the file's size is used, as SQLite does), a damaged or foreign log (ignored, as SQLite ignores it). Nothing is allocated beyond what the input holds, and every walk ends.

## Not yet

- Recovery from index b-trees and `WITHOUT ROWID` tables.
- `WITHOUT ROWID` tables as rows with named columns (their entries are read, primary key first).
- Rollback journals (`-journal`, read or recovered from), and the `-shm` WAL index (not needed: the log is read directly).
- Virtual generated columns and non-literal defaults read as NULL: no SQL is evaluated.

## How it's checked

| Check | Result |
|---|---|
| Databases made by the `sqlite3` shell 3.46.1 from synthetic SQL (`tests/fixtures/gen.sh`), against `sqlite3 -json` output of every table, index and the schema (`tests/oracle/`, values typed so reals compare to the bit): serial types 0 to 9 (every integer width), empty and non-empty text and blobs, quoted names, constraints, rowid aliases and non-aliases, `ALTER TABLE … ADD COLUMN` defaults, generated columns, a `WITHOUT ROWID` table; 300 rows on 512-byte pages with interior table and index pages, overflow chains, payloads either side of every spill threshold, deleted rows; UTF-16le and UTF-16be with text outside the BMP; 65536-byte pages; 32 reserved bytes per page (480 usable); auto-vacuum pointer map pages | 9 databases, 1,173 rows and entries: all match |
| A write-ahead log copied while a writer was mid-transaction: one committed transaction after a checkpoint restarted the log, the spilled frames of one that never committed, earlier frames under old salts | matches sqlite3 reading the same pair; the file alone matches sqlite3 reading the file alone; the same log re-checksummed big-endian, as a big-endian machine writes it, reads the same |
| Logs damaged in a committed frame, in the header, cut mid-frame, or not logs at all; databases with b-tree and overflow cycles, pages out of range, cut in half, a stale page count | each reported, every walk ends |
| Recovery from databases made by the `sqlite3` shell 3.46.1 from synthetic SQL with `secure_delete` off (`tests/fixtures/recovery/gen.sh`), against the rows the generator deleted (`tests/oracle/recovery/`, dumped before each deletion): single rows, a range, a table without a rowid alias, rowids and payloads either side of 127 bytes, texts, integers, reals, blobs and NULLs, a long text spilling to overflow pages, a dropped table; a history in the shape of Chromium's (`urls`, `visits`, `keyword_search_terms`) with a time range cleared; saved logins in a log, rows deleted after a checkpoint and a transaction never committed; six rounds of inserts reusing space deletions freed. A deleted row counts as left on disk when its record, encoded independently by the test as SQLite writes it, is in the file or log from its third byte on | deleted rows left on disk recovered, and records reported that are deleted rows: 324 of 324 and 324 of 324 (`deleted.db`); 85 of 85 and 85 of 85 (Chromium-like); 19 of 19 and 26 of 26 (log; 7 rows whole with their rowids in the superseded frame, 12 never committed); 175 of 177 and 175 of 176 (churn: two cells under a freeblock header from another layout of their page, one whole cell whose last value later writes overwrote) |
| The same deletions with `secure_delete` on, and the reader's own fixtures above (deletions zeroed, or none) | nothing recovered from the database files; no live row reported. From the reader's log fixture: the row its round two deleted (whole, in the frame round two superseded), the body it replaced, and the spilled rows of round three that never committed |
| Property tests: arbitrary bytes, arbitrary pages behind a real header, real databases (with deletions too) and logs (with older page versions too) damaged and cut anywhere | no panic, row counts and recovered records bounded by the input (256 cases each per run) |

## Licence

MIT or Apache-2.0, at your option. The test databases are made by the scripts in this repository from synthetic data.
