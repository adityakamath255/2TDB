import os
import uuid
from collections.abc import Callable, Iterator

import psycopg
import pytest
from psycopg import sql

from two_tdb import Reader, Store, install

# a Postgres server whose user may create databases, such as `docker compose up db`
SERVER = os.environ.get("TWO_TDB_TEST_SERVER", "postgresql://postgres:postgres@localhost:5432")
DATABASE = "two_tdb_test"


@pytest.fixture(scope="session")
def database() -> str:
    with psycopg.connect(f"{SERVER}/postgres", autocommit=True) as admin:
        admin.execute(sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(sql.Identifier(DATABASE)))
        admin.execute(sql.SQL("CREATE DATABASE {}").format(sql.Identifier(DATABASE)))
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
    """Opens connections as two_tdb_app, the role the service uses."""
    opened: list[psycopg.Connection] = []

    def connect() -> psycopg.Connection:
        connection = psycopg.connect(database, autocommit=True)
        connection.execute("SET ROLE two_tdb_app")
        opened.append(connection)
        return connection

    yield connect
    for connection in opened:
        connection.close()


@pytest.fixture
def new_tenant(owner: psycopg.Connection) -> Callable[[], int]:
    def create() -> int:
        return owner.execute(
            "INSERT INTO tenants (name) VALUES (%s) RETURNING id", [str(uuid.uuid4())]
        ).fetchone()[0]

    return create


@pytest.fixture
def new_store(new_tenant: Callable[[], int], connect_app) -> Callable[[], Store]:
    """Returns a Store for a fresh tenant on its own connection, so each starts empty."""
    return lambda: Store(connect_app(), new_tenant())


@pytest.fixture
def store(new_store) -> Store:
    return new_store()


@pytest.fixture
def open_reader(connect_app) -> Callable[[int], Reader]:
    return lambda tenant: Reader(connect_app(), tenant)
