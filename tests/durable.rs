use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use time_travel_db_rs::{OpenError, Value, connect};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("ttdb-{}-{}.log", std::process::id(), n));
    path
}

#[test]
fn commits_persist_and_replay() {
    let path = temp_path();
    {
        let mut db = connect(&path).unwrap();
        db.batch().set("a", 1).set("b", "hi").commit().unwrap();
        db.batch().delete("a").commit().unwrap();
        // No close(): each commit already fsynced, and drop releases the lock.
    }

    let db = connect(&path).unwrap();
    assert_eq!(db.latest().get("a"), None);
    assert_eq!(db.latest().get("b"), Some(&Value::Str("hi".into())));

    std::fs::remove_file(&path).ok();
}

#[test]
fn connect_recovers_from_a_torn_tail() {
    let path = temp_path();
    {
        let mut db = connect(&path).unwrap();
        db.batch().set("a", 1).commit().unwrap();
        db.batch().set("b", 2).commit().unwrap();
    }

    // Simulate a crash mid-write: append a partial, newline-less line.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"changes":{"c""#).unwrap();
    }

    let mut db = connect(&path).unwrap();
    assert_eq!(db.latest().get("a"), Some(&Value::Int(1)));
    assert_eq!(db.latest().get("b"), Some(&Value::Int(2)));
    assert_eq!(db.latest().get("c"), None);

    // The torn tail was truncated, so the log is appendable again.
    db.batch().set("d", 4).commit().unwrap();
    db.close().unwrap();
    assert_eq!(connect(&path).unwrap().latest().get("d"), Some(&Value::Int(4)));

    std::fs::remove_file(&path).ok();
}

#[test]
fn second_connect_is_locked() {
    let path = temp_path();
    let _held = connect(&path).unwrap();

    match connect(&path) {
        Err(OpenError::Locked { .. }) => {}
        Err(other) => panic!("expected Locked, got {other:?}"),
        Ok(_) => panic!("expected Locked, but the second connect succeeded"),
    }

    std::fs::remove_file(&path).ok();
}
