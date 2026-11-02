//! Identity-only recovery session reader tests ported from the named wiring cases in Python.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    hooks: PathBuf,
    bindings: PathBuf,
    cwd: PathBuf,
    pane: Value,
    entries: Vec<PaneProcess>,
    environment: BTreeMap<String, String>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ds-recovery-native-")
            .tempdir()
            .unwrap();
        let home = dir.path().join("codex-home");
        let hooks = home.join("hooks");
        let cwd = dir.path().join("workspace");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
        std::fs::write(&rollout, "").unwrap();
        let db = Connection::open(home.join("state_5.sqlite")).unwrap();
        db.execute(
            "create table threads (id text primary key, rollout_path text, cwd text)",
            [],
        )
        .unwrap();
        db.execute(
            "insert into threads values (?,?,?)",
            (SESSION, rollout.to_str().unwrap(), cwd.to_str().unwrap()),
        )
        .unwrap();

        let pid = i64::from(std::process::id());
        let start = proc::process_start(pid).expect("the test process has /proc identity");
        let pane = json!({
            "pane_id": "w1:p2",
            "pane_pid": pid,
            "agent": "codex",
            "cwd": cwd,
        });
        let entries = vec![PaneProcess {
            pid,
            comm: "codex".into(),
            state: "S".into(),
            start: Some(start),
            argv: vec!["codex".into()],
        }];
        let bindings = dir.path().join("bindings.json");

        Self {
            _dir: dir,
            home: home.clone(),
            hooks: hooks.clone(),
            bindings,
            cwd,
            pane,
            entries,
            environment: BTreeMap::new(),
        }
    }

    fn pid(&self) -> i64 {
        self.entries[0].pid
    }

    fn start(&self) -> &str {
        self.entries[0].start.as_deref().unwrap()
    }

    fn binding_record(&self) -> Value {
        json!({
            "pid": self.pid(),
            "start": self.start(),
            "cwd": self.cwd,
            "codex_home": self.home,
            "session_id": SESSION,
        })
    }

    fn hook_record(&self) -> Value {
        json!({
            "session_id": SESSION,
            "pane_id": self.pane["pane_id"],
            "root_pid": self.pid(),
            "root_start": self.start(),
            "cwd": self.cwd,
        })
    }

    fn resolve(
        &self,
        binding_path: Option<&Path>,
    ) -> Result<PromptSessionBinding, NativeSessionError> {
        resolve_prompt_session_from(&self.pane, &self.entries, &self.environment, binding_path)
    }
}

#[test]
fn test_legacy_bare_codex_requires_generation_bound_registration() {
    let fixture = Fixture::new();
    let refusal = fixture.resolve(None).unwrap_err();
    assert!(refusal
        .to_string()
        .contains("no process-bound native session id"));

    std::fs::write(
        &fixture.bindings,
        json!({"w1:p2": fixture.binding_record()}).to_string(),
    )
    .unwrap();
    let session = fixture.resolve(Some(&fixture.bindings)).unwrap();
    assert_eq!(fixture.pid(), session.pid);
    assert_eq!(fixture.start(), session.start);
    assert_eq!(SESSION, session.session_id);
    assert_eq!(fixture.cwd.to_string_lossy(), session.cwd);

    let mut stale = fixture.binding_record();
    stale["start"] = json!("stale");
    std::fs::write(&fixture.bindings, json!({"w1:p2": stale}).to_string()).unwrap();
    assert!(fixture
        .resolve(Some(&fixture.bindings))
        .unwrap_err()
        .to_string()
        .contains("wrong process generation/cwd"));
}

#[test]
fn test_different_native_process_binding_not_adopted() {
    let fixture = Fixture::new();
    let mut record = fixture.binding_record();
    record["pid"] = json!(fixture.pid() + 1);
    std::fs::write(&fixture.bindings, json!({"w1:p2": record}).to_string()).unwrap();
    let error = fixture.resolve(Some(&fixture.bindings)).unwrap_err();
    assert!(error.to_string().contains("wrong process generation/cwd"));
}

#[test]
fn test_hook_session_bound_to_root_pid_start_and_pane() {
    let mut fixture = Fixture::new();
    fixture.environment.insert(
        CODEX_HOME.into(),
        fixture.home.to_string_lossy().into_owned(),
    );
    fixture.environment.insert(
        STATE_DIR.into(),
        fixture.hooks.to_string_lossy().into_owned(),
    );
    let hook = fixture.hooks.join(format!("{SESSION}.json"));
    std::fs::write(&hook, fixture.hook_record().to_string()).unwrap();

    let resolved = fixture.resolve(None).unwrap();
    assert_eq!(SESSION, resolved.session_id);
    assert_eq!(fixture.pid(), resolved.pid);

    let mut stale = fixture.hook_record();
    stale["root_start"] = json!("stale");
    std::fs::write(&hook, stale.to_string()).unwrap();
    assert!(fixture
        .resolve(None)
        .unwrap_err()
        .to_string()
        .contains("no unique process-bound native session id"));

    // An unrelated hook/session file is never adopted based on recency or filename.
    std::fs::write(
        fixture.hooks.join("newest.json"),
        json!({"session_id":"newest"}).to_string(),
    )
    .unwrap();
    assert!(fixture.resolve(None).is_err());
}
