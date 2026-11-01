//! `dunk_recovery.py` (T55, `recovery.rs`): t504's bounded, deterministic recovery for a
//! DS-owned prompt that is sitting unsubmitted in an ambiguous frame.
//!
//! The rule, in one sentence: submit again ONLY when the box provably holds exactly the
//! prompt from the CURRENT durable attempt, that attempt is still unconsumed, the pane is the
//! same generation it was typed into, the lane's own session never received it, and the lane
//! is provably idle with no dialog open. Everything else refuses, with a reason, BEFORE any
//! key is authorised. It never relaxes the blank-row rule (contents decide, not geometry) and
//! it is bounded to `max_recoveries` submissions so a pane that swallows Enter is not hammered.
//!
//! `dunk_recovery_native.py` (the native-session reader that produces `native_session_text`)
//! is the next slice of this module.

use serde_json::{json, Map, Value};

use crate::error::DunkError;

/// The attempt states that may be recovered. `consumed` was already taken by the lane and
/// `cancelled` will never be typed; every other state, including one this code does not know,
/// is NOT recoverable, so a new state cannot silently become actionable.
pub const RECOVERABLE_ATTEMPT_STATES: [&str; 3] = ["pending", "unconfirmed", "typed"];

/// Python `\s` (Unicode whitespace; `str.isspace` also counts U+001C..U+001F).
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str(value)` for the JSON values a record can carry.
fn py_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `_norm`: compare what a person sees, not what a terminal wrapped. Wrapping and the
/// composer's ellipsis are rendering and are removed; nothing else is normalised, because a
/// single changed word is a human edit and must refuse.
pub fn norm(text: &str) -> String {
    let replaced = text.replace('…', " ");
    replaced
        .split(is_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn refuse(reason: &str, extra: &[(&str, Value)]) -> Value {
    let mut out = Map::new();
    out.insert("action".into(), json!("refuse"));
    out.insert("reason".into(), json!(reason));
    for (key, value) in extra {
        out.insert((*key).to_string(), value.clone());
    }
    Value::Object(out)
}

/// `plan_recovery`: decide whether to submit an owned, stalled prompt once more.
///
/// * `attempt`: the durable record of what we typed: `{attempt_id, text, state, generation}`.
/// * `frame`: what the screen shows: `{ambiguous, prompt_text, dialog_open}`.
/// * `pane`: `{pane_id, generation, turn_state}`; `turn_state` is `idle`, `working` or unknown.
/// * `native_session_text`: the lane's own harness session text, or `None` when it could not be
///   read. `None` is UNKNOWN and refuses: "we could not look" is not "it never arrived".
pub fn plan_recovery(
    attempt: &Value,
    frame: &Value,
    pane: &Value,
    native_session_text: Option<&str>,
    recoveries_done: i64,
    max_recoveries: i64,
) -> Result<Value, DunkError> {
    if !attempt.is_object() || !frame.is_object() || !pane.is_object() {
        return Err(DunkError::Refused(
            "attempt, frame and pane must be mappings".into(),
        ));
    }
    let text = norm(&py_text(attempt.get("text")));
    if text.is_empty() {
        return Ok(refuse("attempt_has_no_text", &[]));
    }
    let state = py_text(attempt.get("state"));
    let state = state.trim();
    if state == "consumed" {
        return Ok(refuse("attempt_consumed", &[]));
    }
    if !RECOVERABLE_ATTEMPT_STATES.contains(&state) {
        // Fail closed on unknown states: a state this code does not know might mean the lane
        // already took the text.
        return Ok(refuse(
            "attempt_state_not_recoverable",
            &[("state", json!(state))],
        ));
    }
    let attempt_generation = attempt.get("generation");
    let current_generation = pane.get("generation");
    if !truthy(attempt_generation) || !truthy(current_generation) {
        return Ok(refuse("generation_unknown", &[]));
    }
    if attempt_generation != current_generation {
        return Ok(refuse(
            "wrong_generation",
            &[
                (
                    "attempt",
                    attempt_generation.cloned().unwrap_or(Value::Null),
                ),
                (
                    "current",
                    current_generation.cloned().unwrap_or(Value::Null),
                ),
            ],
        ));
    }
    let turn = pane.get("turn_state").cloned().unwrap_or(Value::Null);
    if turn != json!("idle") {
        let reason = if turn == json!("working") {
            "mid_turn"
        } else {
            "turn_unknown"
        };
        return Ok(refuse(reason, &[("turn_state", turn)]));
    }
    if truthy(frame.get("dialog_open")) {
        return Ok(refuse("dialog_open", &[]));
    }
    let Some(session) = native_session_text else {
        return Ok(refuse("native_session_unknown", &[]));
    };
    if norm(session).contains(&text) {
        // The lane's own transcript already contains this text: it WAS submitted, and
        // pressing Enter again would duplicate it.
        return Ok(refuse("already_submitted", &[]));
    }
    let visible = norm(&py_text(frame.get("prompt_text")));
    if visible != text {
        // Covers both directions: a human edit (visible differs) and a frame that is showing
        // something else entirely. The blank-row shape is not consulted - contents decide.
        return Ok(refuse(
            if visible.is_empty() {
                "frame_has_no_text"
            } else {
                "human_edited"
            },
            &[
                ("visible_chars", json!(visible.chars().count())),
                ("attempt_chars", json!(text.chars().count())),
            ],
        ));
    }
    if recoveries_done >= max_recoveries {
        return Ok(refuse(
            "recovery_exhausted",
            &[
                ("done", json!(recoveries_done)),
                ("limit", json!(max_recoveries)),
            ],
        ));
    }
    let pane_id = pane
        .get("pane_id")
        .map(|v| py_str(v))
        .unwrap_or_else(|| "None".into());
    let attempt_id = attempt
        .get("attempt_id")
        .map(|v| py_str(v))
        .unwrap_or_else(|| "None".into());
    Ok(json!({
        "action": "submit_once",
        "reason": "owned_pending_prompt_unsubmitted",
        "attempt_id": attempt.get("attempt_id").cloned().unwrap_or(Value::Null),
        "pane_id": pane.get("pane_id").cloned().unwrap_or(Value::Null),
        "generation": current_generation.cloned().unwrap_or(Value::Null),
        "record": format!(
            "pane {pane_id}: resubmitting attempt {attempt_id} because the composer holds exactly its text, the attempt is {state}, the generation is unchanged and the lane's session never received it"
        ),
    }))
}

/// `str(value)`: strings raw, None/True/False as Python spells them.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "Owner2370/2371 CoS two-minute check: answer owner messages FIRST. Inspect queue and actual worker claims, native starts, completions, stuck deliveries and coordinator latency.";
    const GEN: &str = "pid:1480236:start:1034306831";

    fn attempt(text: &str, state: &str, generation: Option<&str>) -> Value {
        json!({"attempt_id": "a-1", "text": text, "state": state, "generation": generation})
    }
    fn frame(prompt_text: &str, dialog_open: bool) -> Value {
        json!({"ambiguous": true, "prompt_text": prompt_text, "dialog_open": dialog_open})
    }
    fn pane(generation: Option<&str>, turn_state: Option<&str>) -> Value {
        json!({"pane_id": "%2", "generation": generation, "turn_state": turn_state})
    }
    fn plan(a: &Value, f: &Value, p: &Value, session: Option<&str>, done: i64) -> Value {
        plan_recovery(a, f, p, session, done, 1).unwrap()
    }
    fn ok_attempt() -> Value {
        attempt(PROMPT, "unconfirmed", Some(GEN))
    }

    #[test]
    fn every_row_of_the_table() {
        struct Row(
            &'static str,
            Value,
            Value,
            Value,
            Option<String>,
            i64,
            &'static str,
            &'static str,
        );
        let idle = || pane(Some(GEN), Some("idle"));
        let edited = format!("{PROMPT} and also call Hunter");
        let rows = vec![
            Row(
                "real %2 frame, owned pending prompt, never submitted -> submit once",
                ok_attempt(),
                frame(PROMPT, false),
                idle(),
                Some("earlier assistant reply only".into()),
                0,
                "submit_once",
                "owned_pending_prompt_unsubmitted",
            ),
            Row(
                "already in the lane's session -> refuse (would duplicate)",
                ok_attempt(),
                frame(PROMPT, false),
                idle(),
                Some(format!("... {PROMPT} ...")),
                0,
                "refuse",
                "already_submitted",
            ),
            Row(
                "human edited the text -> refuse",
                ok_attempt(),
                frame(&edited, false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "human_edited",
            ),
            Row(
                "stale attempt state -> refuse",
                attempt(PROMPT, "stale", Some(GEN)),
                frame(PROMPT, false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "attempt_state_not_recoverable",
            ),
            Row(
                "unknown attempt state -> refuse (fail closed)",
                attempt(PROMPT, "delivered-somewhere", Some(GEN)),
                frame(PROMPT, false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "attempt_state_not_recoverable",
            ),
            Row(
                "consumed attempt -> refuse",
                attempt(PROMPT, "consumed", Some(GEN)),
                frame(PROMPT, false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "attempt_consumed",
            ),
            Row(
                "pane replaced (wrong generation) -> refuse",
                ok_attempt(),
                frame(PROMPT, false),
                pane(Some("pid:999:start:1"), Some("idle")),
                Some("".into()),
                0,
                "refuse",
                "wrong_generation",
            ),
            Row(
                "generation unknown -> refuse",
                attempt(PROMPT, "unconfirmed", None),
                frame(PROMPT, false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "generation_unknown",
            ),
            Row(
                "pane generation unreadable -> refuse",
                ok_attempt(),
                frame(PROMPT, false),
                pane(None, Some("idle")),
                Some("".into()),
                0,
                "refuse",
                "generation_unknown",
            ),
            Row(
                "active turn -> refuse",
                ok_attempt(),
                frame(PROMPT, false),
                pane(Some(GEN), Some("working")),
                Some("".into()),
                0,
                "refuse",
                "mid_turn",
            ),
            Row(
                "unknown turn state -> refuse",
                ok_attempt(),
                frame(PROMPT, false),
                pane(Some(GEN), None),
                Some("".into()),
                0,
                "refuse",
                "turn_unknown",
            ),
            Row(
                "dialog open -> refuse",
                ok_attempt(),
                frame(PROMPT, true),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "dialog_open",
            ),
            Row(
                "native session unreadable -> refuse (unknown is not absent)",
                ok_attempt(),
                frame(PROMPT, false),
                idle(),
                None,
                0,
                "refuse",
                "native_session_unknown",
            ),
            Row(
                "recovery budget spent -> refuse",
                ok_attempt(),
                frame(PROMPT, false),
                idle(),
                Some("".into()),
                1,
                "refuse",
                "recovery_exhausted",
            ),
            Row(
                "frame shows nothing -> refuse",
                ok_attempt(),
                frame("", false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "frame_has_no_text",
            ),
            Row(
                "attempt with no text -> refuse",
                attempt("   ", "unconfirmed", Some(GEN)),
                frame(PROMPT, false),
                idle(),
                Some("".into()),
                0,
                "refuse",
                "attempt_has_no_text",
            ),
        ];
        for Row(name, a, f, p, session, done, action, reason) in rows {
            let got = plan(&a, &f, &p, session.as_deref(), done);
            assert_eq!(got["action"], action, "{name}: {got}");
            assert_eq!(got["reason"], reason, "{name}: {got}");
        }
    }

    #[test]
    fn a_positive_plan_names_its_attempt_and_generation() {
        let got = plan(
            &ok_attempt(),
            &frame(PROMPT, false),
            &pane(Some(GEN), Some("idle")),
            Some(""),
            0,
        );
        assert_eq!(got["attempt_id"], "a-1");
        assert_eq!(got["generation"], GEN);
        assert_eq!(got["pane_id"], "%2");
        assert!(got["record"]
            .as_str()
            .unwrap()
            .contains("resubmitting attempt a-1"));
        assert_eq!(got["record"], "pane %2: resubmitting attempt a-1 because the composer holds exactly its text, the attempt is unconfirmed, the generation is unchanged and the lane's session never received it");
    }

    #[test]
    fn geometry_is_not_consulted() {
        let same = frame(PROMPT, false);
        let mut other = same.clone();
        other["prompt_rows"] = json!(9);
        other["blank_rows"] = json!(0);
        let p = pane(Some(GEN), Some("idle"));
        let (a, b) = (
            plan(&ok_attempt(), &same, &p, Some(""), 0),
            plan(&ok_attempt(), &other, &p, Some(""), 0),
        );
        assert_eq!((&a["action"], &a["reason"]), (&b["action"], &b["reason"]));
    }

    #[test]
    fn wrapping_and_ellipsis_do_not_cause_a_false_human_edit() {
        let wrapped = format!("{}…", PROMPT.replacen(' ', "\n  ", 3));
        let got = plan(
            &ok_attempt(),
            &frame(&wrapped, false),
            &pane(Some(GEN), Some("idle")),
            Some(""),
            0,
        );
        assert_eq!(got["action"], "submit_once");
    }

    #[test]
    fn one_changed_word_is_a_human_edit() {
        let edited = PROMPT.replace("FIRST", "first");
        let got = plan(
            &ok_attempt(),
            &frame(&edited, false),
            &pane(Some(GEN), Some("idle")),
            Some(""),
            0,
        );
        assert_eq!(got["reason"], "human_edited");
        assert_eq!(got["visible_chars"], edited.chars().count());
        assert_eq!(got["attempt_chars"], PROMPT.chars().count());
    }

    #[test]
    fn budget_is_one_by_default() {
        let p = pane(Some(GEN), Some("idle"));
        assert_eq!(
            plan(&ok_attempt(), &frame(PROMPT, false), &p, Some(""), 0)["action"],
            "submit_once"
        );
        let again = plan(&ok_attempt(), &frame(PROMPT, false), &p, Some(""), 1);
        assert_eq!(again["reason"], "recovery_exhausted");
        assert_eq!(
            (again["done"].as_i64(), again["limit"].as_i64()),
            (Some(1), Some(1))
        );
        // a larger budget allows another submission
        assert_eq!(
            plan_recovery(&ok_attempt(), &frame(PROMPT, false), &p, Some(""), 1, 2).unwrap()
                ["action"],
            "submit_once"
        );
    }

    #[test]
    fn norm_matches_python_whitespace_and_ellipsis_rules() {
        assert_eq!(norm("  a\t\nb \u{a0} c…d  "), "a b c d");
        assert_eq!(
            norm("x\u{1c}y"),
            "x y",
            "U+001C is whitespace to Python's \\s"
        );
        assert_eq!(norm(""), "");
    }

    #[test]
    fn non_mappings_are_a_value_error_and_a_wrong_generation_reports_both_sides() {
        assert_eq!(
            plan_recovery(&json!([]), &json!({}), &json!({}), Some(""), 0, 1)
                .unwrap_err()
                .to_string(),
            "attempt, frame and pane must be mappings"
        );
        let got = plan(
            &ok_attempt(),
            &frame(PROMPT, false),
            &pane(Some("pid:9:start:1"), Some("idle")),
            Some(""),
            0,
        );
        assert_eq!(
            (got["attempt"].as_str(), got["current"].as_str()),
            (Some(GEN), Some("pid:9:start:1"))
        );
        let unknown = plan(
            &attempt(PROMPT, "weird", Some(GEN)),
            &frame(PROMPT, false),
            &pane(Some(GEN), Some("idle")),
            Some(""),
            0,
        );
        assert_eq!(unknown["state"], "weird");
    }
}
