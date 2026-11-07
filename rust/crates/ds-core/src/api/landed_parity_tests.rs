//! Parity for `task_complete`'s `landed` argument.
//!
//! Python: `dunk_api.h_task_complete` passes `True if landed is None else _bool(landed, True)`,
//! with `_bool` at dunk_api.py:72 (true only for "1", "true", "yes", "y", "on" after strip and
//! lower-case). Expected values below are the output of running that Python code on each input.

use serde_json::{json, Value};

use super::{python_bool_arg, Args};

fn landed_from(value: Value) -> bool {
    let mut args = Args::new();
    args.insert("landed".into(), value);
    python_bool_arg(&args, "landed", true)
}

#[test]
fn parity_landed_coercion_matches_python_bool() {
    let rows: Vec<(Value, bool)> = vec![
        (Value::Null, true),
        (json!(true), true),
        (json!(false), false),
        (json!("true"), true),
        (json!("TRUE"), true),
        (json!(" yes "), true),
        (json!("y"), true),
        (json!("on"), true),
        (json!("1"), true),
        (json!("false"), false),
        (json!("no"), false),
        (json!("0"), false),
        (json!(""), false),
        (json!("maybe"), false),
        (json!(0), false),
        (json!(1), true),
        (json!(2), false),
        (json!(0.0), false),
        (json!(1.0), false),
        (json!(2.5), false),
    ];
    for (value, python) in rows {
        assert_eq!(
            landed_from(value.clone()),
            python,
            "landed={value} diverges from h_task_complete/_bool"
        );
    }
}

#[test]
fn parity_landed_absent_means_landed() {
    // Python: `args.get("landed")` is None when the key is missing -> True.
    let args = Args::new();
    assert!(python_bool_arg(&args, "landed", true));
}
