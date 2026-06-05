# time-travel-db-rs

An append-only, time-travelling key-value store. Every write is an event appended to a log, and state is never mutated in place; it is folded from the events. Because each version is a persistent map that shares structure with the one before it, the store keeps every version it has ever held and can answer questions about any of them cheaply.

It is a Rust rewrite of a small Python original, written as a study in software design.

## Usage

```rust
use time_travel_db_rs::{in_memory, Value};

let mut db = in_memory();
db.batch().set("votes", 1).commit().unwrap();
db.batch().set("votes", 2).commit().unwrap();

// the value right after the first event
assert_eq!(db.at(0).unwrap().get("votes"), Some(&Value::Int(1)));
// the current value
assert_eq!(db.latest().get("votes"), Some(&Value::Int(2)));
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

- `in_memory()` is a writable store that keeps nothing on disk.
- `connect(path)` is a writable, durable store backed by an append-only log. It takes an exclusive lock, replays the log, and repairs a torn tail left behind by a crash. Every commit is written and fsynced before it returns.
- `inspect(path)` is a read-only view of a log on disk. It does not lock or modify the file, and code that tries to write to it does not compile.

## Time travel

- `at(seq)` is the state immediately after a given event.
- `as_of(time)` is the state as it stood at a wall-clock time.
- `latest()` is the current state.
- `diff(a, b)` reports the keys that differ between two states, as `(key, before, after)`.
- `history(events, key)` traces one key across a run of events.

## On-disk format

The log is JSON, one event per line:

```json
{"changes":{"age":36,"name":"ada"},"ts":"2026-06-05T05:57:56.409Z"}
{"changes":{"age":null},"ts":"2026-06-05T05:57:57.118Z"}
```

A value serializes bare (`36`, `"ada"`), and a deletion serializes as `null`. That is unambiguous because a stored value can never itself be null: absence is a missing key, not a null value.

## License

MIT
