import os
from collections.abc import AsyncIterator, Iterator
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import datetime
from typing import Annotated, Literal

from fastapi import Depends, FastAPI, HTTPException, Query, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from psycopg_pool import ConnectionPool, PoolTimeout
from pydantic import AwareDatetime, BaseModel, Field, model_validator

from py2tdb.embedding import Embedder
from py2tdb.model import (
    Added,
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
)
from py2tdb.store import Embed, Reader, Snapshot, Store


class At(BaseModel, extra="forbid"):
    """A snapshot: the latest event, or the one named by event or known_at,
    read at valid_at (default: that event's commit time, known_at, or now
    for the latest event).
    """

    event: int | None = None
    known_at: AwareDatetime | None = None
    valid_at: AwareDatetime | None = None

    @model_validator(mode="after")
    def _one_transaction_time(self) -> At:
        if self.event is not None and self.known_at is not None:
            raise ValueError("give event or known_at, not both")
        return self

    def snapshot(self, reader: Reader) -> Snapshot:
        if self.event is not None:
            snapshot = reader.at(self.event)
        elif self.known_at is not None:
            snapshot = reader.known_at(self.known_at)
        else:
            snapshot = reader.latest()
        return (
            snapshot
            if self.valid_at is None
            else snapshot.valid_at(self.valid_at)
        )


class KeyAt(At):
    key: str


class SearchAt(At):
    q: str
    limit: Annotated[int, Field(ge=1, le=50)] = 5


class KeyQuery(BaseModel, extra="forbid"):
    key: str


class DiffQuery(BaseModel, extra="forbid"):
    from_event: int | None = None
    from_known_at: AwareDatetime | None = None
    from_valid_at: AwareDatetime | None = None
    to_event: int | None = None
    to_known_at: AwareDatetime | None = None
    to_valid_at: AwareDatetime | None = None

    @model_validator(mode="after")
    def _sides_are_snapshots(self) -> DiffQuery:
        self.sides()
        return self

    def sides(self) -> tuple[At, At]:
        return (
            At(
                event=self.from_event,
                known_at=self.from_known_at,
                valid_at=self.from_valid_at,
            ),
            At(
                event=self.to_event,
                known_at=self.to_known_at,
                valid_at=self.to_valid_at,
            ),
        )


class WriteIn(BaseModel, extra="forbid"):
    key: str
    value: Value | None  # null deletes the key
    valid_from: AwareDatetime | None = None


class CommitIn(BaseModel, extra="forbid"):
    source: str
    writes: list[WriteIn]


@dataclass(frozen=True)
class SnapshotOut:
    event: int | None
    valid_at: datetime | None

    @classmethod
    def of(cls, snapshot: Snapshot) -> SnapshotOut:
        return cls(snapshot.event_id, snapshot.valid_through)


@dataclass(frozen=True)
class ValueOut:
    snapshot: SnapshotOut
    key: str
    value: Value | None


@dataclass(frozen=True)
class BlameOut:
    snapshot: SnapshotOut
    key: str
    assertion: RecordedAssertion | None


@dataclass(frozen=True)
class StateOut:
    snapshot: SnapshotOut
    state: dict[str, Value]


@dataclass(frozen=True)
class Match:
    key: str
    value: Value


@dataclass(frozen=True)
class SearchOut:
    snapshot: SnapshotOut
    matches: list[Match]


@dataclass(frozen=True)
class ChangepointsOut:
    snapshot: SnapshotOut
    changepoints: list[datetime]


@dataclass(frozen=True)
class DeltaOut:
    change: Literal["added", "removed", "changed"]
    before: Value | None
    after: Value | None

    @classmethod
    def of(cls, delta: Delta) -> DeltaOut:
        match delta:
            case Added(value):
                return cls("added", None, value)
            case Removed(value):
                return cls("removed", value, None)
            case Changed(before, after):
                return cls("changed", before, after)


@dataclass(frozen=True)
class DiffOut:
    before: SnapshotOut
    after: SnapshotOut
    changes: dict[str, DeltaOut]


@dataclass(frozen=True)
class Committed:
    event: int


_bearer = HTTPBearer(description="An API key from create_api_key.")


def _reader(
    request: Request,
    credentials: Annotated[HTTPAuthorizationCredentials, Depends(_bearer)],
) -> Iterator[Reader]:
    pool: ConnectionPool = request.app.state.pool
    embed: Embed = request.app.state.embed
    with pool.connection() as connection:
        [(tenant, writable)] = connection.execute(
            "SELECT tenant, writable FROM api_key_tenant(%s)",
            [credentials.credentials],
        ).fetchall()
        if tenant is None:
            raise HTTPException(
                401, "unknown API key", headers={"WWW-Authenticate": "Bearer"}
            )
        yield (Store if writable else Reader)(connection, tenant, embed)


def _store(reader: Annotated[Reader, Depends(_reader)]) -> Store:
    if not isinstance(reader, Store):
        raise HTTPException(403, "this API key is read-only")
    return reader


type ReaderDep = Annotated[Reader, Depends(_reader)]


def create_app(
    conninfo: str, embed: Embed, cors_origin: str | None = None
) -> FastAPI:
    """Returns the HTTP API; conninfo must log in as pg2tdb_app.

    A page served from cors_origin may read through the API from a browser.
    """

    @asynccontextmanager
    async def lifespan(app: FastAPI) -> AsyncIterator[None]:
        with ConnectionPool(
            conninfo, kwargs={"autocommit": True}, open=False
        ) as pool:
            pool.open(wait=True)
            app.state.pool = pool
            app.state.embed = embed
            yield

    app = FastAPI(
        title="2TDB",
        summary="A bitemporal key-value store.",
        lifespan=lifespan,
    )
    if cors_origin:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=[cors_origin],
            allow_methods=["GET"],
            allow_headers=["Authorization"],
        )

    @app.exception_handler(InvalidInput)
    @app.exception_handler(EmptyCommit)
    def _unprocessable(request: Request, error: Exception) -> JSONResponse:
        return JSONResponse({"detail": str(error)}, status_code=422)

    @app.exception_handler(OutOfRange)
    def _not_found(request: Request, error: OutOfRange) -> JSONResponse:
        return JSONResponse({"detail": str(error)}, status_code=404)

    @app.exception_handler(PoolTimeout)
    def _busy(request: Request, error: PoolTimeout) -> JSONResponse:
        return JSONResponse(
            {"detail": "no database connection free; try again"},
            status_code=503,
        )

    @app.post("/events", status_code=201)
    def commit(
        body: CommitIn, store: Annotated[Store, Depends(_store)]
    ) -> Committed:
        """Records the writes as one event."""
        writes = [
            Write(write.key, write.value, write.valid_from)
            for write in body.writes
        ]
        return Committed(store.commit(writes, source=body.source))

    @app.get("/events/{event_id}")
    def event(event_id: int, reader: ReaderDep) -> Event:
        return reader.event(event_id)

    @app.get("/value")
    def value(query: Annotated[KeyAt, Query()], reader: ReaderDep) -> ValueOut:
        snapshot = query.snapshot(reader)
        return ValueOut(
            SnapshotOut.of(snapshot), query.key, snapshot.get(query.key)
        )

    @app.get("/blame")
    def blame(query: Annotated[KeyAt, Query()], reader: ReaderDep) -> BlameOut:
        """Returns the assertion deciding key's value, deletions included."""
        snapshot = query.snapshot(reader)
        return BlameOut(
            SnapshotOut.of(snapshot), query.key, snapshot.blame(query.key)
        )

    @app.get("/state")
    def state(query: Annotated[At, Query()], reader: ReaderDep) -> StateOut:
        snapshot = query.snapshot(reader)
        return StateOut(SnapshotOut.of(snapshot), snapshot.state())

    @app.get("/search")
    def search(
        query: Annotated[SearchAt, Query()], reader: ReaderDep
    ) -> SearchOut:
        """Returns the facts held at the snapshot that are closest in meaning
        to q, closest first.
        """
        snapshot = query.snapshot(reader)
        matches = [
            Match(key, value)
            for key, value in snapshot.search(query.q, query.limit).items()
        ]
        return SearchOut(SnapshotOut.of(snapshot), matches)

    @app.get("/changepoints")
    def changepoints(
        query: Annotated[At, Query()], reader: ReaderDep
    ) -> ChangepointsOut:
        snapshot = query.snapshot(reader)
        return ChangepointsOut(
            SnapshotOut.of(snapshot), snapshot.changepoints()
        )

    @app.get("/diff")
    def diff(
        query: Annotated[DiffQuery, Query()], reader: ReaderDep
    ) -> DiffOut:
        before, after = (side.snapshot(reader) for side in query.sides())
        changes = {
            key: DeltaOut.of(delta)
            for key, delta in before.diff(after).items()
        }
        return DiffOut(SnapshotOut.of(before), SnapshotOut.of(after), changes)

    @app.get("/history")
    def history(
        query: Annotated[KeyQuery, Query()], reader: ReaderDep
    ) -> list[RecordedAssertion]:
        return reader.history(query.key)

    @app.get("/keys")
    def keys(reader: ReaderDep) -> list[str]:
        """Returns every key ever asserted, including deleted ones."""
        return reader.keys()

    return app


def from_environment() -> FastAPI:
    """The app for `uvicorn --factory`, connecting to PY2TDB_DATABASE_URL.

    PY2TDB_CORS_ORIGIN, if set, names the one origin whose pages may read.
    """
    return create_app(
        os.environ["PY2TDB_DATABASE_URL"],
        Embedder(),
        os.environ.get("PY2TDB_CORS_ORIGIN"),
    )
