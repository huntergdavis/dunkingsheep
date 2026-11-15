//! Lazy watchdog inventory adapter. The daemon owns terminal/backend configuration.
use ds_core::{error::DunkError, watchdog::rules_host::Panes};
use serde_json::Value;

pub(crate) fn load(
    source: &str,
    mut request: impl FnMut() -> Result<Value, DunkError>,
) -> Result<Option<Panes>, DunkError> {
    match source {
        "none" => return Ok(None),
        "daemon" => {}
        "herdr" => return Err(DunkError::Unavailable(
            "watchdog panes.source=herdr has no native CLI adapter; use panes.source=daemon with the daemon's configured terminal backend".into(),
        )),
        _ => return Err(DunkError::Refused("invalid watchdog pane source".into())),
    }
    let response = request()?;
    let rows = response
        .get("panes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            DunkError::Unavailable(
                "daemon pane inventory is unavailable: expected panes array".into(),
            )
        })?;
    let mut panes = Panes::new();
    for row in rows {
        let row = row
            .as_object()
            .ok_or_else(|| DunkError::Unavailable("invalid daemon pane inventory row".into()))?;
        let id = match row.get("pane_id") {
            None | Some(Value::Null) => continue,
            Some(Value::String(id)) if id.is_empty() => continue,
            Some(Value::String(id)) => id,
            _ => {
                return Err(DunkError::Unavailable(
                    "invalid daemon pane inventory ID".into(),
                ))
            }
        };
        let status = match row.get("agent_status") {
            None | Some(Value::Null) => "unknown",
            Some(Value::String(status)) if status.is_empty() => "unknown",
            Some(Value::String(status)) => status,
            _ => {
                return Err(DunkError::Unavailable(
                    "invalid daemon pane inventory status".into(),
                ))
            }
        };
        panes.insert(id.clone(), status.into());
    }
    Ok(Some(panes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::client::{FlockClient, Transport};
    use serde_json::{json, Map};
    use std::{cell::Cell, time::Duration};

    struct FixtureTransport {
        response: Value,
        calls: Cell<usize>,
    }
    impl Transport for FixtureTransport {
        fn exchange(
            &self,
            line: &str,
            timeout: Duration,
        ) -> Result<Option<String>, std::io::Error> {
            assert_eq!(timeout, Duration::from_secs(5));
            let request: Value = serde_json::from_str(line).unwrap();
            assert_eq!(request["cmd"], "list_panes");
            self.calls.set(self.calls.get() + 1);
            Ok(Some(format!(
                "{}\n",
                json!({"ok":true,"result":self.response})
            )))
        }
    }

    #[test]
    fn daemon_projection_uses_client_and_preserves_unknown_status() {
        let transport = FixtureTransport {
            response: json!({"panes":[{"pane_id":"%1","agent_status":"idle"},{"pane_id":"%2"},{"pane_id":"%3","agent_status":""},{"pane_id":""},{"agent_status":"working"}]}),
            calls: Cell::new(0),
        };
        let mut client = FlockClient::new(
            transport,
            None,
            "fixture".into(),
            Duration::from_secs(5),
            None,
        );
        let panes = load("daemon", || {
            client
                .call("list_panes", Map::new())
                .map_err(|e| DunkError::Unavailable(e.to_string()))
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            panes,
            Panes::from([
                ("%1".into(), "idle".into()),
                ("%2".into(), "unknown".into()),
                ("%3".into(), "unknown".into())
            ])
        );
    }

    #[test]
    fn disabled_and_unsupported_sources_never_request_inventory() {
        assert!(load("none", || panic!("disabled source requested panes"))
            .unwrap()
            .is_none());
        assert!(load("herdr", || panic!("must not silently remap herdr"))
            .unwrap_err()
            .to_string()
            .contains("panes.source=herdr"));
    }

    #[test]
    fn rpc_and_malformed_inventory_errors_remain_errors() {
        assert!(load("daemon", || Err(DunkError::Unavailable(
            "fixture RPC failure".into()
        )))
        .unwrap_err()
        .to_string()
        .contains("fixture RPC failure"));
        for response in [
            json!({}),
            json!({"panes":null}),
            json!({"panes":[false]}),
            json!({"panes":[{"pane_id":42}]}),
            json!({"panes":[{"pane_id":"%1","agent_status":true}]}),
        ] {
            assert!(
                load("daemon", || Ok(response.clone())).is_err(),
                "{response}"
            );
        }
        assert_eq!(
            load("daemon", || Ok(json!({"panes":[]}))).unwrap(),
            Some(Panes::new())
        );
    }
}
