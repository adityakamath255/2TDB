# time-travel-db-rs

An append-only, bitemporal key-value store on SQLite. Every write is an event appended to a log, and state is never mutated in place. A read happens at a coordinate on two time axes: transaction time (what the store knew, and when) and valid time (what was true in the world, and when). Snapshots are coordinates, not copies, answered by indexed queries rather than materialized maps, so any point in either time stays cheap to reach no matter how large the log grows.

It began as a Rust rewrite of a small Python original, written as a study in software design.

## Usage

```rust
use time_travel_db_rs::{in_memory, Value};

let mut db = in_memory().unwrap();
db.batch().set("votes", 1).commit().unwrap();
db.batch().set("votes", 2).commit().unwrap();

// the state right after the first event
assert_eq!(db.at(0).unwrap().get("votes").unwrap(), Some(Value::Int(1)));
// the current state
assert_eq!(db.latest().unwrap().get("votes").unwrap(), Some(Value::Int(2)));
```

A write is a batch of assertions that commit together as one atomic event. Each assertion says: this key holds this value from this valid time onward. `set` and `delete` default the valid time to the commit time; `set_from` and `delete_from` choose it, in the past to correct the record or in the future to schedule a change:

```rust
db.batch()
    .set("name", "ada")                     // true from now
    .set_from("employer", "acme", march)    // true since March, learnt today
    .delete_from("discount", next_month)    // scheduled to end
    .commit()
    .unwrap();
```

## Two time axes

Transaction time is when the store learnt something. It is store-assigned at commit and never caller-controlled, which is what makes the log an honest audit record. Valid time is when something holds in the modeled world, and it is entirely the caller's: any assertion may place its valid time anywhere.

A `Snapshot` is one coordinate on both axes. Constructors pick the transaction time and default the valid time to match it, so each reads as "the world as it stood, as it was then known":

- `at(seq)` is the store right after a given event.
- `known_at(t)` is the store as it was known at a wall-clock time.
- `latest()` is everything known, valid as of now.
- `valid_at(v)` re-views the same knowledge at another valid time; `valid_unbounded()` lifts the bound so scheduled future changes show.

So `db.latest().valid_at(last_year)` is what we now believe was true last year, and `db.at(3).valid_at(last_year)` is what we believed about last year back then.

The value at a coordinate is the assertion with the lexicographically greatest `(valid, seq)` among those visible there: one indexed lookup, nothing replayed. A consequence worth internalizing: corrections splice into the timeline rather than overriding everything after them. If the record says `a` since January and `b` since June, a later correction "actually `c` since March" changes March through May and leaves June onward with `b`, because at any valid time the latest valid-from at or before it wins. Re-asserting the same key at the same valid time supersedes that point outright.

Scheduled changes fall out of the same rule: an assertion with a future valid time is invisible to `latest()` until the clock reaches it. `diff(a, b)` reports the keys that differ between any two coordinates, which covers both "what changed in the world" and "what did we learn" depending on which axis the coordinates vary along. `history(key)` lists every assertion ever made about a key; `event(seq)` shows one event as committed. `changepoints()` enumerates the valid axis of a snapshot: the distinct valid times at which its knowledge changes, so between two adjacent ones every read answers identically. It describes the knowledge state, not the view position, so scheduled changes are included.

`when(pred)` bisects the log for the first event after which a predicate on the state holds: log2(n) probes instead of a replay, under the git-bisect contract that the predicate flips once from false to true. Each probe sees `at(seq)`, valid time tracking the event; pinning it inside the predicate asks instead when a fixed moment was first believed to satisfy it:

```rust
// when did votes first reach 100?
db.when(|s| Ok(matches!(s.get("votes")?, Some(Value::Int(n)) if n >= 100)))?;
// when did we first believe anyone was employed in March?
db.when(|s| Ok(s.valid_at(march).get("employer")?.is_some()))?;
```

## Modes

A store is opened in one of three modes, and the mode is part of its type, so the compiler enforces what each can do:

- `in_memory()` is a writable store in a private in-memory database.
- `connect(path)` is a writable, durable store. Every commit is a SQLite transaction, so it is on disk before `commit` returns and a crash can never leave a partial event. Concurrent connections are safe: SQLite serializes writers and isolates readers.
- `inspect(path)` opens the database read-only. Code that tries to write to it does not compile.

`cargo run --example scrub -- path.db` opens a TUI scrubber over both axes: `h`/`l` walks the log, `[`/`]` hops between valid-time changepoints, `:`/`@` jump to a typed event number or date on either axis, `n`/`N` walk the selected key's own events, `/` filters keys, and `m` marks a baseline that every later position is diff-colored against.

## Schema

Two tables hold everything. An event is a moment of learning; a change is one assertion made at that moment:

```sql
CREATE TABLE events (
    seq INTEGER PRIMARY KEY,   -- position in the log
    ts  INTEGER NOT NULL       -- transaction time
) STRICT;

CREATE TABLE changes (
    key   TEXT NOT NULL,
    valid INTEGER NOT NULL,    -- valid time of this assertion
    seq   INTEGER NOT NULL REFERENCES events (seq),
    kind  INTEGER,             -- tags the value's type: bool, int, float, str
    value ANY,                 -- stored natively; NULL means deleted
    PRIMARY KEY (key, valid, seq)
) STRICT, WITHOUT ROWID;
```

Timestamps are i64 microseconds since the epoch; finer input is truncated. A CHECK constraint ties `kind` to the stored type of `value`, so a mismatched row is unrepresentable for any writer, not just this library. Transaction timestamps are wall-clock readings under serialized writers: ordinarily nondecreasing, but a clock step can produce disorder and two events can share a microsecond, so `known_at` anchors on the latest timestamp at or before the target and breaks ties by `seq`. The `changes` table is the complete history; `SELECT * FROM changes WHERE key = ?` ordered however you like is the raw material of every other question.

The views make the store fully usable from any SQLite client, no library needed:

- `latest` is the current state: `SELECT * FROM latest`.
- `timeline` is each key's currently-believed history as half-open valid-time intervals; NULL `valid_to` is open-ended, NULL `kind` marks an interval where the key is absent.
- `scheduled` lists pending future changes, scheduled deletes included.
- `corrections` lists assertions that rewrote the record, flagged `backdated` (valid time before its own commit) and `supersedes` (re-asserting an already-asserted valid time). Note that any write recording something that happened earlier counts as backdated; that is the honest definition, not an alarm.
- `assertions` is the log made readable: type names instead of kind tags, ISO-8601 timestamps.
- `keys` enumerates distinct keys with one index seek each.

`latest` and `scheduled` read the live clock inside SQLite, so their answers move as time passes; the Rust `latest()` freezes its clock when the snapshot is taken, so a snapshot is repeatable. Each is the right behavior for its consumer.

## Recipes

The timeline as it was known at transaction time T, for any SQLite client (substitute T, in applied-event units, i.e. seq + 1):

```sql
SELECT c.key, c.valid AS valid_from,
       lead(c.valid) OVER (PARTITION BY c.key ORDER BY c.valid) AS valid_to,
       c.kind, c.value
FROM changes c
WHERE c.seq < :T
  AND NOT EXISTS (SELECT 1 FROM changes k
                   WHERE k.key = c.key AND k.valid = c.valid
                     AND k.seq > c.seq AND k.seq < :T);
```

A full bitemporal decomposition also exists: every assertion's region of authority in the (transaction, valid) plane, as half-open rectangles, derived entirely from the log. `tx` bounds are in applied-event units, NULL bounds are open ends. It is deliberately not installed as a view: enumerating it costs quadratic time on keys with long plain-append histories, which measurement showed makes it wrong as a default read path. For offline analysis on modest data:

```sql
WITH bounds AS (
    SELECT a.key, a.seq AS aseq, a.valid AS avalid, a.kind, a.value,
           a.seq AS bseq,
           (SELECT d.valid FROM changes d
             WHERE d.key = a.key AND d.valid > a.valid AND d.seq <= a.seq
             ORDER BY d.valid LIMIT 1) AS vto
    FROM changes a
    UNION ALL
    SELECT a.key, a.seq, a.valid, a.kind, a.value, d.seq, d.valid
    FROM changes d
    JOIN changes a
      ON a.key = d.key AND a.seq < d.seq
     AND a.valid = (SELECT e.valid FROM changes e
                     WHERE e.key = d.key AND e.valid < d.valid
                       AND e.seq <= d.seq
                     ORDER BY e.valid DESC LIMIT 1)
    WHERE NOT EXISTS (SELECT 1 FROM changes p
                       WHERE p.key = d.key AND p.valid = d.valid
                         AND p.seq < d.seq)
),
epochs AS (
    SELECT b.*,
           lead(bseq) OVER (PARTITION BY key, aseq, avalid
                            ORDER BY bseq) AS nseq,
           (SELECT k.seq FROM changes k
             WHERE k.key = b.key AND k.valid = b.avalid AND k.seq > b.aseq
             ORDER BY k.seq LIMIT 1) AS kseq
    FROM bounds b
)
SELECT key, kind, value,
       bseq + 1 AS tx_from,
       CASE WHEN kseq IS NOT NULL AND (nseq IS NULL OR kseq < nseq)
            THEN kseq + 1 ELSE nseq + 1 END AS tx_to,
       avalid AS valid_from,
       vto AS valid_to
FROM epochs
WHERE kseq IS NULL OR bseq < kseq;
```

## License

MIT
