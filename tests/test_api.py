import psycopg
import pytest
from fastapi.testclient import TestClient

from py2tdb.api import create_app


def auth(key: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {key}"}


def commit(app, key: str, *writes: dict, source: str = "test") -> int:
    response = app.post(
        "/events",
        json={"source": source, "writes": list(writes)},
        headers=auth(key),
    )
    assert response.status_code == 201, response.text
    return response.json()["event"]


def test_reads_travel_on_both_time_axes(app, new_key):
    key = new_key()
    commit(
        app,
        key,
        {
            "key": "city",
            "value": "Mumbai",
            "valid_from": "2026-01-01T00:00:00Z",
        },
    )
    commit(
        app,
        key,
        {
            "key": "city",
            "value": "Pune",
            "valid_from": "2026-10-01T00:00:00+05:30",
        },
    )

    def city(**params) -> dict:
        return app.get(
            "/value", params={"key": "city", **params}, headers=auth(key)
        ).json()

    latest = city()
    assert latest["value"] == "Pune"
    assert latest["snapshot"]["event"] == 2
    assert city(valid_at="2026-06-01T00:00:00Z")["value"] == "Mumbai"
    assert city(event=1)["value"] == "Mumbai"
    # a snapshot's coordinates read it again exactly
    assert city(**latest["snapshot"]) == latest


def test_values_keep_their_json_types(app, new_key):
    key = new_key()
    values = {"b": True, "i": 1, "f": 1.0, "s": "1"}
    commit(app, key, *({"key": k, "value": v} for k, v in values.items()))

    state = app.get("/state", headers=auth(key)).json()["state"]
    assert state == values
    assert [type(state[k]) for k in "bifs"] == [bool, int, float, str]


def test_deletes_are_explicit_nulls(app, new_key):
    key = new_key()
    commit(app, key, {"key": "x", "value": 1})
    commit(app, key, {"key": "x", "value": None})

    assert (
        app.get("/value", params={"key": "x"}, headers=auth(key)).json()[
            "value"
        ]
        is None
    )
    missing = app.post(
        "/events",
        json={"source": "t", "writes": [{"key": "x"}]},
        headers=auth(key),
    )
    assert missing.status_code == 422


def test_keys_identify_the_tenant_and_its_rights(app, new_key, new_tenant):
    tenant = new_tenant()
    writer, reader, stranger = (
        new_key(tenant),
        new_key(tenant, writable=False),
        new_key(),
    )
    commit(app, writer, {"key": "plan", "value": "pro"})

    assert app.get("/keys").status_code == 401
    assert (
        app.get("/keys", headers=auth("2tdb_" + "0" * 64)).status_code == 401
    )
    assert app.get("/keys", headers=auth(reader)).json() == ["plan"]
    assert app.get("/keys", headers=auth(stranger)).json() == []
    assert (
        app.post(
            "/events",
            json={"source": "t", "writes": [{"key": "plan", "value": "free"}]},
            headers=auth(reader),
        ).status_code
        == 403
    )


@pytest.mark.parametrize(
    "params",
    [
        {"key": "x", "valid_at": "2026-01-01T00:00:00"},
        {"key": "x", "valid_af": "2026-01-01T00:00:00Z"},
        {"key": "x", "event": 1, "known_at": "2026-01-01T00:00:00Z"},
    ],
)
def test_reads_reject_ambiguous_coordinates(app, new_key, params):
    response = app.get("/value", params=params, headers=auth(new_key()))
    assert response.status_code == 422, response.text


@pytest.mark.parametrize(
    "body",
    [
        {"source": "t", "writes": []},
        {"source": "", "writes": [{"key": "x", "value": 1}]},
        {"source": "t", "writes": [{"key": "x", "value": 2**63}]},
        {
            "source": "t",
            "writes": [
                {"key": "x", "value": 1, "valid_form": "2026-01-01T00:00:00Z"}
            ],
        },
        {
            "source": "t",
            "writes": [
                {"key": "x", "value": 1, "valid_from": "2026-01-01T00:00:00"}
            ],
        },
        {"source": "t", "writes": [{"key": "x", "value": [1]}]},
    ],
)
def test_commits_reject_what_the_store_cannot_hold(app, new_key, body):
    key = new_key()
    assert app.post("/events", json=body, headers=auth(key)).status_code == 422
    assert app.get("/keys", headers=auth(key)).json() == []


def test_history_blame_and_diff_explain_a_change(app, new_key):
    key = new_key()
    commit(
        app,
        key,
        {"key": "plan", "value": "free"},
        {"key": "seats", "value": 1},
        source="signup",
    )
    commit(
        app,
        key,
        {"key": "plan", "value": "pro"},
        {"key": "seats", "value": None},
        source="msg-7",
    )

    blame = app.get(
        "/blame", params={"key": "plan"}, headers=auth(key)
    ).json()["assertion"]
    assert (
        blame["event_id"],
        blame["source"],
        blame["assertion"]["value"],
    ) == (2, "msg-7", "pro")
    history = app.get(
        "/history", params={"key": "plan"}, headers=auth(key)
    ).json()
    assert [entry["assertion"]["value"] for entry in history] == [
        "free",
        "pro",
    ]
    diff = app.get("/diff", params={"from_event": 1}, headers=auth(key)).json()
    assert diff["changes"] == {
        "plan": {"change": "changed", "before": "free", "after": "pro"},
        "seats": {"change": "removed", "before": 1, "after": None},
    }
    event = app.get("/events/2", headers=auth(key)).json()
    assert event["source"] == "msg-7"
    assert app.get("/events/3", headers=auth(key)).status_code == 404


def test_the_app_cannot_read_api_keys(connect_app):
    with pytest.raises(psycopg.errors.InsufficientPrivilege):
        connect_app().execute("SELECT * FROM api_keys")


def test_search_returns_facts_closest_first(app, new_key):
    key = new_key()
    commit(
        app,
        key,
        {"key": "city", "value": "Pune"},
        {"key": "pet", "value": "a beagle"},
    )

    found = app.get(
        "/search", params={"q": "beagle", "limit": 1}, headers=auth(key)
    ).json()
    assert found["matches"] == [{"key": "pet", "value": "a beagle"}]
    assert (
        app.get(
            "/search", params={"q": "x", "limit": 0}, headers=auth(key)
        ).status_code
        == 422
    )


def test_only_the_configured_origin_may_read_from_a_browser():
    # preflight requests never reach the database, so none is configured
    client = TestClient(
        create_app(
            "", embed=lambda texts: [], cors_origin="https://example.org"
        )
    )
    for origin, allowed in [
        ("https://example.org", "https://example.org"),
        ("https://other.example", None),
    ]:
        response = client.options(
            "/keys",
            headers={
                "Origin": origin,
                "Access-Control-Request-Method": "GET",
                "Access-Control-Request-Headers": "authorization",
            },
        )
        assert response.headers.get("access-control-allow-origin") == allowed
