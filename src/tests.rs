use std::collections::BTreeMap;
use std::io::Cursor;

use super::*;

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

#[test]
fn resolve_ts_rejects_an_explicit_time_before_the_last() {
    let last = ts("2020-06-01T00:00:00Z");
    let earlier = ts("2020-01-01T00:00:00Z");
    assert!(matches!(
        resolve_ts(Some(earlier), Some(last)),
        Err(CommitError::Backwards { .. })
    ));
}

#[test]
fn resolve_ts_accepts_an_explicit_time_at_or_after_the_last() {
    let last = ts("2020-06-01T00:00:00Z");
    let later = ts("2021-01-01T00:00:00Z");
    assert_eq!(resolve_ts(Some(later), Some(last)).unwrap(), later);
    assert_eq!(resolve_ts(Some(last), Some(last)).unwrap(), last);
}

#[test]
fn resolve_ts_never_precedes_the_last_when_implicit() {
    // `last` is far in the future, so `now()` would be earlier; the clamp wins.
    let last = ts("2999-01-01T00:00:00Z");
    assert_eq!(resolve_ts(None, Some(last)).unwrap(), last);
}

#[test]
fn resolve_ts_uses_now_for_the_first_event() {
    let before = Timestamp::now();
    assert!(resolve_ts(None, None).unwrap() >= before);
}

fn line(key: &str, value: i64) -> Vec<u8> {
    encode(&Event {
        changes: BTreeMap::from([(key.to_string(), Change::Set(Value::Int(value)))]),
        ts: Timestamp::now(),
    })
}

#[test]
fn parse_log_stops_at_a_torn_line() {
    let mut bytes = Vec::new();
    bytes.extend(line("a", 1));
    bytes.extend(line("b", 2));
    let clean = bytes.len();
    bytes.extend_from_slice(br#"{"changes":{"c""#); // truncated, no newline

    let (events, consumed) = parse_log(Cursor::new(bytes.as_slice())).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(consumed as usize, clean);
}

#[test]
fn parse_log_stops_at_an_unparsable_line() {
    let mut bytes = Vec::new();
    bytes.extend(line("a", 1));
    let clean = bytes.len();
    bytes.extend_from_slice(b"not json at all\n");

    let (events, consumed) = parse_log(Cursor::new(bytes.as_slice())).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(consumed as usize, clean);
}
