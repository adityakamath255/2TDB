# time-travel-db-rs

An append-only, time-travelling key-value store on SQLite. Every write is an event appended to a log, and state is never mutated in place. A snapshot is a view of the log up to some point, answered by indexed queries rather than materialized maps, so any version of the store stays cheap to reach no matter how large the log grows.

It began as a Rust rewrite of a small Python original, written as a study in software design.

## Usage

```rust
use time_travel_db_rs::{in_memory, Value};

let mut db = in_memory().unwrap();
db.batch().set("votes", 1).commit().unwrap();
db.batch().set("votes", 2).commit().unwrap();

// the value right after the first event
assert_eq!(db.at(0).unwrap().get("votes").unwrap(), Some(Value::Int(1)));
// the current value
assert_eq!(db.latest().unwrap().get("votes").unwrap(), Some(Value::Int(2)));
```

A write is a batch of changes that commit together as a single, atomic event:

```rust
db.batch()
    .set("name", "ada")
    .set("age", 36)
    .delete("retired")
    .commit()
    .unwrap();
```

## Modes

A store is opened in one of three modes, and the mode is part of its type, so the compiler enforces what each can do:

- `in_memory()` is a writable store in a private in-memory database.
- `connect(path)` is a writable, durable store. Every commit is a SQLite transaction, so it is on disk before `commit` returns and a crash can never leave a partial event. Concurrent connections are safe: SQLite serializes writers and isolates readers.
- `inspect(path)` opens the database read-only. Code that tries to write to it does not compile.

## Time travel

- `at(seq)` is the state immediately after a given event.
- `as_of(time)` is the state as it stood at a wall-clock time.
- `latest()` is the current state.
- `diff(a, b)` reports the keys that differ between two snapshots, as `(key, before, after)`.
- `history(key)` traces one key across the whole log.
- `event(seq)` is a single event as committed: its changes and timestamp.

A snapshot answers `get` with one indexed lookup: the last change to that key at or before the snapshot's event. Nothing is replayed and no state is copied, so a snapshot of event 3 costs the same after a million more events.

## Schema

One table holds the whole log, one row per change:

```sql
CREATE TABLE changes (
    seq   INTEGER NOT NULL,  -- position of the event in the log
    key   TEXT NOT NULL,
    kind  INTEGER,           -- tags the value's type: bool, int, float, str
    value ANY,               -- stored natively; NULL means deleted
    ts    INTEGER NOT NULL,  -- nanoseconds since the epoch
    PRIMARY KEY (key, seq)
) STRICT, WITHOUT ROWID;
CREATE INDEX changes_by_seq ON changes (seq, key);
```

Everything else is a view of it. `firsts` lists each distinct key and the event that introduced it, computed by a recursive CTE that skip-scans the `(key, seq)` index, so it costs one seek per distinct key rather than a scan of the log. Snapshot listing and `diff` start from `firsts` to know which keys can exist at a given event, then seek each key's last change with the same index. `latest` is the current state as plain SQL: `SELECT * FROM latest` works from any SQLite client, no library needed.

`kind` exists because SQLite has no boolean storage class: it is what keeps `true` and `1` distinct on the way back out. A value can never itself be null; absence is a missing key, which frees SQL `NULL` in `value` to mean "deleted". Timestamps never decrease along the log and every event holds at least one change, so seqs are dense and time is sorted by seq: `as_of` binary-searches for its event with a recursive CTE, in O(log n) seeks on the seq index.

## License

MIT
