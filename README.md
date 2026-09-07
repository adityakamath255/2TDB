# 2TDB

2TDB is a bitemporal key-value store written in Rust and backed by SQLite. Each commit adds an event to the log. Reads select a transaction time and a valid time, which supports historical snapshots, corrections to past facts, scheduled changes, diffs, and audit queries.

The project began as a Rust rewrite of a smaller Python implementation.

## Usage

```rust
use two_tdb::{Database, Value, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::memory()?;

    let first = db.commit([Write::set("votes", 1)])?;
    db.commit([Write::set("votes", 2)])?;

    assert_eq!(db.at(first)?.get("votes")?, Some(Value::Int(1)));
    assert_eq!(db.latest()?.get("votes")?, Some(Value::Int(2)));

    Ok(())
}
```

`Database::memory()` creates an in-memory store. `Database::open(path)` creates or opens a file-backed store. A commit accepts an array, vector, or iterator of `Write` values and records its assertions as one event in one SQLite transaction.

```rust
db.commit([
    Write::set("name", "Ada"),
    Write::set("active", true),
    Write::delete("pending"),
])?;

let writes = ["alice", "bob"].into_iter().map(|name| {
    Write::set(format!("users/{name}/active"), true)
});
db.commit(writes)?;
```

The input is collected before acquiring the database's write lock. Empty input returns `Error::EmptyCommit` without starting a transaction or creating an event. For conditional writes, assemble a vector and skip the commit if it is empty.

`Write::set` and `Write::delete` use the commit timestamp as their valid time. `Write::set_at` and `Write::delete_at` accept a caller-supplied valid time for corrections and scheduled changes.

Within one commit, the last supplied write wins for each key and valid timestamp, after truncating timestamps to microseconds. Writes for the same key at different valid times are retained.

## Time model

The two axes answer different questions:

- Transaction time records when the database learnt an assertion. The store assigns an event ID and commit timestamp.
- Valid time records when the assertion holds in the modeled domain. The caller may place it in the past or future.

A `Snapshot` contains one cutoff on each axis:

- `at(id)` includes events through `id` and uses that event's commit timestamp as the valid-time cutoff.
- `known_at(time)` includes the events known by `time` and uses `time` as the valid-time cutoff.
- `latest()` includes the latest event and uses the current time as the valid-time cutoff.
- `valid_at(time)` changes only the valid-time cutoff of an existing snapshot.
- `valid_unbounded()` includes valid times beyond the current clock.

For a key at transaction cutoff `T` and valid-time cutoff `V`, `get` selects the assertion with the greatest `(valid, event ID)` where `valid <= V` and `event ID <= T`. The query uses the primary-key index; it does not replay the event log.

This rule preserves later valid-time assertions when a correction is recorded. If a key is `a` from January and `b` from June, a later assertion of `c` from March makes the value `c` from March through May. The June assertion still wins from June onward.

## API

The public API separates writes, snapshots, and log inspection:

- `commit` groups a collection of writes into one atomic event and returns its event ID.
- `Database` can commit and read. `Reader::inspect(path)` opens the same store read-only; its type has no `commit` method.
- `Snapshot::get` reads one key. `state` returns all values at the coordinate, and `diff` compares two coordinates.
- `blame` returns the assertion selected for a key, including the deleting assertion when a key is absent. It returns `None` when the key was never asserted.
- `changepoints` lists the valid times known at the snapshot's transaction cutoff.
- `event`, `history`, and `keys` inspect the append-only log.
- `bisect` finds the first event at which a monotonic predicate becomes true.

Values may be booleans, signed 64-bit integers, 64-bit floats, or strings. A deletion is stored as an assertion without a value.

## Storage

The library writes two tables, both defined in [`src/schema.sql`](src/schema.sql):

- `events` stores the event ID and transaction timestamp.
- `changes` stores the key, valid timestamp, event ID, value kind, and value. Its primary key is `(key, valid, event ID)`.

The library only inserts into these tables. File-backed writable connections use WAL mode, and each batch commits with an immediate SQLite transaction. The schema uses `STRICT` tables and a `CHECK` constraint to keep each value's kind consistent with its SQLite storage type.

The schema also installs `latest`, `timeline`, `scheduled`, `corrections`, `assertions`, and `keys` views for direct inspection with SQLite tools. The `latest` and `scheduled` views read the clock for each query. A Rust snapshot returned by `latest()` retains the cutoff chosen when the snapshot was created.

Timestamps are stored as signed 64-bit microseconds since the Unix epoch. Input with finer precision is truncated.

## TUI scrubber

The `scrub` example opens a database through the read-only `Reader` API:

```bash
cargo run --example scrub -- path.db
```

It can move independently through event and valid time, inspect a key's history, filter keys, and compare the current position with a marked snapshot. The footer lists the active key bindings.

## Tests

The project uses the Rust 2024 edition and bundles SQLite through `rusqlite`. Run the test suite with:

```bash
cargo test
```

The integration tests cover transaction and valid-time reads, corrections, scheduled changes, value encoding, diffs, blame, changepoints, read-only connections, concurrent connections, schema views, and persistence.

## Limitations

- The value model has no byte strings, collections, or application-defined types.
- Timestamp precision is limited to microseconds.
- Transaction timestamps come from the system clock and may tie or move backward. Event IDs define commit order; `known_at` breaks equal timestamps by event ID.
- `bisect` requires a predicate that changes at most once from false to true.
- Append-only behavior is enforced by the Rust API. A SQLite client with write access can modify the base tables directly.

## License

MIT
