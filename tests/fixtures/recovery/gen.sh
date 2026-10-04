#!/bin/sh
# Recreates tests/fixtures/recovery/*.db (and recovery-wal.db-wal) and
# tests/oracle/recovery/: databases with deleted rows, and the rows deleted.
#
# Every database is made here by the sqlite3 command-line shell (public
# domain) from the synthetic SQL below; no real data goes in. Each fixture's
# setup SQL is first run alone in a scratch database, and the rows its
# deletions will remove are dumped from there with `sqlite3 -json`, one file
# per deletion in tests/oracle/recovery/<fixture>/: the table, the row's
# kind (deleted, dropped with its table, or never committed), its rowid and
# its values c0, c1, ... as "type:value" text (as in ../gen.sh, but reals
# with the `!` flag: SQLite's printf gives 16 significant digits without
# it, too few to round-trip). The values are deterministic, so the fixture
# made next holds the same rows.
#
# Debian's sqlite3 is built with SECURE_DELETE, which zeroes deleted
# content: every session that deletes says which it wants.
#
# Run on a Linux machine (made with sqlite3 3.46.1 on Debian 13):
#   sudo apt-get install sqlite3
#   sh tests/fixtures/recovery/gen.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
oracle_root="$here/../../oracle/recovery"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

# A value as "type:value" text; @ stands for the expression.
typed="CASE typeof(@) WHEN 'integer' THEN 'integer:' || @ WHEN 'real' THEN 'real:' || printf('%!.17g', @) WHEN 'text' THEN 'text:' || @ WHEN 'blob' THEN 'blob:' || hex(@) END"
typed_literal=$(printf '%s' "$typed" | sed "s/'/''/g")

# rows_oracle DATABASE FILE TABLE KIND CONDITION: the rows of TABLE meeting
# CONDITION (single quotes doubled), in rowid order, as oracle objects.
rows_oracle() {
    query=$(sqlite3 "$1" "SELECT 'SELECT ''$3'' AS \"table\", ''$4'' AS \"kind\", rowid AS \"rowid\"' || group_concat(', ' || replace('$typed_literal', '@', '\"' || name || '\"') || ' AS c' || cid, '') || ' FROM \"$3\" WHERE $5 ORDER BY rowid' FROM pragma_table_xinfo('$3')")
    sqlite3 -json "$1" "$query" > "$2"
}

# fixture_start FIXTURE: an empty oracle directory for FIXTURE.
fixture_start() {
    rm -rf "$oracle_root/$1" "$here/$1" "$here/$1-wal"
    mkdir -p "$oracle_root/$1"
}

# --- deleted.db and secure.db -------------------------------------------
# Texts, integers (one- to eight-byte), reals, blobs and NULLs; a rowid
# alias table with rowids either side of 127 (one- and two-byte varints) and
# payloads either side of 127 bytes; a table without a rowid alias; long
# text spilling to overflow pages; a table dropped whole.
people_setup() {
    cat <<'SQL'
CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, score REAL, avatar BLOB, note TEXT);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 200)
INSERT INTO people SELECT
    i,
    printf('person %03d', i),
    CASE WHEN i % 11 = 0 THEN NULL ELSE i * 1000003 % 100000 - 40000 END,
    CASE WHEN i % 13 = 0 THEN NULL ELSE i + 0.25 END,
    CASE WHEN i % 5 = 0 THEN NULL ELSE unhex(printf('%04x%04x', i, i * 7)) END,
    CASE WHEN i % 7 = 0 THEN NULL ELSE printf('note %d: ', i) || substr(replace(hex(zeroblob(120)), '00', 'np'), 1, i % 160) END
FROM seq;
CREATE TABLE plain (label TEXT, amount INTEGER, ratio REAL);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 40)
INSERT INTO plain SELECT printf('label-%02d', i), i * 9007, i / 8.0 + 0.0625 FROM seq;
CREATE TABLE documents (id INTEGER PRIMARY KEY, title TEXT, body TEXT);
INSERT INTO documents VALUES
    (1, 'short one', 'a short body'),
    (2, 'long two', 'long two: ' || replace(hex(zeroblob(3000)), '00', 'tw')),
    (3, 'short three', 'another short body'),
    (4, 'long four', 'long four: ' || replace(hex(zeroblob(2600)), '00', 'fo')),
    (5, 'short five', 'the last short body');
CREATE TABLE scratch (k TEXT, v BLOB, n INTEGER);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 300)
INSERT INTO scratch SELECT printf('scratch key %04d ', i) || substr('abcdefghijklmnopqrstuvwxyz', 1 + i % 26), unhex(printf('%08x', i * 2654435)), i * i FROM seq;
SQL
}

# The deletions, each "TABLE|KIND|CONDITION|SQL".
people_deletions() {
    cat <<'LIST'
people|deleted|id = 7|DELETE FROM people WHERE id = 7;
people|deleted|id = 23|DELETE FROM people WHERE id = 23;
people|deleted|id = 150|DELETE FROM people WHERE id = 150;
people|deleted|id BETWEEN 60 AND 75|DELETE FROM people WHERE id BETWEEN 60 AND 75;
plain|deleted|rowid IN (5, 17)|DELETE FROM plain WHERE rowid IN (5, 17);
plain|deleted|rowid = 40|DELETE FROM plain WHERE rowid = 40;
documents|deleted|id = 2|DELETE FROM documents WHERE id = 2;
sqlite_schema|deleted|tbl_name = ''scratch''|SELECT 1;
scratch|dropped|1|DROP TABLE scratch;
LIST
}

# make_people FIXTURE SECURE_DELETE: the people fixture, deleting with
# secure_delete set as given, and its oracle.
make_people() {
    fixture_start "$1"
    rm -f scratch.db
    people_setup | sqlite3 scratch.db > /dev/null
    step=0
    people_deletions | while IFS='|' read -r table kind condition sql; do
        step=$((step + 1))
        rows_oracle scratch.db "$oracle_root/$1/$step-$table.json" "$table" "$kind" "$condition"
        printf 'PRAGMA secure_delete = %s;\n%s\n' "$2" "$sql" | sqlite3 scratch.db > /dev/null
    done
    {
        echo "PRAGMA secure_delete = $2;"
        people_setup
        people_deletions | cut -d'|' -f4
    } | sqlite3 "$here/$1" > /dev/null
}

make_people deleted.db OFF
make_people secure.db ON

# --- chromium.db -----------------------------------------------------------
# A browser history in the shape of Chromium's History file (urls, visits,
# keyword_search_terms, their indexes, AUTOINCREMENT), with a time range of
# history cleared: its visits, then the urls left without visits, and their
# search terms.
chromium_setup() {
    cat <<'SQL'
CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
INSERT INTO meta VALUES ('version', '70'), ('last_compatible_version', '16');
CREATE TABLE urls(id INTEGER PRIMARY KEY AUTOINCREMENT,url LONGVARCHAR,title LONGVARCHAR,visit_count INTEGER DEFAULT 0 NOT NULL,typed_count INTEGER DEFAULT 0 NOT NULL,last_visit_time INTEGER NOT NULL,hidden INTEGER DEFAULT 0 NOT NULL);
CREATE TABLE visits(id INTEGER PRIMARY KEY AUTOINCREMENT,url INTEGER NOT NULL,visit_time INTEGER NOT NULL,from_visit INTEGER,external_referrer_url TEXT,transition INTEGER DEFAULT 0 NOT NULL,segment_id INTEGER,visit_duration INTEGER DEFAULT 0 NOT NULL,incremented_omnibox_typed_score BOOLEAN DEFAULT FALSE NOT NULL,opener_visit INTEGER,originator_cache_guid TEXT,originator_visit_id INTEGER,originator_from_visit INTEGER,originator_opener_visit INTEGER,is_known_to_sync BOOLEAN DEFAULT FALSE NOT NULL,consider_for_ntp_most_visited BOOLEAN DEFAULT FALSE NOT NULL,visited_link_id INTEGER DEFAULT 0 NOT NULL,app_id TEXT);
CREATE TABLE keyword_search_terms (keyword_id INTEGER NOT NULL,url_id INTEGER NOT NULL,term LONGVARCHAR NOT NULL,normalized_term LONGVARCHAR NOT NULL);
CREATE INDEX urls_url_index ON urls (url);
CREATE INDEX visits_url_index ON visits (url);
CREATE INDEX visits_time_index ON visits (visit_time);
CREATE INDEX keyword_search_terms_index1 ON keyword_search_terms (keyword_id, normalized_term);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 150)
INSERT INTO urls (url, title, visit_count, typed_count, last_visit_time, hidden) SELECT
    printf('https://site%02d.example.%s/path/%d?q=%s', i % 37, CASE i % 3 WHEN 0 THEN 'com' WHEN 1 THEN 'org' ELSE 'net' END, i, substr('alphabravocharliedeltaechofoxtrotgolfhotel', 1 + i % 20, 3 + i % 17)),
    CASE WHEN i % 9 = 0 THEN '' ELSE printf('Example page %d - Site %02d', i, i % 37) END,
    1 + i % 4, i % 2, 13370000000000000 + i * 3600000000, 0
FROM seq;
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 450)
INSERT INTO visits (url, visit_time, from_visit, external_referrer_url, transition, segment_id, visit_duration, opener_visit, originator_cache_guid, is_known_to_sync, visited_link_id)
SELECT 1 + (i - 1) / 3, 13370000000000000 + i * 1000000000, CASE WHEN i % 4 = 0 THEN i - 1 ELSE 0 END, '',
    CASE i % 3 WHEN 0 THEN 805306368 WHEN 1 THEN 268435457 ELSE 838860801 END, i % 23, i * 1500000, 0, '', 0, 1 + (i - 1) / 3
FROM seq;
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 150)
INSERT INTO keyword_search_terms SELECT 2, i, printf('Search Term %d', i), printf('search term %d', i) FROM seq WHERE i % 5 = 0;
SQL
}

# Clearing the history of one time range.
chromium_deletions() {
    cat <<'LIST'
visits|deleted|visit_time BETWEEN 13370100000000000 AND 13370160000000000|DELETE FROM visits WHERE visit_time BETWEEN 13370100000000000 AND 13370160000000000;
keyword_search_terms|deleted|url_id NOT IN (SELECT url FROM visits)|DELETE FROM keyword_search_terms WHERE url_id NOT IN (SELECT url FROM visits);
urls|deleted|id NOT IN (SELECT url FROM visits)|DELETE FROM urls WHERE id NOT IN (SELECT url FROM visits);
LIST
}

fixture_start chromium.db
rm -f scratch.db
chromium_setup | sqlite3 scratch.db > /dev/null
step=0
chromium_deletions | while IFS='|' read -r table kind condition sql; do
    step=$((step + 1))
    rows_oracle scratch.db "$oracle_root/chromium.db/$step-$table.json" "$table" "$kind" "$condition"
    printf 'PRAGMA secure_delete = OFF;\n%s\n' "$sql" | sqlite3 scratch.db > /dev/null
done
{
    echo "PRAGMA secure_delete = OFF;"
    chromium_setup
    chromium_deletions | cut -d'|' -f4
} | sqlite3 "$here/chromium.db" > /dev/null

# --- churn.db --------------------------------------------------------------
# Six rounds of inserts and deletions on 1024-byte pages, each round's
# deletions scattered by a hash of the id, so later inserts reuse space
# freed earlier: some deleted records are overwritten, the rest are where
# deletions and reuse happened to leave them. A rowid table with an alias
# and one without.
churn_setup() {
    echo "PRAGMA page_size = 1024;"
    echo "CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, detail TEXT, n INTEGER, r REAL);"
    echo "CREATE TABLE tags (name TEXT, weight INTEGER, seen REAL);"
}
# churn_round ROUND: the inserts of one round.
churn_round() {
    cat <<SQL
WITH RECURSIVE seq(i) AS (SELECT $1 * 100 + 1 UNION ALL SELECT i + 1 FROM seq WHERE i < $1 * 100 + 100)
INSERT INTO events SELECT i, printf('kind-%d', i % 9), printf('detail %d ', i) || substr(replace(hex(zeroblob(90)), '00', 'dx'), 1, (i * 37) % 170), (i * 7919) % 1000003 - 500000, i / 7.0 + 0.5 FROM seq;
WITH RECURSIVE seq(i) AS (SELECT $1 * 20 + 1 UNION ALL SELECT i + 1 FROM seq WHERE i < $1 * 20 + 20)
INSERT INTO tags SELECT printf('tag-%04d', i), i * 3, i / 4.0 + 0.125 FROM seq;
SQL
}
churn_condition_events() { echo "(id * 7919) % 5 = $1 % 5"; }
churn_condition_tags() { echo "(rowid * 31) % 4 = $1 % 4"; }

fixture_start churn.db
rm -f scratch.db
churn_setup | sqlite3 scratch.db > /dev/null
for round in 1 2 3 4 5 6; do
    churn_round "$round" | sqlite3 scratch.db > /dev/null
    rows_oracle scratch.db "$oracle_root/churn.db/$round-events.json" events deleted "$(churn_condition_events "$round")"
    rows_oracle scratch.db "$oracle_root/churn.db/$round-tags.json" tags deleted "$(churn_condition_tags "$round")"
    printf 'PRAGMA secure_delete = OFF;\nDELETE FROM events WHERE %s;\nDELETE FROM tags WHERE %s;\n' \
        "$(churn_condition_events "$round")" "$(churn_condition_tags "$round")" | sqlite3 scratch.db > /dev/null
done
{
    echo "PRAGMA secure_delete = OFF;"
    churn_setup
    for round in 1 2 3 4 5 6; do
        churn_round "$round"
        echo "DELETE FROM events WHERE $(churn_condition_events "$round");"
        echo "DELETE FROM tags WHERE $(churn_condition_tags "$round");"
    done
} | sqlite3 "$here/churn.db" > /dev/null

# --- recovery-wal.db and its log --------------------------------------------
# Saved logins in a write-ahead log, copied while a writer is mid-
# transaction. Round one is checkpointed into the database file. Round two
# commits more rows into the log; round three deletes rows of both rounds,
# so the log's frames from round two, superseded, still hold them as they
# were, and the file still holds round one's. Round four never commits: its
# spilled frames hold rows that never existed in the database.
wal_setup() {
    cat <<'SQL'
CREATE TABLE logins (id INTEGER PRIMARY KEY, origin_url TEXT NOT NULL, username_value TEXT, password_value BLOB, date_created INTEGER NOT NULL, times_used INTEGER);
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 30)
INSERT INTO logins SELECT i, printf('https://login%02d.example.org/', i), printf('user%02d@example.net', i), unhex(printf('%016x', i * 1311768467)), 13370000000000000 + i * 86400000000, i % 6 FROM seq;
SQL
}
wal_round_two() {
    cat <<'SQL'
WITH RECURSIVE seq(i) AS (SELECT 31 UNION ALL SELECT i + 1 FROM seq WHERE i < 45)
INSERT INTO logins SELECT i, printf('https://login%02d.example.org/', i), printf('user%02d@example.net', i), unhex(printf('%016x', i * 1311768467)), 13370000000000000 + i * 86400000000, i % 6 FROM seq;
SQL
}
wal_round_four() {
    cat <<'SQL'
WITH RECURSIVE seq(i) AS (SELECT 101 UNION ALL SELECT i + 1 FROM seq WHERE i < 160)
INSERT INTO logins SELECT i, printf('https://never%03d.example.org/', i) || replace(hex(zeroblob(30)), '00', 'n'), printf('ghost%03d@example.net', i), unhex(printf('%016x', i * 1311768467)), 13370000000000000 + i * 86400000000, 0 FROM seq;
SQL
}

fixture_start recovery-wal.db
rm -f scratch.db
{ wal_setup; wal_round_two; } | sqlite3 scratch.db > /dev/null
rows_oracle scratch.db "$oracle_root/recovery-wal.db/1-logins.json" logins deleted "id IN (4, 5, 6, 20, 33, 34, 40)"
sqlite3 scratch.db "DELETE FROM logins WHERE id IN (4, 5, 6, 20, 33, 34, 40);"
wal_round_four | sqlite3 scratch.db > /dev/null
rows_oracle scratch.db "$oracle_root/recovery-wal.db/2-logins.json" logins uncommitted "id > 100"

rm -f live.db live.db-wal
{
    echo "PRAGMA journal_mode = WAL;"
    echo "PRAGMA wal_autocheckpoint = 0;"
    echo "PRAGMA secure_delete = OFF;"
    wal_setup
    echo "PRAGMA wal_checkpoint(PASSIVE);"
    wal_round_two
    echo "DELETE FROM logins WHERE id IN (4, 5, 6, 20, 33, 34, 40);"
    echo "PRAGMA cache_size = 1;"
    echo "PRAGMA cache_spill = 1;"
    echo "BEGIN;"
    wal_round_four
    echo ".shell cp live.db '$here/recovery-wal.db' && cp live.db-wal '$here/recovery-wal.db-wal'"
    echo "ROLLBACK;"
} | sqlite3 live.db > /dev/null
