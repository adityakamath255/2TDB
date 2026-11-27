import threading
from collections.abc import Callable, Iterable, Iterator, Sequence
from contextlib import contextmanager
from dataclasses import dataclass, replace
from datetime import datetime
from importlib.resources import files
from string.templatelib import Template

import psycopg

from two_tdb.model import (
    Added,
    Assertion,
    Changed,
    Delta,
    EmptyCommit,
    Event,
    InvalidInput,
    OutOfRange,
    RecordedAssertion,
    Removed,
    Value,
    Write,
    aware,
    check_source,
)

_KINDS = ("bool", "int", "float", "str")

type Embed = Callable[[Sequence[str]], Sequence[Sequence[float]]]

# every key once, ascending, with one index probe per key; SELECT DISTINCT
# reads every assertion and measured 4.5x slower at 1,000 keys of 200
# assertions each
_KEYS = t"""
    WITH RECURSIVE keys (key) AS (
        (SELECT key FROM changes ORDER BY key LIMIT 1)
        UNION ALL
        SELECT (
            SELECT c.key FROM changes c
            WHERE c.key > keys.key ORDER BY c.key LIMIT 1
        )
        FROM keys WHERE keys.key IS NOT NULL
    )
"""


def install(connection: psycopg.Connection) -> None:
    """Creates the schema and the two_tdb_app role.

    Run as the owner of an empty database.
    """
    connection.execute(files("two_tdb").joinpath("schema.sql").read_text())


def _encode(
    value: Value | None,
) -> tuple[str, bool | None, int | None, float | None, str | None]:
    match value:
        case None:
            return ("delete", None, None, None, None)
        case bool():
            return ("bool", value, None, None, None)
        case int():
            return ("int", None, value, None, None)
        case float():
            return ("float", None, None, value, None)
        case str():
            return ("str", None, None, None, value)


def _text(key: str, value: Value) -> str:
    return f"{key}: {value}"


def _vector(vector: Sequence[float]) -> str:
    return "[" + ",".join(map(str, vector)) + "]"


# expects columns: kind, bool_value, int_value, float_value, str_value
def _decode(columns: Sequence) -> Value | None:
    kind, *values = columns
    return None if kind == "delete" else values[_KINDS.index(kind)]


# expects columns: key, valid, seq, committed_at, source, then _decode's
def _recorded(row: tuple) -> RecordedAssertion:
    key, valid, seq, committed_at, source, *value = row
    return RecordedAssertion(
        seq, committed_at, source, Assertion(key, valid, _decode(value))
    )


class Reader:
    """Read access to one tenant's history.

    The connection must be in autocommit mode; each call runs in its own
    read-only transaction, and calls from several threads take turns. embed
    turns text into vectors for search, and must be the model that embedded
    the stored facts.
    """

    def __init__(
        self, connection: psycopg.Connection, tenant: int, embed: Embed
    ) -> None:
        if not connection.autocommit:
            raise ValueError("the connection must be in autocommit mode")
        self._connection = connection
        self.tenant = tenant
        self._embed = embed
        # a connection runs one transaction at a time
        self._lock = threading.RLock()

    @contextmanager
    def _transaction(
        self, *, read_only: bool = True
    ) -> Iterator[psycopg.Connection]:
        with self._lock, self._connection.transaction():
            if read_only:
                self._connection.execute("SET TRANSACTION READ ONLY")
            self._connection.execute(
                t"SELECT set_config('app.tenant', {str(self.tenant)}, true)"
            )
            yield self._connection

    @staticmethod
    def _length(db: psycopg.Connection) -> int:
        return db.execute(
            "SELECT coalesce(max(seq), 0) FROM events"
        ).fetchone()[0]

    def length(self) -> int:
        """Returns the number of events committed."""
        with self._transaction() as db:
            return self._length(db)

    def is_empty(self) -> bool:
        return self.length() == 0

    def at(self, event_id: int) -> "Snapshot":
        """Returns the store as it stood right after event_id was committed.

        Raises OutOfRange if there is no such event.
        """
        with self._transaction() as db:
            row = db.execute(
                t"SELECT committed_at FROM events WHERE seq = {event_id}"
            ).fetchone()
            if row is None:
                raise OutOfRange(event_id, self._length(db))
        return Snapshot(self, event_id, row[0])

    def known_at(self, time: datetime) -> "Snapshot":
        """Returns what the store held at wall-clock time, valid up to that
        time.
        """
        aware(time)
        with self._transaction() as db:
            (event_id,) = db.execute(
                t"SELECT max(seq) FROM events WHERE committed_at <= {time}"
            ).fetchone()
        return Snapshot(self, event_id, time)

    def latest(self) -> "Snapshot":
        """Returns the store after its last event, valid up to now."""
        with self._transaction() as db:
            event_id, now = db.execute("""
                SELECT max(seq),
                       greatest(clock_timestamp(), max(committed_at))
                FROM events
            """).fetchone()
        return Snapshot(self, event_id, now)

    def event(self, event_id: int) -> Event:
        """Returns an event and its assertions; raises OutOfRange if there is
        no such event.
        """
        with self._transaction() as db:
            row = db.execute(t"""
                SELECT committed_at, source FROM events WHERE seq = {event_id}
            """).fetchone()
            if row is None:
                raise OutOfRange(event_id, self._length(db))
            assertions = db.execute(t"""
                SELECT key, valid,
                       kind, bool_value, int_value, float_value, str_value
                FROM changes WHERE seq = {event_id}
                ORDER BY key, valid
            """).fetchall()
        committed_at, source = row
        return Event(
            event_id,
            committed_at,
            source,
            tuple(
                Assertion(key, valid, _decode(value))
                for key, valid, *value in assertions
            ),
        )

    def keys(self) -> list[str]:
        """Returns every key ever asserted, deleted ones included, in ascending
        order.
        """
        with self._transaction() as db:
            rows = db.execute(
                t"{_KEYS:q} SELECT key FROM keys WHERE key IS NOT NULL"
            ).fetchall()
        return [key for (key,) in rows]

    def history(self, key: str) -> list[RecordedAssertion]:
        """Returns every assertion recorded for key, in commit order."""
        with self._transaction() as db:
            rows = db.execute(t"""
                SELECT c.key, c.valid, c.seq, e.committed_at, e.source,
                       c.kind, c.bool_value, c.int_value, c.float_value,
                       c.str_value
                FROM changes c JOIN events e USING (tenant, seq)
                WHERE c.key = {key}
                ORDER BY c.seq, c.valid
            """).fetchall()
        return [_recorded(row) for row in rows]

    def bisect(self, predicate: Callable[["Snapshot"], bool]) -> int | None:
        """Returns the first event whose snapshot satisfies predicate, or None.

        Assumes predicate is false up to some event and true from it on.
        """
        first, last, found = 1, self.length(), None
        while first <= last:
            middle = (first + last) // 2
            if predicate(self.at(middle)):
                found, last = middle, middle - 1
            else:
                first = middle + 1
        return found


class Store(Reader):
    """Read and append access to one tenant's history."""

    def commit(self, writes: Iterable[Write], *, source: str) -> int:
        """Records writes as one event and returns its ID.

        Writes without a valid time take effect at the commit time. When
        several writes share a key and valid time, the last one wins. Raises
        EmptyCommit for no writes, before touching the database.
        """
        writes = list(writes)
        if not writes:
            raise EmptyCommit()
        check_source(source)
        vectors = iter(
            self._embed(
                [_text(w.key, w.value) for w in writes if w.value is not None]
            )
        )
        embedded = [
            (write, None if write.value is None else _vector(next(vectors)))
            for write in writes
        ]
        with self._transaction(read_only=False) as db:
            db.execute(t"SELECT pg_advisory_xact_lock({self.tenant}::bigint)")
            event_id, committed_at = db.execute(t"""
                INSERT INTO events (seq, committed_at, source)
                SELECT coalesce(max(seq), 0) + 1,
                       greatest(clock_timestamp(), max(committed_at)),
                       {source}
                FROM events
                RETURNING seq, committed_at
            """).fetchone()
            latest = {
                (
                    write.key,
                    committed_at
                    if write.valid_from is None
                    else write.valid_from,
                ): (write.value, vector)
                for write, vector in embedded
            }
            keys, valids, kinds, bools, ints, floats, strs, vectors = (
                list(column)
                for column in zip(
                    *(
                        (key, valid, *_encode(value), vector)
                        for (key, valid), (value, vector) in latest.items()
                    ),
                    strict=True,
                )
            )
            db.execute(t"""
                INSERT INTO changes
                    (seq, key, valid, kind, bool_value, int_value, float_value,
                     str_value, embedding)
                SELECT {event_id}, k, v, kind, b, i, f, s, e::vector
                FROM unnest(
                    {keys}::text[], {valids}::timestamptz[],
                    {kinds}::value_kind[], {bools}::boolean[],
                    {ints}::bigint[], {floats}::float8[], {strs}::text[],
                    {vectors}::text[]
                ) AS u (k, v, kind, b, i, f, s, e)
            """)
        return event_id


@dataclass(frozen=True)
class Snapshot:
    """The store at one point on each time axis.

    event_id is the last event included (None: before the first event).
    valid_through is the valid time read at (None: unbounded, so scheduled
    changes count).
    """

    reader: Reader
    event_id: int | None
    valid_through: datetime | None

    def valid_at(self, time: datetime) -> "Snapshot":
        return replace(self, valid_through=aware(time))

    def valid_unbounded(self) -> "Snapshot":
        return replace(self, valid_through=None)

    def get(self, key: str) -> Value | None:
        with self.reader._transaction() as db:
            row = db.execute(t"""
                SELECT kind, bool_value, int_value, float_value, str_value
                FROM changes
                WHERE key = {key}
                  AND valid <= coalesce(
                      {self.valid_through}::timestamptz, 'infinity'
                  )
                  AND seq <= coalesce({self.event_id}::bigint, 0)
                ORDER BY valid DESC, seq DESC
                LIMIT 1
            """).fetchone()
        return None if row is None else _decode(row)

    def blame(self, key: str) -> RecordedAssertion | None:
        """Returns the assertion that decides get(key), including a
        deletion.
        """
        with self.reader._transaction() as db:
            row = db.execute(t"""
                SELECT c.key, c.valid, c.seq, e.committed_at, e.source,
                       c.kind, c.bool_value, c.int_value, c.float_value,
                       c.str_value
                FROM changes c JOIN events e USING (tenant, seq)
                WHERE c.key = {key}
                  AND c.valid <= coalesce(
                      {self.valid_through}::timestamptz, 'infinity'
                  )
                  AND c.seq <= coalesce({self.event_id}::bigint, 0)
                ORDER BY c.valid DESC, c.seq DESC
                LIMIT 1
            """).fetchone()
        return None if row is None else _recorded(row)

    def _winners(self) -> Template:
        """The assertion deciding each key here, deletions included."""
        return t"""
            {_KEYS:q}
            SELECT w.key, w.kind, w.bool_value, w.int_value, w.float_value,
                   w.str_value, w.embedding
            FROM keys CROSS JOIN LATERAL (
                SELECT * FROM changes
                WHERE key = keys.key
                  AND valid <= coalesce(
                      {self.valid_through}::timestamptz, 'infinity'
                  )
                  AND seq <= coalesce({self.event_id}::bigint, 0)
                ORDER BY valid DESC, seq DESC
                LIMIT 1
            ) AS w
        """

    def state(self) -> dict[str, Value]:
        """Returns every key that has a value here, in ascending key order."""
        with self.reader._transaction() as db:
            rows = db.execute(t"""
                SELECT key, kind, bool_value, int_value, float_value, str_value
                FROM ({self._winners():q}) AS w
                WHERE kind <> 'delete'
                ORDER BY key
            """).fetchall()
        return {key: _decode(columns) for key, *columns in rows}

    def search(self, text: str, limit: int = 5) -> dict[str, Value]:
        """Returns up to limit of the values held here, closest in meaning to
        text first.
        """
        if limit < 1:
            raise InvalidInput(f"limit must be positive, got {limit}")
        (query,) = self.reader._embed([text])
        with self.reader._transaction() as db:
            rows = db.execute(t"""
                SELECT key, kind, bool_value, int_value, float_value, str_value
                FROM ({self._winners():q}) AS w
                WHERE kind <> 'delete'
                ORDER BY embedding <=> {_vector(query)}::vector, key
                LIMIT {limit}
            """).fetchall()
        return {key: _decode(columns) for key, *columns in rows}

    def changepoints(self) -> list[datetime]:
        """Returns every valid time at which some key changes, as of
        event_id.
        """
        with self.reader._transaction() as db:
            rows = db.execute(t"""
                SELECT DISTINCT valid FROM changes
                WHERE seq <= coalesce({self.event_id}::bigint, 0)
                ORDER BY valid
            """).fetchall()
        return [valid for (valid,) in rows]

    def diff(self, other: "Snapshot") -> dict[str, Delta]:
        """Returns how other differs from this snapshot, by key."""
        before, after = self.state(), other.state()
        deltas: dict[str, Delta] = {}
        for key in before.keys() - after.keys():
            deltas[key] = Removed(before[key])
        for key in after.keys() - before.keys():
            deltas[key] = Added(after[key])
        for key in before.keys() & after.keys():
            # 1 == True == 1.0 in Python, but they are different values here
            if (
                type(before[key]) is not type(after[key])
                or before[key] != after[key]
            ):
                deltas[key] = Changed(before[key], after[key])
        return dict(sorted(deltas.items()))
