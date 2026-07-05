use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use time_travel_db_rs::{
    Assertion, Change, Db, Error, Snapshot, Timestamp, Value, connect, diff, in_memory, inspect,
};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
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
    let mut db = in_memory().unwrap();
    db.batch().set("x", 1).commit().unwrap();
    db.batch().set("x", 2).set("y", 9).commit().unwrap();
    db.batch().delete("x").commit().unwrap();

    assert_eq!(db.at(0).unwrap().get("x").unwrap(), Some(Value::Int(1)));
    assert_eq!(db.at(1).unwrap().get("x").unwrap(), Some(Value::Int(2)));
    assert_eq!(db.latest().unwrap().get("x").unwrap(), None);
    assert_eq!(db.latest().unwrap().get("y").unwrap(), Some(Value::Int(9)));
    assert_eq!(db.len().unwrap(), 3);
    assert!(matches!(
        db.at(3),
        Err(Error::OutOfRange { seq: 3, len: 3 })
    ));
    assert!(matches!(db.event(3), Err(Error::OutOfRange { .. })));
}

#[test]
fn values_round_trip_with_their_types() {
    let mut db = in_memory().unwrap();
    db.batch()
        .set("b", true)
        .set("i", 1)
        .set("f", 2.5)
        .set("s", "hi")
        .commit()
        .unwrap();

    let state = db.latest().unwrap();
    assert_eq!(state.get("b").unwrap(), Some(Value::Bool(true)));
    assert_eq!(state.get("i").unwrap(), Some(Value::Int(1)));
    assert_eq!(state.get("f").unwrap(), Some(Value::Float(2.5)));
    assert_eq!(state.get("s").unwrap(), Some(Value::Str("hi".into())));
}

#[test]
fn corrections_splice_into_the_timeline() {
    let mut db = in_memory().unwrap();
    db.batch()
        .set_from("x", "a", ts("2020-01-01T00:00:00Z"))
        .commit()
        .unwrap();
    db.batch()
        .set_from("x", "b", ts("2020-06-01T00:00:00Z"))
        .commit()
        .unwrap();
    db.batch()
        .set_from("x", "c", ts("2020-03-01T00:00:00Z"))
        .commit()
        .unwrap();

    let now = db.latest().unwrap();
    let get = |t| now.valid_at(ts(t)).get("x").unwrap();
    assert_eq!(get("2020-02-01T00:00:00Z"), Some(Value::Str("a".into())));
    assert_eq!(get("2020-04-01T00:00:00Z"), Some(Value::Str("c".into())));
    // the correction splices: it does not override past the next assertion
    assert_eq!(get("2020-07-01T00:00:00Z"), Some(Value::Str("b".into())));

    let then = db.at(1).unwrap().valid_at(ts("2020-04-01T00:00:00Z"));
    assert_eq!(then.get("x").unwrap(), Some(Value::Str("a".into())));
}

#[test]
fn reasserting_the_same_valid_time_supersedes() {
    let mut db = in_memory().unwrap();
    let v = ts("2021-01-01T00:00:00Z");
    db.batch().set_from("x", 1, v).commit().unwrap();
    db.batch().set_from("x", 2, v).commit().unwrap();

    assert_eq!(
        db.latest().unwrap().valid_at(v).get("x").unwrap(),
        Some(Value::Int(2))
    );
    assert_eq!(
        db.at(0).unwrap().valid_at(v).get("x").unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn scheduled_changes_wait_for_their_valid_time() {
    let future = Timestamp::now() + Duration::from_secs(3600);
    let mut db = in_memory().unwrap();
    db.batch()
        .set("price", 3)
        .set_from("price", 4, future)
        .commit()
        .unwrap();

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
    assert_eq!(
        now.entries().unwrap(),
        vec![("price".into(), Value::Int(3))]
    );
}

#[test]
fn known_at_walks_the_log_by_wall_clock() {
    let mut db = in_memory().unwrap();
    db.batch().set("x", 1).commit().unwrap();
    std::thread::sleep(Duration::from_millis(2));
    db.batch().set("x", 2).commit().unwrap();
    std::thread::sleep(Duration::from_millis(2));
    db.batch().set("x", 3).commit().unwrap();

    let stamp = |seq| db.event(seq).unwrap().ts;
    let before = stamp(0) - Duration::from_secs(1);
    assert_eq!(db.known_at(before).unwrap().get("x").unwrap(), None);
    assert!(db.known_at(before).unwrap().entries().unwrap().is_empty());
    let get = |t| db.known_at(t).unwrap().get("x").unwrap();
    assert_eq!(get(stamp(0)), Some(Value::Int(1)));
    assert_eq!(
        get(stamp(1) - Duration::from_micros(1)),
        Some(Value::Int(1))
    );
    assert_eq!(get(stamp(1)), Some(Value::Int(2)));
    assert_eq!(get(stamp(2) + Duration::from_secs(1)), Some(Value::Int(3)));
}

#[test]
fn entries_lists_the_state_at_a_coordinate() {
    let mut db = in_memory().unwrap();
    db.batch().set("b", 2).set("a", 1).commit().unwrap();
    db.batch().delete("b").set("c", 3).commit().unwrap();

    assert_eq!(
        db.latest().unwrap().entries().unwrap(),
        vec![("a".into(), Value::Int(1)), ("c".into(), Value::Int(3))]
    );
    assert_eq!(
        db.at(0).unwrap().entries().unwrap(),
        vec![("a".into(), Value::Int(1)), ("b".into(), Value::Int(2))]
    );
}

#[test]
fn diff_spans_both_axes() {
    let mut db = in_memory().unwrap();
    db.batch()
        .set("keep", 1)
        .set("change", 2)
        .set("drop", 3)
        .commit()
        .unwrap();
    db.batch()
        .set("change", 20)
        .delete("drop")
        .set("add", 4)
        .commit()
        .unwrap();

    let changed = diff(&db.at(0).unwrap(), &db.latest().unwrap()).unwrap();
    assert_eq!(
        changed,
        vec![
            ("add".into(), None, Some(Value::Int(4))),
            ("change".into(), Some(Value::Int(2)), Some(Value::Int(20))),
            ("drop".into(), Some(Value::Int(3)), None),
        ]
    );

    let mut db = in_memory().unwrap();
    let jan = ts("2022-01-01T00:00:00Z");
    let jun = ts("2022-06-01T00:00:00Z");
    db.batch()
        .set_from("x", 1, jan)
        .set_from("x", 2, jun)
        .commit()
        .unwrap();
    let now = db.latest().unwrap();
    assert_eq!(
        diff(&now.valid_at(jan), &now.valid_at(jun)).unwrap(),
        vec![("x".into(), Some(Value::Int(1)), Some(Value::Int(2)))]
    );
}

#[test]
fn when_bisects_the_log() {
    let mut db = in_memory().unwrap();
    let above = |db: &Db<_>, thresh: i64| {
        db.when(|s| Ok(matches!(s.get("n")?, Some(Value::Int(i)) if i >= thresh)))
            .unwrap()
    };
    assert_eq!(above(&db, 0), None);
    for i in 0..10 {
        db.batch().set("n", i).commit().unwrap();
    }
    assert_eq!(above(&db, 0), Some(0));
    assert_eq!(above(&db, 7), Some(7));
    assert_eq!(above(&db, 10), None);

    // pinning valid time inside the predicate
    let mut db = in_memory().unwrap();
    let future = Timestamp::now() + Duration::from_secs(3600);
    db.batch().set_from("x", 1, future).commit().unwrap();
    let seen = |s: Snapshot<'_>| Ok(s.get("x")?.is_some());
    assert_eq!(db.when(seen).unwrap(), None);
    assert_eq!(
        db.when(|s| seen(s.valid_unbounded())).unwrap(),
        Some(0)
    );
}

#[test]
fn history_reports_raw_assertions() {
    let mut db = in_memory().unwrap();
    let v = ts("2020-01-01T00:00:00Z");
    db.batch().set_from("x", 1, v).commit().unwrap();
    db.batch().set("y", 9).commit().unwrap();
    db.batch().delete_from("x", v).commit().unwrap();

    let seen = db.history("x").unwrap();
    let ts1 = db.event(2).unwrap().ts;
    assert_eq!(
        seen,
        vec![
            Assertion {
                seq: 0,
                valid: v,
                value: Some(Value::Int(1)),
                ts: db.event(0).unwrap().ts
            },
            Assertion {
                seq: 2,
                valid: v,
                value: None,
                ts: ts1
            },
        ]
    );
}

#[test]
fn one_event_can_assert_one_key_at_many_valid_times() {
    let mut db = in_memory().unwrap();
    let jan = ts("2023-01-01T00:00:00Z");
    let jun = ts("2023-06-01T00:00:00Z");
    db.batch()
        .set_from("x", 1, jan)
        .set_from("x", 2, jun)
        .commit()
        .unwrap();

    let event = db.event(0).unwrap();
    assert_eq!(
        event.changes,
        vec![
            Change {
                key: "x".into(),
                valid: jan,
                value: Some(Value::Int(1))
            },
            Change {
                key: "x".into(),
                valid: jun,
                value: Some(Value::Int(2))
            },
        ]
    );
}

#[test]
fn batches_reject_empty_and_dedupe_by_key_and_valid() {
    let mut db = in_memory().unwrap();
    assert!(matches!(db.batch().commit(), Err(Error::Empty)));

    let v = ts("2020-01-01T00:00:00Z");
    db.batch()
        .set_from("x", 1, v)
        .set_from("x", 2, v)
        .commit()
        .unwrap();
    assert_eq!(
        db.latest().unwrap().valid_at(v).get("x").unwrap(),
        Some(Value::Int(2))
    );
}

#[test]
fn valid_times_are_truncated_to_microseconds() {
    let mut db = in_memory().unwrap();
    db.batch()
        .set_from("x", 1, ts("2020-01-01T00:00:00.0000005Z"))
        .commit()
        .unwrap();
    assert_eq!(
        db.history("x").unwrap()[0].valid,
        ts("2020-01-01T00:00:00Z")
    );
}

#[test]
fn commits_persist_and_the_views_answer_plain_sql() {
    let path = temp_path();
    let future = Timestamp::now() + Duration::from_secs(3600);
    {
        let mut db = connect(&path).unwrap();
        db.batch().set("a", 1).set("b", "hi").commit().unwrap();
        db.batch()
            .delete("a")
            .set_from("c", true, future)
            .commit()
            .unwrap();
    }

    let db = inspect(&path).unwrap();
    assert_eq!(db.latest().unwrap().get("a").unwrap(), None);
    assert_eq!(
        db.latest().unwrap().get("b").unwrap(),
        Some(Value::Str("hi".into()))
    );
    db.close().unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let current: Vec<String> = conn
        .prepare("SELECT key FROM latest ORDER BY key")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(current, vec!["b"]);
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
    assert_eq!(intervals, 4);

    std::fs::remove_file(&path).ok();
}
