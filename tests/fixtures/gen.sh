#!/bin/sh
# Recreates tests/fixtures/*.db (and wal.db-wal) and tests/oracle/.
#
# Every database is made here by the sqlite3 command-line shell (public
# domain) from the synthetic SQL below; no real data goes in. The oracle is
# sqlite3's own reading of each file: `sqlite3 -json` output of every table,
# index and the schema, one file per object in tests/oracle/<fixture>/.
# Each value is written as "type:value" text, so the JSON keeps what SQLite
# stored: integers in decimal, reals to 17 significant digits (which round
# trip), text as text, blobs in hex; NULL stays null.
#
# Run on a Linux machine (made with sqlite3 3.46.1 on Debian 13):
#   sudo apt-get install sqlite3
#   sh tests/fixtures/gen.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
oracle_root="$here/../oracle"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

# A value as "type:value" text; @ stands for the expression.
typed="CASE typeof(@) WHEN 'integer' THEN 'integer:' || @ WHEN 'real' THEN 'real:' || printf('%.17g', @) WHEN 'text' THEN 'text:' || @ WHEN 'blob' THEN 'blob:' || hex(@) END"
# The same, quoted to sit inside an SQL string literal.
typed_literal=$(printf '%s' "$typed" | sed "s/'/''/g")

# typed_expression EXPRESSION: $typed applied to EXPRESSION.
typed_expression() {
    printf '%s' "$typed" | sed "s/@/$1/g"
}

# table_oracle DATABASE DIRECTORY TABLE: every row, rowid then every column
# (generated ones included), in rowid order.
table_oracle() {
    query=$(sqlite3 "$1" "SELECT 'SELECT rowid AS \"rowid\"' || group_concat(', ' || replace('$typed_literal', '@', '\"' || replace(name, '\"', '\"\"') || '\"') || ' AS \"' || replace(name, '\"', '\"\"') || '\"', '') || ' FROM \"$3\" ORDER BY rowid' FROM pragma_table_xinfo('$3')")
    sqlite3 -json "$1" "$query" > "$2/$3.json"
}

# entries_oracle DATABASE DIRECTORY NAME FROM EXPRESSION...: the values of
# EXPRESSIONs, as columns c0, c1, ..., over FROM (which orders them as the
# index does).
entries_oracle() {
    database=$1 directory=$2 name=$3 from=$4
    shift 4
    columns="" index=0
    for expression in "$@"; do
        columns="$columns${columns:+, }$(typed_expression "$expression") AS c$index"
        index=$((index + 1))
    done
    sqlite3 -json "$database" "SELECT $columns $from" > "$directory/$name.json"
}

# write_oracle FIXTURE [DATABASE]: the schema and every rowid table of DATABASE
# (default: the fixture itself), into tests/oracle/FIXTURE/.
write_oracle() {
    database=${2:-$here/$1}
    directory="$oracle_root/$1"
    rm -rf "$directory"
    mkdir -p "$directory"
    entries_oracle "$database" "$directory" sqlite_schema \
        "FROM sqlite_schema ORDER BY rowid" type name tbl_name rootpage sql
    sqlite3 "$database" "SELECT name FROM sqlite_schema WHERE type = 'table' AND sql NOT LIKE 'CREATE VIRTUAL%' AND sql NOT LIKE '%WITHOUT ROWID%'" |
        while IFS= read -r table; do
            table_oracle "$database" "$directory" "$table"
        done
}

# create FIXTURE: a database from the SQL on standard input.
create() {
    rm -f "$here/$1"
    sqlite3 "$here/$1" > /dev/null
}

# Every serial type, quoted names, constraints, rowid aliases, columns added
# later with defaults, generated columns, a WITHOUT ROWID table, an index, a
# view and a trigger.
create types.db <<'SQL'
PRAGMA page_size = 512;
CREATE TABLE kinds (
    id INTEGER PRIMARY KEY,
    "odd ""name""" TEXT,
    [bracket col] BLOB,
    `tick` REAL,
    n NUMERIC DEFAULT 0, -- a comment
    CHECK (id > 0)
);
INSERT INTO kinds VALUES (1, NULL, NULL, NULL, NULL);
INSERT INTO kinds (id, n) VALUES
    (2, 0), (3, 1), (4, 127), (5, -128), (6, 32767), (7, -32768),
    (8, 8388607), (9, -8388608), (10, 2147483647), (11, -2147483648),
    (12, 140737488355327), (13, -140737488355328),
    (14, 9223372036854775807), (15, -9223372036854775808);
INSERT INTO kinds VALUES
    (16, '', x'', 0.5, 3.25),
    (17, 'héllo wörld ✓ 😀', x'00ff10', -1.5e300, 1e-300),
    (18, 'tab	and "quotes"', x'01', 3.0, 2.5),
    (19, 'x', x'02', 1e15, -0.0);
ALTER TABLE kinds ADD COLUMN added TEXT DEFAULT 'later';
ALTER TABLE kinds ADD COLUMN added_real REAL DEFAULT -2;
INSERT INTO kinds VALUES (20, 'after', NULL, 7, 8, 'set', 4.5);
CREATE INDEX kinds_n ON kinds (n);
CREATE TABLE alias_by_table_key (x INTEGER, y TEXT, PRIMARY KEY (x DESC));
INSERT INTO alias_by_table_key VALUES (5, 'five'), (-3, 'minus three');
CREATE TABLE no_alias_desc (x INTEGER PRIMARY KEY DESC, y TEXT);
INSERT INTO no_alias_desc VALUES (5, 'five'), (7, 'seven');
CREATE TABLE no_alias_int (x INT PRIMARY KEY, y TEXT);
INSERT INTO no_alias_int VALUES (5, 'five');
CREATE TABLE gen (
    a INTEGER, b TEXT, c AS (a * 2) VIRTUAL,
    d TEXT GENERATED ALWAYS AS (upper(b)) STORED, e TEXT
);
INSERT INTO gen (a, b, e) VALUES (1, 'one', 'e1'), (2, 'two', 'e2');
CREATE TABLE wr (k TEXT PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO wr VALUES ('b', 2), ('a', 1), ('c', x'03');
CREATE TABLE "quoted table" ("a b", 'c' TEXT /* inline */, d DEFAULT x'0a0b');
INSERT INTO "quoted table" VALUES (1, 'two', 3);
CREATE VIEW v AS SELECT id FROM kinds;
CREATE TRIGGER trg AFTER INSERT ON gen BEGIN SELECT 1; END;
SQL
write_oracle types.db
entries_oracle "$here/types.db" "$oracle_root/types.db" kinds_n \
    "FROM kinds ORDER BY n, id" n id
entries_oracle "$here/types.db" "$oracle_root/types.db" wr "FROM wr ORDER BY k" k v

# Interior pages (table and index), overflow chains, and deleted rows. On
# 512-byte pages a table cell keeps its whole payload up to X = 477 bytes,
# an index cell up to 102; a larger one keeps K = M + (P - M) % 508 bytes
# (M = 39) when K fits under X, else M. The "edge" rows run from 475 to 480
# bytes in the table and, in events_note, from 101 to 104; the 560-character
# note keeps K in both trees; the 4500-character one keeps K in the table and
# M in the index.
create pages.db <<'SQL'
PRAGMA page_size = 512;
CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, note TEXT, data BLOB);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 300)
INSERT INTO events SELECT i, 'kind-' || (i % 7), printf('note %04d', i), NULL FROM seq;
WITH RECURSIVE seq(i) AS (SELECT 465 UNION ALL SELECT i + 1 FROM seq WHERE i < 470)
INSERT INTO events SELECT 1000 + i, 'edge', replace(hex(zeroblob(i)), '00', 'e'), NULL FROM seq;
WITH RECURSIVE seq(i) AS (SELECT 95 UNION ALL SELECT i + 1 FROM seq WHERE i < 98)
INSERT INTO events SELECT 1000 + i, 'edge', replace(hex(zeroblob(i)), '00', 'i'), NULL FROM seq;
INSERT INTO events VALUES (1560, 'edge', replace(hex(zeroblob(560)), '00', 'k'), NULL);
INSERT INTO events VALUES (
    2001, 'big', replace(hex(zeroblob(500)), '00', 'overflow '),
    CAST(replace(hex(zeroblob(1500)), '00', 'ab') AS BLOB)
);
CREATE INDEX events_note ON events (note);
CREATE INDEX events_kind ON events (kind, id);
DELETE FROM events WHERE id % 50 = 0;
SQL
write_oracle pages.db
entries_oracle "$here/pages.db" "$oracle_root/pages.db" events_note \
    "FROM events ORDER BY note, id" note id
entries_oracle "$here/pages.db" "$oracle_root/pages.db" events_kind \
    "FROM events ORDER BY kind, id" kind id id

# Text in UTF-16, both byte orders, with characters outside the BMP and a
# value that spills onto an overflow page.
for order in le be; do
    create "utf16$order.db" <<SQL
PRAGMA encoding = 'UTF-16$order';
PRAGMA page_size = 512;
CREATE TABLE "wörter" (id INTEGER PRIMARY KEY, word TEXT, note TEXT);
INSERT INTO "wörter" VALUES
    (1, 'héllo', 'accents'),
    (2, '日本語', 'CJK'),
    (3, '😀 grinning', 'outside the BMP'),
    (4, replace(hex(zeroblob(150)), '00', 'é✓'), 'overflowing UTF-16');
SQL
    write_oracle "utf16$order.db"
done

# The largest page size, stored as 1 in the header.
create page65536.db <<'SQL'
PRAGMA page_size = 65536;
CREATE TABLE big_pages (id INTEGER PRIMARY KEY, body TEXT);
INSERT INTO big_pages VALUES (1, 'on a 64 KiB page'), (2, replace(hex(zeroblob(100)), '00', 'x'));
SQL
write_oracle page65536.db

# 32 reserved bytes per page: a usable size of 480, the smallest allowed.
create reserved.db <<'SQL'
.filectrl reserve_bytes 32
PRAGMA page_size = 512;
CREATE TABLE reserved (id INTEGER PRIMARY KEY, body TEXT);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 40)
INSERT INTO reserved SELECT i, printf('row %d of the reserved-space table', i) FROM seq;
INSERT INTO reserved VALUES (100, replace(hex(zeroblob(300)), '00', 'spill'));
SQL
write_oracle reserved.db

# Auto-vacuum: pointer map pages among the b-tree pages.
create autovacuum.db <<'SQL'
PRAGMA page_size = 512;
PRAGMA auto_vacuum = FULL;
CREATE TABLE a (x INTEGER);
CREATE TABLE b (y TEXT);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 120)
INSERT INTO a SELECT i * 1000003 FROM seq;
INSERT INTO b VALUES ('short'), (replace(hex(zeroblob(400)), '00', 'vac'));
DELETE FROM a WHERE x % 3 = 0;
SQL
write_oracle autovacuum.db

# Write-ahead log, copied while a writer is mid-transaction. Round one is
# checkpointed into the database file; round two then restarts the log
# (new salts) and commits only into it, leaving round one's frames behind it
# under the old salts; round three spills uncommitted frames after round
# two's. sqlite3 reading the copy (the oracle) applies round two only.
rm -f "$here/wal.db" "$here/wal.db-wal"
{
    echo "PRAGMA page_size = 512;"
    echo "PRAGMA journal_mode = WAL;"
    echo "PRAGMA wal_autocheckpoint = 0;"
    echo "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);"
    for i in $(seq 1 24); do
        echo "INSERT INTO notes (body) VALUES ('round one, note $i: ' || replace(hex(zeroblob(40)), '00', 'w'));"
    done
    echo "PRAGMA wal_checkpoint(PASSIVE);"
    echo "INSERT INTO notes (body) VALUES ('round two: committed to the WAL only');"
    echo "UPDATE notes SET body = 'round two: edited in the WAL' WHERE id = 3;"
    echo "DELETE FROM notes WHERE id = 4;"
    echo "PRAGMA cache_size = 1;"
    echo "PRAGMA cache_spill = 1;"
    echo "BEGIN;"
    echo "WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 12)"
    echo "INSERT INTO notes (body) SELECT 'round three, never committed: ' || replace(hex(zeroblob(100)), '00', 'u') FROM seq;"
    echo ".shell cp live.db '$here/wal.db' && cp live.db-wal '$here/wal.db-wal'"
    echo "ROLLBACK;"
} | sqlite3 live.db > /dev/null

# sqlite3 checkpoints and removes a log it opens: read throwaway copies.
cp "$here/wal.db" with-wal.db && cp "$here/wal.db-wal" with-wal.db-wal
write_oracle wal.db "$work/with-wal.db"
cp "$here/wal.db" file-only.db
write_oracle wal.db-file-only "$work/file-only.db"
