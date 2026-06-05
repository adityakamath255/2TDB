use time_travel_db_rs::{Change, Value};

#[test]
fn value_is_untagged() {
    for (value, json) in [
        (Value::Bool(true), "true"),
        (Value::Int(5), "5"),
        (Value::Float(2.5), "2.5"),
        (Value::Str("hi".into()), "\"hi\""),
    ] {
        assert_eq!(serde_json::to_string(&value).unwrap(), json);
        assert_eq!(serde_json::from_str::<Value>(json).unwrap(), value);
    }
}

#[test]
fn integers_and_floats_stay_distinct() {
    assert_eq!(serde_json::from_str::<Value>("5").unwrap(), Value::Int(5));
    assert_eq!(serde_json::from_str::<Value>("5.0").unwrap(), Value::Float(5.0));
}

#[test]
fn set_is_bare_and_delete_is_null() {
    assert_eq!(
        serde_json::to_string(&Change::Set(Value::Int(1))).unwrap(),
        "1"
    );
    assert_eq!(serde_json::to_string(&Change::Delete).unwrap(), "null");
    assert_eq!(
        serde_json::from_str::<Change>("1").unwrap(),
        Change::Set(Value::Int(1))
    );
    assert_eq!(
        serde_json::from_str::<Change>("null").unwrap(),
        Change::Delete
    );
}
