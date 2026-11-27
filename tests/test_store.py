import time
from concurrent.futures import ThreadPoolExecutor
from datetime import UTC, datetime, timedelta

import psycopg
import pytest

from two_tdb import (
    Added,
    Assertion,
    Changed,
    EmptyCommit,
    InvalidInput,
    OutOfRange,
    RecordedAssertion,
    Removed,
    Store,
    Write,
)
from two_tdb.embedding import Embedder


def ts(text: str) -> datetime:
    return datetime.fromisoformat(text)


def commit(store: Store, *writes: Write) -> int:
    return store.commit(writes, source="test")


def test_transaction_time_travel_reflects_each_commit(store):
    commit(store, Write.set("x", 1))
    commit(store, Write.set("x", 2), Write.set("y", 9))
    commit(store, Write.delete("x"))

    assert store.at(1).get("x") == 1
    assert store.at(2).get("x") == 2
    assert store.latest().get("x") is None
    assert store.latest().get("y") == 9
    assert store.length() == 3
    with pytest.raises(OutOfRange) as raised:
        store.at(4)
    assert (raised.value.event_id, raised.value.length) == (4, 3)
    with pytest.raises(OutOfRange):
        store.event(4)


def test_values_round_trip_with_their_types(store):
    commit(
        store,
        Write.set("b", True),
        Write.set("i", 1),
        Write.set("f", 2.5),
        Write.set("s", "hi"),
    )

    state = store.latest().state()
    assert state == {"b": True, "i": 1, "f": 2.5, "s": "hi"}
    assert [type(state[key]) for key in "bifs"] == [bool, int, float, str]


def test_corrections_splice_into_the_timeline(store):
    commit(store, Write.set("x", "a", ts("2020-01-01T00:00:00Z")))
    commit(store, Write.set("x", "b", ts("2020-06-01T00:00:00Z")))
    commit(store, Write.set("x", "c", ts("2020-03-01T00:00:00Z")))

    now = store.latest()
    assert now.valid_at(ts("2020-02-01T00:00:00Z")).get("x") == "a"
    assert now.valid_at(ts("2020-04-01T00:00:00Z")).get("x") == "c"
    # the correction splices: it does not override past the next assertion
    assert now.valid_at(ts("2020-07-01T00:00:00Z")).get("x") == "b"
    assert store.at(2).valid_at(ts("2020-04-01T00:00:00Z")).get("x") == "a"


def test_reasserting_the_same_valid_time_supersedes(store):
    valid = ts("2021-01-01T00:00:00Z")
    commit(store, Write.set("x", 1, valid))
    commit(store, Write.set("x", 2, valid))

    assert store.latest().valid_at(valid).get("x") == 2
    assert store.at(1).valid_at(valid).get("x") == 1


def test_scheduled_changes_wait_for_their_valid_time(store):
    future = datetime.now(UTC) + timedelta(hours=1)
    commit(store, Write.set("price", 3), Write.set("price", 4, future))

    now = store.latest()
    assert now.get("price") == 3
    assert now.valid_at(future).get("price") == 4
    assert now.valid_unbounded().get("price") == 4
    assert now.valid_unbounded().valid_through is None
    assert now.state() == {"price": 3}


def test_known_at_walks_the_log_by_wall_clock(store):
    for value in (1, 2, 3):
        commit(store, Write.set("x", value))
        time.sleep(0.002)

    def stamp(event_id: int) -> datetime:
        return store.event(event_id).committed_at

    before = stamp(1) - timedelta(seconds=1)
    assert store.known_at(before).get("x") is None
    assert store.known_at(before).event_id is None
    assert store.known_at(before).state() == {}
    assert store.known_at(stamp(2)).event_id == 2
    assert store.known_at(stamp(1)).get("x") == 1
    assert store.known_at(stamp(2) - timedelta(microseconds=1)).get("x") == 1
    assert store.known_at(stamp(2)).get("x") == 2
    assert store.known_at(stamp(3) + timedelta(seconds=1)).get("x") == 3


def test_state_lists_the_values_at_a_coordinate(store):
    commit(store, Write.set("b", 2), Write.set("a", 1))
    commit(store, Write.delete("b"), Write.set("c", 3))

    assert list(store.latest().state().items()) == [("a", 1), ("c", 3)]
    assert list(store.at(1).state().items()) == [("a", 1), ("b", 2)]


def test_keys_sort_as_python_sorts_strings(store):
    keys = ["b", "B", "a-c", "ab", "é", "A"]
    commit(store, *(Write.set(key, 0) for key in keys))

    assert store.keys() == sorted(keys)
    assert list(store.latest().state()) == sorted(keys)


def test_diff_spans_both_axes(store, new_store):
    commit(
        store,
        Write.set("keep", 1),
        Write.set("change", 2),
        Write.set("drop", 3),
    )
    commit(
        store,
        Write.set("change", 20),
        Write.delete("drop"),
        Write.set("add", 4),
    )

    assert store.at(1).diff(store.latest()) == {
        "add": Added(4),
        "change": Changed(2, 20),
        "drop": Removed(3),
    }

    other = new_store()
    jan, jun = ts("2022-01-01T00:00:00Z"), ts("2022-06-01T00:00:00Z")
    commit(other, Write.set("x", 1, jan), Write.set("x", 2, jun))
    now = other.latest()
    assert now.valid_at(jan).diff(now.valid_at(jun)) == {"x": Changed(1, 2)}


def test_diff_compares_stores_and_tells_types_apart(new_store):
    a, b = new_store(), new_store()
    commit(a, Write.set("same", 1), Write.set("old", 2), Write.set("flag", 1))
    commit(
        b, Write.set("same", 1), Write.set("new", 3), Write.set("flag", True)
    )

    assert a.latest().diff(b.latest()) == {
        "flag": Changed(1, True),
        "new": Added(3),
        "old": Removed(2),
    }


def test_changepoints_enumerate_the_valid_axis(store):
    assert store.latest().changepoints() == []

    jan, mar, jun = (
        ts("2020-01-01T00:00:00Z"),
        ts("2020-03-01T00:00:00Z"),
        ts("2020-06-01T00:00:00Z"),
    )
    commit(store, Write.set("x", "a", jan))
    commit(store, Write.set("x", "b", jun))
    commit(store, Write.set("y", True, mar), Write.set("z", 1, jun))

    assert store.latest().changepoints() == [jan, mar, jun]
    assert store.at(1).changepoints() == [jan]
    assert store.at(2).changepoints() == [jan, jun]


def test_bisect_finds_a_monotonic_boundary(store):
    assert store.bisect(lambda _: True) is None

    for value in range(10):
        commit(store, Write.set("n", value))

    def first_at_least(threshold: int) -> int | None:
        return store.bisect(lambda snapshot: snapshot.get("n") >= threshold)

    assert first_at_least(0) == 1
    assert first_at_least(7) == 8
    assert first_at_least(10) is None


def test_blame_names_the_assertion_in_force_and_its_source(store):
    jan, mar = ts("2020-01-01T00:00:00Z"), ts("2020-03-01T00:00:00Z")
    store.commit([Write.set("x", "a", jan)], source="message 1")
    store.commit([Write.set("x", "c", mar)], source="message 2")
    store.commit([Write.delete("x")], source="message 3")

    now = store.latest()
    assert now.valid_at(ts("2020-02-01T00:00:00Z")).blame(
        "x"
    ) == RecordedAssertion(
        1, store.event(1).committed_at, "message 1", Assertion("x", jan, "a")
    )
    assert (
        now.valid_at(ts("2020-04-01T00:00:00Z")).blame("x").source
        == "message 2"
    )

    # absent now, and blame names the deleting event
    assert now.get("x") is None
    gone = now.blame("x")
    assert (gone.event_id, gone.assertion.value) == (3, None)

    assert now.blame("y") is None


def test_empty_store_is_empty_until_the_first_commit(store):
    assert store.is_empty()
    commit(store, Write.set("x", 1))
    assert not store.is_empty()


def test_a_snapshot_reports_its_coordinates(store):
    commit(store, Write.set("x", 1))
    valid = ts("2030-01-01T00:00:00Z")
    snapshot = store.at(1)

    assert snapshot.event_id == 1
    assert snapshot.valid_at(valid).valid_through == valid
    assert snapshot.valid_unbounded().valid_through is None


def test_held_snapshots_stay_fixed_across_connections(store, open_reader):
    valid = ts("2020-01-01T00:00:00Z")
    first = commit(store, Write.set("x", 1, valid))
    reader = open_reader(store.tenant)
    held = reader.latest()
    assert held.get("x") == 1

    # same valid time, so the correction would win at the held snapshot if
    # it could see it
    second = commit(store, Write.set("x", 2, valid))
    fresh = reader.latest()

    assert held.event_id == first
    assert held.get("x") == 1
    assert held.state() == {"x": 1}
    assert fresh.event_id == second
    assert fresh.get("x") == 2
    assert fresh.state() == {"x": 2}


def test_keys_enumerate_every_key_ever_asserted(store):
    assert store.keys() == []

    commit(store, Write.set("b", 1), Write.set("a", 2))
    commit(store, Write.delete("a"), Write.set("c", 3))

    # ascending, and a deleted key still counts as ever-asserted
    assert store.keys() == ["a", "b", "c"]


def test_history_reports_recorded_assertions(store):
    valid = ts("2020-01-01T00:00:00Z")
    commit(store, Write.set("x", 1, valid))
    commit(store, Write.set("y", 9))
    commit(store, Write.delete("x", valid))

    assert store.history("x") == [
        RecordedAssertion(
            1, store.event(1).committed_at, "test", Assertion("x", valid, 1)
        ),
        RecordedAssertion(
            3, store.event(3).committed_at, "test", Assertion("x", valid, None)
        ),
    ]


def test_one_event_can_assert_one_key_at_many_valid_times(store):
    jan, jun = ts("2023-01-01T00:00:00Z"), ts("2023-06-01T00:00:00Z")
    commit(store, Write.set("x", 1, jan), Write.set("x", 2, jun))

    event = store.event(1)
    assert (event.id, event.source) == (1, "test")
    assert event.assertions == (Assertion("x", jan, 1), Assertion("x", jun, 2))


def test_batches_dedupe_by_key_and_valid_time(store):
    valid = ts("2020-01-01T00:00:00Z")
    commit(store, Write.set("x", 1, valid), Write.set("x", 2, valid))

    assert store.latest().valid_at(valid).get("x") == 2
    assert len(store.event(1).assertions) == 1


def test_commit_takes_any_iterable_and_rejects_empty_input(store):
    with pytest.raises(EmptyCommit):
        store.commit([], source="test")
    assert store.is_empty()

    event_id = store.commit(
        (Write.set(key, True) for key in "ab"), source="test"
    )

    event = store.event(event_id)
    assert event_id == 1
    assert len(event.assertions) == 2
    # writes without a valid time take effect when committed
    assert all(
        a.valid_from == event.committed_at and a.value is True
        for a in event.assertions
    )


def test_commit_rolls_back_all_writes_when_one_fails(
    new_tenant, connect_app, open_store
):
    tenant = new_tenant()
    # a vector of the wrong size fails the insert into changes, which runs
    # after the event's row is inserted
    broken = Store(
        connect_app(),
        tenant,
        lambda texts: [[0.1] * (3 if t[0] == "z" else 384) for t in texts],
    )
    with pytest.raises(psycopg.errors.DataException):
        commit(broken, Write.set("a", 1), Write.set("z", 1))

    store = open_store(tenant)
    assert store.is_empty()
    assert store.latest().state() == {}
    assert commit(store, Write.set("a", 2)) == 1


@pytest.mark.parametrize(
    "make",
    [
        lambda: Write.set("x", float("nan")),
        lambda: Write.set("x", float("inf")),
        lambda: Write.set("x", 2**63),
        lambda: Write.set("x", b"bytes"),  # pyright: ignore[reportArgumentType]
        lambda: Write.set("x", "nul\x00"),
        lambda: Write.set("", 1),
        lambda: Write.set("x", None),  # pyright: ignore[reportArgumentType]
        lambda: Write.set("x", 1, datetime(2020, 1, 1)),
    ],
)
def test_writes_reject_what_the_store_cannot_hold(make):
    with pytest.raises(InvalidInput):
        make()


def test_reads_reject_timestamps_without_an_offset(store):
    with pytest.raises(InvalidInput):
        store.known_at(datetime(2020, 1, 1))
    with pytest.raises(InvalidInput):
        store.latest().valid_at(datetime(2020, 1, 1))


def test_every_event_names_its_source(store):
    with pytest.raises(InvalidInput):
        store.commit([Write.set("x", 1)], source="")
    assert store.is_empty()


def test_tenants_cannot_see_each_others_history(new_store, connect_app):
    a, b = new_store(), new_store()
    commit(a, Write.set("secret", 1))

    assert b.is_empty()
    assert b.keys() == []
    assert b.history("secret") == []
    assert b.latest().get("secret") is None
    assert b.latest().state() == {}
    with pytest.raises(OutOfRange):
        b.event(1)

    # a transaction that names no tenant sees no rows at all
    app = connect_app()
    assert app.execute("SELECT count(*) FROM changes").fetchall() == [(0,)]


def test_a_tenant_cannot_write_into_another(new_store, connect_app):
    a, b = new_store(), new_store()
    app = connect_app()
    with (
        pytest.raises(psycopg.errors.InsufficientPrivilege),
        app.transaction(),
    ):
        app.execute(
            "SELECT set_config('app.tenant', %s, true)", [str(b.tenant)]
        )
        app.execute(
            "INSERT INTO events (tenant, seq, committed_at, source)"
            " VALUES (%s, 1, now(), 'x')",
            [a.tenant],
        )


def test_the_app_cannot_rewrite_history(store, connect_app):
    commit(store, Write.set("x", 1))
    app = connect_app()
    for statement in (
        "UPDATE changes SET int_value = 2",
        "DELETE FROM changes",
        "DELETE FROM events",
    ):
        with (
            pytest.raises(psycopg.errors.InsufficientPrivilege),
            app.transaction(),
        ):
            app.execute(
                "SELECT set_config('app.tenant', %s, true)",
                [str(store.tenant)],
            )
            app.execute(statement)
    assert store.latest().get("x") == 1


def test_the_database_rejects_values_that_contradict_their_kind(store, owner):
    commit(store, Write.set("x", 1))
    with pytest.raises(psycopg.errors.CheckViolation):
        owner.execute(
            "INSERT INTO changes (tenant, key, valid, seq, kind, str_value)"
            " VALUES (%s, 'y', now(), 1, 'int', 'not an int')",
            [store.tenant],
        )


def test_concurrent_commits_number_events_in_commit_order(store, open_store):
    writers = [open_store(store.tenant) for _ in range(4)]

    def write(writer: Store) -> None:
        for value in range(10):
            commit(writer, Write.set("n", value))

    with ThreadPoolExecutor(len(writers)) as pool:
        list(pool.map(write, writers))

    assert store.length() == 40
    stamps = [store.event(event_id).committed_at for event_id in range(1, 41)]
    assert stamps == sorted(stamps)


def test_commit_times_never_go_backwards(store, owner):
    # an event stamped an hour ahead stands in for a clock that later
    # steps back
    [(ahead,)] = owner.execute(
        "INSERT INTO events (tenant, seq, committed_at, source)"
        " VALUES (%s, 1, now() + interval '1 hour', 'test')"
        " RETURNING committed_at",
        [store.tenant],
    ).fetchall()

    commit(store, Write.set("x", 1))

    assert store.event(2).committed_at >= ahead
    assert store.latest().get("x") == 1


def test_search_ranks_only_the_facts_held_at_the_snapshot(store):
    commit(
        store,
        Write.set("city", "Mumbai", ts("2026-01-01T00:00:00Z")),
        Write.set("pet", "a beagle"),
    )
    commit(
        store,
        Write.set("city", "Pune", ts("2026-10-01T00:00:00Z")),
        Write.set("plan", "pro"),
    )
    commit(store, Write.delete("pet"))

    now = store.latest()
    assert list(now.search("city Mumbai")) == ["city", "plan"]
    assert now.search("city Mumbai", limit=1) == {"city": "Pune"}
    assert now.valid_at(ts("2026-06-01T00:00:00Z")).search(
        "city", limit=1
    ) == {"city": "Mumbai"}
    assert store.at(1).search("beagle", limit=1) == {"pet": "a beagle"}
    with pytest.raises(InvalidInput):
        now.search("city", limit=0)


def test_the_embedding_model_fits_the_schema(new_tenant, connect_app):
    store = Store(connect_app(), new_tenant(), Embedder())
    commit(
        store,
        Write.set("city", "Pune"),
        Write.set("pet", "a beagle named Bruno"),
        Write.delete("plan"),
    )

    assert list(store.latest().search("where does she live")) == [
        "city",
        "pet",
    ]


def test_threads_can_share_a_store(store):
    commit(store, Write.set("x", 1))

    def read() -> None:
        for _ in range(20):
            assert store.latest().get("x") == 1

    with ThreadPoolExecutor(4) as pool:
        for future in [pool.submit(read) for _ in range(4)]:
            future.result()
