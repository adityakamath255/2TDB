use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use time_travel_db_rs::{
    Assertion, Batch, Database, Delta, Error, Reader, RecordedAssertion, Timestamp, Value, Write,
};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

macro_rules! commit {
    ($db:expr, $first:expr $(, $rest:expr)* $(,)?) => {{
        let batch = Batch::new($first)$(.and($rest))*;
        $db.commit(batch).unwrap()
    }};
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("ttdb-{}-{}.db", std::process::id(), n));
    path
}

#[test]
fn transaction_time_travel_reflects_each_commit() {
    let mut db = Database::memory().unwrap();
    commit!(db, Write::set("x", 1));
    commit!(db, Write::set("x", 2), Write::set("y", 9));
    commit!(db, Write::delete("x"));

    assert_eq!(db.at(1).unwrap().get("x").unwrap(), Some(Value::Int(1)));
    assert_eq!(db.at(2).unwrap().get("x").unwrap(), Some(Value::Int(2)));
    assert_eq!(db.latest().unwrap().get("x").unwrap(), None);
    assert_eq!(db.latest().unwrap().get("y").unwrap(), Some(Value::Int(9)));
    assert_eq!(db.len().unwrap(), 3);
    assert!(matches!(db.at(4), Err(Error::OutOfRange { id: 4, len: 3 })));
    assert!(matches!(db.event(4), Err(Error::OutOfRange { .. })));
}

#[test]
fn values_round_trip_with_their_types() {
    let mut db = Database::memory().unwrap();
    commit!(
        db,
        Write::set("b", true),
        Write::set("i", 1),
        Write::set("f", 2.5),
        Write::set("s", "hi"),
    );

    let state = db.latest().unwrap();
    assert_eq!(state.get("b").unwrap(), Some(Value::Bool(true)));
    assert_eq!(state.get("i").unwrap(), Some(Value::Int(1)));
    assert_eq!(state.get("f").unwrap(), Some(Value::Float(2.5)));
    assert_eq!(state.get("s").unwrap(), Some(Value::Str("hi".into())));
}

#[test]
fn corrections_splice_into_the_timeline() {
    let mut db = Database::memory().unwrap();
    commit!(db, Write::set_at("x", "a", ts("2020-01-01T00:00:00Z")));
    commit!(db, Write::set_at("x", "b", ts("2020-06-01T00:00:00Z")));
    commit!(db, Write::set_at("x", "c", ts("2020-03-01T00:00:00Z")));

    let now = db.latest().unwrap();
    let get = |t| now.valid_at(ts(t)).get("x").unwrap();
    assert_eq!(get("2020-02-01T00:00:00Z"), Some(Value::Str("a".into())));
    assert_eq!(get("2020-04-01T00:00:00Z"), Some(Value::Str("c".into())));
    // the correction splices: it does not override past the next assertion
    assert_eq!(get("2020-07-01T00:00:00Z"), Some(Value::Str("b".into())));

    let then = db.at(2).unwrap().valid_at(ts("2020-04-01T00:00:00Z"));
    assert_eq!(then.get("x").unwrap(), Some(Value::Str("a".into())));
}

#[test]
fn reasserting_the_same_valid_time_supersedes() {
    let mut db = Database::memory().unwrap();
    let v = ts("2021-01-01T00:00:00Z");
    commit!(db, Write::set_at("x", 1, v));
    commit!(db, Write::set_at("x", 2, v));

    assert_eq!(
        db.latest().unwrap().valid_at(v).get("x").unwrap(),
        Some(Value::Int(2))
    );
    assert_eq!(
        db.at(1).unwrap().valid_at(v).get("x").unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn scheduled_changes_wait_for_their_valid_time() {
    let future = Timestamp::now() + Duration::from_secs(3600);
    let mut db = Database::memory().unwrap();
    commit!(
        db,
        Write::set("price", 3),
        Write::set_at("price", 4, future),
    );

    let now = db.latest().unwrap();
    assert_eq!(now.get("price").unwrap(), Some(Value::Int(3)));
    assert_eq!(
        now.valid_at(future).get("price").unwrap(),
        Some(Value::Int(4))
    );
    assert_eq!(
        now.valid_unbounded().get("price").unwrap(),
        Some(Value::Int(4))
    );
    assert_eq!(now.valid_unbounded().valid_through(), None);
    assert_eq!(
        now.state().unwrap(),
        BTreeMap::from([("price".into(), Value::Int(3))])
    );
}

#[test]
fn known_at_walks_the_log_by_wall_clock() {
    let mut db = Database::memory().unwrap();
    commit!(db, Write::set("x", 1));
    std::thread::sleep(Duration::from_millis(2));
    commit!(db, Write::set("x", 2));
    std::thread::sleep(Duration::from_millis(2));
    commit!(db, Write::set("x", 3));

    let stamp = |id| db.event(id).unwrap().committed_at;
    let before = stamp(1) - Duration::from_secs(1);
    assert_eq!(db.known_at(before).unwrap().get("x").unwrap(), None);
    assert_eq!(db.known_at(before).unwrap().event_id(), None);
    assert!(db.known_at(before).unwrap().state().unwrap().is_empty());
    assert_eq!(db.known_at(stamp(2)).unwrap().event_id(), Some(2));
    let get = |t| db.known_at(t).unwrap().get("x").unwrap();
    assert_eq!(get(stamp(1)), Some(Value::Int(1)));
    assert_eq!(
        get(stamp(2) - Duration::from_micros(1)),
        Some(Value::Int(1))
    );
    assert_eq!(get(stamp(2)), Some(Value::Int(2)));
    assert_eq!(get(stamp(3) + Duration::from_secs(1)), Some(Value::Int(3)));
}

#[test]
fn state_lists_the_values_at_a_coordinate() {
    let mut db = Database::memory().unwrap();
    commit!(db, Write::set("b", 2), Write::set("a", 1));
    commit!(db, Write::delete("b"), Write::set("c", 3));

    assert_eq!(
        db.latest().unwrap().state().unwrap(),
        BTreeMap::from([("a".into(), Value::Int(1)), ("c".into(), Value::Int(3))])
    );
    assert_eq!(
        db.at(1).unwrap().state().unwrap(),
        BTreeMap::from([("a".into(), Value::Int(1)), ("b".into(), Value::Int(2))])
    );
}

#[test]
fn diff_spans_both_axes() {
    let mut db = Database::memory().unwrap();
    commit!(
        db,
        Write::set("keep", 1),
        Write::set("change", 2),
        Write::set("drop", 3),
    );
    commit!(
        db,
        Write::set("change", 20),
        Write::delete("drop"),
        Write::set("add", 4),
    );

    let changed = db.at(1).unwrap().diff(&db.latest().unwrap()).unwrap();
    assert_eq!(
        changed,
        BTreeMap::from([
            ("add".into(), Delta::Added(Value::Int(4))),
            (
                "change".into(),
                Delta::Changed {
                    before: Value::Int(2),
                    after: Value::Int(20),
                },
            ),
            ("drop".into(), Delta::Removed(Value::Int(3))),
        ])
    );

    let mut db = Database::memory().unwrap();
    let jan = ts("2022-01-01T00:00:00Z");
    let jun = ts("2022-06-01T00:00:00Z");
    commit!(db, Write::set_at("x", 1, jan), Write::set_at("x", 2, jun));
    let now = db.latest().unwrap();
    assert_eq!(
        now.valid_at(jan).diff(&now.valid_at(jun)).unwrap(),
        BTreeMap::from([(
            "x".into(),
            Delta::Changed {
                before: Value::Int(1),
                after: Value::Int(2),
            },
        )])
    );
}

#[test]
fn diff_can_compare_databases() {
    let mut a = Database::memory().unwrap();
    let mut b = Database::memory().unwrap();
    commit!(a, Write::set("same", 1), Write::set("old", 2));
    commit!(b, Write::set("same", 1), Write::set("new", 3));

    assert_eq!(
        a.latest().unwrap().diff(&b.latest().unwrap()).unwrap(),
        BTreeMap::from([
            ("new".into(), Delta::Added(Value::Int(3))),
            ("old".into(), Delta::Removed(Value::Int(2))),
        ])
    );
}

#[test]
fn changepoints_enumerate_the_valid_axis() {
    let mut db = Database::memory().unwrap();
    assert!(db.latest().unwrap().changepoints().unwrap().is_empty());

    let jan = ts("2020-01-01T00:00:00Z");
    let mar = ts("2020-03-01T00:00:00Z");
    let jun = ts("2020-06-01T00:00:00Z");
    commit!(db, Write::set_at("x", "a", jan));
    commit!(db, Write::set_at("x", "b", jun));
    commit!(
        db,
        Write::set_at("y", true, mar),
        Write::set_at("z", 1, jun)
    );

    assert_eq!(
        db.latest().unwrap().changepoints().unwrap(),
        vec![jan, mar, jun]
    );
    assert_eq!(db.at(1).unwrap().changepoints().unwrap(), vec![jan]);
    assert_eq!(db.at(2).unwrap().changepoints().unwrap(), vec![jan, jun]);
}

#[test]
fn bisect_finds_a_monotonic_boundary() {
    let mut db = Database::memory().unwrap();
    assert_eq!(db.bisect(|_| Ok(true)).unwrap(), None);

    for value in 0..10 {
        commit!(db, Write::set("n", value));
    }

    let above = |threshold| {
        db.bisect(|snapshot| {
            Ok(matches!(
                snapshot.get("n")?,
                Some(Value::Int(value)) if value >= threshold
            ))
        })
        .unwrap()
    };
    assert_eq!(above(0), Some(1));
    assert_eq!(above(7), Some(8));
    assert_eq!(above(10), None);
}

#[test]
fn blame_names_the_assertion_in_force() {
    let mut db = Database::memory().unwrap();
    let jan = ts("2020-01-01T00:00:00Z");
    let mar = ts("2020-03-01T00:00:00Z");
    commit!(db, Write::set_at("x", "a", jan));
    commit!(db, Write::set_at("x", "c", mar));
    commit!(db, Write::delete("x"));

    let now = db.latest().unwrap();
    assert_eq!(
        now.valid_at(ts("2020-02-01T00:00:00Z")).blame("x").unwrap(),
        Some(RecordedAssertion {
            event_id: 1,
            committed_at: db.event(1).unwrap().committed_at,
            assertion: Assertion {
                key: "x".into(),
                valid_from: jan,
                value: Some(Value::Str("a".into())),
            },
        })
    );
    let apr = now.valid_at(ts("2020-04-01T00:00:00Z"));
    assert_eq!(apr.blame("x").unwrap().unwrap().event_id, 2);

    // absent now, and blame names the deleting event
    assert_eq!(now.get("x").unwrap(), None);
    let gone = now.blame("x").unwrap().unwrap();
    assert_eq!((gone.event_id, gone.assertion.value), (3, None));

    assert_eq!(now.blame("y").unwrap(), None);
}

#[test]
fn empty_store_is_empty_until_the_first_commit() {
    let mut db = Database::memory().unwrap();
    assert!(db.is_empty().unwrap());
    commit!(db, Write::set("x", 1));
    assert!(!db.is_empty().unwrap());
}

#[test]
fn a_snapshot_reports_its_coordinates() {
    let mut db = Database::memory().unwrap();
    commit!(db, Write::set("x", 1));
    let v = ts("2030-01-01T00:00:00Z");
    let snap = db.at(1).unwrap();
    assert_eq!(snap.event_id(), Some(1));
    assert_eq!(snap.valid_at(v).valid_through(), Some(v));
    assert_eq!(snap.valid_unbounded().valid_through(), None);
}

#[test]
fn keys_enumerate_every_key_ever_asserted() {
    let mut db = Database::memory().unwrap();
    assert!(db.keys().unwrap().is_empty());

    commit!(db, Write::set("b", 1), Write::set("a", 2));
    commit!(db, Write::delete("a"), Write::set("c", 3));

    // ascending, and a deleted key still counts as ever-asserted
    assert_eq!(db.keys().unwrap(), vec!["a", "b", "c"]);
}

#[test]
fn history_reports_recorded_assertions() {
    let mut db = Database::memory().unwrap();
    let v = ts("2020-01-01T00:00:00Z");
    commit!(db, Write::set_at("x", 1, v));
    commit!(db, Write::set("y", 9));
    commit!(db, Write::delete_at("x", v));

    let seen = db.history("x").unwrap();
    let deleted_at = db.event(3).unwrap().committed_at;
    assert_eq!(
        seen,
        vec![
            RecordedAssertion {
                event_id: 1,
                committed_at: db.event(1).unwrap().committed_at,
                assertion: Assertion {
                    key: "x".into(),
                    valid_from: v,
                    value: Some(Value::Int(1)),
                },
            },
            RecordedAssertion {
                event_id: 3,
                committed_at: deleted_at,
                assertion: Assertion {
                    key: "x".into(),
                    valid_from: v,
                    value: None,
                },
            },
        ]
    );
}

#[test]
fn one_event_can_assert_one_key_at_many_valid_times() {
    let mut db = Database::memory().unwrap();
    let jan = ts("2023-01-01T00:00:00Z");
    let jun = ts("2023-06-01T00:00:00Z");
    commit!(db, Write::set_at("x", 1, jan), Write::set_at("x", 2, jun));

    let event = db.event(1).unwrap();
    assert_eq!(event.id, 1);
    assert_eq!(
        event.assertions,
        vec![
            Assertion {
                key: "x".into(),
                valid_from: jan,
                value: Some(Value::Int(1))
            },
            Assertion {
                key: "x".into(),
                valid_from: jun,
                value: Some(Value::Int(2))
            },
        ]
    );
}

#[test]
fn batches_dedupe_by_key_and_valid() {
    let mut db = Database::memory().unwrap();
    let v = ts("2020-01-01T00:00:00Z");
    commit!(db, Write::set_at("x", 1, v), Write::set_at("x", 2, v));
    assert_eq!(
        db.latest().unwrap().valid_at(v).get("x").unwrap(),
        Some(Value::Int(2))
    );
}

#[test]
fn valid_times_are_truncated_to_microseconds() {
    let mut db = Database::memory().unwrap();
    commit!(
        db,
        Write::set_at("x", 1, ts("2020-01-01T00:00:00.0000005Z"))
    );
    assert_eq!(
        db.history("x").unwrap()[0].assertion.valid_from,
        ts("2020-01-01T00:00:00Z")
    );
}

#[test]
fn commits_persist_and_the_views_answer_plain_sql() {
    let path = temp_path();
    let future = Timestamp::now() + Duration::from_secs(3600);
    {
        let mut db = Database::open(&path).unwrap();
        commit!(
            db,
            Write::set("a", 1),
            Write::set("b", "hi"),
            Write::set("d", 2.5),
        );
        commit!(db, Write::delete("a"), Write::set_at("c", true, future),);
    }

    let db = Reader::inspect(&path).unwrap();
    assert_eq!(db.latest().unwrap().get("a").unwrap(), None);
    assert_eq!(
        db.latest().unwrap().get("b").unwrap(),
        Some(Value::Str("hi".into()))
    );
    db.close().unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let current: Vec<String> = conn
        .prepare("SELECT key FROM latest ORDER BY key")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(current, vec!["b", "d"]);
    let delete_kind: i64 = conn
        .query_row(
            "SELECT kind FROM changes WHERE key = 'a' ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delete_kind, 4);
    let pending: Vec<String> = conn
        .prepare("SELECT key FROM scheduled")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(pending, vec!["c"]);
    let intervals: i64 = conn
        .query_row("SELECT count(*) FROM timeline", [], |row| row.get(0))
        .unwrap();
    assert_eq!(intervals, 5);
    let types: Vec<String> = conn
        .prepare("SELECT type FROM assertions ORDER BY type")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(types, vec!["bool", "delete", "float", "int", "str"]);

    std::fs::remove_file(&path).ok();
}
