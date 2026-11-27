import os
import re
import uuid
import zlib
from collections.abc import Callable, Iterator, Sequence

import psycopg
import pytest
from fastapi.testclient import TestClient
from psycopg import sql

from py2tdb import Reader, Store, install
from py2tdb.api import create_app

# a Postgres server whose user may create databases, such as
# `docker compose up db`
SERVER = os.environ.get(
    "PY2TDB_TEST_SERVER", "postgresql://postgres:postgres@localhost:5432"
)
DATABASE = "pg2tdb_test"
# the role is shared by every database on the server; compose.yaml uses
# the same default
APP_PASSWORD = os.environ.get("PY2TDB_APP_PASSWORD", "pg2tdb_app")


def embed(texts: Sequence[str]) -> list[list[float]]:
    """A stand-in for the model: texts sharing words are close."""
    vectors = []
    for text in texts:
        vector = [0.0] * 383 + [0.1]
        for word in re.findall(r"\w+", text.lower()):
            vector[zlib.crc32(word.encode()) % 383] += 1
        vectors.append(vector)
    return vectors


@pytest.fixture(scope="session")
def database() -> str:
    with psycopg.connect(f"{SERVER}/postgres", autocommit=True) as admin:
        admin.execute(
            sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(
                sql.Identifier(DATABASE)
            )
        )
        admin.execute(
            sql.SQL("CREATE DATABASE {}").format(sql.Identifier(DATABASE))
        )
    url = f"{SERVER}/{DATABASE}"
    with psycopg.connect(url, autocommit=True) as owner:
        install(owner)
    return url


@pytest.fixture
def owner(database: str) -> Iterator[psycopg.Connection]:
    with psycopg.connect(database, autocommit=True) as connection:
        yield connection


@pytest.fixture
def connect_app(database: str) -> Iterator[Callable[[], psycopg.Connection]]:
    """Opens connections as pg2tdb_app, the role the service uses."""
    opened: list[psycopg.Connection] = []

    def connect() -> psycopg.Connection:
        connection = psycopg.connect(database, autocommit=True)
        connection.execute("SET ROLE pg2tdb_app")
        opened.append(connection)
        return connection

    yield connect
    for connection in opened:
        connection.close()


@pytest.fixture
def new_tenant(owner: psycopg.Connection) -> Callable[[], int]:
    def create() -> int:
        [(tenant,)] = owner.execute(
            "INSERT INTO tenants (name) VALUES (%s) RETURNING id",
            [str(uuid.uuid4())],
        ).fetchall()
        return tenant

    return create


@pytest.fixture
def new_store(
    new_tenant: Callable[[], int], connect_app
) -> Callable[[], Store]:
    """Returns a Store for a fresh, empty tenant on its own connection."""
    return lambda: Store(connect_app(), new_tenant(), embed)


@pytest.fixture
def store(new_store) -> Store:
    return new_store()


@pytest.fixture
def open_store(connect_app) -> Callable[[int], Store]:
    return lambda tenant: Store(connect_app(), tenant, embed)


@pytest.fixture
def open_reader(connect_app) -> Callable[[int], Reader]:
    return lambda tenant: Reader(connect_app(), tenant, embed)


@pytest.fixture(scope="session")
def app(database: str) -> Iterator[TestClient]:
    with psycopg.connect(database, autocommit=True) as owner:
        owner.execute(
            sql.SQL("ALTER ROLE pg2tdb_app LOGIN PASSWORD {}").format(
                APP_PASSWORD
            )
        )
    host = SERVER.split("@", 1)[1]
    with TestClient(
        create_app(
            f"postgresql://pg2tdb_app:{APP_PASSWORD}@{host}/{DATABASE}", embed
        )
    ) as client:
        yield client


@pytest.fixture
def new_key(
    owner: psycopg.Connection, new_tenant: Callable[[], int]
) -> Callable[..., str]:
    """Returns an API key for tenant, a fresh one by default."""

    def create(tenant: int | None = None, *, writable: bool = True) -> str:
        tenant = new_tenant() if tenant is None else tenant
        [(key,)] = owner.execute(
            "SELECT create_api_key(%s, %s)", [tenant, writable]
        ).fetchall()
        return key

    return create
