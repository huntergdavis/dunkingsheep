//! Pure parity port of `web.roster` (T62, t2363).
//!
//! The caller supplies configured channels, pane records, and optional authority evidence. This
//! module never reads process, daemon, database, or terminal state.

use ds_core::unicode_casefold::full_casefold;
use serde_json::{json, Map, Value};

/// Channels whose messages go to the owner mailbox rather than a pane input.
pub const OWNER_MAILBOX_CHANNELS: &[&str] = &["cos"];

/// Optional caller-supplied authority check used for configured channels.
pub type CanMessage<'a> = dyn for<'pane> FnMut(Option<&'pane Value>) -> bool + 'a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterError(pub String);

impl std::fmt::Display for RosterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RosterError {}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, RosterError> {
    value
        .as_object()
        .ok_or_else(|| RosterError(format!("{label} must be an object")))
}

fn required<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a Value, RosterError> {
    fields
        .get(key)
        .ok_or_else(|| RosterError(format!("{label}.{key} is required")))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn string_field(
    fields: &Map<String, Value>,
    key: &str,
    default: &str,
    label: &str,
) -> Result<String, RosterError> {
    match fields.get(key) {
        None => Ok(default.to_owned()),
        Some(Value::String(value)) => Ok(full_casefold(value)),
        Some(_) => Err(RosterError(format!("{label}.{key} must be a string"))),
    }
}

fn ensure_hashable(value: &Value, label: &str) -> Result<(), RosterError> {
    if matches!(value, Value::Array(_) | Value::Object(_)) {
        return Err(RosterError(format!("{label} must be hashable")));
    }
    Ok(())
}

fn python_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Bool(left), Value::Number(right)) => {
            right.as_f64() == Some(if *left { 1.0 } else { 0.0 })
        }
        (Value::Number(left), Value::Bool(right)) => {
            left.as_f64() == Some(if *right { 1.0 } else { 0.0 })
        }
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => left == right,
    }
}

/// Python `discovered_id`: full Unicode case folding, ASCII-only slug characters, and collapsed
/// separators. `str::to_lowercase` is not equivalent for values such as `ß` and `K`.
pub fn discovered_id(pane_id: &str) -> String {
    let folded = full_casefold(pane_id);
    let mut output = String::from("pane-");
    let mut pending_separator = false;
    for character in folded.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if pending_separator && output.len() > "pane-".len() {
                output.push('-');
            }
            pending_separator = false;
            output.push(character);
        } else {
            pending_separator = true;
        }
    }
    output
}

/// Python `match_pane`: pinned identity first, then the first exact tab/cwd match, then the
/// first tab/workspace substring match. A missing pinned pane deliberately has no fallback.
pub fn match_pane(config: &Value, panes: &[Value]) -> Result<Option<Value>, RosterError> {
    let config_fields = object(config, "config")?;
    if let Some(pinned) = config_fields.get("pane").filter(|value| truthy(value)) {
        for pane in panes {
            let fields = object(pane, "pane")?;
            if fields.get("pane_id") == Some(pinned) {
                return Ok(Some(pane.clone()));
            }
        }
        return Ok(None);
    }

    let target = config_fields
        .get("match")
        .ok_or_else(|| RosterError("config.match is required".into()))?
        .as_str()
        .map(full_casefold)
        .ok_or_else(|| RosterError("config.match must be a string".into()))?;

    for pane in panes {
        let fields = object(pane, "pane")?;
        let tab = string_field(fields, "tab_label", "", "pane")?;
        if tab == target {
            return Ok(Some(pane.clone()));
        }
        let cwd = string_field(fields, "cwd", "", "pane")?;
        let cwd = cwd.trim_end_matches('/');
        if cwd == target || cwd.ends_with(&format!("/{target}")) {
            return Ok(Some(pane.clone()));
        }
    }
    for pane in panes {
        let fields = object(pane, "pane")?;
        let tab = string_field(fields, "tab_label", "", "pane")?;
        if tab.contains(&target) {
            return Ok(Some(pane.clone()));
        }
        let workspace = string_field(fields, "workspace_label", "", "pane")?;
        if workspace.contains(&target) {
            return Ok(Some(pane.clone()));
        }
    }
    Ok(None)
}

/// Python `is_lane`: a nonempty agent or a declared working/idle/blocked turn identifies a lane.
pub fn is_lane(pane: Option<&Value>) -> Result<bool, RosterError> {
    let Some(pane) = pane else {
        return Ok(false);
    };
    let fields = object(pane, "pane")?;
    let agent = fields.get("agent").is_some_and(truthy);
    let status = fields.get("agent_status").and_then(Value::as_str);
    Ok(agent || matches!(status, Some("working" | "idle" | "blocked")))
}

/// Python `presence`; non-string hashable statuses map to `unknown`, while JSON arrays/objects
/// are refused because Python dictionary lookup would fail on an unhashable key.
pub fn presence(raw_status: Option<&Value>) -> Result<&'static str, RosterError> {
    let Some(raw_status) = raw_status else {
        return Ok("unknown");
    };
    ensure_hashable(raw_status, "agent_status")?;
    Ok(match raw_status.as_str() {
        Some("working") => "working",
        Some("idle") => "idle",
        Some("blocked") => "waiting",
        _ => "unknown",
    })
}

type Authority<'a> = Option<&'a mut CanMessage<'a>>;

fn can_message(pane: Option<&Value>, authority: &mut Authority<'_>) -> Result<bool, RosterError> {
    if let Some(authority) = authority.as_mut() {
        Ok(authority(pane))
    } else {
        is_lane(pane)
    }
}

/// Python `build_roster`, with all state and authority supplied by the caller.
///
/// Callback call order matches Python: every output row asks once for `activity_group` (including
/// offline rows); online non-`cos` rows ask a second time for `can_message`. Discovered-pane
/// eligibility always uses `is_lane`, not the callback.
pub fn build_roster<'a>(
    configured: &[Value],
    panes: &[Value],
    include_terminals: bool,
    mut authority: Option<&'a mut CanMessage<'a>>,
) -> Result<Vec<Value>, RosterError> {
    let mut rows: Vec<(Value, Option<Value>)> = Vec::new();
    let mut used: Vec<Value> = Vec::new();

    for config in configured {
        let pane = match_pane(config, panes)?;
        if let Some(pane) = &pane {
            let fields = object(pane, "pane")?;
            let pane_id = required(fields, "pane_id", "pane")?;
            ensure_hashable(pane_id, "pane.pane_id")?;
            if !used.iter().any(|known| python_equal(known, pane_id)) {
                used.push(pane_id.clone());
            }
        }
        rows.push((config.clone(), pane));
    }

    for pane in panes {
        if is_lane(Some(pane))? || include_terminals {
            let fields = object(pane, "pane")?;
            let pane_id = fields.get("pane_id").cloned().unwrap_or(Value::Null);
            ensure_hashable(&pane_id, "pane.pane_id")?;
            if !used.iter().any(|known| python_equal(known, &pane_id)) {
                let raw_pane_id = required(fields, "pane_id", "pane")?;
                let pane_id_text = raw_pane_id
                    .as_str()
                    .ok_or_else(|| RosterError("pane.pane_id must be a string".into()))?;
                let name = fields
                    .get("tab_label")
                    .filter(|value| truthy(value))
                    .or_else(|| fields.get("agent").filter(|value| truthy(value)))
                    .unwrap_or(raw_pane_id)
                    .clone();
                rows.push((
                    json!({
                        "id": discovered_id(pane_id_text),
                        "name": name,
                        "kind": "discovered",
                        "dashboard": false,
                    }),
                    Some(pane.clone()),
                ));
            }
        }
    }

    let mut output = Vec::with_capacity(rows.len());
    for (config, pane) in rows {
        let status = if let Some(pane) = &pane {
            let fields = object(pane, "pane")?;
            presence(fields.get("agent_status"))?.to_owned()
        } else {
            "offline".to_owned()
        };
        let config_fields = object(&config, "config")?;
        let id = required(config_fields, "id", "config")?;
        let name = required(config_fields, "name", "config")?;
        let kind = required(config_fields, "kind", "config")?;
        let id_hashable = ensure_hashable(id, "config.id");

        let activity_allowed = can_message(pane.as_ref(), &mut authority)?;
        let activity_group = if activity_allowed && matches!(status.as_str(), "working" | "waiting")
        {
            "active"
        } else {
            "standby"
        };

        let can_message_row = if let Some(pane) = pane.as_ref() {
            id_hashable?;
            let owner_mailbox = id
                .as_str()
                .is_some_and(|value| OWNER_MAILBOX_CHANNELS.contains(&value));
            owner_mailbox || can_message(Some(pane), &mut authority)?
        } else {
            false
        };

        let fields = pane
            .as_ref()
            .map(|entry| entry.as_object().expect("validated pane"));
        output.push(json!({
            "id": id,
            "name": name,
            "kind": kind,
            "dashboard": config_fields.get("dashboard").cloned().unwrap_or(Value::Bool(false)),
            "online": pane.is_some(),
            "status": status,
            "activity_group": activity_group,
            "pane_id": fields.and_then(|pane| pane.get("pane_id")).cloned().unwrap_or(Value::Null),
            "workspace": fields.and_then(|pane| pane.get("workspace_label")).cloned().unwrap_or(Value::Null),
            "tab_label": fields.and_then(|pane| pane.get("tab_label")).cloned().unwrap_or(Value::Null),
            "agent": fields.and_then(|pane| pane.get("agent")).cloned().unwrap_or(Value::Null),
            "can_message": can_message_row,
        }));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/roster_oracle.json")).unwrap()
    }

    fn build_case(name: &str) {
        let fixture = oracle();
        let case = fixture["build_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("missing Python oracle case {name}"));
        let configured = case["configured"].as_array().unwrap();
        let panes = case["panes"].as_array().unwrap();
        let mut calls = Vec::new();
        let responses = case["authority_responses"].as_array();
        let mut response_index = 0;
        let mut callback = |pane: Option<&Value>| {
            calls.push(
                pane.and_then(|pane| pane.get("pane_id"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            let response = responses
                .and_then(|values| values.get(response_index))
                .expect("Python fixture supplies every callback response")
                .as_bool()
                .expect("authority responses are booleans");
            response_index += 1;
            response
        };
        let got = if responses.is_some() {
            build_roster(
                configured,
                panes,
                case["include_terminals"].as_bool().unwrap(),
                Some(&mut callback),
            )
            .unwrap()
        } else {
            build_roster(
                configured,
                panes,
                case["include_terminals"].as_bool().unwrap(),
                None,
            )
            .unwrap()
        };
        assert_eq!(case["expected"], json!(got), "case {name}");
        assert_eq!(
            case["expected_calls"],
            json!(calls),
            "authority calls {name}"
        );
        assert_eq!(responses.map_or(0, |values| values.len()), response_index);
    }

    #[test]
    fn a_pane_with_an_empty_agent_string_but_a_status_is_an_agent() {
        let fixture = oracle();
        let cases = fixture["functions"]["is_lane"].as_array().unwrap();
        let input = &cases[0]["input"];
        assert_eq!(cases[0]["expected"], json!(is_lane(Some(input)).unwrap()));
        assert!(is_lane(Some(input)).unwrap());
        let named = &cases[1]["input"];
        assert!(is_lane(Some(named)).unwrap());
    }

    #[test]
    fn a_plain_shell_and_no_pane_are_not_agents() {
        let fixture = oracle();
        let cases = fixture["functions"]["is_lane"].as_array().unwrap();
        assert!(!is_lane(Some(&cases[2]["input"])).unwrap());
        assert!(!is_lane(None).unwrap());
        assert_eq!(cases[2]["expected"], json!(false));
        assert_eq!(cases[3]["expected"], json!(false));
    }

    #[test]
    fn the_desk_channel_can_message_the_live_desk_pane() {
        build_case("the_desk_channel_can_message_the_live_desk_pane");
    }

    #[test]
    fn the_cwd_rule_matches_the_desk_directory() {
        let fixture = oracle();
        let case = fixture["match_pane_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == "casefold_and_trailing_slashes_match_cwd")
            .unwrap();
        let pane = match_pane(&case["config"], case["panes"].as_array().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(case["expected"], pane);
    }

    #[test]
    fn a_missing_desk_stays_offline_and_unmessageable() {
        build_case("a_missing_desk_stays_offline_and_unmessageable");
    }

    #[test]
    fn a_quiet_desk_is_messageable_when_the_daemon_has_process_evidence() {
        build_case("a_quiet_desk_is_messageable_when_the_daemon_has_process_evidence");
    }

    #[test]
    fn the_authority_can_refuse_a_pane_the_heuristic_would_allow() {
        build_case("the_authority_can_refuse_a_pane_the_heuristic_would_allow");
    }

    #[test]
    fn the_desk_channel_is_messageable_while_quiet_because_it_goes_through_the_owner_mailbox() {
        build_case(
            "the_desk_channel_is_messageable_while_quiet_because_it_goes_through_the_owner_mailbox",
        );
    }

    #[test]
    fn the_owner_mailbox_rule_does_not_make_an_absent_desk_messageable() {
        build_case("the_owner_mailbox_rule_does_not_make_an_absent_desk_messageable");
    }

    #[test]
    fn other_channels_still_need_an_agent() {
        build_case("other_channels_still_need_an_agent");
    }

    #[test]
    fn pinned_precedence_missing_pinned_and_match_order_match_python() {
        let fixture = oracle();
        for case in fixture["match_pane_cases"].as_array().unwrap() {
            let got = match_pane(&case["config"], case["panes"].as_array().unwrap()).unwrap();
            assert_eq!(
                case["expected"],
                got.unwrap_or(Value::Null),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn discovered_ids_use_full_casefold_and_ascii_sanitization() {
        let fixture = oracle();
        for case in fixture["functions"]["discovered_id"].as_array().unwrap() {
            assert_eq!(
                case["expected"].as_str().unwrap(),
                discovered_id(case["input"].as_str().unwrap()),
                "{}",
                case["input"]
            );
        }
        assert_eq!("pane-strasse-kelvin", discovered_id("%Straße/Kelvin"));
    }

    #[test]
    fn presence_maps_known_and_unknown_values_like_python() {
        let fixture = oracle();
        for case in fixture["functions"]["presence"].as_array().unwrap() {
            let input = &case["input"];
            let got = presence((!input.is_null()).then_some(input)).unwrap();
            assert_eq!(case["expected"].as_str().unwrap(), got, "{input}");
        }
    }

    #[test]
    fn all_roster_cases_match_complete_python_outputs_and_callback_traces() {
        let fixture = oracle();
        for case in fixture["build_cases"].as_array().unwrap() {
            build_case(case["name"].as_str().unwrap());
        }
    }

    #[test]
    fn invalid_casefold_inputs_are_refused_without_string_coercion() {
        assert!(match_pane(&json!({"match": null}), &[]).is_err());
        assert!(match_pane(&json!({"match": 12}), &[]).is_err());
    }
}
