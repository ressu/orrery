# Datastore spike: redb or SQLite

**Recommendation: use redb for orrery's own store, and import from Moonraker's database with a
small read-only SQLite reader written in Rust.** That keeps orrery free of C code, so the build
needs no zig, and the import can run on the printer itself.

The spike's code is in `spikes/datastore/`. It runs the same Moonraker-shaped workloads on both
engines.

## What was compared

- **redb 4.4:** a pure-Rust embedded key-value store with ACID transactions.
- **SQLite 3, through rusqlite 0.41 with its bundled C source:** the engine Moonraker uses. The
  spike sets it up the way Moonraker does: rollback journal and `synchronous=FULL`.

The workloads follow what Moonraker does with its database:

- Frontend settings: 500 writes, each in its own transaction, as Fluidd saves one setting at a time.
- 5,000 setting reads.
- History: each job saved twice, when it starts and when it finishes, each in its own
  transaction.
- `server.history.list`: the newest 50 jobs, and a page of 50 between two times.
- Reopening the database and running the first query, to show startup cost.

## Results

On this machine (Intel i5-12400F, ext4 on NVMe), with 2,000 history jobs:

| Workload | redb | SQLite |
|---|---|---|
| Setting write, committed | 432 µs | 1,812 µs |
| Setting read | 2 µs | 5 µs |
| History save, committed | 479 µs | 1,920 µs |
| History list, newest 50 | 209 µs | 179 µs |
| History list, between two times | 193 µs | 228 µs |
| Reopen and first list | 0.7 ms | 0.5 ms |
| File size | 1.4 MiB | 1.3 MiB |

With 20,000 jobs, redb's history lists stay at about 0.2–0.3 ms. SQLite's grow to about 2.6–2.9
ms, because Moonraker's query counts the matching rows by scanning them. An index would fix that
in either engine.

The aarch64 binary also ran both engines under qemu. Times under qemu don't predict a Pi, so
they're left out. **The next step is to run the spike on a real printer**, because SD-card
`fsync` cost may change how far apart the engines are on writes. See
[Running the spike](#running-the-spike).

redb needs indexes kept by hand. Its first version filtered history by time by reading every job,
which took 6.7 ms. A `(start time, job id)` index table brought that down to 0.2 ms. SQL would
have given this with `CREATE INDEX`. For orrery's handful of queries, a few hand-kept indexes are
a small cost.

## Build

| aarch64 musl release binary | Size | Needs |
|---|---|---|
| No engine (baseline) | 0.56 MB | `rust-lld` |
| redb | 1.54 MB (+0.98 MB) | `rust-lld` |
| SQLite | 2.29 MB (+1.73 MB) | a C cross compiler: zig 0.17 and cargo-zigbuild 0.23 from nixpkgs |
| Both | 3.22 MB | the same |

Without a C cross compiler, the SQLite build fails at link time: `rust-lld` gets an x86 object
for `sqlite3.o`. With zig it works and takes about 40 seconds. Using SQLite would bring zig into
every build: the dev shell, CI, and `scripts/copy-to-printer.sh`.

## Importing Moonraker's database without C

Moonraker's `moonraker-sql.db` uses SQLite's default rollback journal, not WAL. A copy made while
Moonraker is stopped holds all its data in one file.

`spikes/datastore/src/sqlite_read.rs` reads that file in about 300 lines of Rust, with no
dependencies. It handles:

- the page layout of tables, including tables spread over several pages and values that spill
  onto overflow pages;
- records, and `INTEGER PRIMARY KEY` columns, which hold the row id;
- the type-tagged values in `namespace_store`.

It refuses WAL files and files with a leftover journal next to them, and it skips indexes.

To check the reader, upstream Moonraker built a database in the conformance harness. It held
settings of every value type, a 20 KB value, 300 more settings, two history jobs with totals, and
a user. The reader's output was identical to Python's `sqlite3` for all 333 rows:

```
spikes/datastore/target/release/datastore-spike --read-sqlite <copy of moonraker-sql.db> > rs.jsonl
python3 -I spikes/datastore/dump_sqlite.py <copy of moonraker-sql.db> > py.jsonl
```

The real `orrery import` would use this reader, so the import can run on the printer.

## Risks of redb

- **Format changes.** redb has changed its file format between major versions before. orrery
  should pin redb's major version and treat an upgrade as a migration with its own test.
- **No SQL to look inside.** You can't open the store with the `sqlite3` command-line tool. orrery
  should have a `dump` command that prints the store as JSON, for debugging.
- **Hand-kept indexes.** Each query that filters needs its index updated in the same transaction
  as the data. Keep the store behind one module with tests for each index.

## Running the spike

To run the spike on a printer, build it and copy it over:

```
cargo build --release --target aarch64-unknown-linux-musl --no-default-features --features redb --manifest-path spikes/datastore/Cargo.toml
scp spikes/datastore/target/aarch64-unknown-linux-musl/release/datastore-spike <ssh host>:
ssh <ssh host> './datastore-spike ~ --jobs 2000 && rm datastore-spike'
```

The databases go in a temp folder in your home folder on the printer and are removed afterwards.
Point it at the SD card, not `/tmp`, which may be in memory. That build measures redb only.
Building with SQLite needs zig, as described in [Build](#build).
