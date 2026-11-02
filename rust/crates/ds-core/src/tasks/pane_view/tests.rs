use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn sql_value(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer(i64::from(*value)),
        Value::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| SqlValue::Real(value.as_f64().unwrap())),
        Value::String(value) => SqlValue::Text(value.clone()),
        _ => panic!("fixture columns must contain SQL scalar values"),
    }
}

fn dump(conn: &mut Connection) -> Result<Value, DunkError> {
    let mut result = Map::new();
    for (table, order) in [
        ("tasks", "id"),
        ("task_leases", "task_id,pane_id"),
        ("task_events", "id"),
        ("counters", "name"),
        ("allocation_offers", "task_id"),
    ] {
        let mut query = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
            .map_err(map_sqlite)?;
        let rows = query
            .query_map([], row_wire)
            .map_err(map_sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite)?;
        result.insert(table.into(), json!(rows));
    }
    Ok(Value::Object(result))
}

#[tokio::test]
async fn tasks_for_pane_matches_python_oracle_without_mutating_store() {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tasks_for_pane.json"
    )))
    .unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(!cases.is_empty(), "the oracle must execute nonzero cases");
    for case in cases {
        let dir = crate::testsupport::dir("tasks-for-pane-oracle");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        tasks.allocation_init().await.unwrap();
        let tables = case["tables"].clone();
        tasks
            .writer
            .call(move |conn| {
                for table in ["tasks", "task_leases", "allocation_offers"] {
                    for row in tables[table].as_array().into_iter().flatten() {
                        let row = row.as_object().unwrap();
                        let columns: Vec<_> = row.keys().collect();
                        let quoted: Vec<_> = columns
                            .iter()
                            .map(|column| format!("\"{column}\""))
                            .collect();
                        let marks = vec!["?"; columns.len()].join(",");
                        let values: Vec<_> = columns
                            .iter()
                            .map(|column| sql_value(&row[*column]))
                            .collect();
                        conn.execute(
                            &format!(
                                "INSERT INTO {table} ({}) VALUES ({marks})",
                                quoted.join(",")
                            ),
                            rusqlite::params_from_iter(values),
                        )
                        .map_err(map_sqlite)?;
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        tasks.on_event(Box::new(move |_, _, _, _| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let before = tasks.writer.call(dump).await.unwrap();
        let pane = case["pane_id"].as_str().unwrap();
        let limit = case["limit"].as_i64();
        let first = tasks.for_pane(pane, limit).await.unwrap();
        assert_eq!(
            first, case["expected"],
            "Python oracle case {}",
            case["name"]
        );
        assert_eq!(
            tasks.for_pane(pane, limit).await.unwrap(),
            first,
            "repeat {}",
            case["name"]
        );
        assert_eq!(
            tasks.writer.call(dump).await.unwrap(),
            before,
            "read mutated {}",
            case["name"]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "read fired an observer");
        // Drop the writer before its scratch directory, just like Scratch<T> fixtures.
        drop(tasks);
    }
}

#[tokio::test]
async fn tasks_for_pane_propagates_scope_and_database_errors() {
    let dir = crate::testsupport::dir("tasks-for-pane-errors");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    assert!(tasks.for_pane("not a pane", None).await.is_err());
    tasks
        .writer
        .call(|conn| conn.execute_batch("DROP TABLE tasks").map_err(map_sqlite))
        .await
        .unwrap();
    assert!(tasks.for_pane("%1", None).await.is_err());
}

#[test]
fn tasks_for_pane_slice_supports_zero_negative_and_integer_boundaries() {
    assert_eq!(slice_length(4, 0), 0);
    assert_eq!(slice_length(4, -1), 3);
    assert_eq!(slice_length(4, i64::MIN), 0);
    assert_eq!(slice_length(4, i64::MAX), 4);
}
