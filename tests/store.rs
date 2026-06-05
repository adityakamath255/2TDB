use time_travel_db_rs::{CommitError, OutOfRange, Timestamp, Value, diff, history, in_memory};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

#[test]
fn time_travel_reflects_each_commit() {
    let mut db = in_memory();
    db.batch().set("x", 1).commit().unwrap();
    db.batch().set("x", 2).set("y", 9).commit().unwrap();
    db.batch().delete("x").commit().unwrap();

    assert_eq!(db.at(0).unwrap().get("x"), Some(&Value::Int(1)));
    assert_eq!(db.at(1).unwrap().get("x"), Some(&Value::Int(2)));
    assert_eq!(db.at(1).unwrap().get("y"), Some(&Value::Int(9)));
    assert_eq!(db.latest().get("x"), None);
    assert_eq!(db.latest().get("y"), Some(&Value::Int(9)));
}

#[test]
fn at_rejects_an_out_of_range_seq() {
    let mut db = in_memory();
    db.batch().set("x", 1).commit().unwrap();
    assert!(matches!(db.at(1), Err(OutOfRange { seq: 1, .. })));
}

#[test]
fn as_of_returns_the_state_at_a_time() {
    let mut db = in_memory();
    db.batch().set("x", 1).at(ts("2020-01-01T00:00:00Z")).commit().unwrap();
    db.batch().set("x", 2).at(ts("2020-06-01T00:00:00Z")).commit().unwrap();

    assert_eq!(db.as_of(ts("2019-01-01T00:00:00Z")).get("x"), None);
    assert_eq!(db.as_of(ts("2020-01-01T00:00:00Z")).get("x"), Some(&Value::Int(1)));
    assert_eq!(db.as_of(ts("2020-03-01T00:00:00Z")).get("x"), Some(&Value::Int(1)));
    assert_eq!(db.as_of(ts("2021-01-01T00:00:00Z")).get("x"), Some(&Value::Int(2)));
}

#[test]
fn commit_rejects_a_backwards_timestamp() {
    let mut db = in_memory();
    db.batch().set("x", 1).at(ts("2020-06-01T00:00:00Z")).commit().unwrap();
    let result = db.batch().set("x", 2).at(ts("2020-01-01T00:00:00Z")).commit();
    assert!(matches!(result, Err(CommitError::Backwards { .. })));
}

#[test]
fn diff_reports_changes_between_snapshots() {
    let mut db = in_memory();
    db.batch().set("keep", 1).set("change", 2).set("drop", 3).commit().unwrap();
    db.batch().set("change", 20).delete("drop").set("add", 4).commit().unwrap();

    let changed: Vec<_> = diff(db.at(0).unwrap(), db.latest()).collect();
    assert_eq!(
        changed,
        vec![
            ("add", None, Some(&Value::Int(4))),
            ("change", Some(&Value::Int(2)), Some(&Value::Int(20))),
            ("drop", Some(&Value::Int(3)), None),
        ]
    );
}

#[test]
fn history_tracks_one_key_through_deletes() {
    let mut db = in_memory();
    db.batch().set("x", 1).commit().unwrap();
    db.batch().set("y", 9).commit().unwrap();
    db.batch().delete("x").commit().unwrap();

    let seen: Vec<_> = history(db.events(), "x").collect();
    assert_eq!(seen.len(), 2);
    assert_eq!((seen[0].0, seen[0].1), (0, Some(&Value::Int(1))));
    assert_eq!((seen[1].0, seen[1].1), (2, None));
}
