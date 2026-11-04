//! Task statistics and reaping handlers for the T39 API leaf.
use super::*;

impl Dispatcher {
    pub fn register_task_statistics(&mut self) {
        self.register("task_stats", h_task_stats);
        self.register("task_metrics", h_task_metrics);
        self.register("task_reap", h_task_reap);
    }
}

fn scope_arg(args: &Args) -> Option<String> {
    args.get("scope")
        .filter(|value| !value.is_null())
        .map(python_value_string)
}

pub fn h_task_stats(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let scope = scope_arg(&args);
    context.block_on(
        context
            .flock
            .task_stats(scope.as_deref(), context.self_pane_id()),
    )?
}

pub fn h_task_metrics(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let window = args
        .get("window_s")
        .filter(|v| !v.is_null())
        .map(python_float)
        .transpose()?
        .unwrap_or(3600.0);
    // Python's Flock.task_metrics resolves and authorizes scope before Tasks.metrics
    // converts until/since. Preserve that order for requests with multiple bad fields.
    let scope = scope_arg(&args);
    if let Some(scope) = scope
        .as_deref()
        .filter(|scope| !scope.trim().is_empty() && scope.trim() != "all")
    {
        Flock::task_scope(
            Some(scope),
            context.self_pane_id(),
            crate::tasks::TASK_GLOBAL,
        )?;
    }
    // Python converts until before since in Tasks.metrics.
    let until = args
        .get("until")
        .filter(|v| !v.is_null())
        .map(python_float)
        .transpose()?;
    let since = args
        .get("since")
        .filter(|v| !v.is_null())
        .map(python_float)
        .transpose()?;
    let bucket = args
        .get("bucket")
        .filter(|v| truthy(v))
        .map(python_value_string)
        .unwrap_or_else(|| "hour".into());
    context.block_on(context.flock.task_metrics(
        Some(window),
        since,
        until,
        scope.as_deref(),
        Some(&bucket),
        context.self_pane_id(),
    ))?
}

pub fn h_task_reap(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    let (panes, source) = {
        let terminal = context
            .env
            .terminal()
            .lock()
            .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?;
        let panes = terminal
            .try_list_panes()
            .ok()
            .map(|panes| panes.into_iter().map(|pane| pane.pane_id).collect());
        let source = if terminal.is_live_backend() {
            "live"
        } else {
            "fixture"
        };
        (panes, source)
    };
    context.block_on(context.flock.task_reap(panes, Some(source)))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_scope_and_nested_values_preserve_python_number_strings() {
        for (raw, expected) in [
            ("6e1", "60.0"),
            ("1e3", "1000.0"),
            ("1e-3", "0.001"),
            ("1e309", "inf"),
            ("-1e309", "-inf"),
            ("-0.0", "-0.0"),
            (
                "900719925474099312345678901234567890",
                "900719925474099312345678901234567890",
            ),
        ] {
            let number: Value = serde_json::from_str(raw).unwrap();
            let mut args = Args::new();
            args.insert("scope".into(), number.clone());
            assert_eq!(scope_arg(&args).as_deref(), Some(expected));
            assert_eq!(python_value_string(&number), expected);
            assert_eq!(python_value_repr(&number), expected);
            assert_eq!(
                python_value_repr(&Value::Array(vec![number])),
                format!("[{expected}]")
            );
            assert_eq!(python_value_string(&Value::String(raw.into())), raw);
        }
        let mixed: Value = serde_json::from_str(r#"{"x":[6e1,"6e1",null,true]}"#).unwrap();
        assert_eq!(
            python_value_repr(&mixed),
            "{'x': [60.0, '6e1', None, True]}"
        );
    }
}
