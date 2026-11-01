//! Ported from `tests/test_dunk_http_terminals.py` (TerminalsRoute) and
//! `tests/test_dunk_http_resize.py` (ResizeRoute). Not ported here: the geometry-parsing and
//! real-tmux classes (`BackendGeometry`, `TmuxResize`), which exercise the terminal backend, not
//! this module.

use std::cell::RefCell;

use super::*;

fn pane(pane_id: &str, label: &str, agent: &str, cols: i64, rows: i64) -> Value {
    json!({"pane_id": pane_id, "label": label, "agent": agent, "cols": cols, "rows": rows,
        "workspace_label": "bootstrap", "terminal_title_stripped": label})
}

fn p(pane_id: &str, label: &str) -> Value {
    pane(pane_id, label, "pi", 80, 24)
}

struct FakeBackend {
    panes: Vec<Value>,
    capabilities: BTreeSet<String>,
    result: Result<Map<String, Value>, String>,
    calls: RefCell<Vec<(String, Value, Value)>>,
}

impl FakeBackend {
    fn new(panes: Vec<Value>) -> FakeBackend {
        FakeBackend {
            panes,
            capabilities: ["resize".to_string()].into(),
            result: Ok(Map::new()),
            calls: RefCell::new(Vec::new()),
        }
    }
    fn with_result(mut self, result: Result<Value, &str>) -> FakeBackend {
        self.result = result
            .map(|v| v.as_object().cloned().unwrap_or_default())
            .map_err(String::from);
        self
    }
    fn without_capabilities(mut self) -> FakeBackend {
        self.capabilities.clear();
        self
    }
}

impl Backend for FakeBackend {
    fn name(&self) -> String {
        "fake".into()
    }
    fn capabilities(&self) -> BTreeSet<String> {
        self.capabilities.clone()
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn resize(
        &self,
        pane_id: &str,
        cols: &Value,
        rows: &Value,
    ) -> Result<Map<String, Value>, String> {
        self.calls
            .borrow_mut()
            .push((pane_id.into(), cols.clone(), rows.clone()));
        self.result.clone()
    }
}

struct FakeDaemon {
    panes: Vec<Value>,
    error: Option<DaemonError>,
    commands: RefCell<Vec<(String, Value)>>,
    reply: Value,
}

impl FakeDaemon {
    fn new(panes: Vec<Value>) -> FakeDaemon {
        FakeDaemon {
            panes,
            error: None,
            commands: RefCell::new(Vec::new()),
            reply: json!({"ok": true}),
        }
    }
    fn failing(error: DaemonError) -> FakeDaemon {
        FakeDaemon {
            error: Some(error),
            ..FakeDaemon::new(vec![])
        }
    }
}

impl Daemon for FakeDaemon {
    fn call(&self, command: &str, args: &Value) -> Result<Value, DaemonError> {
        self.commands
            .borrow_mut()
            .push((command.into(), args.clone()));
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if command == "list_panes" {
            return Ok(json!({"panes": self.panes}));
        }
        Ok(self.reply.clone())
    }
}

fn get(path: &str, backend: &FakeBackend, daemon: &FakeDaemon) -> Result<Value, ApiError> {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    route(path, &parse_query(query), backend, daemon)
}

fn post(path: &str, payload: Value, backend: &FakeBackend) -> Result<Value, ApiError> {
    route_post(path, &payload, backend, "2026-10-31T00:00:00Z")
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect()
}

// ---- TerminalsRoute ----

#[test]
fn contract_fields_come_from_the_pane_and_the_daemon_status() {
    let backend = FakeBackend::new(vec![pane("%1", "cos", "pi", 100, 30)]);
    let daemon = FakeDaemon::new(vec![json!({"pane_id": "%1", "agent_status": "working"})]);
    let result = get("/api/terminals", &backend, &daemon).unwrap();
    let items = result.as_array().expect("a bare array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    for key in ["name", "agent", "state", "cols", "rows"] {
        assert!(item.get(key).is_some(), "{key}");
    }
    assert_eq!(
        (
            item["name"].as_str(),
            item["agent"].as_str(),
            item["state"].as_str()
        ),
        (Some("cos"), Some("pi"), Some("working"))
    );
    assert_eq!(
        (item["cols"].as_i64(), item["rows"].as_i64()),
        (Some(100), Some(30))
    );
    assert!(item.get("state_reason").is_none());
    assert_eq!(item["pane_id"], "%1");
}

#[test]
fn duplicate_label_gets_a_stable_unique_suffix() {
    // 'opencode-ds41-b' really is on two panes; both must stay addressable.
    let backend = FakeBackend::new(vec![
        p("%28", "opencode-ds41-b"),
        p("%41", "opencode-ds41-b"),
    ]);
    let daemon = FakeDaemon::new(vec![]);
    let first = names(&get("/api/terminals", &backend, &daemon).unwrap());
    assert_eq!(first, ["opencode-ds41-b", "opencode-ds41-b-2"]);
    // Same input, same names: the suffix is not random.
    assert_eq!(
        first,
        names(&get("/api/terminals", &backend, &daemon).unwrap())
    );
}

#[test]
fn suffix_cannot_collide_with_a_real_label() {
    // A pane literally named 'x-2' must not be overwritten by the second 'x'.
    let backend = FakeBackend::new(vec![p("%1", "x-2"), p("%2", "x"), p("%3", "x")]);
    let got = names(&get("/api/terminals", &backend, &FakeDaemon::new(vec![])).unwrap());
    assert_eq!(got.iter().collect::<BTreeSet<_>>().len(), got.len());
    assert!(got.contains(&"x-2".to_string()));
    assert_eq!(got.len(), 3);
}

#[test]
fn unknown_state_is_null_with_a_reason_and_a_dead_daemon_does_not_empty_the_list() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]);
    let daemon = FakeDaemon::failing(DaemonError::Failed("socket closed".into()));
    let item = get("/api/terminals", &backend, &daemon).unwrap()[0].clone();
    assert_eq!(item["name"], "cos");
    assert!(item["state"].is_null());
    assert!(item["state_reason"]
        .as_str()
        .unwrap()
        .contains("socket closed"));
    assert_eq!(item["state_reason"], "daemon call failed: socket closed");
}

#[test]
fn unnamed_shell_pane_reports_no_agent_rather_than_an_empty_string() {
    let backend = FakeBackend::new(vec![pane("%7", "bash", "", 80, 24)]);
    let daemon = FakeDaemon::new(vec![
        json!({"pane_id": "%7", "agent_status": null, "agent_status_reason": "no turn recorded"}),
    ]);
    let item = get("/api/terminals", &backend, &daemon).unwrap()[0].clone();
    assert!(item["agent"].is_null());
    assert!(item["state"].is_null());
    assert_eq!(item["state_reason"], "no turn recorded");
    // and with neither a status row nor an outage the reason is still stated
    let bare = get("/api/terminals", &backend, &FakeDaemon::new(vec![])).unwrap()[0].clone();
    assert_eq!(bare["state_reason"], "no activity source for this pane");
}

// ---- ResizeRoute ----

#[test]
fn success_reports_the_measured_size_not_the_request() {
    let backend = FakeBackend::new(vec![pane("%28", "cos", "pi", 80, 24)]).with_result(Ok(json!({
        "pane_id": "%28", "requested": {"cols": 100, "rows": 30}, "cols": 99, "rows": 29,
        "clients_attached": 0, "overridable_by_client": false, "window_pane_count": 2,
        "resized_panes": [{"pane_id": "%28", "cols": 99, "rows": 29, "cols_before": 80, "rows_before": 24}]})));
    let out = post(
        "/api/terminals/cos/resize",
        json!({"cols": 100, "rows": 30}),
        &backend,
    )
    .unwrap();
    assert_eq!(out["pane_id"], "%28");
    assert_eq!(out["cols"], 99, "must report tmux's size, not the request");
    assert_eq!(out["rows"], 29);
    assert!(out.get("as_of").is_some());
    assert_eq!(out["window_pane_count"], 2);
    assert_eq!(out["resized_panes"][0]["pane_id"], "%28");
    assert_eq!(out["name"], "cos");
    assert_eq!(
        *backend.calls.borrow(),
        [("%28".to_string(), json!(100), json!(30))]
    );
}

#[test]
fn a_collision_name_addresses_the_pane_the_get_list_named() {
    // 'x-2' is the suffix GET gave the second pane called 'x'; POST must hit it.
    let backend = FakeBackend::new(vec![p("%1", "x"), p("%2", "x"), p("%3", "x-2")])
        .with_result(Ok(json!({"pane_id": "%2", "cols": 90, "rows": 30, "clients_attached": 0, "overridable_by_client": false})));
    // Names are: x, x-2 (collision suffix), x-2-2 (the literal 'x-2' pane).
    let got: Vec<String> = unique_names(&backend.panes)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(got, ["x", "x-2", "x-2-2"]);
    post(
        "/api/terminals/x-2/resize",
        json!({"cols": 90, "rows": 30}),
        &backend,
    )
    .unwrap();
    assert_eq!(backend.calls.borrow()[0].0, "%2");
}

#[test]
fn unknown_name_is_404_and_nothing_is_resized() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]);
    let error = post(
        "/api/terminals/nope/resize",
        json!({"cols": 90, "rows": 30}),
        &backend,
    )
    .unwrap_err();
    assert_eq!(error.status, 404);
    assert_eq!(error.message, "no terminal named 'nope'");
    assert!(backend.calls.borrow().is_empty());
}

#[test]
fn backend_refusal_is_400_carrying_tmux_wording() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]).with_result(Err("width too small"));
    let error = post(
        "/api/terminals/cos/resize",
        json!({"cols": 0, "rows": 30}),
        &backend,
    )
    .unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.to_string().contains("width too small"));
    // ... and it names what was refused, so a caller knows WHICH request failed.
    assert_eq!(error.context["name"], "cos");
    assert_eq!(error.context["requested"], json!({"cols": 0, "rows": 30}));
}

#[test]
fn a_refusal_body_carries_no_geometry_at_all() {
    // A refusal must not LOOK like an applied resize: {error, name, pane_id, requested} only.
    let backend = FakeBackend::new(vec![p("%1", "cos")])
        .with_result(Err("cols and rows must be between 1 and 1000"));
    let error = post(
        "/api/terminals/cos/resize",
        json!({"cols": 0, "rows": 24}),
        &backend,
    )
    .unwrap_err();
    let body = error_body(&error);
    let keys: BTreeSet<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["error", "name", "pane_id", "requested"].into());
    for key in [
        "cols",
        "rows",
        "window_pane_count",
        "resized_panes",
        "as_of",
    ] {
        assert!(body.get(key).is_none(), "{key}");
    }
    assert_eq!(body["requested"], json!({"cols": 0, "rows": 24}));
}

#[test]
fn the_error_body_helper_is_the_single_shape() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]);
    let error = post(
        "/api/terminals/nope/resize",
        json!({"cols": 80, "rows": 24}),
        &backend,
    )
    .unwrap_err();
    let body = error_body(&error);
    assert!(body.get("error").is_some() && body.get("cols").is_none());
    assert_eq!(body["error"], "no terminal named 'nope'");
}

#[test]
fn backend_without_the_capability_is_501() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]).without_capabilities();
    let error = post(
        "/api/terminals/cos/resize",
        json!({"cols": 90, "rows": 30}),
        &backend,
    )
    .unwrap_err();
    assert_eq!(error.status, 501);
    assert_eq!(error.message, "backend 'fake' cannot resize panes");
}

#[test]
fn missing_fields_and_a_non_object_body_are_400() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]);
    for body in [
        json!({"cols": 90}),
        json!({"rows": 24}),
        json!([]),
        json!("90x24"),
    ] {
        let error = post("/api/terminals/cos/resize", body.clone(), &backend).unwrap_err();
        assert_eq!(error.status, 400, "{body}");
    }
    assert_eq!(
        post("/api/terminals/cos/resize", json!({"cols": 90}), &backend)
            .unwrap_err()
            .message,
        "missing rows"
    );
    assert_eq!(
        post("/api/terminals/cos/resize", json!({}), &backend)
            .unwrap_err()
            .message,
        "missing cols, rows"
    );
    assert_eq!(
        post("/api/terminals/cos/resize", json!([]), &backend)
            .unwrap_err()
            .message,
        "body must be a JSON object with cols and rows"
    );
}

#[test]
fn an_unknown_write_path_is_404_not_a_silent_get() {
    let backend = FakeBackend::new(vec![p("%1", "cos")]);
    for path in ["/api/terminals/cos/kill", "/api/terminals", "/api/roster"] {
        let error = post(path, json!({}), &backend).unwrap_err();
        assert_eq!(error.status, 404, "{path}");
    }
    // a name-less resize path is a 400, and the slice cannot panic on the short path
    assert_eq!(
        post("/api/terminals/resize", json!({}), &backend)
            .unwrap_err()
            .status,
        400
    );
    assert_eq!(
        post("/api/terminals//resize", json!({}), &backend)
            .unwrap_err()
            .status,
        400
    );
}

#[test]
fn percent_encoded_name_resolves() {
    let backend = FakeBackend::new(vec![p("%7", "π - vp")])
        .with_result(Ok(json!({"pane_id": "%7", "cols": 70, "rows": 20, "clients_attached": 0, "overridable_by_client": false})));
    post(
        "/api/terminals/%CF%80%20-%20vp/resize",
        json!({"cols": 70, "rows": 20}),
        &backend,
    )
    .unwrap();
    assert_eq!(backend.calls.borrow()[0].0, "%7");
}

// ---- the GET routes the Python has no test for (written from the Python) ----

#[test]
fn the_get_routes_call_the_daemon_with_the_pythons_arguments() {
    let backend = FakeBackend::new(vec![]);
    let daemon = FakeDaemon::new(vec![]);
    assert_eq!(
        get("/healthz", &backend, &daemon).unwrap(),
        json!({"ok": true, "service": "dunkingsheep-api"})
    );
    assert_eq!(get("/health", &backend, &daemon).unwrap()["ok"], true);
    get("/api/roster", &backend, &daemon).unwrap();
    get("/api/messages/state?ids=i1,i2", &backend, &daemon).unwrap();
    get("/api/reply-stats", &backend, &daemon).unwrap();
    get("/api/reply-stats?mailboxes=a,b", &backend, &daemon).unwrap();
    get("/api/projects/global/", &backend, &daemon).unwrap();
    let sent = daemon.commands.borrow().clone();
    assert_eq!(
        sent,
        vec![
            ("roster".to_string(), json!({})),
            ("message_state".to_string(), json!({"message_ids": "i1,i2"})),
            ("reply_stats".to_string(), json!({})),
            ("reply_stats".to_string(), json!({"mailboxes": "a,b"})),
            ("project_summary".to_string(), json!({"project": "global"})),
        ]
    );
}

#[test]
fn get_refusals_are_400_404_and_503_with_the_pythons_texts() {
    let backend = FakeBackend::new(vec![]);
    let daemon = FakeDaemon::new(vec![]);
    let err = |path: &str| get(path, &backend, &daemon).unwrap_err();
    assert_eq!(
        (
            err("/api/messages/state").status,
            err("/api/messages/state").message.as_str()
        ),
        (400, "ids is required, e.g. ?ids=i1,i2")
    );
    assert_eq!(err("/api/messages/state?ids=%20").status, 400);
    assert_eq!(
        (
            err("/api/projects/").status,
            err("/api/projects/").message.as_str()
        ),
        (404, "project name required")
    );
    assert_eq!(
        (err("/nope").status, err("/nope").message.as_str()),
        (404, "no route /nope")
    );
    let down = FakeDaemon::failing(DaemonError::Unavailable("no socket".into()));
    let e = get("/api/roster", &backend, &down).unwrap_err();
    assert_eq!(
        (e.status, e.message.as_str()),
        (503, "Dunking Sheep unavailable: no socket")
    );
    let slow = FakeDaemon::failing(DaemonError::Failed("timed out".into()));
    assert_eq!(
        get("/api/roster", &backend, &slow).unwrap_err().message,
        "daemon call failed: timed out"
    );
}

#[test]
fn query_and_path_decoding_follow_the_stdlib() {
    let q = parse_query("ids=i1%2Ci2&ids=x&mailboxes=a+b&blank=&flag");
    assert_eq!(q["ids"], ["i1,i2", "x"]);
    assert_eq!(q["mailboxes"], ["a b"]);
    assert!(
        !q.contains_key("blank") && !q.contains_key("flag"),
        "blank values are dropped"
    );
    assert_eq!(unquote("%CF%80%20-%20vp"), "π - vp");
    assert_eq!(unquote("100%"), "100%");
    assert_eq!(unquote("%zz%4"), "%zz%4");
    assert_eq!(unquote("a%41"), "aA");
    assert_eq!(now_iso().len(), 20);
}
