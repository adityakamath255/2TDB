use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use time_travel_db_rs::{Error, Timestamp, Value, connect, diff, in_memory, inspect};

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
fn time_travel_reflects_each_commit() {
    let mut db = in_memory().unwrap();
    db.batch().set("x", 1).commit().unwrap();
    db.batch().set("x", 2).set("y", 9).commit().unwrap();
    db.batch().delete("x").commit().unwrap();

    assert_eq!(db.at(0).unwrap().get("x").unwrap(), Some(Value::Int(1)));
    assert_eq!(db.at(1).unwrap().get("x").unwrap(), Some(Value::Int(2)));
    assert_eq!(db.latest().unwrap().get("x").unwrap(), None);
    assert_eq!(db.latest().unwrap().get("y").unwrap(), Some(Value::Int(9)));
    assert!(matches!(db.at(3), Err(Error::OutOfRange { seq: 3, len: 3 })));

    let event = db.event(1).unwrap();
    assert_eq!(
        event.changes.into_iter().collect::<Vec<_>>(),
        vec![
            ("x".into(), Some(Value::Int(2))),
            ("y".into(), Some(Value::Int(9))),
        ]
    );
    assert_eq!(db.event(2).unwrap().changes["x"], None);
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
fn as_of_returns_the_state_at_a_time() {
    let mut db = in_memory().unwrap();
    db.batch().set("x", 1).at(ts("2020-01-01T00:00:00Z")).commit().unwrap();
    db.batch().set("x", 2).at(ts("2020-06-01T00:00:00Z")).commit().unwrap();

    let get = |t| db.as_of(ts(t)).unwrap().get("x").unwrap();
    assert_eq!(get("2019-01-01T00:00:00Z"), None);
    assert_eq!(get("2020-01-01T00:00:00Z"), Some(Value::Int(1)));
    assert_eq!(get("2020-03-01T00:00:00Z"), Some(Value::Int(1)));
    assert_eq!(get("2021-01-01T00:00:00Z"), Some(Value::Int(2)));
}

#[test]
fn commit_rejects_a_backwards_timestamp_and_an_empty_batch() {
    let mut db = in_memory().unwrap();
    db.batch().set("x", 1).at(ts("2020-06-01T00:00:00Z")).commit().unwrap();
    let result = db.batch().set("x", 2).at(ts("2020-01-01T00:00:00Z")).commit();
    assert!(matches!(result, Err(Error::Backwards { .. })));
    assert!(matches!(db.batch().commit(), Err(Error::Empty)));
}

#[test]
fn diff_reports_changes_between_snapshots() {
    let mut db = in_memory().unwrap();
    db.batch().set("keep", 1).set("change", 2).set("drop", 3).commit().unwrap();
    db.batch().set("change", 20).delete("drop").set("add", 4).commit().unwrap();

    let changed = diff(&db.at(0).unwrap(), &db.latest().unwrap()).unwrap();
    assert_eq!(
        changed,
        vec![
            ("add".into(), None, Some(Value::Int(4))),
            ("change".into(), Some(Value::Int(2)), Some(Value::Int(20))),
            ("drop".into(), Some(Value::Int(3)), None),
        ]
    );
}

#[test]
fn history_tracks_one_key_through_deletes() {
    let mut db = in_memory().unwrap();
    db.batch().set("x", 1).commit().unwrap();
    db.batch().set("y", 9).commit().unwrap();
    db.batch().delete("x").commit().unwrap();

    let seen = db.history("x").unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!((seen[0].0, seen[0].1.clone()), (0, Some(Value::Int(1))));
    assert_eq!((seen[1].0, seen[1].1.clone()), (2, None));
}

#[test]
fn entries_lists_the_live_state() {
    let mut db = in_memory().unwrap();
    db.batch().set("b", 2).set("a", 1).commit().unwrap();
    db.batch().delete("b").set("c", 3).commit().unwrap();

    assert_eq!(
        db.latest().unwrap().entries().unwrap(),
        vec![("a".into(), Value::Int(1)), ("c".into(), Value::Int(3))]
    );
}

#[test]
fn commits_persist_and_replay() {
    let path = temp_path();
    {
        let mut db = connect(&path).unwrap();
        db.batch().set("a", 1).set("b", "hi").commit().unwrap();
        db.batch().delete("a").commit().unwrap();
    }

    let db = inspect(&path).unwrap();
    assert_eq!(db.latest().unwrap().get("a").unwrap(), None);
    assert_eq!(
        db.latest().unwrap().get("b").unwrap(),
        Some(Value::Str("hi".into()))
    );
    db.close().unwrap();

    std::fs::remove_file(&path).ok();
}
