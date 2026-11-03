//! Tests for `api.rs`, ported from tests/test_command_smoke.py (the gate-order cases) plus the
//! `dispatch` gate sequence in dunk_api.py. The allowlist tests of
//! tests/test_dunk_dispatch_allowlist.py exercise `_dispatch_allowlist`, not `dispatch`, and
//! belong to the dispatch-command slice.

use super::*;
use serde_json::json;
use std::cell::Cell;
use std::sync::Mutex;

use crate::config::Env;
use crate::flock::commands_turns::{TurnHooks, TurnState};
use crate::flock::dunks::DunkList;
use crate::flock::panes::{PaneInfoEnv, Stores};
use crate::flock::status::{StatusEnv, ViewCache};
use crate::proc::Caller;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct FixedRole(&'static str, bool);

impl CallerGate for FixedRole {
    fn caller_role(&self, _pane: Option<&str>) -> Result<String, DunkError> {
        Ok(self.0.to_string())
    }
    fn require_superuser(&self, _pane: Option<&str>, name: &str) -> Result<(), DunkError> {
        if self.1 {
            Ok(())
        } else {
            Err(DunkError::Refused(format!("{name} needs a superuser")))
        }
    }
}

struct TestEnv(Mutex<Box<dyn Terminal>>);

impl Default for TestEnv {
    fn default() -> Self {
        Self(Mutex::new(Box::new(FakeTerminal::default())))
    }
}

impl PaneInfoEnv for TestEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for TestEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.0
    }
    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }
}

struct NoNetworkGithubTransport;

impl crate::github::GitHubTransport for NoNetworkGithubTransport {
    fn send<'a>(
        &'a self,
        _request: crate::github::GitHubRequest,
    ) -> crate::github::PinBoxFuture<'a, Result<crate::github::GitHubResponse, String>> {
        Box::pin(async { Err("unexpected GitHub request with a seeded token cache".into()) })
    }
}

struct GithubTokenApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    tokens: crate::github::Tokens,
    now_s: f64,
}

impl PaneInfoEnv for GithubTokenApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for GithubTokenApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        self.now_s
    }

    fn github_tokens(&self) -> Result<&crate::github::Tokens, DunkError> {
        Ok(&self.tokens)
    }
}

#[derive(Default)]
struct PoolradApiStore {
    tasks: Mutex<Vec<Value>>,
}

impl crate::poolrad::PoolradTaskStore for PoolradApiStore {
    fn list(
        &self,
        _scope: Option<&str>,
        _include_done: bool,
        _limit: usize,
        _kind: &str,
    ) -> Result<Vec<Value>, String> {
        Ok(self.tasks.lock().unwrap().clone())
    }

    fn add(
        &self,
        _scope: &str,
        title: &str,
        body: &str,
        priority: &str,
        kind: &str,
        created_by: &str,
    ) -> Result<Value, String> {
        let mut tasks = self.tasks.lock().unwrap();
        let task = json!({
            "id": format!("t{}", tasks.len() + 1),
            "title": title,
            "body": body,
            "priority": priority,
            "kind": kind,
            "created_by": created_by,
            "status": "open"
        });
        tasks.push(task.clone());
        Ok(task)
    }
}

struct PoolradApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    sync_repo: tempfile::TempDir,
    progress_repo: tempfile::TempDir,
    queue_path: PathBuf,
    credit_path: PathBuf,
    store: PoolradApiStore,
}

impl PoolradApiEnv {
    fn new() -> Self {
        let sync_repo = tempfile::tempdir().unwrap();
        let progress_repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(progress_repo.path().join("docs/re")).unwrap();
        std::fs::write(
            progress_repo
                .path()
                .join("docs/re/strict_full_c_addresses.csv"),
            "id,name\nOVL_0001_0001,fixture\n",
        )
        .unwrap();
        std::fs::write(
            progress_repo
                .path()
                .join("docs/re/reviewed_function_identities.csv"),
            "id,name\nOVL_0001_0001,fixture\nOVL_0001_0002,fixture\n",
        )
        .unwrap();
        let queue_path = progress_repo.path().join("strict-queue.json");
        std::fs::write(
            &queue_path,
            r#"{"summary":{"composable_bodies":7,"candidates":4}}"#,
        )
        .unwrap();
        let credit_path = progress_repo.path().join("credit-pending.json");
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            sync_repo,
            progress_repo,
            queue_path,
            credit_path,
            store: PoolradApiStore::default(),
        }
    }

    fn sync_rows() -> Value {
        json!({
            "rows": (1..=7).map(|index| json!({
                "id": format!("OVL_0001_{index:04X}"),
                "rank": index,
                "score": 50,
                "bytes": 100,
                "grade": "A",
                "unit": 1,
                "offset": index,
                "ovr_offset": index,
                "instructions": 40,
                "behaviour": "leaf",
                "classification": "game_evidenced",
                "confidence": 0.9,
                "aliases": [format!("0x1:0x{index:X}")],
                "prior_translation": null,
                "callee_readiness": {"composable": true, "ready": 2, "counts": {"strict": 2}}
            })).collect::<Vec<_>>()
        })
    }
}

impl PaneInfoEnv for PoolradApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for PoolradApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }

    fn poolrad_sync(&self, limit: usize, dry_run: bool) -> Result<Value, DunkError> {
        let rows = Self::sync_rows();
        let exporter = |_: &Path| Ok(rows.clone());
        crate::poolrad::PoolradSync::new(&self.store)
            .with_repo(self.sync_repo.path())
            .with_exporter(&exporter)
            .with_credit_path(self.sync_repo.path().join("credit-pending.json"))
            .run_with(limit, dry_run, "medium")
            .map_err(DunkError::Refused)
    }

    fn poolrad_progress(&self) -> Result<Value, DunkError> {
        crate::poolrad::progress(
            Some(self.progress_repo.path()),
            Some(&self.queue_path),
            Some(&self.credit_path),
        )
        .map_err(DunkError::Refused)
    }
}
struct CompletionEnv(TestEnv);

impl PaneInfoEnv for CompletionEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        Some(json!({
            "pane_id": pane_id,
            "pane_pid": std::process::id(),
        }))
    }
}

impl CommandEnv for CompletionEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        self.0.terminal()
    }
    fn now_s(&self) -> f64 {
        self.0.now_s()
    }
}

struct ParityEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    dunks: DunkList,
    view_cache: ViewCache,
    now_s: f64,
    pane_pid: Option<i64>,
    inbox_value: Cell<i64>,
    probes: Cell<u32>,
    turn_calls: Mutex<Vec<String>>,
}

impl ParityEnv {
    fn new(pane_pid: Option<i64>) -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            dunks: DunkList::new(),
            view_cache: ViewCache::new(),
            now_s: 1_800_000_100.0,
            pane_pid,
            inbox_value: Cell::new(1),
            probes: Cell::new(0),
            turn_calls: Mutex::new(Vec::new()),
        }
    }
}

impl PaneInfoEnv for ParityEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.pane_pid
            .map(|pid| json!({"pane_id": pane_id, "pane_pid": pid}))
    }
}

impl StatusEnv for ParityEnv {
    fn version(&self) -> String {
        "2.99.0".into()
    }
    fn federated_input(&self) -> Value {
        json!(false)
    }
    fn code_status(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            ("code_hash".into(), json!("abc")),
            ("code_stale".into(), json!(false)),
        ])
    }
    fn dunks(&self) -> &DunkList {
        &self.dunks
    }
    fn started_at(&self) -> f64 {
        1_800_000_000.0
    }
    fn pid(&self) -> u32 {
        42
    }
    fn now(&self) -> f64 {
        self.now_s
    }
    fn queued_messages(&self) -> usize {
        3
    }
    fn inbox_unread(&self) -> Result<i64, DunkError> {
        Ok(7)
    }
    fn task_totals(&self) -> Result<Value, DunkError> {
        Ok(json!({"open": 2}))
    }
    fn task_followers(&self) -> usize {
        1
    }
    fn quarantined_mailboxes(&self) -> Option<Value> {
        None
    }
    fn last_message(&self) -> String {
        "idle".into()
    }
    fn herdr_available(&self) -> bool {
        self.probes.set(self.probes.get() + 1);
        true
    }
    fn herdr_hint(&self) -> String {
        "start herdr".into()
    }
    fn state_file(&self) -> Option<String> {
        Some("/tmp/dunks.json".into())
    }
    fn inbox_count(&self, pane: &str) -> Result<Value, String> {
        Ok(json!({"mailbox": pane, "unread": self.inbox_value.get()}))
    }
}

impl TurnHooks for ParityEnv {
    fn record(
        &self,
        kind: &str,
        pane_id: &str,
        _: Option<&str>,
        _: Option<&str>,
        outcome: &str,
        _: f64,
        _: Option<i64>,
    ) {
        self.turn_calls
            .lock()
            .unwrap()
            .push(format!("record {kind} {pane_id} {outcome}"));
    }
    fn observe_turn(&self, pane_id: &str, state: &TurnState) {
        self.turn_calls
            .lock()
            .unwrap()
            .push(format!("observe {pane_id} {}", state.turn_id.len()));
    }
    fn dispatch_on_turn_end(&self, pane_id: &str) -> Value {
        json!({"trigger": "turn.end", "action": "skip", "reason": pane_id, "delivered": false})
    }
}

impl CommandEnv for ParityEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }
    fn now_s(&self) -> f64 {
        self.now_s
    }
    fn status_env(&self) -> Result<&dyn StatusEnv, DunkError> {
        Ok(self)
    }
    fn view_cache(&self) -> Result<&ViewCache, DunkError> {
        Ok(&self.view_cache)
    }
    fn turn_hooks(&self) -> Result<&dyn TurnHooks, DunkError> {
        Ok(self)
    }
}

fn dispatch<G: CallerGate + ?Sized>(
    dispatcher: &Dispatcher,
    flock: &Flock,
    gate: &G,
    name: &str,
    args: Option<Args>,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: self_pane_id.map(str::to_owned),
        ..RequestCtx::default()
    };
    dispatch_with_env(dispatcher, flock, gate, name, args, &request, &env)
}

fn dispatch_with_env<G: CallerGate + ?Sized>(
    dispatcher: &Dispatcher,
    flock: &Flock,
    gate: &G,
    name: &str,
    args: Option<Args>,
    request: &RequestCtx,
    env: &dyn CommandEnv,
) -> Result<Value, DunkError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock,
        request,
        env,
        runtime: runtime.handle(),
    };
    dispatcher.dispatch(&context, gate, name, args)
}

fn flock() -> (tempfile::TempDir, Flock) {
    let dir = tempfile::tempdir().expect("tempdir");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    (dir, flock)
}

fn staffed_flock(tag: &str) -> (tempfile::TempDir, Flock) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    roles
        .register("w1:p1", "owner", None, None, None, None, None, None)
        .unwrap();
    roles
        .register("w2:p2", "worker", None, None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    (dir, flock)
}

fn github_token_flock(tag: &str) -> (tempfile::TempDir, Flock) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    for (pane, role) in [
        ("w1:p1", "worker"),
        ("w1:p2", "reviewer"),
        ("w1:p3", "owner"),
    ] {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .unwrap();
    }
    flock.attach_roles(roles, None);
    (dir, flock)
}

fn github_token_api_env(dir: &Path) -> GithubTokenApiEnv {
    github_token_api_env_with_author_entry(
        dir,
        json!({
            "token": "fake-author-token",
            "expires_at": 4_102_444_800.0,
            "slug": "author-app"
        }),
    )
}

fn github_token_api_env_with_author_entry(dir: &Path, author_entry: Value) -> GithubTokenApiEnv {
    let config_dir = dir.join("github-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let cache_path = dir.join("github-tokens.json");
    std::fs::write(
        &cache_path,
        serde_json::to_vec(&json!({
            "tokens": {
                "author": author_entry,
                "reviewer": {
                    "token": "fake-reviewer-token",
                    "expires_at": 4_102_444_800.0,
                    "slug": "reviewer-app"
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    GithubTokenApiEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        tokens: crate::github::Tokens::new(
            Some(cache_path),
            config_dir,
            Box::new(NoNetworkGithubTransport),
            Some("https://github.invalid".into()),
        ),
        now_s: 1_800_000_000.0,
    }
}

fn dispatch_as_flock_role(
    dispatcher: &Dispatcher,
    flock: &Flock,
    env: &dyn CommandEnv,
    pane: Option<&str>,
    name: &str,
    args: Option<Args>,
) -> Result<Value, DunkError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let request = RequestCtx {
        self_pane_id: pane.map(str::to_owned),
        ..RequestCtx::default()
    };
    let context = HandlerContext {
        flock,
        request: &request,
        env,
        runtime: runtime.handle(),
    };
    dispatcher.dispatch(&context, &FlockGate(&context), name, args)
}

#[test]
fn h_lanes_matches_python() {
    let (_dir, flock) = staffed_flock("api-lanes");
    let env = ParityEnv::new(Some(std::process::id() as i64));
    let mut registry = serde_json::Map::new();
    registry.insert("w2:p2".into(), json!({"mailbox":"worker", "declared":true}));
    registry.insert("w1:p1".into(), json!("owner"));
    flock.write_lane_registry(&registry);

    let mut dispatcher = Dispatcher::new();
    dispatcher.register_lanes();
    let listed =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w1:p1"), "lanes", None).unwrap();
    assert_eq!(listed["count"], 2);
    assert_eq!(
        listed["lanes"][0],
        json!({
            "pane_id":"w1:p1", "mailbox":"owner", "declared":false,
            "alive":null, "dead_since":null
        })
    );
    assert_eq!(listed["lanes"][1]["pane_id"], "w2:p2");
    assert_eq!(
        dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "lanes",
            args(json!({"action":" LIST "})),
        )
        .unwrap(),
        listed
    );
    assert_eq!(
        refusal(dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "lanes",
            args(json!({"action":"remove"})),
        )),
        "lanes action must be list or set"
    );

    let set = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lanes",
        args(json!({"action":"set", "pane":"w1:p1", "mailbox":"owner-renamed"})),
    )
    .unwrap();
    assert_eq!(
        set,
        json!({"pane_id":"w1:p1", "mailbox":"owner-renamed", "declared":true})
    );
}

#[test]
fn h_lane_set_invalidates_cached_status_view() {
    let (_dir, flock) = staffed_flock("api-lane-set");
    let env = ParityEnv::new(Some(std::process::id() as i64));
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_lanes();

    let before = flock.view_cache().view(&env, Some("w1:p1"), false, 5.0);
    assert_eq!(before["inbox_count"]["unread"], 1);
    env.inbox_value.set(2);
    let updated = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lane_set",
        args(json!({"pane":" w1:p1 ", "mailbox":" owner "})),
    )
    .unwrap();
    assert_eq!(
        updated,
        json!({"pane_id":"w1:p1", "mailbox":"owner", "declared":true})
    );
    let after = flock.view_cache().view(&env, Some("w1:p1"), false, 5.0);
    assert_eq!(after["inbox_count"]["unread"], 2);

    env.inbox_value.set(3);
    assert!(dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lane_set",
        args(json!({"pane":"missing", "mailbox":"bad"})),
    )
    .is_err());
    let after_refusal = flock.view_cache().view(&env, Some("w1:p1"), false, 5.0);
    assert_eq!(after_refusal["inbox_count"]["unread"], 2);

    assert!(refusal(dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "lane_set",
        args(json!({"pane":"w1:p1", "mailbox":"worker-claim"})),
    ))
    .contains("requires a superuser"));
}

#[test]
fn h_lane_register_matches_python() {
    let (_dir, flock) = staffed_flock("api-lane-register");
    let env = ParityEnv::new(Some(std::process::id() as i64));
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_lanes();

    let registered = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lane_register",
        args(json!({
            "pane":"w2:p2", "role":"reviewer", "provider":"codex",
            "model":"gpt-6.1-sol", "label":"api-review"
        })),
    )
    .unwrap();
    assert_eq!(registered["pane_id"], "w2:p2");
    assert_eq!(registered["role"], "reviewer");
    assert_eq!(registered["provider"], "codex");
    assert_eq!(registered["model"], "gpt-6.1-sol");
    assert_eq!(registered["label"], "api-review");

    let refused = refusal(dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "lane_register",
        args(json!({"pane":"w1:p1", "role":"worker"})),
    ));
    assert!(
        refused.contains("lane register requires the owner role"),
        "{refused}"
    );
}

#[test]
fn h_lane_unregister_matches_python() {
    let (_dir, flock) = staffed_flock("api-lane-unregister");
    let env = ParityEnv::new(Some(std::process::id() as i64));
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_lanes();

    let removed = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lane_unregister",
        args(json!({"pane":"w2:p2"})),
    )
    .unwrap();
    assert_eq!(removed["pane_id"], "w2:p2");
    assert_eq!(removed["removed"]["role"], "worker");
    assert_eq!(
        flock.caller_role(Some("w2:p2"), &env).unwrap(),
        crate::roles::DEFAULT_ROLE
    );
}

#[test]
fn h_lane_handlers_refuse_non_string_pane_values() {
    let (_dir, flock) = staffed_flock("api-lane-pane-types");
    let env = ParityEnv::new(Some(std::process::id() as i64));
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_lanes();

    let register_error = refusal(dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lane_register",
        args(json!({"pane":123, "role":"worker"})),
    ));
    assert!(register_error.contains("pane must be a string"));
    assert_eq!(
        flock.caller_role(Some("w1:p1"), &env).unwrap(),
        "owner",
        "invalid pane input must not fall back to registering the caller"
    );
    assert_eq!(
        flock.caller_role(Some("123"), &env).unwrap(),
        crate::roles::DEFAULT_ROLE,
        "invalid pane input must not register a coerced target"
    );

    let unregister_error = refusal(dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "lane_unregister",
        args(json!({"pane":123})),
    ));
    assert!(unregister_error.contains("pane must be a string"));
    assert_eq!(
        flock.caller_role(Some("w2:p2"), &env).unwrap(),
        "worker",
        "invalid pane input must not remove a role"
    );
}

static CONTEXT_LEDGER_TEST_LOCK: Mutex<()> = Mutex::new(());

struct ContextLedgerEnv(Option<std::ffi::OsString>);

impl ContextLedgerEnv {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os("DUNKINGSHEEP_CONTEXT_LEDGER");
        std::env::set_var("DUNKINGSHEEP_CONTEXT_LEDGER", path);
        Self(previous)
    }
}

impl Drop for ContextLedgerEnv {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            std::env::set_var("DUNKINGSHEEP_CONTEXT_LEDGER", previous);
        } else {
            std::env::remove_var("DUNKINGSHEEP_CONTEXT_LEDGER");
        }
    }
}

fn context_flock(root: &Path) -> Flock {
    let flock = Flock::new(root.join("launches.json"), root.join("lanes.json"), false);
    let mut roles = Roles::new(
        &crate::config::Env::default(),
        Some(root.join("roles.json")),
        Some(root.join("tokens.json")),
        None,
    )
    .expect("roles");
    roles
        .register("w1:p1", "owner", None, None, None, None, None, None)
        .expect("owner");
    roles
        .register("w1:p2", "worker", None, None, None, None, None, None)
        .expect("worker");
    flock.attach_roles(roles, None);
    flock
}

fn seed_context_entry(root: &Path, cls: &str, authority: &str) -> crate::context::store::Added {
    let store = crate::context::store::Store::new(Some(root), false);
    let mut ledger = crate::context::ledger::Ledger::new(Some(&root.join("ledger.db")));
    store
        .add(
            "old context body",
            &crate::context::store::AddArgs {
                cls,
                subject: "api-leaf",
                type_: "measurement",
                authority: Some(authority),
                source: "api test",
                stated: Some("2026-11-01"),
                recheck_after: None,
                supersedes: None,
                slug: Some("api-leaf-entry"),
                by: Some("test"),
            },
            Some(&mut ledger),
        )
        .expect("seed entry")
}

fn dispatch_context_command(
    dispatcher: &Dispatcher,
    flock: &Flock,
    env: &TestEnv,
    role: &'static str,
    pane: &'static str,
    name: &str,
    input: Value,
) -> Result<Value, DunkError> {
    let request = RequestCtx {
        self_pane_id: Some(pane.to_string()),
        ..RequestCtx::default()
    };
    dispatch_with_env(
        dispatcher,
        flock,
        &FixedRole(role, role == "owner"),
        name,
        args(input),
        &request,
        env,
    )
}

fn refusal(result: Result<Value, DunkError>) -> String {
    match result {
        Err(DunkError::Refused(text)) => text,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn h_help_matches_python() {
    let dir = tempfile::tempdir().expect("tempdir");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("role registry");
    flock.attach_roles(roles, None);
    let env = ParityEnv::new(None);
    let request = RequestCtx::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_help();
    let got = dispatcher
        .dispatch(&context, &gate, "help", args(json!({"markdown": "yes"})))
        .expect("help");

    assert_eq!(got["guide"], crate::help::guide(true, "2.99.0"));
    assert_eq!(got["herdr_help"], Value::Null);
    assert_eq!(
        got["protocol"],
        json!({
            "transport": "unix socket, newline-delimited JSON",
            "request": {"id": "any", "cmd": "<command name>", "args": {},
                        "self_pane_id": "optional pane id that 'self' resolves to"},
            "response": {"id": "echoed", "ok": true, "result": {}},
            "error": {"id": "echoed", "ok": false, "error": "message"},
        })
    );
    let expected_commands: Vec<Value> = crate::registry::COMMANDS
        .iter()
        .map(|command| {
            json!({
                "name": command.name,
                "description": command.description,
                "input_schema": crate::registry::input_schema(command.name).unwrap(),
                "mcp_tool": command.mcp,
            })
        })
        .collect();
    assert_eq!(got["commands"], json!(expected_commands));
    assert!(got["commands"].as_array().unwrap().len() > 100);
}

fn args(value: Value) -> Option<Args> {
    value.as_object().cloned()
}

fn echo(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    Ok(json!({"pane": context.self_pane_id(), "args": args}))
}

fn current_caller(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    std::thread::sleep(std::time::Duration::from_millis(20));
    Ok(context.flock.current_caller().unwrap_or(Value::Null))
}

fn fail(_: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    Err(DunkError::Refused("handler failed".into()))
}

fn completion_binding_probe(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id().expect("request pane");
    context
        .flock
        .require_completion_binding(pane, context.env, context.request.caller.as_ref())?;
    Ok(json!({"socket_peer": context.flock.socket_peer_ok()}))
}

fn request_context<'a>(
    flock: &'a Flock,
    request: &'a RequestCtx,
    env: &'a dyn CommandEnv,
    runtime: &'a tokio::runtime::Runtime,
) -> HandlerContext<'a> {
    HandlerContext {
        flock,
        request,
        env,
        runtime: runtime.handle(),
    }
}

fn dispatch_request(
    dispatcher: &Dispatcher,
    flock: &Flock,
    request: &RequestCtx,
) -> Result<Value, DunkError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let env = CompletionEnv(TestEnv::default());
    let context = request_context(flock, request, &env, &runtime);
    dispatcher.dispatch(
        &context,
        &FixedRole("owner", true),
        "get_dunk",
        args(json!({"id": "d1"})),
    )
}

#[test]
fn test_every_command_runs_or_refuses_but_never_crashes() {
    let (_dir, flock) = flock();
    let dispatcher = Dispatcher::new();
    for command in registry::COMMANDS {
        let text = refusal(dispatch(
            &dispatcher,
            &flock,
            &FixedRole("owner", true),
            command.name,
            None,
            Some("w1:p1"),
        ));
        assert!(
            !text.trim().is_empty(),
            "{}: refused with no message",
            command.name
        );
        if command.required.is_empty() {
            assert_eq!(text, format!("command {} is not ported yet", command.name));
        }
    }
}

#[test]
fn test_unknown_command_refusal_text() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "nope",
        None,
        None,
    ));
    assert_eq!(text, "unknown command 'nope'");
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "it's",
        None,
        None,
    ));
    assert_eq!(text, "unknown command \"it's\"");
}

#[test]
fn test_role_gate_names_the_pane_or_the_missing_credential() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let with = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("anonymous", true),
        "task_add",
        None,
        Some("%4"),
    ));
    assert_eq!(
        with,
        "task_add is not permitted for role anonymous (pane %4); see docs/FLAT_FLEET.md section 4.3"
    );
    let without = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("anonymous", true),
        "task_add",
        None,
        None,
    ));
    assert_eq!(
        without,
        "task_add is not permitted for role anonymous (no pane, no token); see docs/FLAT_FLEET.md section 4.3"
    );
}

#[test]
fn test_owner_only_commands_go_through_the_superuser_gate() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let denied = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("worker", false),
        "close",
        None,
        Some("%4"),
    ));
    assert_eq!(denied, "close needs a superuser");
    // system and web skip the owner gate and fall to the per-role table (the Python elif):
    // system is denied `shutdown` there; web is allowed whatever WEB_DENIED does not name.
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("system", false),
        "shutdown",
        None,
        None,
    ));
    assert_eq!(
        text,
        "shutdown is not permitted for role system (no pane, no token); see docs/FLAT_FLEET.md section 4.3"
    );
    let open = registry::COMMANDS
        .iter()
        .find(|c| c.owner_only && c.required.is_empty() && roles::allowed("web", c.name))
        .expect("an owner-only command web may call");
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("web", false),
        open.name,
        None,
        None,
    ));
    assert_eq!(text, format!("command {} is not ported yet", open.name));
}

#[test]
fn test_owner_only_matches_the_python_superuser_only_set() {
    let expected = [
        "add_dunk",
        "update_dunk",
        "remove_dunk",
        "start_dunk",
        "stop_dunk",
        "toggle_dunk",
        "stop_all",
        "fire_dunk",
        "start_agent",
        "respawn_pane",
        "create_workspace",
        "create_tab",
        "split_pane",
        "rename",
        "focus",
        "close",
        "herdr",
        "shutdown",
        "memory_ingest",
        "lane_set",
        "lane_unregister",
        "token_issue",
        "token_revoke",
        "review_sync",
        "context_promote",
        "context_retire",
        "poolrad_sync",
        "memory_backfill",
    ];
    let mut got: Vec<&str> = registry::COMMANDS
        .iter()
        .filter(|c| c.owner_only)
        .map(|c| c.name)
        .collect();
    got.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert_eq!(got, want);
}

#[test]
fn test_required_then_unknown_argument_checks() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let owner = FixedRole("owner", true);
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": ""})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": null})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    // missing wins over unknown (the Python order), and unknown names are sorted.
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"zed": 1, "bar": 2})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": "d1", "zed": 1, "bar": 2})),
        None,
    ));
    assert_eq!(text, "get_dunk: unknown argument(s): bar, zed");
    // a falsy-but-present value (0, false) is NOT missing.
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": 0})),
        None,
    ));
    assert_eq!(text, "command get_dunk is not ported yet");
}

#[test]
fn test_a_registered_handler_gets_the_pane_and_args() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register("get_dunk", echo);
    let got = dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "get_dunk",
        args(json!({"id": "d1"})),
        Some("%9"),
    )
    .expect("handler runs");
    assert_eq!(got, json!({"pane": "%9", "args": {"id": "d1"}}));
}

#[test]
fn h_whoami_matches_python() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();

    let got = dispatch(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "whoami",
        None,
        None,
    )
    .expect("anonymous whoami");

    assert_eq!(
        got,
        json!({
            "pane_id": null,
            "role": "anonymous",
            "registered": null,
            "via_token": false,
            "caller": {"pid": null, "start": null, "name": null, "source": null},
            "bound": null,
            "binding_note": null,
        })
    );
}

#[test]
fn h_status_matches_python() {
    let (_dir, flock) = flock();
    let env = ParityEnv::new(None);
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();

    let cached = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "status",
        None,
        &request,
        &env,
    )
    .expect("cached status");
    assert_eq!(cached["version"], "2.99.0");
    assert_eq!(cached["self_pane_id"], "w1:p1");
    assert_eq!(cached["herdr_available"], Value::Null);
    assert_eq!(cached["herdr_hint"], Value::Null);
    assert_eq!(
        cached.get("inbox_unread"),
        None,
        "cached view is the cheap status"
    );
    assert_eq!(env.probes.get(), 0, "default status skips backend probing");

    let full = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "status",
        args(json!({"full": true})),
        &request,
        &env,
    )
    .expect("full status");
    assert_eq!(full["self_pane_id"], "w1:p1");
    assert_eq!(full["herdr_available"], true);
    assert_eq!(full["inbox_unread"], 7);
    assert_eq!(full["tasks"], json!({"open": 2}));
    assert_eq!(env.probes.get(), 1, "full Python status probes the backend");

    let checked = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "status",
        args(json!({"check_herdr": "yes", "fresh": true})),
        &request,
        &env,
    )
    .expect("explicit availability probe");
    assert_eq!(checked["herdr_available"], true);
    assert_eq!(env.probes.get(), 2);
}

#[test]
fn h_turn_start_matches_python() {
    let (_dir, flock) = flock();
    let pane_id = "w1:p1";
    let pid = std::process::id() as i64;
    let caller = Caller::new(
        Some(pid),
        crate::proc::process_start(pid),
        None,
        Some(pane_id.into()),
        "peer",
    );
    let request = RequestCtx {
        self_pane_id: Some(pane_id.into()),
        caller: Some(caller),
        ..RequestCtx::default()
    };
    let env = ParityEnv::new(Some(pid));
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();

    let result = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({
            "ttl_s": "60",
            "source": "pi",
            "handoff_id": format!("nj-{}", "a".repeat(32)),
        })),
        &request,
        &env,
    )
    .expect("authenticated turn start");

    assert_eq!(result["pane_id"], pane_id);
    assert_eq!(result["ttl_s"], 60.0);
    assert_eq!(result["source"], "pi");
    assert_eq!(result["turn_id"].as_str().unwrap().len(), 32);
    assert_eq!(env.turn_calls.lock().unwrap().len(), 2);

    for falsey_ttl in [json!(0), json!(false), json!("")] {
        let default_ttl = dispatch_with_env(
            &d,
            &flock,
            &FixedRole("worker", false),
            "turn_start",
            args(json!({"ttl_s": falsey_ttl})),
            &request,
            &env,
        )
        .expect("falsey Python ttl_s uses the default");
        assert_eq!(
            default_ttl["ttl_s"],
            crate::flock::commands_turns::turn_ttl_s()
        );
    }
    let hook_calls_before_refusals = env.turn_calls.lock().unwrap().len();

    let wrong_pane = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"pane": "w1:p9"})),
        &request,
        &env,
    );
    assert!(matches!(
        wrong_pane,
        Err(DunkError::Refused(message)) if message == "turn event must target the caller's own pane"
    ));
    let invalid_handoff = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"handoff_id": "nj-nope"})),
        &request,
        &env,
    );
    assert!(matches!(
        invalid_handoff,
        Err(DunkError::Refused(message)) if message == "invalid next-job handoff id"
    ));

    let non_string_handoff = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"handoff_id": 123})),
        &request,
        &env,
    );
    assert!(matches!(
        non_string_handoff,
        Err(DunkError::Refused(message))
            if message == "expected string or bytes-like object, got 'int'"
    ));
    assert_eq!(
        env.turn_calls.lock().unwrap().len(),
        hook_calls_before_refusals,
        "refusals have no hooks"
    );
    let invalid_ttl = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"ttl_s": "not-a-number"})),
        &request,
        &env,
    );
    assert!(matches!(
        invalid_ttl,
        Err(DunkError::Refused(message))
            if message == "could not convert string to float: 'not-a-number'"
    ));

    let string_zero_ttl = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"ttl_s": "0"})),
        &request,
        &env,
    );
    assert!(matches!(
        string_zero_ttl,
        Err(DunkError::Refused(message)) if message == "ttl_s must be positive"
    ));
}

#[test]
fn api_handlers_refuse_when_required_adapters_are_not_wired() {
    let (_dir, flock) = flock();
    let request = RequestCtx::default();
    let env = TestEnv::default();
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    assert!(matches!(
        d.dispatch(&context, &FixedRole("anonymous", false), "status", None),
        Err(DunkError::Refused(message)) if message == "status command adapter is not wired"
    ));
    assert!(matches!(
        h_status(&context, Args::new()),
        Err(DunkError::Refused(message)) if message == "status command adapter is not wired"
    ));
    assert!(matches!(
        env.view_cache(),
        Err(DunkError::Refused(message)) if message == "status view cache is not wired"
    ));
    assert!(matches!(
        env.turn_hooks(),
        Err(DunkError::Refused(message)) if message == "turn hooks are not wired"
    ));

    let pane_id = "w1:p1";
    let pid = std::process::id() as i64;
    let caller = Caller::new(
        Some(pid),
        crate::proc::process_start(pid),
        None,
        Some(pane_id.into()),
        "peer",
    );
    let request = RequestCtx {
        self_pane_id: Some(pane_id.into()),
        caller: Some(caller),
        ..RequestCtx::default()
    };
    let env = CompletionEnv(TestEnv::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    assert!(matches!(
        d.dispatch(
            &context,
            &FixedRole("worker", false),
            "turn_start",
            None,
        ),
        Err(DunkError::Refused(message)) if message == "turn hooks are not wired"
    ));
}

fn memory_api_fixture() -> (tempfile::TempDir, Flock, TestEnv) {
    const NOW: f64 = 1_800_000_000.0;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("memory.db");
    let connection = crate::db::open_writer(&path).expect("writer");
    crate::memory::recall::apply_schema(&connection).expect("schema");
    connection
        .execute(
            "INSERT INTO sessions (id, harness, project, cwd, title, started_at, updated_at) \
             VALUES ('session-1', 'codex', 'demo', '/tmp/demo', 'Rust API', ?1, ?1)",
            [NOW - 60.0],
        )
        .expect("session");
    connection
        .execute(
            "INSERT INTO chunks (id, session_id, ordinal, role, kind, text, sha256, created_at) \
             VALUES (1, 'session-1', 0, 'assistant', 'message', \
                     'rust api uses a stable adapter boundary for recall', 'sha-session-1-0', ?1)",
            [NOW - 60.0],
        )
        .expect("chunk");
    drop(connection);
    let pool = crate::db::ReadPool::open(&path, 1).expect("read pool");
    let (_dir, flock) = flock();
    flock.attach_stores(Stores {
        memory: Some(Arc::new(pool)),
        ..Stores::default()
    });
    (dir, flock, TestEnv::default())
}

fn dispatch_memory(
    dispatcher: &Dispatcher,
    flock: &Flock,
    env: &TestEnv,
    name: &str,
    input: Value,
) -> Result<Value, DunkError> {
    let request = RequestCtx::default();
    dispatch_with_env(
        dispatcher,
        flock,
        &FixedRole("worker", false),
        name,
        args(input),
        &request,
        env,
    )
}

#[test]
fn h_memory_recall_matches_python() {
    let (_dir, flock, env) = memory_api_fixture();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();
    let hits = dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_recall",
        json!({"query": "adapter", "limit": 1, "harness": "codex", "project": "demo"}),
    )
    .unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["session_id"], "session-1");
    assert_eq!(hits[0]["harness"], "codex");
    assert_eq!(hits[0]["project"], "demo");
    assert_eq!(
        hits[0]["text"],
        "rust api uses a stable adapter boundary for recall"
    );
}

#[test]
fn h_memory_search_matches_python() {
    let (_dir, flock, env) = memory_api_fixture();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();
    let hits = dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_search",
        json!({"query": "adapter", "limit": 1, "harness": "claude"}),
    )
    .unwrap();
    assert_eq!(hits, json!([]));
}

#[test]
fn h_memory_context_matches_python() {
    let (_dir, flock, env) = memory_api_fixture();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();
    let context = dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_context",
        json!({"query": "adapter", "sessions": 1}),
    )
    .unwrap();
    assert_eq!(context["query"], "adapter");
    assert_eq!(context["generated_at"], 1_800_000_000.0);
    assert_eq!(context["counts"], json!({"sessions": 1, "snippets": 1}));
    assert_eq!(context["sessions"][0]["session_id"], "session-1");
    assert_eq!(
        context["text"],
        "[codex] demo | Rust API\n  - rust api uses a stable adapter boundary for recall"
    );
}

#[test]
fn h_memory_context_negative_sessions_matches_python_slice() {
    let (dir, flock, env) = memory_api_fixture();
    let connection = crate::db::open_writer(&dir.path().join("memory.db")).expect("writer");
    connection
        .execute(
            "INSERT INTO sessions (id, harness, project, cwd, title, started_at, updated_at) \
             VALUES ('session-2', 'codex', 'demo', '/tmp/demo', 'Other API', 1799999940, 1799999940)",
            [],
        )
        .expect("second session");
    connection
        .execute(
            "INSERT INTO chunks (id, session_id, ordinal, role, kind, text, sha256, created_at) \
             VALUES (2, 'session-2', 0, 'assistant', 'message', \
                     'adapter memory context second result', 'sha-session-2-0', 1799999940)",
            [],
        )
        .expect("second chunk");
    drop(connection);

    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();
    let context = dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_context",
        json!({"query": "adapter", "sessions": -1}),
    )
    .unwrap();
    assert_eq!(context["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(context["counts"]["sessions"], 1);
}

#[test]
fn h_memory_handlers_bound_positive_query_limits() {
    let (_dir, flock, env) = memory_api_fixture();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();

    let recall_max = dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_recall",
        json!({"query":"adapter", "limit":50}),
    )
    .unwrap();
    assert_eq!(recall_max.as_array().unwrap().len(), 1);
    for limit in [51_i64, i64::MAX] {
        let error = refusal(dispatch_memory(
            &dispatcher,
            &flock,
            &env,
            "memory_recall",
            json!({"query":"adapter", "limit":limit}),
        ));
        assert!(error.contains("limit must not exceed 50"), "{error}");
    }

    let search_error = refusal(dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_search",
        json!({"query":"adapter", "limit":i64::MAX}),
    ));
    assert!(search_error.contains("limit must not exceed 50"));

    let context_max = dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_context",
        json!({"query":"adapter", "sessions":20}),
    )
    .unwrap();
    assert_eq!(context_max["sessions"].as_array().unwrap().len(), 1);
    for sessions in [21_i64, i64::MAX] {
        let error = refusal(dispatch_memory(
            &dispatcher,
            &flock,
            &env,
            "memory_context",
            json!({"query":"adapter", "sessions":sessions}),
        ));
        assert!(error.contains("sessions must not exceed 20"), "{error}");
    }
}

#[test]
fn h_context_supersede_matches_python() {
    let _serial = CONTEXT_LEDGER_TEST_LOCK.lock().expect("ledger test lock");
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger_env = ContextLedgerEnv::set(&dir.path().join("context-ledger.db"));
    let project = dir.path().join("project-store");
    std::fs::create_dir_all(&project).expect("project store root");
    let old = seed_context_entry(&project, "fact", "agent");
    let flock = context_flock(dir.path());
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_supersede_retire();

    let result = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "worker",
        "w1:p2",
        "context_supersede",
        json!({
            "slug": old.slug,
            "body": "replacement context body",
            "global_store": false,
            "project_path": project,
        }),
    )
    .unwrap();
    assert_eq!(result["class"], "fact");
    assert_ne!(result["slug"], "api-leaf-entry");
    assert!(Path::new(result["path"].as_str().unwrap()).is_file());
    let store = crate::context::store::Store::new(Some(&project), false);
    let original = store.find("api-leaf-entry").expect("old entry");
    assert!(original.get("superseded_by").is_some());
    assert_eq!(
        store.resolve("api-leaf-entry").unwrap().unwrap().body,
        "replacement context body"
    );

    let rule = seed_context_entry(&project, "ground-truth", "owner");
    let refused = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "worker",
        "w1:p2",
        "context_supersede",
        json!({
            "slug": rule.slug,
            "body": "worker cannot replace owner rule",
            "global_store": false,
            "project_path": project,
        }),
    )
    .unwrap_err();
    assert!(
        matches!(refused, DunkError::Refused(message) if message == "only the owner supersedes a rule")
    );
    drop(ledger_env);
}

#[test]
fn h_context_retire_matches_python() {
    let _serial = CONTEXT_LEDGER_TEST_LOCK.lock().expect("ledger test lock");
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger_env = ContextLedgerEnv::set(&dir.path().join("context-ledger.db"));
    let project = dir.path().join("project-store");
    std::fs::create_dir_all(&project).expect("project store root");
    let old = seed_context_entry(&project, "fact", "agent");
    let flock = context_flock(dir.path());
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_supersede_retire();

    let denied = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "worker",
        "w1:p2",
        "context_retire",
        json!({
            "slug": old.slug,
            "reason": "obsolete",
            "global_store": false,
            "project_path": project,
        }),
    )
    .unwrap_err();
    assert!(matches!(denied, DunkError::Refused(_)));

    let result = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "owner",
        "w1:p1",
        "context_retire",
        json!({
            "slug": old.slug,
            "reason": "obsolete",
            "global_store": false,
            "project_path": project,
        }),
    )
    .unwrap();
    assert_eq!(
        result,
        json!({"slug": "api-leaf-entry", "retired": true, "reason": "obsolete"})
    );
    let entry = crate::context::store::Store::new(Some(&project), false)
        .find("api-leaf-entry")
        .expect("retired entry");
    assert!(entry.get("retired_at").is_some());
    assert_eq!(entry.get("retired_reason"), Some("obsolete"));
    drop(ledger_env);
}

#[test]
fn test_desk_args_are_clamped_before_the_argument_checks() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register("task_list", echo);
    d.register("read_pane", echo);
    let desk = FixedRole("desk", false);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "task_list",
        args(json!({"limit": 500})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = dispatch(&d, &flock, &desk, "task_list", None, None).expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "task_list",
        args(json!({"limit": "junk"})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "task_list",
        args(json!({"limit": -3})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["limit"], 1);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "read_pane",
        args(json!({"target": "%1", "lines": 2000})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["lines"], 40);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "read_pane",
        args(json!({"target": "%1"})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["lines"], 40);
}

#[test]
fn test_desk_results_are_clamped_and_other_roles_are_not() {
    let (_dir, flock) = flock();
    fn big(_: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
        Ok(json!({"blob": "x".repeat(10_000)}))
    }
    let mut d = Dispatcher::new();
    d.register("task_list", big);
    let desk = dispatch(
        &d,
        &flock,
        &FixedRole("desk", false),
        "task_list",
        None,
        None,
    )
    .expect("ok");
    assert_eq!(desk["truncated"], true);
    let owner = dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "task_list",
        None,
        None,
    )
    .expect("ok");
    assert_eq!(owner["blob"].as_str().map(str::len), Some(10_000));
}

#[test]
fn concurrent_requests_keep_caller_identity_on_their_own_thread() {
    let (_dir, flock) = flock();
    let flock = std::sync::Arc::new(flock);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register("get_dunk", current_caller);
    let dispatcher = std::sync::Arc::new(dispatcher);
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|name| {
            let flock = std::sync::Arc::clone(&flock);
            let dispatcher = std::sync::Arc::clone(&dispatcher);
            let start = std::sync::Arc::clone(&start);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("runtime");
                let env = TestEnv::default();
                let request = RequestCtx {
                    caller: Some(crate::proc::Caller {
                        name: Some(name.into()),
                        source: "test",
                        ..crate::proc::Caller::default()
                    }),
                    socket_peer: Some(true),
                    ..RequestCtx::default()
                };
                let context = request_context(&flock, &request, &env, &runtime);
                start.wait();
                dispatcher
                    .dispatch(
                        &context,
                        &FixedRole("owner", true),
                        "get_dunk",
                        args(json!({"id": "d1"})),
                    )
                    .expect("handler runs")
            })
        })
        .collect();

    let mut names: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("request thread")
                .get("name")
                .and_then(Value::as_str)
                .expect("caller name")
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["first", "second"]);
}

#[test]
fn request_caller_fields_are_cleared_after_handler_error() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register("get_dunk", fail);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let env = TestEnv::default();
    let request = RequestCtx {
        caller: Some(crate::proc::Caller {
            name: Some("owner".into()),
            source: "test",
            ..crate::proc::Caller::default()
        }),
        service_token: Some("secret".into()),
        socket_peer: Some(true),
        ..RequestCtx::default()
    };
    let context = request_context(&flock, &request, &env, &runtime);
    let error = dispatcher
        .dispatch(
            &context,
            &FixedRole("owner", true),
            "get_dunk",
            args(json!({"id": "d1"})),
        )
        .expect_err("handler refusal");
    assert!(matches!(error, DunkError::Refused(text) if text == "handler failed"));
    assert_eq!(flock.caller_state(), crate::flock::CallerState::default());
    assert_eq!(flock.socket_peer_ok(), None);
}

#[test]
fn socket_peer_verdict_stays_distinct_from_trusted_in_process_dispatch() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register("get_dunk", completion_binding_probe);
    let pane = Some("w1:p1".to_string());
    let pid = std::process::id() as i64;
    let caller = crate::proc::Caller::new(
        Some(pid),
        crate::proc::process_start(pid),
        None,
        pane.clone(),
        "peer",
    );

    let unverified = RequestCtx {
        self_pane_id: pane.clone(),
        socket_peer: Some(false),
        ..RequestCtx::default()
    };
    let error = dispatch_request(&dispatcher, &flock, &unverified)
        .expect_err("unverified socket must not complete caller-sensitive work");
    assert!(
        matches!(error, DunkError::Refused(text) if text.contains("socket peer credentials unavailable"))
    );
    assert_eq!(
        flock.socket_peer_ok(),
        None,
        "refusal clears request provenance"
    );

    let verified = RequestCtx {
        caller: Some(caller.clone()),
        self_pane_id: pane.clone(),
        socket_peer: Some(true),
        ..RequestCtx::default()
    };
    let result = dispatch_request(&dispatcher, &flock, &verified).expect("verified socket");
    assert_eq!(result["socket_peer"], true);
    assert_eq!(
        flock.socket_peer_ok(),
        None,
        "success clears request provenance"
    );

    let in_process = RequestCtx {
        caller: Some(caller),
        self_pane_id: pane,
        socket_peer: None,
        ..RequestCtx::default()
    };
    let result = dispatch_request(&dispatcher, &flock, &in_process).expect("trusted in-process");
    assert_eq!(result["socket_peer"], Value::Null);
    assert_eq!(flock.socket_peer_ok(), None);
}

#[test]
fn test_python_repr_escapes() {
    assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    assert_eq!(py_repr("a\nb"), "'a\\nb'");
    assert_eq!(py_repr("both'\""), "'both\\'\"'");
}
struct PaneApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    panes: Vec<Value>,
}

struct RetirePaneApiEnv {
    inner: PaneApiEnv,
    retired_path: PathBuf,
}

impl PaneInfoEnv for RetirePaneApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.inner.list_panes_grouped()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.inner.get_pane(pane_id)
    }
}

impl CommandEnv for RetirePaneApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        self.inner.terminal()
    }

    fn now_s(&self) -> f64 {
        self.inner.now_s()
    }

    fn retired_panes_path(&self) -> Result<PathBuf, DunkError> {
        Ok(self.retired_path.clone())
    }
}

impl PaneInfoEnv for PaneApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|pane| pane["pane_id"] == pane_id)
            .cloned()
    }
}

impl CommandEnv for PaneApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_100.0
    }
}
fn pane_api_env() -> PaneApiEnv {
    pane_api_env_with_screen(None)
}

fn pane_api_env_with_screen(screen: Option<&str>) -> PaneApiEnv {
    let mut terminal = FakeTerminal::default();
    if let Some(screen) = screen {
        terminal.screens.insert("w1:p1".into(), screen.into());
    }
    PaneApiEnv {
        terminal: Mutex::new(Box::new(terminal)),
        panes: vec![json!({
            "pane_id": "w1:p1",
            "terminal_id": "w1:p1",
            "workspace_id": "w1",
            "tab_id": "w1",
            "workspace_label": "work",
            "tab_label": "1",
            "agent": "codex",
            "agent_status": "working",
            "cwd": "/tmp/project",
            "terminal_title_stripped": "api writer",
            "pane_pid": 123,
        })],
    }
}

fn pane_api_dispatch(
    command: &str,
    value: Value,
    env: &PaneApiEnv,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_list_panes_read_pane_workspaces();
    let request = RequestCtx {
        self_pane_id: self_pane_id.map(str::to_owned),
        ..RequestCtx::default()
    };
    dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        command,
        args(value),
        &request,
        env,
    )
}

#[test]
fn retire_pane_dispatch_is_owner_only_and_requires_a_reason_before_recording() {
    let (dir, flock) = staffed_flock("api-retire-pane");
    let retired_path = dir.path().join("retired-panes.json");
    let env = RetirePaneApiEnv {
        inner: pane_api_env(),
        retired_path: retired_path.clone(),
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_retire_pane();

    let listed = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "retire_pane",
        Some(serde_json::Map::from_iter([(
            "action".into(),
            json!("list"),
        )])),
    )
    .expect("owner can list retired panes");
    assert_eq!(listed, json!({"retired": [], "count": 0}));

    let refused = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "retire_pane",
        Some(serde_json::Map::from_iter([
            ("action".into(), json!("retire")),
            ("pane".into(), json!("w1:p1")),
        ])),
    )
    .expect_err("retiring without a reason is refused");
    assert_eq!(refused.to_string(), "'reason' is required");
    assert!(!retired_path.exists(), "refusal must not create an archive");

    let worker_request = RequestCtx {
        self_pane_id: Some("w2:p2".into()),
        ..RequestCtx::default()
    };
    let denied = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "retire_pane",
        Some(serde_json::Map::from_iter([(
            "action".into(),
            json!("list"),
        )])),
        &worker_request,
        &env,
    )
    .expect_err("the registry restricts this command to the owner role");
    assert!(denied
        .to_string()
        .contains("retire_pane is not permitted for role worker"));
}

#[test]
fn h_list_panes_matches_python() {
    let env = pane_api_env();
    assert_eq!(
        pane_api_dispatch("list_panes", json!({}), &env, Some("w1:p1")).unwrap(),
        json!({
            "self_pane_id": "w1:p1",
            "panes": [{
                "pane_id": "w1:p1",
                "is_self": true,
                "workspace_label": "work",
                "tab_label": "1",
                "agent": "codex",
                "agent_status": "working",
                "agent_status_source": "terminal backend",
                "agent_status_reason": null,
                "agent_status_inferred": false,
                "cwd": "/tmp/project",
                "terminal_title_stripped": "api writer",
                "terminal_id": "w1:p1",
                "workspace_id": "w1",
                "tab_id": "w1",
                "pane_pid": 123,
            }]
        })
    );
}

#[test]
fn h_list_panes_sorts_registry_gap_like_python() {
    let (_dir, flock) = flock();
    let mut lanes = Map::new();
    lanes.insert("z-pane".into(), json!({}));
    lanes.insert("a-pane".into(), json!({}));
    assert!(flock.write_lane_registry(&Map::from_iter([("lanes".into(), Value::Object(lanes),)])));

    let env = pane_api_env();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_list_panes_read_pane_workspaces();
    let request = RequestCtx::default();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "list_panes",
        args(json!({})),
        &request,
        &env,
    )
    .unwrap();

    assert_eq!(
        result["registry_gap"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["pane"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a-pane", "z-pane"]
    );
}

#[test]
fn h_read_pane_matches_python() {
    let env = pane_api_env();
    assert_eq!(
        pane_api_dispatch(
            "read_pane",
            json!({"target": "self", "lines": 0}),
            &env,
            Some("w1:p1"),
        )
        .unwrap(),
        json!({
            "pane_id": "w1:p1",
            "agent": "codex",
            "agent_status": "working",
            "tab_label": "1",
            "text": "",
        })
    );
}

#[test]
fn h_read_pane_normalizes_negative_line_counts() {
    let env = pane_api_env_with_screen(Some("one\ntwo\nthree"));
    let result = pane_api_dispatch(
        "read_pane",
        json!({"target":"self", "lines":-2}),
        &env,
        Some("w1:p1"),
    )
    .unwrap();
    assert_eq!(result["text"], "two\nthree");

    let minimum = pane_api_dispatch(
        "read_pane",
        json!({"target":"self", "lines":i64::MIN}),
        &env,
        Some("w1:p1"),
    )
    .unwrap();
    assert_eq!(minimum["text"], "one\ntwo\nthree");
}

#[test]
fn h_list_workspaces_matches_python() {
    let env = pane_api_env();
    assert_eq!(
        pane_api_dispatch("list_workspaces", json!({}), &env, Some("w1:p1")).unwrap(),
        json!({
            "self": {"workspace_id": "w1", "tab_id": "w1"},
            "workspaces": [
                {
                    "workspace_id": "w1",
                    "label": "work",
                    "number": 1,
                    "active_tab_id": "w1",
                    "tabs": [{
                        "tab_id": "w1",
                        "workspace_id": "w1",
                        "label": "1",
                        "number": 1,
                        "active": true,
                    }],
                },
                {
                    "workspace_id": "w2",
                    "label": "other",
                    "number": 2,
                    "active_tab_id": "w2",
                    "tabs": [{
                        "tab_id": "w2",
                        "workspace_id": "w2",
                        "label": "2",
                        "number": 2,
                        "active": true,
                    }],
                },
            ]
        })
    );
}
fn superuser_fixture() -> (tempfile::TempDir, Flock, Dispatcher) {
    const OWNER: &str = "w1:p2";
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("role registry");
    roles
        .register(OWNER, "owner", None, None, None, None, None, None)
        .expect("owner registration");
    flock.attach_roles(roles, None);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_superuser_status_add_remove();
    (dir, flock, dispatcher)
}

fn dispatch_superuser(
    dispatcher: &Dispatcher,
    flock: &Flock,
    name: &str,
    args: Option<Args>,
    pane: Option<&str>,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: pane.map(str::to_owned),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    dispatcher.dispatch(&context, &gate, name, args)
}

#[test]
fn h_superuser_status_matches_python() {
    let (_dir, flock, dispatcher) = superuser_fixture();
    let result = dispatch_superuser(&dispatcher, &flock, "superuser_status", None, Some("w1:p2"))
        .expect("status");
    assert_eq!(result["superuser"], true);
    assert_eq!(result["pane_id"], "w1:p2");
    assert_eq!(result["role"], "owner");
    assert_eq!(result["default"], "anonymous");
    assert!(result["lanes"]["w1:p2"].is_object());
}

#[test]
fn h_superuser_add_matches_python() {
    let (_dir, flock, dispatcher) = superuser_fixture();
    let result = dispatch_superuser(
        &dispatcher,
        &flock,
        "superuser_add",
        args(json!({"entry": "w2:p9"})),
        Some("w1:p2"),
    )
    .expect("add");
    assert_eq!(result["added"], "w2:p9");
    assert_eq!(result["superuser"], true);
    assert_eq!(result["entry"]["role"], "owner");

    assert!(matches!(
        dispatch_superuser(
            &dispatcher,
            &flock,
            "superuser_add",
            args(json!({"entry": "owner name"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.starts_with("superuser add takes a pane id;")
    ));
}

#[test]
fn h_superuser_remove_matches_python() {
    let (_dir, flock, dispatcher) = superuser_fixture();
    let mut roles = Roles::new(
        &Env::default(),
        Some(
            flock
                .lane_registry_path
                .parent()
                .unwrap()
                .join("roles.json"),
        ),
        Some(
            flock
                .lane_registry_path
                .parent()
                .unwrap()
                .join("tokens.json"),
        ),
        None,
    )
    .expect("role registry");
    roles
        .register("w2:p9", "owner", None, None, None, None, None, None)
        .expect("second owner registration");
    flock.attach_roles(roles, None);
    let result = dispatch_superuser(
        &dispatcher,
        &flock,
        "superuser_remove",
        args(json!({"entry": "w2:p9"})),
        Some("w1:p2"),
    )
    .expect("remove");
    assert_eq!(result["removed"], true);
    assert_eq!(result["entry"], "w2:p9");
    assert_eq!(result["superuser"], true);

    assert!(matches!(
        dispatch_superuser(
            &dispatcher,
            &flock,
            "superuser_remove",
            args(json!({"entry": "w1:p2"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message == "refusing to remove the last owner"
    ));
}

fn recovery_token_fixture(owner: Option<&str>) -> (tempfile::TempDir, Flock, Dispatcher) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("role registry");
    if let Some(owner) = owner {
        roles
            .register(owner, "owner", None, None, None, None, None, None)
            .expect("owner registration");
    }
    flock.attach_roles(roles, None);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_superuser_recover_tokens();
    (dir, flock, dispatcher)
}

fn dispatch_recovery_token(
    dispatcher: &Dispatcher,
    flock: &Flock,
    name: &str,
    args: Option<Args>,
    pane: Option<&str>,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: pane.map(str::to_owned),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    dispatcher.dispatch(&context, &gate, name, args)
}

#[test]
fn h_superuser_recover_matches_python() {
    let (_dir, flock, dispatcher) = recovery_token_fixture(None);
    let result = dispatch_recovery_token(
        &dispatcher,
        &flock,
        "superuser_recover",
        args(json!({"name": "ignored by recovery"})),
        Some("w1:p2"),
    )
    .expect("bootstrap caller as owner");
    assert_eq!(result["recovered"], true);
    assert_eq!(result["pane_id"], "w1:p2");
    assert_eq!(result["superuser"], true);
    assert_eq!(result["entry"]["role"], "owner");

    assert!(matches!(
        dispatch_recovery_token(&dispatcher, &flock, "superuser_recover", None, None),
        Err(DunkError::Refused(message))
            if message == "superuser recover requires a known caller pane"
    ));
}

#[test]
fn h_token_issue_matches_python() {
    let (_dir, flock, dispatcher) = recovery_token_fixture(Some("w1:p2"));
    let result = dispatch_recovery_token(
        &dispatcher,
        &flock,
        "token_issue",
        args(json!({"role": "web", "label": "owner transport"})),
        Some("w1:p2"),
    )
    .expect("issue token");
    assert_eq!(result["role"], "web");
    assert_eq!(result["label"], "owner transport");
    assert!(!result["token"].as_str().unwrap_or_default().is_empty());
    assert_eq!(
        result["note"],
        "shown once; store it in the service's 0600 token file"
    );

    assert!(matches!(
        dispatch_recovery_token(
            &dispatcher,
            &flock,
            "token_issue",
            args(json!({"role": "web"})),
            Some("w2:p9"),
        ),
        Err(DunkError::Refused(message)) if message.contains("requires a superuser")
    ));

    assert!(matches!(
        dispatch_recovery_token(
            &dispatcher,
            &flock,
            "token_issue",
            args(json!({"role": "web", "label": 17})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message == "token_issue: label must be a string"
    ));
}

#[test]
fn h_token_revoke_matches_python() {
    let (_dir, flock, dispatcher) = recovery_token_fixture(Some("w1:p2"));
    let issued = dispatch_recovery_token(
        &dispatcher,
        &flock,
        "token_issue",
        args(json!({"role": "system", "label": "watchdog"})),
        Some("w1:p2"),
    )
    .expect("issue token");
    assert!(!issued["token"].as_str().unwrap_or_default().is_empty());
    let revoked = dispatch_recovery_token(
        &dispatcher,
        &flock,
        "token_revoke",
        args(json!({"label": "watchdog"})),
        Some("w1:p2"),
    )
    .expect("revoke token");
    assert_eq!(revoked, json!({"label": "watchdog", "revoked": 1}));

    let again = dispatch_recovery_token(
        &dispatcher,
        &flock,
        "token_revoke",
        args(json!({"label": "watchdog"})),
        Some("w1:p2"),
    )
    .expect("idempotent revoke");
    assert_eq!(again, json!({"label": "watchdog", "revoked": 0}));
}

#[test]
fn h_task_follow_handlers_match_python() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_task_follow_unfollow_followers();
    let gate = FixedRole("worker", false);

    let first = dispatch(
        &dispatcher,
        &flock,
        &gate,
        "task_follow",
        args(json!({"scope": "mine"})),
        Some("w1:p1"),
    )
    .expect("first pane follows its own assigned work");
    assert_eq!(
        first,
        json!({
            "pane_id": "w1:p1",
            "scope": "mine",
            "following": true,
            "followers": ["w1:p1"],
        })
    );

    let second = dispatch(
        &dispatcher,
        &flock,
        &gate,
        "task_follow",
        None,
        Some("w1:p2"),
    )
    .expect("omitted scope defaults to global");
    assert_eq!(second["scope"], "global");

    let followers = dispatch(&dispatcher, &flock, &gate, "task_followers", None, None)
        .expect("read sorted followers");
    assert_eq!(
        followers,
        json!({"followers": [
            {"pane_id": "w1:p1", "scope": "mine"},
            {"pane_id": "w1:p2", "scope": "global"},
        ]})
    );

    let unfollowed = dispatch(
        &dispatcher,
        &flock,
        &gate,
        "task_unfollow",
        None,
        Some("w1:p1"),
    )
    .expect("first pane unfollows");
    assert_eq!(
        unfollowed,
        json!({
            "pane_id": "w1:p1",
            "following": false,
            "was_following": true,
            "followers": ["w1:p2"],
        })
    );
    let repeated = dispatch(
        &dispatcher,
        &flock,
        &gate,
        "task_unfollow",
        None,
        Some("w1:p1"),
    )
    .expect("repeated unfollow is idempotent");
    assert_eq!(repeated["was_following"], false);

    assert_eq!(
        refusal(dispatch(
            &dispatcher,
            &flock,
            &gate,
            "task_follow",
            None,
            None,
        )),
        "task follow requires a known caller pane"
    );
    assert_eq!(
        refusal(dispatch(
            &dispatcher,
            &flock,
            &gate,
            "task_follow",
            args(json!({"scope": "w2:p4"})),
            Some("w1:p1"),
        )),
        "per-pane tasks are private to w2:p4; caller runs in w1:p1"
    );
}

#[test]
fn h_task_follow_rejects_falsey_non_string_scopes() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_task_follow_unfollow_followers();
    let gate = FixedRole("worker", false);

    for (scope, rendered) in [
        (json!(false), "False"),
        (json!(0), "0"),
        (json!([]), "[]"),
        (json!({}), "{}"),
    ] {
        let error = dispatch(
            &dispatcher,
            &flock,
            &gate,
            "task_follow",
            args(json!({"scope": scope})),
            Some("w1:p1"),
        );
        assert_eq!(
            refusal(error),
            format!(
                "scope must be 'global' or a pane id such as 'w8:p3' or '%4' (got '{rendered}')"
            )
        );
    }

    let followers = dispatch(&dispatcher, &flock, &gate, "task_followers", None, None)
        .expect("read followers after invalid scopes");
    assert_eq!(followers, json!({"followers": []}));
}

#[test]
fn test_gh_token_follows_the_role() {
    let (dir, flock) = github_token_flock("gh-token-role");
    let env = github_token_api_env(dir.path());
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_gh_token();

    let worker =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w1:p1"), "gh_token", None).unwrap();
    assert_eq!(
        json!({
            "token": "fake-author-token",
            "kind": "author",
            "role": "worker",
            "slug": "author-app",
            "expires_at": 4_102_444_800.0,
            "cached": true
        }),
        worker
    );
    assert_eq!(
        Some(&env.now_s),
        flock.gh_token_mints.lock().unwrap().get("w1:p1")
    );
    assert_eq!(
        Some("w1:p1".to_owned()),
        crate::github::author_from_mints(
            Some(env.now_s - 1800.0),
            None,
            Some(env.now_s),
            env.tokens.config_dir(),
        )
        .unwrap()
    );
    let reviewer =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w1:p2"), "gh_token", None).unwrap();
    assert_eq!("fake-reviewer-token", reviewer["token"]);
    assert_eq!("reviewer", reviewer["kind"]);

    let owner =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w1:p3"), "gh_token", None).unwrap();
    assert_eq!(Value::Null, owner["token"]);
    assert_eq!(Value::Null, owner["kind"]);
    assert_eq!("owner", owner["role"]);
    assert!(owner["note"]
        .as_str()
        .unwrap()
        .contains("keeps their own login"));

    let owner_reviewer = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p3"),
        "gh_token",
        args(json!({"kind": "reviewer"})),
    )
    .unwrap();
    assert_eq!("fake-reviewer-token", owner_reviewer["token"]);
    assert_eq!("reviewer", owner_reviewer["kind"]);

    let forbidden = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "gh_token",
        args(json!({"kind": "reviewer"})),
    )
    .unwrap_err();
    assert!(forbidden
        .to_string()
        .contains("may only act as the author App"));

    let bad_worker_kind = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "gh_token",
        args(json!({"kind": 7})),
    )
    .unwrap_err();
    assert!(bad_worker_kind
        .to_string()
        .contains("may only act as the author App, not 7"));

    let bad_owner_kind = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p3"),
        "gh_token",
        args(json!({"kind": 7})),
    )
    .unwrap_err();
    assert!(bad_owner_kind.to_string().contains("unknown app kind 7"));
}

#[test]
fn test_gh_token_rejects_cached_entry_without_token() {
    let (dir, flock) = github_token_flock("gh-token-missing-cached-token");
    let env = github_token_api_env_with_author_entry(
        dir.path(),
        json!({
            "expires_at": 4_102_444_800.0,
            "slug": "author-app"
        }),
    );
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_gh_token();

    assert_eq!(
        "'token'",
        refusal(dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "gh_token",
            None,
        ))
    );
    assert!(flock.gh_token_mints.lock().unwrap().is_empty());
    assert!(!env
        .tokens
        .config_dir()
        .join("github-author-mints.json")
        .exists());
}

#[test]
fn test_poolrad_sync_files_n_and_never_refiles() {
    let (_dir, flock) = staffed_flock("api-poolrad-sync");
    let env = PoolradApiEnv::new();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_poolrad_sync_progress();

    let dry_run = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        args(json!({"limit": "1_0", "dry_run": "yes"})),
    )
    .unwrap();
    assert_eq!(dry_run["filed"].as_array().unwrap().len(), 7);
    assert!(env.store.tasks.lock().unwrap().is_empty());

    let unicode_digits = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        args(json!({"limit": "١٢٣", "dry_run": true})),
    )
    .unwrap();
    assert_eq!(unicode_digits["filed"].as_array().unwrap().len(), 7);

    let negative_limit = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        args(json!({"limit": -1, "dry_run": true})),
    )
    .unwrap();
    assert_eq!(negative_limit["filed"].as_array().unwrap().len(), 1);
    assert!(env.store.tasks.lock().unwrap().is_empty());

    let first = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        args(json!({"limit": "2"})),
    )
    .unwrap();
    assert_eq!(first["filed"].as_array().unwrap().len(), 2);
    assert_eq!(env.store.tasks.lock().unwrap().len(), 2);

    // Python's `int(limit or 6)` makes numeric zero select the default limit.
    let rest = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        args(json!({"limit": 0})),
    )
    .unwrap();
    assert_eq!(rest["filed"].as_array().unwrap().len(), 5);
    assert_eq!(env.store.tasks.lock().unwrap().len(), 7);

    let repeated = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        None,
    )
    .unwrap();
    assert!(repeated["filed"].as_array().unwrap().is_empty());

    assert_eq!(
        refusal(dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "poolrad_sync",
            args(json!({"limit": "bad"})),
        )),
        "invalid literal for int() with base 10: 'bad'"
    );
    assert_eq!(
        refusal(dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "poolrad_sync",
            args(json!({"limit": "1__0"})),
        )),
        "invalid literal for int() with base 10: '1__0'"
    );
    assert_eq!(
        refusal(dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "poolrad_sync",
            args(json!({"limit": [1]})),
        )),
        "int() argument must be a string, a bytes-like object or a real number, not 'list'"
    );
    assert!(refusal(dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "poolrad_sync",
        None,
    ))
    .contains("requires a superuser"));
}

#[test]
fn test_poolrad_sync_dry_run_matches_python_bool() {
    let (_dir, flock) = staffed_flock("api-poolrad-dry-run-bool");
    let env = PoolradApiEnv::new();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_poolrad_sync_progress();

    for dry_run in [
        json!("false"),
        json!(2),
        json!(["yes"]),
        json!({"yes": true}),
    ] {
        let result = dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "poolrad_sync",
            args(json!({"limit": 1, "dry_run": dry_run})),
        )
        .unwrap();
        assert_eq!(result["filed"].as_array().unwrap().len(), 1);
    }
    assert_eq!(env.store.tasks.lock().unwrap().len(), 4);

    let dry_run = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "poolrad_sync",
        args(json!({"limit": 1, "dry_run": "on"})),
    )
    .unwrap();
    assert_eq!(dry_run["filed"].as_array().unwrap().len(), 1);
    assert_eq!(env.store.tasks.lock().unwrap().len(), 4);
}

#[test]
fn test_poolrad_limit_matches_python_int_oracle() {
    let missing = args(json!({})).unwrap();
    assert_eq!(poolrad_limit(&missing).unwrap(), 6);

    let cases = [
        (json!(null), 6),
        (json!(false), 6),
        (json!(true), 1),
        (json!(0), 6),
        (json!("0"), 1),
        (json!(-1), 1),
        (json!(1.9), 1),
        (json!(-0.5), 1),
        (json!("1_0"), 10),
        (json!("١٢٣"), 123),
        (json!("１２３"), 123),
        (json!(" \t+١٢٣ \n"), 123),
        (json!("999999999999999999999999999999999999"), usize::MAX),
        (json!("-999999999999999999999999999999999999"), 1),
        (json!(1e100), usize::MAX),
        (json!(-1e100), 1),
        (json!([]), 6),
        (json!({}), 6),
        (json!(u64::MAX), usize::MAX),
    ];
    for (limit, expected) in cases {
        assert_eq!(
            poolrad_limit(&args(json!({"limit": limit})).unwrap()).unwrap(),
            expected
        );
    }

    for (limit, expected_error) in [
        (
            json!("bad"),
            "invalid literal for int() with base 10: 'bad'",
        ),
        (
            json!("1__0"),
            "invalid literal for int() with base 10: '1__0'",
        ),
        (
            json!([1]),
            "int() argument must be a string, a bytes-like object or a real number, not 'list'",
        ),
        (
            json!({"x": 1}),
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'",
        ),
    ] {
        assert_eq!(
            poolrad_limit(&args(json!({"limit": limit})).unwrap())
                .unwrap_err()
                .to_string(),
            expected_error
        );
    }
}

#[test]
fn test_poolrad_dry_run_bool_matches_python_oracle() {
    assert!(!poolrad_dry_run(None));
    let cases = [
        (json!(null), false),
        (json!(false), false),
        (json!(true), true),
        (json!("false"), false),
        (json!("no"), false),
        (json!("yes"), true),
        (json!("on"), true),
        (json!("1"), true),
        (json!("0"), false),
        (json!(" YES "), true),
        (json!("2"), false),
        (json!(0), false),
        (json!(1), true),
        (json!(2), false),
        (json!([]), false),
        (json!(["yes"]), false),
        (json!({}), false),
        (json!({"yes": true}), false),
    ];
    for (value, expected) in cases {
        assert_eq!(poolrad_dry_run(Some(&value)), expected, "{value}");
    }
}

#[test]
fn test_poolrad_progress_reports_the_recorded_count() {
    let (_dir, flock) = staffed_flock("api-poolrad-progress");
    let env = PoolradApiEnv::new();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_poolrad_sync_progress();

    let report = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "poolrad_progress",
        None,
    )
    .unwrap();
    assert_eq!(report["credited"], 1);
    assert_eq!(report["identities"], 2);
    assert_eq!(report["ceiling"], 815);
    assert_eq!(
        report["ceiling_note"],
        "CEILING: 815 is an upper bound, not a verified denominator"
    );
    assert_eq!(report["composable_bodies"], 7);
    assert_eq!(report["queue_candidates"], 4);
    assert_eq!(report["credit_pending"], 0);
    assert!(report["queue_exported_at"].is_number());

    assert_eq!(
        refusal(TestEnv::default().poolrad_sync(6, false)),
        "poolrad sync adapter is not wired"
    );
    assert_eq!(
        refusal(TestEnv::default().poolrad_progress()),
        "poolrad progress adapter is not wired"
    );
}

fn task_api_fixture() -> (
    tempfile::TempDir,
    Flock,
    Arc<crate::tasks::Tasks>,
    Dispatcher,
    tokio::runtime::Runtime,
) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("role registry");
    for (pane, role) in [("w1:p1", "worker"), ("w1:p2", "owner"), ("w1:p3", "worker")] {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .expect("role registration");
    }
    flock.attach_roles(roles, None);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let tasks = Arc::new(
        runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .expect("task store"),
    );
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Stores::default()
    });
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_task_add_list_read();
    (dir, flock, tasks, dispatcher, runtime)
}

fn dispatch_task_api(
    dispatcher: &Dispatcher,
    flock: &Flock,
    runtime: &tokio::runtime::Runtime,
    name: &str,
    input: Value,
    pane: &str,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: Some(pane.to_string()),
        ..RequestCtx::default()
    };
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    dispatcher.dispatch(&context, &gate, name, args(input))
}

#[test]
fn h_task_add_matches_python_defaults_and_role_gate() {
    let (_dir, flock, _tasks, dispatcher, runtime) = task_api_fixture();
    let plain = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_add",
        json!({"title": "default"}),
        "w1:p1",
    )
    .expect("default task");
    assert_eq!(plain["scope"], "global");
    assert_eq!(plain["status"], "open");
    assert_eq!(plain["priority"], "low");
    assert_eq!(plain["body"], "");
    assert_eq!(plain["created_by"], "w1:p1");
    assert_eq!(plain["assignees"], json!([]));
    assert_eq!(plain["children"], json!([]));

    let high = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_add",
        json!({
            "title": "explicit",
            "priority": "high",
            "scope": "self",
            "body": "details"
        }),
        "w1:p1",
    )
    .expect("explicit high-priority task");
    assert_eq!(high["scope"], "w1:p1");
    assert_eq!(high["priority"], "high");
    assert_eq!(high["body"], "details");

    assert!(matches!(
        dispatch_task_api(
            &dispatcher,
            &flock,
            &runtime,
            "task_add",
            json!({"title": "not assignable", "assignees": ["w1:p3"]}),
            "w1:p1",
        ),
        Err(DunkError::Refused(message)) if message.contains("only to yourself")
    ));

    let owner_assigned = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_add",
        json!({"title": "owner assigned", "assignees": ["w1:p3"]}),
        "w1:p2",
    )
    .expect("owner assignment");
    assert_eq!(owner_assigned["assignees"], json!(["w1:p3"]));
}

#[test]
fn h_task_read_returns_full_wire_and_latest_three_note_reasons() {
    let (_dir, flock, tasks, dispatcher, runtime) = task_api_fixture();
    let created = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_add",
        json!({"title": "read me", "scope": "self", "body": "full row"}),
        "w1:p1",
    )
    .expect("create task");
    let id = created["id"].as_str().expect("task id");
    for index in 0..5 {
        runtime
            .block_on(tasks.note(id, "note", "w1:p1", Some(&format!("note {index}"))))
            .expect("append note");
    }

    let read = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_read",
        json!({"id": id}),
        "w1:p1",
    )
    .expect("read task");
    assert_eq!(read["id"], id);
    assert_eq!(read["scope"], "w1:p1");
    assert_eq!(read["body"], "full row");
    assert_eq!(read["children"], json!([]));
    assert!(read["leases"].is_array());
    assert_eq!(
        read["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["reason"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["note 2", "note 3", "note 4"]
    );
    assert_eq!(read["notes"][0]["actor"], "w1:p1");

    assert!(matches!(
        dispatch_task_api(
            &dispatcher,
            &flock,
            &runtime,
            "task_read",
            json!({"id": id}),
            "w1:p3",
        ),
        Err(DunkError::Refused(message)) if message.contains("not visible")
    ));
    assert!(matches!(
        dispatch_task_api(
            &dispatcher,
            &flock,
            &runtime,
            "task_read",
            json!({"id": "t999999"}),
            "w1:p1",
        ),
        Err(DunkError::Refused(message)) if message.contains("no task with id")
    ));
}

#[test]
fn h_task_list_matches_python_defaults_and_slice_limits() {
    let (_dir, flock, tasks, dispatcher, runtime) = task_api_fixture();
    let mut ids = Vec::new();
    for title in ["first", "second", "third", "fourth"] {
        let created = dispatch_task_api(
            &dispatcher,
            &flock,
            &runtime,
            "task_add",
            json!({"title": title}),
            "w1:p1",
        )
        .expect("create list fixture");
        ids.push(created["id"].as_str().unwrap().to_string());
    }
    runtime
        .block_on(tasks.complete(&ids[0]))
        .expect("complete first task");

    for args in [json!({}), json!({"limit": null}), json!({"limit": 0})] {
        let listed = dispatch_task_api(&dispatcher, &flock, &runtime, "task_list", args, "w1:p1")
            .expect("default task list");
        assert_eq!(listed["scope"], "visible");
        assert_eq!(listed["pane_id"], "w1:p1");
        assert_eq!(listed["tasks"].as_array().unwrap().len(), 4);
    }

    let two = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_list",
        json!({"limit": "2"}),
        "w1:p1",
    )
    .expect("positive limit");
    assert_eq!(two["tasks"].as_array().unwrap().len(), 2);

    let negative = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_list",
        json!({"limit": -1}),
        "w1:p1",
    )
    .expect("negative Python slice");
    assert_eq!(negative["tasks"].as_array().unwrap().len(), 3);

    let active = dispatch_task_api(
        &dispatcher,
        &flock,
        &runtime,
        "task_list",
        json!({"include_done": false}),
        "w1:p1",
    )
    .expect("exclude completed tasks");
    assert_eq!(active["tasks"].as_array().unwrap().len(), 3);
    assert!(active["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|task| task["status"] != "done"));

    assert!(matches!(
        dispatch_task_api(
            &dispatcher,
            &flock,
            &runtime,
            "task_list",
            json!({"limit": "nope"}),
            "w1:p1",
        ),
        Err(DunkError::Refused(message)) if message == "invalid literal for int() with base 10: 'nope'"
    ));
    assert!(matches!(
        dispatch_task_api(
            &dispatcher,
            &flock,
            &runtime,
            "task_list",
            json!({"scope": "w1:p3"}),
            "w1:p1",
        ),
        Err(DunkError::Refused(_))
    ));
}
