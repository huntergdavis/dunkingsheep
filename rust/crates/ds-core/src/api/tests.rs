//! Tests for `api.rs`, ported from tests/test_command_smoke.py (the gate-order cases) plus the
//! `dispatch` gate sequence in dunk_api.py. The allowlist tests of
//! tests/test_dunk_dispatch_allowlist.py exercise `_dispatch_allowlist`, not `dispatch`, and
//! belong to the dispatch-command slice.

use super::*;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::config::Env;
use crate::flock::commands_inbox::{InboxEnv, Nudge, RouteEntry};
use crate::flock::commands_turns::{TurnHooks, TurnState};
use crate::flock::dunks::DunkList;
use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::{PaneInfoEnv, Stores};
use crate::flock::roster::RosterEnv;
use crate::flock::status::{StatusEnv, ViewCache};
use crate::inbox_store::Store;
use crate::proc::Caller;
use crate::report::{ReportPaths, ReportSources};
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::tasks::Tasks;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct FixedMemoryClock;

impl crate::memory::service::MemoryClock for FixedMemoryClock {
    fn wall(&self) -> f64 {
        1_800_000_000.0
    }

    fn performance(&self) -> f64 {
        1.0
    }
}

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

struct ProjectSpawnTestEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    config: ProjectSpawnConfig,
}

impl PaneInfoEnv for ProjectSpawnTestEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for ProjectSpawnTestEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn project_spawn_config(&self) -> Result<ProjectSpawnConfig, DunkError> {
        Ok(self.config.clone())
    }

    fn now_s(&self) -> f64 {
        self.config.now
    }
}

struct DunkHandlerEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    panes: Vec<Value>,
    dunks: DunkList,
    fire_calls: Mutex<Vec<(String, i64, bool)>>,
}

impl DunkHandlerEnv {
    fn new() -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            panes: vec![json!({
                "pane_id": "w1:p1",
                "tab_id": "w1:t1",
                "workspace_id": "w1",
                "cwd": "/repo",
                "agent": "codex",
                "terminal_id": "term-1",
                "terminal_title_stripped": "editor",
                "tab_label": "Writer",
                "workspace_label": "project"
            })],
            dunks: DunkList::new(),
            fire_calls: Mutex::new(Vec::new()),
        }
    }
}

impl PaneInfoEnv for DunkHandlerEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id))
            .cloned()
    }
}

impl CommandEnv for DunkHandlerEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn add_dunk(
        &self,
        pane: Option<&serde_json::Map<String, Value>>,
        args: &crate::flock::dunks::AddArgs,
    ) -> Result<serde_json::Map<String, Value>, DunkError> {
        self.dunks.add(pane, args)
    }

    fn update_dunk(
        &self,
        id: &str,
        pane: Option<&serde_json::Map<String, Value>>,
        args: &crate::flock::dunks::UpdateArgs,
    ) -> Result<serde_json::Map<String, Value>, DunkError> {
        self.dunks.update(id, pane, args)
    }

    fn remove_dunk(&self, id: &str) -> Result<serde_json::Map<String, Value>, DunkError> {
        self.dunks.remove(id)
    }

    fn stop_all_dunks(&self) -> Result<Vec<serde_json::Map<String, Value>>, DunkError> {
        Ok(self.dunks.stop_all())
    }

    fn fire_dunk(&self, id: &str, countdown_s: i64, wait: bool) -> Result<Value, DunkError> {
        self.fire_calls
            .lock()
            .unwrap()
            .push((id.into(), countdown_s, wait));
        Ok(json!({"id": id, "countdown_s": countdown_s, "wait": wait}))
    }

    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }
}

struct OwnerApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    owner_push: crate::owner::OwnerPush,
    now_s: f64,
}

impl OwnerApiEnv {
    fn new(path: &Path, now_s: f64) -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            owner_push: crate::owner::OwnerPush::open(path).unwrap(),
            now_s,
        }
    }
}

impl PaneInfoEnv for OwnerApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        Some(json!({"pane_id": pane_id, "pane_pid": std::process::id()}))
    }
}

impl CommandEnv for OwnerApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        self.now_s
    }

    fn owner_push(&self) -> Result<&crate::owner::OwnerPush, DunkError> {
        Ok(&self.owner_push)
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

struct ReviewApiTransport {
    requests: Mutex<Vec<crate::github::GitHubRequest>>,
    responses: Mutex<std::collections::VecDeque<crate::github::GitHubResponse>>,
}

impl ReviewApiTransport {
    fn with_json_responses(responses: impl IntoIterator<Item = Value>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(
                responses
                    .into_iter()
                    .map(|value| crate::github::GitHubResponse {
                        status: 200,
                        reason: "OK".into(),
                        body: serde_json::to_vec(&value).unwrap(),
                    })
                    .collect(),
            ),
        }
    }
}

impl crate::github::GitHubTransport for ReviewApiTransport {
    fn send<'a>(
        &'a self,
        request: crate::github::GitHubRequest,
    ) -> crate::github::PinBoxFuture<'a, Result<crate::github::GitHubResponse, String>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "unexpected review API request".into())
        })
    }
}

struct ReviewApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    now_s: f64,
    tokens: crate::github::Tokens,
    transport: ReviewApiTransport,
    transport_error: Option<String>,
}

impl PaneInfoEnv for ReviewApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for ReviewApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        self.now_s
    }

    fn github_tokens(&self) -> Result<&crate::github::Tokens, DunkError> {
        Ok(&self.tokens)
    }

    fn review_transport(&self) -> Result<&dyn crate::github::GitHubTransport, DunkError> {
        match &self.transport_error {
            Some(error) => Err(DunkError::Refused(error.clone())),
            None => Ok(&self.transport),
        }
    }
}

fn review_api_env(dir: &Path, responses: impl IntoIterator<Item = Value>) -> ReviewApiEnv {
    let config_dir = dir.join("github-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("github-repos.json"), r#"{"repos":["o/r"]}"#).unwrap();
    let cache_path = dir.join("github-tokens.json");
    std::fs::write(
        &cache_path,
        serde_json::to_vec(&json!({
            "tokens": {
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
    ReviewApiEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        now_s: 1_800_000_000.0,
        tokens: crate::github::Tokens::new(
            Some(cache_path),
            config_dir,
            Box::new(NoNetworkGithubTransport),
            Some("https://github.invalid".into()),
        ),
        transport: ReviewApiTransport::with_json_responses(responses),
        transport_error: None,
    }
}

fn review_dispatch_context<'a>(
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

#[test]
fn h_review_sync_matches_python() {
    let (dir, flock) = staffed_flock("review-sync-api");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .unwrap(),
    );
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    let env = review_api_env(
        dir.path(),
        [
            json!([{
                "number": 7,
                "title": "candidate",
                "html_url": "https://example.test/pull/7",
                "draft": false,
                "head": {"sha":"head-7", "ref":"work/topic"},
                "base": {"ref":"main"},
                "user": {"login":"author"}
            }]),
            json!({"mergeable":true, "mergeable_state":"clean"}),
            json!({"state":"open", "body":""}),
            json!({}),
        ],
    );
    {
        let request = RequestCtx::default();
        let context = review_dispatch_context(&flock, &request, &env, &runtime);
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_review_sync_list();

        let report = dispatcher
            .dispatch(&context, &FixedRole("owner", true), "review_sync", None)
            .unwrap();

        assert_eq!(json!([{"repo":"o/r", "open":1}]), report["repos"]);
        assert_eq!(1, report["filed"].as_array().unwrap().len());
        assert!(report["errors"].as_array().unwrap().is_empty());
        let rows = runtime.block_on(tasks.list_review_rows_visible()).unwrap();
        assert_eq!(1, rows.len());
        assert_eq!("open", rows[0].status);
        let requests = env.transport.requests.lock().unwrap();
        assert_eq!(4, requests.len());
        assert!(requests
            .iter()
            .all(|request| request.url.starts_with("https://github.invalid/")));
        assert!(requests[0]
            .url
            .ends_with("/repos/o/r/pulls?state=open&per_page=100&page=1"));
        assert!(requests[3].url.ends_with("/repos/o/r/statuses/head-7"));
        let status: Value = serde_json::from_slice(requests[3].body.as_ref().unwrap()).unwrap();
        assert_eq!("pending", status["state"]);
    }
    drop(tasks);
    drop(flock);
    drop(runtime);
}

#[test]
fn h_review_list_matches_python() {
    let (dir, flock) = flock();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .unwrap(),
    );
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    runtime
        .block_on(tasks.add_with(
            "global",
            "review title",
            "",
            crate::tasks::AddOptions {
                kind: Some("review".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some("123456789abcde".into()),
                assignees: vec!["%9".into()],
                ..Default::default()
            },
        ))
        .unwrap();
    runtime
        .block_on(tasks.add_with(
            "%2",
            "pane-scoped review title",
            "",
            crate::tasks::AddOptions {
                kind: Some("review".into()),
                repo: Some("o/pane-scoped".into()),
                pr: Some("8".into()),
                head: Some("fedcba98765432".into()),
                assignees: vec!["%2".into()],
                ..Default::default()
            },
        ))
        .unwrap();
    {
        let env = TestEnv::default();
        let request = RequestCtx {
            self_pane_id: Some("%2".into()),
            ..RequestCtx::default()
        };
        let context = review_dispatch_context(&flock, &request, &env, &runtime);
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_review_sync_list();

        let result = dispatcher
            .dispatch(&context, &FixedRole("worker", false), "review_list", None)
            .unwrap();

        assert_eq!(1, result["count"]);
        assert_eq!("123456789abc", result["reviews"][0]["head"]);
        assert_eq!(json!(["%9"]), result["reviews"][0]["reviewer"]);
        assert_eq!("review title", result["reviews"][0]["title"]);
        assert_eq!("open", result["reviews"][0]["status"]);
        assert_eq!("o/r", result["reviews"][0]["repo"]);
        assert_eq!(7, result["reviews"][0]["pr"]);
        assert_eq!(Value::Null, result["reviews"][0]["author"]);
        assert_eq!(Value::Null, result["reviews"][0]["verdict"]);
        assert_eq!(result["reviews"][0]["age_s"], result["oldest_age_s"]);
    }
    drop(tasks);
    drop(flock);
    drop(runtime);
}

#[test]
fn review_sync_dispatch_preserves_the_owner_gate_before_service_access() {
    let (dir, flock) = staffed_flock("review-sync-denied");
    let env = review_api_env(dir.path(), []);
    let request = RequestCtx::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = review_dispatch_context(&flock, &request, &env, &runtime);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_review_sync_list();

    let error = dispatcher
        .dispatch(&context, &FixedRole("worker", false), "review_sync", None)
        .unwrap_err()
        .to_string();

    assert_eq!("review_sync needs a superuser", error);
    assert!(env.transport.requests.lock().unwrap().is_empty());
}

#[test]
fn review_sync_dispatch_forwards_transport_provider_errors() {
    let (dir, flock) = staffed_flock("review-sync-transport-error");
    let mut env = review_api_env(dir.path(), []);
    env.transport_error = Some("review transport unavailable".into());
    let request = RequestCtx::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = review_dispatch_context(&flock, &request, &env, &runtime);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_review_sync_list();

    let error = dispatcher
        .dispatch(&context, &FixedRole("owner", true), "review_sync", None)
        .unwrap_err()
        .to_string();

    assert_eq!("review transport unavailable", error);
    assert!(env.transport.requests.lock().unwrap().is_empty());
}

#[test]
fn review_list_dispatch_forwards_missing_task_store_error() {
    let (_dir, flock) = flock();
    let env = TestEnv::default();
    let request = RequestCtx::default();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    {
        let context = review_dispatch_context(&flock, &request, &env, &runtime);
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_review_sync_list();

        let error = dispatcher
            .dispatch(&context, &FixedRole("worker", false), "review_list", None)
            .unwrap_err()
            .to_string();

        assert_eq!("the task store is not attached", error);
    }
    drop(flock);
    drop(runtime);
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
    inbox_store: Store,
    inbox_pool: crate::db::ReadPool,
    _inbox_tmp: tempfile::TempDir,
}

impl ParityEnv {
    fn new(pane_pid: Option<i64>) -> Self {
        let inbox_tmp = tempfile::tempdir().expect("inbox tempdir");
        let inbox_path = inbox_tmp.path().join("inbox.db");
        let inbox_store =
            Store::open(&inbox_path, inbox_tmp.path().join("blobs")).expect("inbox store");
        let inbox_pool = crate::db::ReadPool::open(&inbox_path, 2).expect("inbox pool");
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            dunks: DunkList::new(),
            view_cache: ViewCache::new(),
            now_s: 1_800_000_100.0,
            pane_pid,
            inbox_value: Cell::new(1),
            probes: Cell::new(0),
            turn_calls: Mutex::new(Vec::new()),
            inbox_store,
            inbox_pool,
            _inbox_tmp: inbox_tmp,
        }
    }
}

impl PaneInfoEnv for ParityEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        vec![json!({
            "pane_id": "w1:p1", "agent_name": "worker", "agent": "pi",
            "label": "worker", "workspace_label": "w1", "tab_label": "t1"
        })]
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
    fn observe_turn(&self, pane_id: &str, state: &TurnState) -> Result<(), DunkError> {
        self.turn_calls
            .lock()
            .unwrap()
            .push(format!("observe {pane_id} {}", state.turn_id.len()));
        Ok(())
    }
    fn dispatch_on_turn_end(&self, pane_id: &str) -> Result<Value, DunkError> {
        Ok(json!({"trigger": "turn.end", "action": "skip", "reason": pane_id, "delivered": false}))
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
    fn with_inbox_env(
        &self,
        operation: &mut dyn FnMut(&dyn InboxEnv) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        operation(self)
    }
    fn roster_env(&self) -> Result<&dyn RosterEnv, DunkError> {
        Ok(self)
    }
}

impl MailboxEnv for ParityEnv {
    fn list_panes(&self) -> Vec<Value> {
        <Self as PaneInfoEnv>::list_panes_grouped(self)
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        <Self as PaneInfoEnv>::list_panes_grouped(self)
    }
    fn registered_pane(&self, pane_id: &str) -> (bool, bool, Option<String>) {
        (
            pane_id == "w1:p1",
            pane_id == "w1:p1",
            self.pane_pid.map(|pid| pid.to_string()),
        )
    }
}

impl InboxEnv for ParityEnv {
    fn caller_role(&self, _pane_id: &str) -> Result<String, DunkError> {
        Ok("worker".into())
    }
    fn mailbox_override(&self) -> Option<String> {
        None
    }
    fn backend_name(&self) -> Result<String, DunkError> {
        Ok("test".into())
    }
    fn lane_panes(&self) -> Vec<String> {
        Vec::new()
    }
    fn pool(&self) -> &crate::db::ReadPool {
        &self.inbox_pool
    }
    fn store(&self) -> &Store {
        &self.inbox_store
    }
    fn now(&self) -> f64 {
        self.now_s
    }
    fn record_metric(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: Option<&str>,
        _: Option<i64>,
        _: Option<i64>,
    ) {
    }
    fn pm_routing(&self) -> Vec<RouteEntry> {
        Vec::new()
    }
    fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn send_now(&self, _: &str, _: &str, _: &str) -> Result<Nudge, DunkError> {
        Err(DunkError::Refused("unused test nudge".into()))
    }
    fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
    fn tasks_for_pane(&self, _: &str) -> Result<Value, DunkError> {
        Ok(Value::Null)
    }
}

impl RosterEnv for ParityEnv {
    fn lanes(&self) -> Result<Vec<Value>, String> {
        Ok(vec![json!({
            "pane_id": "w1:p1", "agent": "pi", "agent_name": "worker"
        })])
    }
    fn leases_for_pane(&self, _: &str, _: f64) -> Vec<Value> {
        Vec::new()
    }
    fn task(&self, _: &str) -> Option<Value> {
        None
    }
    fn inbox_count(&self, _: &str) -> Option<(i64, Option<f64>)> {
        Some((0, None))
    }
    fn activity(&self, _: &Value, _: f64) -> Result<Value, String> {
        Ok(json!({"state": "idle", "since": null, "source": "test", "reason": null}))
    }
    fn cpu_sample(&self, _: &Value, _: f64) -> Result<Option<Value>, String> {
        Ok(None)
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

struct PmApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    store: Store,
    config: Env,
    config_error: Option<String>,
    panes: Vec<Value>,
    fail_notify: bool,
    notices: Mutex<Vec<String>>,
    now: f64,
    _dir: tempfile::TempDir,
}

impl PmApiEnv {
    fn new(config: Env, fail_notify: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("inbox.sqlite3"), dir.path().join("blobs"))
            .expect("inbox store");
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            store,
            config,
            config_error: None,
            panes: vec![
                json!({"pane_id":"w2:p2", "label":"worker", "tab_label":"worker", "workspace_label":"work", "agent":"codex"}),
                json!({"pane_id":"w9:pH", "label":"VP", "tab_label":"VP", "workspace_label":"lead", "agent":"codex"}),
            ],
            fail_notify,
            notices: Mutex::new(Vec::new()),
            now: 2_000_000_000.0,
            _dir: dir,
        }
    }
}

impl PaneInfoEnv for PmApiEnv {
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

impl MailboxEnv for PmApiEnv {
    fn list_panes(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn registered_pane(&self, _pane_id: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}

impl InboxEnv for PmApiEnv {
    fn caller_role(&self, _pane_id: &str) -> Result<String, DunkError> {
        Ok("worker".into())
    }

    fn mailbox_override(&self) -> Option<String> {
        None
    }

    fn backend_name(&self) -> Result<String, DunkError> {
        Ok("test terminal".into())
    }

    fn lane_panes(&self) -> Vec<String> {
        Vec::new()
    }

    fn pool(&self) -> &crate::db::ReadPool {
        self.store.read_pool()
    }

    fn store(&self) -> &Store {
        &self.store
    }

    fn now(&self) -> f64 {
        self.now
    }

    fn record_metric(
        &self,
        _kind: &str,
        _pane_id: Option<&str>,
        _mailbox: &str,
        _sender: Option<&str>,
        _duration_ms: Option<i64>,
        _bytes: Option<i64>,
    ) {
    }

    fn pm_routing(&self) -> Vec<RouteEntry> {
        Vec::new()
    }

    fn fold_into_queued_notice(
        &self,
        _pane_id: &str,
        _text: &str,
        _message_id: &str,
    ) -> Option<String> {
        None
    }

    fn send_now(&self, _pane_id: &str, text: &str, _message_id: &str) -> Result<Nudge, DunkError> {
        if self.fail_notify {
            return Err(DunkError::Refused("test notification failure".into()));
        }
        self.notices.lock().unwrap().push(text.to_string());
        Ok(Nudge::default())
    }

    fn record_sent(
        &self,
        _pane_id: Option<&str>,
        _mailbox: &str,
        _sender: Option<&str>,
        _bytes: usize,
        _attachments: usize,
    ) {
    }

    fn tasks_for_pane(&self, _pane_id: &str) -> Result<Value, DunkError> {
        Ok(json!({"tasks": []}))
    }
}

impl crate::pm::PmEnv for PmApiEnv {
    fn config(&self) -> Result<&Env, DunkError> {
        if let Some(message) = &self.config_error {
            return Err(DunkError::Refused(message.clone()));
        }
        Ok(&self.config)
    }
}

impl CommandEnv for PmApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        self.now
    }

    fn pm_env(&self) -> Result<&dyn crate::pm::PmEnv, DunkError> {
        Ok(self)
    }
}

fn pm_dispatcher() -> Dispatcher {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_pm_digest_routing();
    dispatcher
}

#[test]
fn h_pm_routing_matches_python_defaults_and_propagates_bad_config() {
    let (_dir, flock) = staffed_flock("pm-routing");
    let dir = tempfile::tempdir().unwrap();
    let routing_path = dir.path().join("routing.json");
    std::fs::write(
        &routing_path,
        r#"{"poolrad":{"pm":"w2:p2","members":["w2:p2"],"mode":"flag"}}"#,
    )
    .unwrap();
    let routing_path = routing_path.to_string_lossy().into_owned();
    let home = dir.path().to_string_lossy().into_owned();
    let env = PmApiEnv::new(
        Env::from_pairs(&[
            ("HOME", &home),
            ("DUNKINGSHEEP_PM_ROUTING", &routing_path),
            ("DUNKINGSHEEP_VP_PANE", "w9:pH"),
        ]),
        false,
    );
    let dispatcher = pm_dispatcher();
    let result =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w2:p2"), "pm_routing", None)
            .unwrap();
    assert_eq!(
        result["routing"]["poolrad"],
        json!({"pm":"w2:p2", "members":["w2:p2"], "mode":"flag", "vp_pane":"w9:pH"})
    );
    assert_eq!(result["registry"], routing_path);
    assert_eq!(result["vp_pane"], "w9:pH");

    let default_env = PmApiEnv::new(Env::from_pairs(&[("HOME", &home)]), false);
    let defaults =
        dispatch_as_flock_role(&dispatcher, &flock, &default_env, None, "pm_routing", None)
            .unwrap();
    assert_eq!(defaults["routing"], json!({}));
    assert_eq!(defaults["vp_pane"], "w9:pH");
    assert_eq!(
        defaults["registry"],
        dir.path()
            .join(".config/dunkingsheep/pm-routing.json")
            .to_string_lossy()
            .as_ref()
    );

    let invalid_path = dir.path().join("invalid.json");
    std::fs::write(&invalid_path, "{broken").unwrap();
    let invalid_path = invalid_path.to_string_lossy().into_owned();
    let invalid_env = PmApiEnv::new(
        Env::from_pairs(&[("HOME", &home), ("DUNKINGSHEEP_PM_ROUTING", &invalid_path)]),
        false,
    );
    let error = dispatch_as_flock_role(&dispatcher, &flock, &invalid_env, None, "pm_routing", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("PM routing"));
    assert!(error.contains("not readable JSON"));

    let empty_env = PmApiEnv::new(
        Env::from_pairs(&[("HOME", &home), ("DUNKINGSHEEP_PM_ROUTING", "")]),
        false,
    );
    let empty_path_error =
        dispatch_as_flock_role(&dispatcher, &flock, &empty_env, None, "pm_routing", None)
            .unwrap_err()
            .to_string();
    assert!(empty_path_error.contains("not readable JSON"));
}

#[test]
fn h_pm_digest_matches_python_store_update_checkpoint_and_notify_contract() {
    let (_dir, flock) = staffed_flock("pm-digest");
    let dir = tempfile::tempdir().unwrap();
    let checkpoint = dir.path().join("checkpoint.json");
    std::fs::write(
        &checkpoint,
        r#"{"strict_full_c_originals":42,"verified_game_identities":60,"reviewed_change":{"reviewed_commit":"abc123"}}"#,
    )
    .unwrap();
    let home = dir.path().to_string_lossy().into_owned();
    let checkpoint = checkpoint.to_string_lossy().into_owned();
    let env = PmApiEnv::new(
        Env::from_pairs(&[
            ("HOME", &home),
            ("DUNKINGSHEEP_VP_PANE", "w9:pH"),
            ("POOLRAD_CHECKPOINT", &checkpoint),
        ]),
        false,
    );
    let dispatcher = pm_dispatcher();
    let first = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "pm_digest",
        args(json!({"project":"poolrad", "payload":"{\"demo\":true,\"changed\":[\"new source\"],\"numbers\":[{\"name\":\"strict C bodies\",\"value\":1,\"unit\":\"/old\"}]}"})),
    )
    .unwrap();
    assert_eq!(first["action"], "created");
    assert_eq!(first["digest_id"], "pm-digest:poolrad");
    assert_eq!(first["subject"], "PM digest (DEMO): poolrad");
    assert_eq!(first["mailbox"], "VP");
    assert_eq!(first["notified"], true);
    assert_eq!(first["attachments"], json!([]));
    assert!(first["body"].as_str().unwrap().contains("new source"));
    assert!(first["body"]
        .as_str()
        .unwrap()
        .contains("strict C bodies: 42 /60"));
    assert!(!first["body"].as_str().unwrap().contains("/old"));
    assert_eq!(env.notices.lock().unwrap().len(), 1);

    let second = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "pm_digest",
        args(json!({"project":"poolrad", "payload":"{broken"})),
    )
    .unwrap();
    assert_eq!(second["action"], "updated");
    assert_eq!(second["message_id"], first["message_id"]);
    assert!(second["body"].as_str().unwrap().contains("{broken"));
    assert_eq!(second["subject"], "PM digest: poolrad");
    let row_count: i64 = env
        .pool()
        .with(|connection| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE digest_id = 'pm-digest:poolrad'",
                    [],
                    |row| row.get(0),
                )
                .map_err(crate::db::map_sqlite)
        })
        .unwrap();
    assert_eq!(row_count, 1);

    let fallback = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "pm_digest",
        args(json!({"project":"alpha", "payload":"{\"numbers\":[{\"name\":\"payload value\",\"value\":7}]}"})),
    )
    .unwrap();
    assert!(fallback["body"]
        .as_str()
        .unwrap()
        .contains("payload value: 7"));

    let object_sections = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "pm_digest",
        args(json!({"project":"objects", "payload":"{\"changed\":{\"module\":\"routing\"},\"blocked\":{\"ticket\":\"stale\"},\"owner_decisions\":{\"owner\":\"decision\"},\"numbers\":{\"latest\":{\"value\":9}}}"})),
    )
    .unwrap();
    let object_body = object_sections["body"].as_str().unwrap();
    assert!(
        object_body.contains("## Changed\n- module\n"),
        "{object_body}"
    );
    assert!(
        object_body.contains("## Blocked\n- ticket\n"),
        "{object_body}"
    );
    assert!(
        object_body.contains("## Owner decisions\n- owner\n"),
        "{object_body}"
    );
    assert!(
        object_body.contains("## Numbers\n- latest\n"),
        "{object_body}"
    );

    let failed_notify = PmApiEnv::new(
        Env::from_pairs(&[("HOME", &home), ("DUNKINGSHEEP_VP_PANE", "w9:pH")]),
        true,
    );
    let stored = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &failed_notify,
        Some("w2:p2"),
        "pm_digest",
        args(json!({"project":"beta", "payload":"brief"})),
    )
    .unwrap();
    assert_eq!(stored["notified"], false);
    assert_eq!(stored["action"], "created");
    assert!(failed_notify
        .store
        .digest_exists("pm-digest:beta")
        .expect("stored before notification"));
}

#[test]
fn pm_commands_refuse_unwired_and_failed_config_adapters() {
    let (_dir, flock) = staffed_flock("pm-config-errors");
    let dispatcher = pm_dispatcher();
    let env = PmApiEnv {
        config_error: Some("PM config unavailable".into()),
        ..PmApiEnv::new(Env::default(), false)
    };
    let error = dispatch_as_flock_role(&dispatcher, &flock, &env, None, "pm_routing", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("PM config unavailable"));
}

struct ReportApiSources {
    paths: ReportPaths,
    config_dir: PathBuf,
}

impl ReportSources for ReportApiSources {
    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }
    fn report_paths(&self) -> ReportPaths {
        self.paths.clone()
    }
    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone()
    }
    fn task_metrics(&self, _window_s: f64) -> Result<Value, String> {
        Ok(json!({"window":{"seconds":86400},"current":{},"throughput":{},"latency_ms":{}}))
    }
    fn list_panes(&self) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
    fn list_tasks(&self, _limit: usize) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
    fn needs_owner_tasks(&self, _limit: usize) -> Result<Value, String> {
        Ok(json!({"tasks": []}))
    }
    fn lane_registry(&self) -> Result<Value, String> {
        Ok(json!({"lanes": {}}))
    }
    fn review_list(&self) -> Result<Value, String> {
        Ok(json!({"reviews": [], "count": 0, "oldest_age_s": null}))
    }
    fn poolrad_progress(&self) -> Result<Value, String> {
        Ok(json!({"credited": 0}))
    }
    fn post_report(&self, _report: &Value, _text: &str) -> Result<Value, String> {
        Ok(json!({"message_id": "m-test", "transport": "report"}))
    }
}

struct ReportApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    sources: ReportApiSources,
}

impl PaneInfoEnv for ReportApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for ReportApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }
    fn now_s(&self) -> f64 {
        self.sources.now_s()
    }
    fn report_sources(&self) -> Result<&dyn ReportSources, DunkError> {
        Ok(&self.sources)
    }
}

#[test]
fn dispatcher_report_run_list_handlers_use_sources_and_preserve_role_gates() {
    let temp = tempfile::tempdir().unwrap();
    let (_flock_dir, flock) = staffed_flock("report-api");
    let config_dir = temp.path().join("config");
    let sources = ReportApiSources {
        paths: ReportPaths::new(temp.path().join("reports"), temp.path().join("home")),
        config_dir,
    };
    let env = ReportApiEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        sources,
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_report_run_list();

    let mut empty_out_dir = Args::new();
    empty_out_dir.insert("out_dir".to_owned(), json!(""));
    let run = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "report_run",
        Some(empty_out_dir),
    )
    .unwrap();
    let report_path = Path::new(run["path"].as_str().unwrap());
    assert!(report_path.is_file());
    assert_eq!(
        report_path.parent(),
        Some(env.sources.paths.output_dir.as_path())
    );
    assert_eq!(run["posted"]["transport"], "report");
    assert_eq!(run["generated_at"], "2027-01-15T08:00:00Z");

    let mut empty_list_dir = Args::new();
    empty_list_dir.insert("out_dir".to_owned(), json!(""));
    let list = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "report_list",
        Some(empty_list_dir),
    )
    .unwrap();
    assert_eq!(list["count"], 1);

    let denied =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w2:p2"), "report_run", None)
            .unwrap_err();
    assert!(denied.to_string().contains("not permitted for role worker"));
}

#[test]
fn h_report_list_matches_python() {
    use std::fs;
    use std::time::{Duration, SystemTime};

    let temp = tempfile::tempdir().unwrap();
    let (_flock_dir, flock) = staffed_flock("report-list-api");
    let home = temp.path().join("home");
    let output_dir = home.join("reports");
    fs::create_dir_all(&output_dir).unwrap();
    let older = output_dir.join("older.md");
    let newer = output_dir.join("newer.md");
    fs::write(&older, "old report").unwrap();
    fs::write(&newer, "new report").unwrap();
    fs::write(output_dir.join("ignored.txt"), "not a report").unwrap();
    fs::File::open(&older)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(100)),
        )
        .unwrap();
    fs::File::open(&newer)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(200)),
        )
        .unwrap();

    let env = ReportApiEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        sources: ReportApiSources {
            paths: ReportPaths::new(&output_dir, &home),
            config_dir: temp.path().join("config"),
        },
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_report_list();
    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "report_list",
        args(json!({"out_dir": ""})),
    )
    .expect("report list via dispatcher");

    assert_eq!(result["dir"], "~/reports");
    assert_eq!(result["count"], 2);
    assert_eq!(
        result["reports"],
        json!([
            {
                "name": "newer.md",
                "path": newer,
                "size": 10,
                "at": 200.0
            },
            {
                "name": "older.md",
                "path": older,
                "size": 10,
                "at": 100.0
            }
        ])
    );
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

#[test]
fn rust_project_spawn_is_desk_only_and_requires_explicit_runtime_config() {
    let command = crate::registry::command("project_spawn").unwrap();
    assert_eq!(command.roles, &["owner", "desk"]);
    assert!(!command.owner_only);
    assert!(crate::roles::allowed("desk", "project_spawn"));
    assert!(!crate::roles::allowed("web", "project_spawn"));
    assert!(!crate::roles::allowed("worker", "project_spawn"));
    assert!(!crate::roles::allowed("desk", "project_stop"));
    assert!(!crate::roles::allowed("desk", "start_agent"));
    assert!(!crate::roles::allowed("desk", "create_workspace"));
    assert!(!crate::roles::allowed("desk", "close"));

    let (_dir, flock) = flock();
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_project_spawn();
    let request = RequestCtx::default();
    let supplied = args(json!({"name": "budget-app", "brief": "test"}));
    let error = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("desk", false),
        "project_spawn",
        supplied.clone(),
        &request,
        &env,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "project spawn config adapter is not wired",
        "the Rust handler is reached, then refuses before any side effect without explicit runtime config"
    );

    let denied = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "project_spawn",
        supplied,
        &request,
        &env,
    )
    .unwrap_err();
    assert!(denied.to_string().contains("not permitted for role worker"));
}

#[test]
fn invalid_project_budget_refuses_before_opening_the_durable_store() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("project-store.sqlite3");
    let env = ProjectSpawnTestEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        config: ProjectSpawnConfig {
            project_root: dir.path().join("projects"),
            store_path: store_path.clone(),
            budget_json: r#"{"providers":{"codex":{"subscription":null,"credit_fallback":null}}}"#
                .into(),
            provider: "codex".into(),
            provider_home: dir.path().to_path_buf(),
            five_hour_usage: crate::projects::Usage::Percent(10.0),
            provider_reading: None,
            approved: true,
            now: 1_800_000_000.0,
        },
    };
    let (_flock_dir, flock) = flock();
    let request = RequestCtx::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_project_spawn();
    let error = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("desk", false),
        "project_spawn",
        args(json!({"name": "budget-app", "brief": "test"})),
        &request,
        &env,
    )
    .unwrap_err();

    assert_eq!("project budget not configured", error.to_string());
    assert!(
        !store_path.exists(),
        "invalid config must not create SQLite"
    );
}

#[test]
fn missing_provider_policy_refuses_before_opening_the_durable_store() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("project-store.sqlite3");
    let env = ProjectSpawnTestEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        config: ProjectSpawnConfig {
            project_root: dir.path().join("projects"),
            store_path: store_path.clone(),
            budget_json: r#"{"credit_ceiling":null,"providers":{"claude":{"subscription":null,"credit_fallback":null}}}"#.into(),
            provider: "codex".into(),
            provider_home: dir.path().to_path_buf(),
            five_hour_usage: crate::projects::Usage::Percent(10.0),
            provider_reading: None,
            approved: true,
            now: 1_800_000_000.0,
        },
    };
    let (_flock_dir, flock) = flock();
    let request = RequestCtx::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_project_spawn();
    let error = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("desk", false),
        "project_spawn",
        args(json!({"name": "budget-app", "brief": "test"})),
        &request,
        &env,
    )
    .unwrap_err();

    assert_eq!("no budget policy for provider codex", error.to_string());
    assert!(
        !store_path.exists(),
        "missing policy must not create SQLite"
    );
}

#[test]
fn project_spawn_rejects_caller_supplied_authority_paths_and_argv() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("project-store.sqlite3");
    let env = ProjectSpawnTestEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        config: ProjectSpawnConfig {
            project_root: dir.path().join("projects"),
            store_path: store_path.clone(),
            budget_json: String::new(),
            provider: "codex".into(),
            provider_home: dir.path().to_path_buf(),
            five_hour_usage: crate::projects::Usage::Unknown("fixture".into()),
            provider_reading: None,
            approved: false,
            now: 1_800_000_000.0,
        },
    };
    let (_flock_dir, flock) = flock();
    let request = RequestCtx::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_project_spawn();
    let error = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("desk", false),
        "project_spawn",
        args(json!({
            "name":"budget-app",
            "brief":"test",
            "approved":true,
            "provider":"codex",
            "project_root":"/tmp/attacker",
            "argv":["sh","-c","false"]
        })),
        &request,
        &env,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown argument(s)"));
    assert!(!store_path.exists());
}

#[test]
fn stale_and_future_project_usage_refuse_before_opening_the_durable_store() {
    for (label, reading) in [
        (
            "stale",
            json!({
                "provider":"codex", "status":"unknown", "meaning":"percent_used",
                "source":"fixture", "measured_at":1.0, "reason":"stale fixture",
                "windows":[{"name":"300min","window_minutes":300,"percent_used":10.0}]
            }),
        ),
        (
            "future",
            json!({
                "provider":"codex", "status":"ok", "meaning":"percent_used",
                "source":"fixture", "measured_at":1_800_000_100.0,
                "windows":[{"name":"300min","window_minutes":300,"percent_used":10.0}]
            }),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("projects");
        std::fs::create_dir_all(&project_root).unwrap();
        let store_path = dir.path().join("project-store.sqlite3");
        let provider_home = dir.path().join("provider-home");
        std::fs::create_dir_all(&provider_home).unwrap();
        let env = ProjectSpawnTestEnv {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            config: ProjectSpawnConfig {
                project_root,
                store_path: store_path.clone(),
                budget_json: json!({
                    "credit_ceiling": null,
                    "providers": {"codex": {
                        "subscription": {
                            "window":"300min", "max_used_percent":70.0,
                            "max_age_seconds":60.0
                        },
                        "credit_fallback": {
                            "enabled":false,"limit":25.0,"max_age_seconds":60.0
                        }
                    }}
                })
                .to_string(),
                provider: "codex".into(),
                provider_home,
                five_hour_usage: crate::projects::Usage::Percent(10.0),
                provider_reading: Some(reading),
                approved: true,
                now: 1_800_000_000.0,
            },
        };
        let (_flock_dir, flock) = flock();
        let request = RequestCtx::default();
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_project_spawn();
        let error = dispatch_with_env(
            &dispatcher,
            &flock,
            &FixedRole("desk", false),
            "project_spawn",
            args(json!({"name":"budget-app", "brief":"test"})),
            &request,
            &env,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown"), "{label}: {error}");
        assert!(!store_path.exists(), "{label} usage opened SQLite");
    }
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
fn h_get_dunk_matches_python() {
    let (_dir, flock) = flock();
    let env = ParityEnv::new(None);
    let created = flock
        .dunks
        .add(None, &crate::flock::dunks::AddArgs::default())
        .expect("seed dunk");
    let before_snapshot = flock.dunks.snapshot();

    let cache_before = flock.view_cache().view(&env, Some("w1:p1"), true, 3_600.0);
    assert_eq!(cache_before["inbox_count"]["unread"], 1);
    env.inbox_value.set(99);

    let mut dispatcher = Dispatcher::new();
    dispatcher.register_get_dunk();
    assert_eq!(dispatcher.registration_attempts(), &["get_dunk"]);
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };

    let missing_id = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "get_dunk",
        args(json!({})),
        &request,
        &env,
    ));
    assert_eq!(missing_id, "get_dunk: missing required argument(s): id");

    let missing_dunk = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "get_dunk",
        args(json!({"id": "d404"})),
        &request,
        &env,
    ));
    assert_eq!(missing_dunk, "no dunk with id 'd404'");

    let prefixed = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "get_dunk",
        args(json!({"id": "d1"})),
        &request,
        &env,
    )
    .expect("worker can read a dunk by its prefixed id");
    assert_eq!(prefixed, Value::Object(created.clone()));

    // All 29 Python Unicode-16 whitespace scalars, including U+001C..U+001F.
    // Actual Python _find probes cover prefixed/bare success and missing/empty IDs.
    for codepoint in [
        0x9, 0xa, 0xb, 0xc, 0xd, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x85, 0xa0, 0x1680, 0x2000, 0x2001,
        0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a, 0x2028, 0x2029,
        0x202f, 0x205f, 0x3000,
    ] {
        let separator = char::from_u32(codepoint).unwrap();
        for id in ["d1", "1", "d404", ""] {
            let result = dispatch_with_env(
                &dispatcher,
                &flock,
                &FixedRole("worker", false),
                "get_dunk",
                args(json!({"id": format!("{separator}{id}{separator}")})),
                &request,
                &env,
            );
            match id {
                "d1" | "1" => assert_eq!(result.unwrap(), Value::Object(created.clone())),
                "d404" => assert_eq!(refusal(result), "no dunk with id 'd404'"),
                _ => assert_eq!(refusal(result), "no dunk with id ''"),
            }
        }
    }

    let bare_number = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "get_dunk",
        args(json!({"id": 1})),
        &request,
        &env,
    )
    .expect("Python accepts a bare numeric dunk id");
    assert_eq!(bare_number, Value::Object(created));

    let raw_id_error = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "get_dunk",
        args(json!({"id": []})),
        &request,
        &env,
    ));
    assert_eq!(raw_id_error, "no dunk with id '[]'");

    // Byte-exact Python 3.14 repr oracles: Cc/Cf/Cn/Co/Zs/Zl/Zp, printable
    // Unicode, quotes and backslash. Surrogates cannot occur in Rust strings.
    for (codepoint, expected) in [
        (0x0, r###"no dunk with id 'a\x00b'"###),
        (0x9, r###"no dunk with id 'a\tb'"###),
        (0xa, r###"no dunk with id 'a\nb'"###),
        (0xd, r###"no dunk with id 'a\rb'"###),
        (0x1f, r###"no dunk with id 'a\x1fb'"###),
        (0x7f, r###"no dunk with id 'a\x7fb'"###),
        (0x85, r###"no dunk with id 'a\x85b'"###),
        (0xa0, r###"no dunk with id 'a\xa0b'"###),
        (0xad, r###"no dunk with id 'a\xadb'"###),
        (0x378, r###"no dunk with id 'a\u0378b'"###),
        (0x2003, r###"no dunk with id 'a\u2003b'"###),
        (0x200b, r###"no dunk with id 'a\u200bb'"###),
        (0x2028, r###"no dunk with id 'a\u2028b'"###),
        (0x2029, r###"no dunk with id 'a\u2029b'"###),
        (0xe000, r###"no dunk with id 'a\ue000b'"###),
        (0xfffe, r###"no dunk with id 'a\ufffeb'"###),
        (0x10000, r###"no dunk with id 'a𐀀b'"###),
        (0x1f600, r###"no dunk with id 'a😀b'"###),
        (0xe0001, r###"no dunk with id 'a\U000e0001b'"###),
        (0xf0000, r###"no dunk with id 'a\U000f0000b'"###),
        (0x10ffff, r###"no dunk with id 'a\U0010ffffb'"###),
        (0x301, r###"no dunk with id 'áb'"###),
        (0x20, r###"no dunk with id 'a b'"###),
        (0xe9, r###"no dunk with id 'aéb'"###),
        (0x4e2d, r###"no dunk with id 'a中b'"###),
        (0x27, r###"no dunk with id "a'b""###),
        (0x22, r###"no dunk with id 'a"b'"###),
        (0x5c, r###"no dunk with id 'a\\b'"###),
    ] {
        let id = format!("a{}b", char::from_u32(codepoint).unwrap());
        assert_eq!(
            refusal(dispatch_with_env(
                &dispatcher,
                &flock,
                &FixedRole("worker", false),
                "get_dunk",
                args(json!({"id": id})),
                &request,
                &env,
            )),
            expected,
            "codepoint U+{codepoint:04X}"
        );
    }

    let unconfigured_role = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("unknown-role", false),
        "get_dunk",
        args(json!({"id": "d1"})),
        &request,
        &env,
    )
    .expect("unknown roles retain the read-only fallback for get_dunk");
    assert_eq!(
        unconfigured_role,
        Value::Object(flock.dunks.get("d1").unwrap())
    );

    assert_eq!(flock.dunks.snapshot(), before_snapshot);
    let cache_after = flock.view_cache().view(&env, Some("w1:p1"), false, 3_600.0);
    assert_eq!(
        cache_after, cache_before,
        "get_dunk must not invalidate the view cache"
    );
}

#[test]
fn h_add_dunk_assigns_ids_and_resolves_target_labels() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_dunk_mutations();
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "add_dunk",
        args(json!({
            "target": "Writer",
            "text": "nudge {id}",
            "interval_minutes": 2,
            "start": true
        })),
        &request,
        &env,
    )
    .unwrap();

    assert_eq!(result["id"], "d1");
    assert_eq!(result["target_pane_id"], "w1:p1");
    assert_eq!(result["target_agent"], "codex");
    assert_eq!(result["text"], "nudge {id}");
    assert_eq!(result["interval_minutes"], 2.0);
    assert_eq!(result["only_when"], "idle");
    assert_eq!(result["running"], true);
}

#[test]
fn h_add_dunk_uses_python_defaults() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let request = RequestCtx::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_dunk_mutations();
    assert_eq!(
        crate::registry::input_schema("add_dunk").unwrap()["required"],
        json!(["target", "text", "interval_minutes"])
    );
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "add_dunk",
        args(json!({})),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(result["id"], "d1");
    assert_eq!(result["text"], crate::flock::dunks::DEFAULT_TEXT);
    assert_eq!(result["interval_minutes"], 10.0);
    assert_eq!(result["status"], "No target - press c");
    assert_eq!(result["running"], false);
}

#[test]
fn h_update_dunk_changes_fields_and_clears_optionals() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let seed = crate::flock::dunks::AddArgs {
        only_when: Some("idle".into()),
        max_sends: Some("3".into()),
        ..Default::default()
    };
    env.dunks.add(None, &seed).unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_dunk_mutations();
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "update_dunk",
        args(json!({
            "id": "d1",
            "target": "Writer",
            "text": "updated",
            "only_when": null,
            "max_sends": null
        })),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(result["target_pane_id"], "w1:p1");
    assert_eq!(result["text"], "updated");
    assert_eq!(result["only_when"], Value::Null);
    assert_eq!(result["max_sends"], Value::Null);

    // A target-only update is a valid Python call and resolves the pane through the same path.
    let target_only = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "update_dunk",
        args(json!({"id": "d1", "target": "Writer"})),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(target_only["target_pane_id"], "w1:p1");

    let invalid_interval = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "update_dunk",
        args(json!({"id": "d1", "target": "Writer", "interval_minutes": true})),
        &request,
        &env,
    ));
    assert_eq!(invalid_interval, "interval must be a number of minutes");
}

#[test]
fn h_remove_dunk_stops_and_deletes() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let pane = env.panes[0].as_object().unwrap().clone();
    env.dunks
        .add(
            Some(&pane),
            &crate::flock::dunks::AddArgs {
                start: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(env.dunks.list()[0]["running"], true);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_dunk_mutations();
    let request = RequestCtx::default();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "remove_dunk",
        args(json!({"id": "d1"})),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(result["running"], false);
    assert!(env.dunks.list().is_empty());
}

#[test]
fn h_stop_all_matches_python_and_stops_every_running_dunk() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let pane = env.panes[0].as_object().unwrap().clone();
    for text in ["one", "two"] {
        env.dunks
            .add(
                Some(&pane),
                &crate::flock::dunks::AddArgs {
                    text: Some(text.into()),
                    start: true,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_stop_all_fire_list_dunks();
    let request = RequestCtx::default();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "stop_all",
        None,
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(result["dunks"].as_array().unwrap().len(), 2);
    assert!(result["dunks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| !row["running"].as_bool().unwrap()));
    assert_eq!(
        dispatcher.registration_attempts(),
        &["stop_all", "fire_dunk", "list_dunks"]
    );
}

#[test]
fn h_fire_dunk_parses_python_arguments_before_runtime_lookup_and_enforces_owner_gate() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_stop_all_fire_list_dunks();
    let request = RequestCtx::default();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "fire_dunk",
        args(json!({"id": "d3", "countdown_s": "2", "wait": "no"})),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(result, json!({"id": "d3", "countdown_s": 2, "wait": false}));
    assert_eq!(
        *env.fire_calls.lock().unwrap(),
        vec![("d3".into(), 2, false)]
    );

    let invalid = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "fire_dunk",
        args(json!({"id": "missing", "countdown_s": "not-an-int"})),
        &request,
        &env,
    ));
    assert_eq!(
        invalid,
        "invalid literal for int() with base 10: 'not-an-int'"
    );
    assert_eq!(env.fire_calls.lock().unwrap().len(), 1);

    let denied = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "fire_dunk",
        args(json!({"id": "d3"})),
        &request,
        &env,
    ));
    assert_eq!(denied, "fire_dunk needs a superuser");
    assert_eq!(env.fire_calls.lock().unwrap().len(), 1);
}

#[test]
fn dunk_lifecycle_handlers_register_only_this_group_and_refuse_without_provider() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_dunk_lifecycle();
    assert_eq!(
        dispatcher.registration_attempts(),
        &["start_dunk", "stop_dunk", "toggle_dunk"]
    );
    let owner = FixedRole("owner", true);
    for name in ["start_dunk", "stop_dunk", "toggle_dunk"] {
        let error = refusal(dispatch(
            &dispatcher,
            &flock,
            &owner,
            name,
            args(json!({"id": "d1"})),
            None,
        ));
        assert_eq!(error, "dunk lifecycle adapter is not wired", "{name}");
    }
}

#[test]
fn dunk_mutation_dispatcher_registers_only_this_group_and_enforces_role_gate() {
    let (_dir, flock) = flock();
    let env = DunkHandlerEnv::new();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_dunk_mutations();
    assert_eq!(
        dispatcher.registration_attempts(),
        &["add_dunk", "update_dunk", "remove_dunk"]
    );
    let request = RequestCtx::default();
    let error = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "add_dunk",
        args(json!({})),
        &request,
        &env,
    ));
    assert_eq!(error, "add_dunk needs a superuser");

    let invalid_add = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "add_dunk",
        args(json!({
            "target": "missing target",
            "text": "x",
            "interval_minutes": 0
        })),
        &request,
        &env,
    ));
    assert_eq!(invalid_add, "interval must be positive");

    let invalid_update = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "update_dunk",
        args(json!({
            "id": "d1",
            "target": "missing target",
            "interval_minutes": 0
        })),
        &request,
        &env,
    ));
    assert!(invalid_update.contains("no pane matches"));
}

#[test]
fn h_respawn_pane_matches_python() {
    let (_dir, flock) = flock();
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_respawn_pane();
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let gate = FixedRole("owner", true);

    let invalid_argv = json!({
        "pane": "w1:p2",
        "expected_pid": 42,
        "expected_start": "start",
        "argv": "codex",
        "confirm_idle": true,
        "rollback_argv": []
    });
    assert_eq!(
        refusal(dispatch_with_env(
            &dispatcher,
            &flock,
            &gate,
            "respawn_pane",
            args(invalid_argv),
            &request,
            &env,
        )),
        "respawn_pane requires argv as a list of strings"
    );

    let invalid_rollback = json!({
        "pane": "w1:p2",
        "expected_pid": 42,
        "expected_start": "start",
        "argv": ["codex"],
        "confirm_idle": true,
        "rollback_argv": []
    });
    assert_eq!(
        refusal(dispatch_with_env(
            &dispatcher,
            &flock,
            &gate,
            "respawn_pane",
            args(invalid_rollback),
            &request,
            &env,
        )),
        "respawn_pane rollback_argv must be a list of strings"
    );
}

#[test]
fn respawn_pane_registers_one_handler_and_refuses_without_runtime_provider() {
    let (_dir, flock) = flock();
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_respawn_pane();
    assert_eq!(dispatcher.registration_attempts(), &["respawn_pane"]);
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let error = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "respawn_pane",
        args(json!({
            "pane": "w1:p2",
            "expected_pid": 42,
            "expected_start": "start",
            "argv": ["codex"],
            "confirm_idle": true
        })),
        &request,
        &env,
    ));
    assert_eq!(error, "respawn pane adapter is not wired");
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
        if command.dispatch_required.is_empty() {
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
        .find(|c| c.owner_only && c.dispatch_required.is_empty() && roles::allowed("web", c.name))
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
    // Rust-only owner decisions have no counterpart in the frozen Python registry.
    want.extend(["task_owner_answer", "task_owner_question"]);
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
fn h_list_dunks_matches_python_and_honors_fresh() {
    let (_dir, flock) = flock();
    let env = ParityEnv::new(None);
    let mut d = Dispatcher::new();
    d.register_stop_all_fire_list_dunks();
    let role = FixedRole("anonymous", false);

    env.dunks
        .add(
            None,
            &crate::flock::dunks::AddArgs {
                text: Some("first".into()),
                ..Default::default()
            },
        )
        .expect("first dunk");
    let first = dispatch_with_env(
        &d,
        &flock,
        &role,
        "list_dunks",
        None,
        &RequestCtx::default(),
        &env,
    )
    .expect("list dunks");
    assert_eq!(first["dunks"].as_array().unwrap().len(), 1);
    assert_eq!(first["dunks"][0]["text"], "first");

    env.dunks
        .add(
            None,
            &crate::flock::dunks::AddArgs {
                text: Some("second".into()),
                ..Default::default()
            },
        )
        .expect("second dunk");
    let cached = dispatch_with_env(
        &d,
        &flock,
        &role,
        "list_dunks",
        None,
        &RequestCtx::default(),
        &env,
    )
    .expect("cached list dunks");
    assert_eq!(cached["dunks"].as_array().unwrap().len(), 1);

    let fresh = dispatch_with_env(
        &d,
        &flock,
        &role,
        "list_dunks",
        args(json!({"fresh": true})),
        &RequestCtx::default(),
        &env,
    )
    .expect("fresh list dunks");
    assert_eq!(fresh["dunks"].as_array().unwrap().len(), 2);
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
fn h_turn_end_matches_python() {
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
        caller: Some(caller.clone()),
        ..RequestCtx::default()
    };
    let env = ParityEnv::new(Some(pid));
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_status_whoami_turn_start();
    dispatcher.register_turn_end_poll_roster();

    let started = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"source": "test"})),
        &request,
        &env,
    )
    .expect("start turn");
    let refused = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "turn_end",
        args(json!({"pane": "w2:p2", "turn_id": started["turn_id"]})),
        &request,
        &env,
    )
    .unwrap_err();
    assert!(refused.to_string().contains("turn event must target"));
    assert!(!env
        .turn_calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("record turn.end")));

    let ended = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "turn_end",
        args(json!({"turn_id": started["turn_id"]})),
        &request,
        &env,
    )
    .expect("end authenticated turn");
    assert_eq!(ended["pane_id"], pane_id);
    assert_eq!(ended["ended"], true);
    assert_eq!(ended["reason"], "turn_end");
    assert_eq!(ended["dispatch"]["action"], "skip");
    assert!(env
        .turn_calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.starts_with("record turn.end")));
}

#[test]
fn h_poll_matches_python() {
    use crate::inbox_store::Outgoing;

    let (_dir, flock) = flock();
    let env = ParityEnv::new(None);
    let mut ids = Vec::new();
    for index in 0..6 {
        ids.push(
            env.inbox_store
                .send(&Outgoing {
                    mailbox: "worker",
                    body: Some("poll unread message"),
                    subject: Some(&format!("message {index}")),
                    ..Outgoing::default()
                })
                .expect("seed unread message"),
        );
    }
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_turn_end_poll_roster();
    assert_eq!(
        dispatcher.registration_attempts(),
        &["turn_end", "poll", "roster"]
    );

    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"mailbox": "w1:p1"})),
        &request,
        &env,
    )
    .expect("poll sidebar data");
    assert_eq!(result["status"]["self_pane_id"], "w1:p1");
    assert_eq!(result["status"]["herdr_available"], Value::Null);
    assert_eq!(
        env.probes.get(),
        0,
        "poll must not probe backend availability"
    );
    assert_eq!(result["inbox_count"]["unread"], 6);
    assert_eq!(result["dunks"]["messages"].as_array().unwrap().len(), 5);
    assert!(ids.iter().any(|id| result["dunks"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["message_id"] == *id)));
    let after = flock
        .inbox_count(&env, Some("w1:p1"), Some("w1:p1"))
        .expect("count after poll");
    assert_eq!(after.unread, 6, "poll must not mark unread messages read");

    let missing_self = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"limit": []})),
        &RequestCtx::default(),
        &env,
    )
    .expect("inbox count errors precede limit conversion and are partial results");
    assert!(missing_self["error"]
        .as_str()
        .unwrap()
        .contains("no mailbox given"));
    assert!(!missing_self["error"]
        .as_str()
        .unwrap()
        .contains("int() argument"));
    assert!(missing_self.get("inbox_count").is_none());
    assert_eq!(missing_self["dunks"]["messages"], json!([]));

    let list_error = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"mailbox": "w2:p2"})),
        &request,
        &env,
    )
    .expect("inbox list domain errors remain partial poll results");
    assert!(list_error.get("inbox_count").is_some());
    assert!(list_error["error"].as_str().unwrap().contains("not yours"));
    assert_eq!(list_error["dunks"]["messages"], json!([]));

    let bad_limit_type = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"mailbox": "w1:p1", "limit": []})),
        &request,
        &env,
    )
    .unwrap_err();
    assert!(bad_limit_type.to_string().contains("int() argument"));

    let bad_limit_value = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"mailbox": "w1:p1", "limit": "not-a-number"})),
        &request,
        &env,
    )
    .unwrap_err();
    assert!(bad_limit_value
        .to_string()
        .contains("invalid literal for int()"));

    let overflow_limit = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"mailbox": "w1:p1", "limit": "9223372036854775808"})),
        &request,
        &env,
    )
    .unwrap_err();
    assert!(overflow_limit
        .to_string()
        .contains("too large to convert to SQLite INTEGER"));

    let panes = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"mailbox": "w1:p1", "panes": "yes"})),
        &request,
        &env,
    )
    .expect("opt-in pane data");
    assert_eq!(panes["panes"]["panes"].as_array().unwrap().len(), 1);
}

#[test]
fn h_poll_tasks_matches_python_wire_defaults_and_visibility() {
    let (dir, flock) = flock();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("task runtime");
    let tasks = Arc::new(
        runtime
            .block_on(Tasks::open(dir.path().join("tasks.db")))
            .expect("task store"),
    );
    runtime
        .block_on(tasks.allocation_init())
        .expect("task init");
    let global = runtime
        .block_on(tasks.add("global", "global poll task", "full wire body"))
        .expect("global task");
    let private = runtime
        .block_on(tasks.add("w2:p2", "other private task", "private body"))
        .expect("private task");
    flock.attach_stores(Stores {
        tasks: Some(tasks),
        ..Stores::default()
    });
    let env = ParityEnv::new(None);
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_turn_end_poll_roster();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "poll",
        args(json!({"tasks": true, "task_scope": "all"})),
        &request,
        &env,
    )
    .expect("opt-in full task wire");
    let listed = result["tasks"]["tasks"].as_array().expect("task list");
    assert!(listed.iter().any(|task| task["id"] == global.id));
    assert!(!listed.iter().any(|task| task["id"] == private.id));
    assert!(
        listed.iter().find(|task| task["id"] == global.id).unwrap()["body"]
            .as_str()
            .is_some()
    );
    assert_eq!(result["tasks"]["pane_id"], "w1:p1");
}

#[test]
fn h_roster_matches_python() {
    let (_dir, flock) = flock();
    let env = ParityEnv::new(None);
    let request = RequestCtx::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_turn_end_poll_roster();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "roster",
        args(json!({})),
        &request,
        &env,
    )
    .expect("roster reads actual roster adapter");
    assert_eq!(result["lanes"].as_array().unwrap().len(), 1);
    assert_eq!(result["lanes"][0]["pane_id"], "w1:p1");
    assert_eq!(result["lanes"][0]["state"], "idle");
    assert_eq!(result["lanes"][0]["name"], "worker");
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
    let service = Arc::new(
        crate::memory::service::MemoryService::with_clock(
            &path,
            1,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
            Arc::new(FixedMemoryClock),
        )
        .expect("memory service"),
    );
    let (_dir, flock) = flock();
    flock
        .attach_memory_service(service)
        .expect("attach canonical memory service");
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
fn h_memory_stats_matches_python() {
    let (_dir, flock, env) = memory_api_fixture();
    let service = flock.memory_service().unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_stats_status_ingest();
    let result = dispatch_memory(&dispatcher, &flock, &env, "memory_stats", json!({})).unwrap();
    assert_eq!(result, service.stats().unwrap());
    assert_eq!(result["sources"], 0);
    assert_eq!(result["sessions"], 1);
    assert_eq!(result["chunks"], 1);
    assert_eq!(result["queue_depth"], 0);
    assert_eq!(result["queries"]["count"], 0);
}

#[test]
fn h_memory_status_matches_python() {
    let (_dir, flock, env) = memory_api_fixture();
    let service = flock.memory_service().unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_stats_status_ingest();
    let result = dispatch_memory(&dispatcher, &flock, &env, "memory_status", json!({})).unwrap();
    assert_eq!(result, service.status().unwrap());
    assert_eq!(result["ok"], true);
    assert_eq!(result["ingestor"]["running"], false);
}

#[test]
fn h_memory_ingest_matches_python() {
    let (dir, flock, env) = memory_api_fixture();
    let source_dir = dir.path().join(".codex");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("thread_history.jsonl");
    std::fs::write(
        &source,
        "{\"type\":\"session\",\"id\":\"queued-session\",\"harness\":\"codex\",\"project\":\"demo\"}\n",
    )
    .unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_stats_status_ingest();
    let request = RequestCtx::default();
    let result = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "memory_ingest",
        args(json!({
            "path": source.to_string_lossy(),
            "harness": "",
            "project": ""
        })),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(
        result,
        json!({
            "source_id": 1,
            "queue_id": 1,
            "path": source.to_string_lossy(),
            "harness": "codex",
            "project": null
        })
    );
    let status = flock.memory_service().unwrap().status().unwrap();
    assert_eq!(status["queue"]["queued"], 1);
    assert_eq!(status["ingestor"]["running"], false);
    let persisted_harness = flock
        .memory_service()
        .unwrap()
        .read_pool()
        .with(|connection| {
            Ok(connection
                .query_row("SELECT harness FROM sources WHERE id=1", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("persisted enqueue harness"))
        })
        .unwrap();
    assert_eq!(persisted_harness, "codex");
}

#[test]
fn memory_handlers_share_attached_service() {
    let (_dir, flock, env) = memory_api_fixture();
    let service = flock.memory_service().unwrap();
    assert!(Arc::ptr_eq(&flock.memory_service().unwrap(), &service));
    let stores = flock.stores();
    assert!(Arc::ptr_eq(
        stores.memory.as_ref().unwrap(),
        service.read_pool()
    ));
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();
    dispatcher.register_memory_stats_status_ingest();
    dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_recall",
        json!({"query": "adapter", "limit": 1}),
    )
    .unwrap();
    dispatch_memory(
        &dispatcher,
        &flock,
        &env,
        "memory_search",
        json!({"query": "adapter", "limit": 1}),
    )
    .unwrap();
    let stats = dispatch_memory(&dispatcher, &flock, &env, "memory_stats", json!({})).unwrap();
    assert!(stats["queries"]["cache_hits"].as_u64().unwrap_or_default() >= 1);
}

#[test]
fn memory_service_attachment_rejects_replacement() {
    let (_dir, flock, _) = memory_api_fixture();
    let original = flock.memory_service().unwrap();
    let replacement_dir = tempfile::tempdir().unwrap();
    let replacement_path = replacement_dir.path().join("other-memory.db");
    let connection = crate::db::open_writer(&replacement_path).unwrap();
    crate::memory::recall::apply_schema(&connection).unwrap();
    drop(connection);
    let replacement = Arc::new(
        crate::memory::service::MemoryService::open(
            &replacement_path,
            1,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        )
        .unwrap(),
    );

    let error = flock.attach_memory_service(replacement).unwrap_err();
    assert!(error
        .to_string()
        .contains("a different memory service is already attached"));
    assert!(Arc::ptr_eq(&flock.memory_service().unwrap(), &original));
    assert!(Arc::ptr_eq(
        flock.stores().memory.as_ref().unwrap(),
        original.read_pool()
    ));
}

#[test]
fn memory_handlers_refuse_without_service() {
    let (_dir, flock) = flock();
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_recall_search_context();
    dispatcher.register_memory_stats_status_ingest();
    for command in [
        "memory_stats",
        "memory_status",
        "memory_recall",
        "memory_search",
        "memory_context",
    ] {
        let input = if matches!(command, "memory_stats" | "memory_status") {
            json!({})
        } else {
            json!({"query":"adapter"})
        };
        let error = refusal(dispatch_memory(&dispatcher, &flock, &env, command, input));
        assert!(
            error.contains("memory service is not enabled"),
            "{command}: {error}"
        );
    }
    let error = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "memory_ingest",
        args(json!({"path":"missing.jsonl"})),
        &RequestCtx::default(),
        &env,
    ));
    assert!(error.contains("memory service is not enabled"), "{error}");
}

#[test]
fn memory_ingest_only_enqueues_on_canonical_ingestor() {
    let (dir, flock, env) = memory_api_fixture();
    let service = flock.memory_service().unwrap();
    let source = dir.path().join("enqueue.jsonl");
    std::fs::write(&source, "{}").unwrap();
    let expected = service
        .enqueue(source.to_str().unwrap(), Some("codex"), Some("demo"))
        .unwrap();
    let second = dir.path().join("enqueue-second.jsonl");
    std::fs::write(&second, "{}").unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_stats_status_ingest();
    let request = RequestCtx::default();
    let actual = dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("owner", true),
        "memory_ingest",
        args(json!({
            "path": second.to_string_lossy(),
            "harness": "codex",
            "project": "demo"
        })),
        &request,
        &env,
    )
    .unwrap();
    assert_eq!(
        actual,
        json!({
            "source_id": expected.0 + 1,
            "queue_id": expected.1 + 1,
            "path": second.to_string_lossy(),
            "harness": "codex",
            "project": "demo"
        })
    );
    assert_eq!(service.status().unwrap()["ingestor"]["running"], false);
    let enqueued = service
        .read_pool()
        .with(|connection| {
            Ok(connection
                .query_row(
                    "SELECT path, harness, project FROM sources WHERE path=?1",
                    [second.to_string_lossy().as_ref()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .expect("queued source"))
        })
        .unwrap();
    assert_eq!(
        enqueued,
        (
            second.to_string_lossy().into_owned(),
            "codex".into(),
            "demo".into()
        )
    );
}

#[test]
fn memory_ingest_preserves_required_and_enqueue_errors() {
    let (_dir, flock, env) = memory_api_fixture();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_stats_status_ingest();
    let request = RequestCtx::default();
    let owner = FixedRole("owner", true);
    let missing = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &owner,
        "memory_ingest",
        args(json!({})),
        &request,
        &env,
    ));
    assert_eq!(missing, "memory_ingest: missing required argument(s): path");

    let nonexistent = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &owner,
        "memory_ingest",
        args(json!({"path":"does-not-exist.jsonl"})),
        &request,
        &env,
    ));
    assert!(
        nonexistent.contains("source does not exist:"),
        "{nonexistent}"
    );
}

#[test]
fn memory_handlers_preserve_registry_roles() {
    let (_dir, flock, env) = memory_api_fixture();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_memory_stats_status_ingest();
    for role in [
        "owner",
        "system",
        "web",
        "desk",
        "planner",
        "worker",
        "reviewer",
        "anonymous",
    ] {
        for command in ["memory_stats", "memory_status"] {
            dispatch_with_env(
                &dispatcher,
                &flock,
                &FixedRole(role, false),
                command,
                args(json!({})),
                &RequestCtx::default(),
                &env,
            )
            .unwrap_or_else(|error| panic!("{role} could not call {command}: {error}"));
        }
    }
    let denied = refusal(dispatch_with_env(
        &dispatcher,
        &flock,
        &FixedRole("worker", false),
        "memory_ingest",
        args(json!({"path":"missing.jsonl"})),
        &RequestCtx::default(),
        &env,
    ));
    assert!(denied.contains("needs a superuser"), "{denied}");
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
    fail_path: bool,
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
        if self.fail_path {
            return Err(DunkError::Refused("archive configuration missing".into()));
        }
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
        fail_path: false,
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
fn retire_api_defers_path_failure_until_action_needs_archive() {
    let (dir, flock) = staffed_flock("api-retire-path-error");
    let env = RetirePaneApiEnv {
        inner: pane_api_env(),
        retired_path: dir.path().join("unused"),
        fail_path: true,
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_retire_pane();
    let call = |args: serde_json::Map<String, Value>| {
        dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            "retire_pane",
            Some(args),
        )
    };
    let invalid = call(serde_json::Map::from_iter([(
        "action".into(),
        json!("bad"),
    )]))
    .unwrap_err();
    assert!(invalid.to_string().contains("retire action"));
    let missing_pane = call(serde_json::Map::from_iter([(
        "action".into(),
        json!("unretire"),
    )]))
    .unwrap_err();
    assert!(missing_pane.to_string().contains("'pane' is required"));
    let missing_reason = call(serde_json::Map::from_iter([
        ("action".into(), json!("retire")),
        ("pane".into(), json!("w1:p1")),
    ]))
    .unwrap_err();
    assert!(missing_reason.to_string().contains("'reason' is required"));
    let list_error = call(serde_json::Map::from_iter([(
        "action".into(),
        json!("list"),
    )]))
    .unwrap_err();
    assert!(list_error
        .to_string()
        .contains("archive configuration missing"));
    assert!(!env.retired_path.exists());
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

    let mut worker =
        dispatch_as_flock_role(&dispatcher, &flock, &env, Some("w1:p1"), "gh_token", None).unwrap();
    let provenance = worker.as_object_mut().unwrap().remove("provenance");
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
    assert_eq!(Some(json!({"recorded": true})), provenance);
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
fn h_gh_token_matches_python() {
    let (dir, flock) = github_token_flock("gh-token-system-default");
    let env = github_token_api_env(dir.path());
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_gh_token();
    let system_token = flock
        .token_issue("system", Some("test-service"), Some("w1:p3"), &env)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let request = RequestCtx {
        service_token: Some(system_token),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let system = dispatcher
        .dispatch(&context, &FlockGate(&context), "gh_token", None)
        .unwrap();

    assert_eq!(
        json!({
            "token": "fake-reviewer-token",
            "kind": "reviewer",
            "role": "system",
            "slug": "reviewer-app",
            "expires_at": 4_102_444_800.0,
            "cached": true
        }),
        system
    );
    assert!(flock.gh_token_mints.lock().unwrap().is_empty());
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
    tokio::runtime::Runtime,
    tempfile::TempDir,
    Flock,
    Arc<crate::tasks::Tasks>,
    Dispatcher,
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
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
    (runtime, dir, flock, tasks, dispatcher)
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

fn task_mutation_api_fixture() -> (
    tokio::runtime::Runtime,
    tempfile::TempDir,
    Arc<Flock>,
    Arc<crate::tasks::Tasks>,
    Dispatcher,
) {
    let dir = tempfile::tempdir().expect("task mutation tempdir");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("independently driven multi-thread task runtime");
    let tasks = Arc::new(
        runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .expect("temporary task store actor"),
    );
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    ));
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("roles");
    for (pane, role) in [("w1:p1", "worker"), ("w1:p2", "owner"), ("w1:p3", "worker")] {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .expect("role registration");
    }
    flock.attach_roles(roles, None);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Stores::default()
    });
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_task_mutation_leases();
    dispatcher.register_task_statistics();
    (runtime, dir, flock, tasks, dispatcher)
}

fn dispatch_task_mutation(
    dispatcher: &Dispatcher,
    flock: &Flock,
    runtime: &tokio::runtime::Runtime,
    name: &str,
    input: Value,
    pane: Option<&str>,
    caller: Option<Caller>,
) -> Result<Value, DunkError> {
    let request = RequestCtx {
        caller,
        self_pane_id: pane.map(str::to_string),
        ..RequestCtx::default()
    };
    dispatch_task_mutation_request(dispatcher, flock, runtime, name, input, &request)
}

fn dispatch_task_mutation_request(
    dispatcher: &Dispatcher,
    flock: &Flock,
    runtime: &tokio::runtime::Runtime,
    name: &str,
    input: Value,
    request: &RequestCtx,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let context = HandlerContext {
        flock,
        request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    dispatcher.dispatch(&context, &gate, name, args(input))
}

fn task_test_caller(pane: &str, pid: i64, start: &str, name: &str) -> Caller {
    Caller::new(
        Some(pid),
        Some(start.to_string()),
        Some(name.to_string()),
        Some(pane.to_string()),
        "agent",
    )
}

#[test]
fn api_t39_task_update_and_assign_preserve_dispatch_roles_values_and_full_wire() {
    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    let task = runtime
        .block_on(tasks.add("global", "before", "body"))
        .expect("seed task");
    let dep_a = runtime
        .block_on(tasks.add("global", "dependency a", ""))
        .expect("dependency a");
    let dep_b = runtime
        .block_on(tasks.add("global", "dependency b", ""))
        .expect("dependency b");
    let updated = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({
            "id": task.id,
            "title": "after",
            "assignees": [" w1:p1 ", "", 42, true, null, {"k": 1}, ["x"]],
            "depends_on": format!(" {}, , {} ", dep_a.id, dep_b.id),
            "multi_assignee": " yes "
        }),
        Some("w1:p2"),
        None,
    )
    .expect("owner update");
    assert_eq!(updated["title"], "after");
    assert_eq!(
        updated["assignees"],
        json!(["w1:p1", "42", "True", "None", "{'k': 1}", "['x']"])
    );
    assert_eq!(updated["depends_on"], json!([dep_a.id, dep_b.id]));
    assert_eq!(updated["multi_assignee"], true);
    assert!(updated["children"].is_array());
    assert!(updated["leases"].is_array());
    assert!(updated.get("claimable").is_some());

    // Python None is a no-op for _task_values; an empty list explicitly clears it.
    let unchanged = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": task.id, "assignees": null}),
        Some("w1:p2"),
        None,
    )
    .expect("None assignee update is a no-op");
    assert_eq!(unchanged["assignees"], updated["assignees"]);
    let cleared = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": task.id, "assignees": []}),
        Some("w1:p2"),
        None,
    )
    .expect("empty assignee list clears");
    assert_eq!(cleared["assignees"], json!([]));

    let assigned = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_assign",
        json!({"id": task.id, "assignees": "w1:p1, w1:p3"}),
        Some("w1:p2"),
        None,
    )
    .expect("owner assignment with default mode");
    assert_eq!(assigned["assignees"], json!(["w1:p1", "w1:p3"]));
    assert!(assigned["children"].is_array());
    let falsey_mode_defaults = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_assign",
        json!({"id": task.id, "assignees": ["w1:p1"], "mode": []}),
        Some("w1:p2"),
        None,
    )
    .expect("Python falsey mode defaults to add");
    assert_eq!(falsey_mode_defaults["assignees"], json!(["w1:p1", "w1:p3"]));

    let private_assignment = runtime
        .block_on(tasks.add("w1:p3", "private assignment", ""))
        .expect("private assignment task");
    let private_mode_error = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_assign",
        json!({"id": private_assignment.id, "assignees": ["w1:p1"], "mode": [1]}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(private_mode_error.contains("belongs to another pane's private queue"));
    let empty_assignees = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_assign",
        json!({"id": task.id, "assignees": [], "mode": [1]}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(empty_assignees.contains("at least one assignee is required"));
    let falsey_mode_empty_assignees = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_assign",
        json!({"id": task.id, "assignees": [], "mode": []}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(falsey_mode_empty_assignees.contains("at least one assignee is required"));

    let parent = runtime
        .block_on(tasks.add("global", "parent for coercion", ""))
        .expect("coercion parent");
    let coercion_task = runtime
        .block_on(tasks.add_with(
            "global",
            "before coercion",
            "before body",
            crate::tasks::AddOptions {
                parent_id: Some(parent.id.clone()),
                ..Default::default()
            },
        ))
        .expect("coercion task");
    let coerced = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({
            "id": coercion_task.id,
            "title": 42,
            "body": false,
            "parent_id": 0
        }),
        Some("w1:p2"),
        None,
    )
    .expect("Python stringifies title/body and clears falsey parent ids");
    assert_eq!(coerced["title"], "42");
    assert_eq!(coerced["body"], "False");
    assert_eq!(coerced["parent_id"], Value::Null);
    let nonstring_status = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": coercion_task.id, "status": 42}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(nonstring_status.contains("status must be one of"));
    let nonstring_priority = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": coercion_task.id, "priority": true}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(nonstring_priority.contains("priority must be one of"));

    let raw_reason = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": task.id, "block_reason": true}),
        Some("w1:p2"),
        None,
    )
    .expect("nonstring block reason keeps SQLite scalar affinity");
    assert_eq!(raw_reason["block_reason"], "1");
    let invalid_raw_reason = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": task.id, "title": "must roll back", "block_reason": {"x": 1}}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(invalid_raw_reason.contains("Error binding parameter 2: type 'dict' is not supported"));
    let after_failed_raw_reason = runtime.block_on(tasks.get(&task.id)).unwrap();
    assert_eq!(after_failed_raw_reason.title, "after");
    assert_eq!(after_failed_raw_reason.block_reason.as_deref(), Some("1"));

    for (fields, ordinal) in [
        (json!({}), 1),
        (json!({"body": "rollback", "priority": "high"}), 3),
        (json!({"status": "cancelled"}), 3),
        (json!({"status": "blocked"}), 5),
        (json!({"title": "rollback", "status": "done"}), 6),
        (
            json!({"parent_id": null, "assignees": [], "multi_assignee": false, "depends_on": []}),
            5,
        ),
    ] {
        let before = runtime.block_on(tasks.events(&task.id));
        let mut input = fields.as_object().unwrap().clone();
        input.insert("id".into(), json!(task.id));
        input.insert("block_reason".into(), json!([]));
        let error = dispatch_task_mutation(
            &dispatcher,
            &flock,
            &runtime,
            "task_update",
            Value::Object(input),
            Some("w1:p2"),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(&format!(
                "Error binding parameter {ordinal}: type 'list' is not supported"
            )),
            "{error}"
        );
        assert_eq!(runtime.block_on(tasks.events(&task.id)), before);
        assert_eq!(
            runtime.block_on(tasks.get(&task.id)).unwrap().title,
            "after"
        );
    }

    let role_refusal = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": task.id, "status": "done", "title": 7}),
        Some("w1:p1"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(role_refusal.contains("task update cannot change status for role worker"));
    let assignment_refusal = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_assign",
        json!({"id": task.id, "assignees": ["w1:p3"]}),
        Some("w1:p1"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(assignment_refusal.contains("not permitted for role worker"));
    assert_eq!(
        runtime.block_on(tasks.get(&task.id)).unwrap().status,
        "open"
    );

    let terminal = runtime
        .block_on(tasks.add("global", "terminal update guard", "before"))
        .expect("terminal guard task");
    runtime
        .block_on(tasks.complete(&terminal.id))
        .expect("finish guard task");
    let refused_update = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_update",
        json!({"id": terminal.id, "title": "must roll back", "status": "open"}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(refused_update.contains("finished task is not reopened"));
    assert_eq!(
        runtime.block_on(tasks.get(&terminal.id)).unwrap().title,
        "terminal update guard",
        "field edits roll back with the refused status transition"
    );
}

#[test]
fn api_t39_task_note_preserves_raw_event_meta_and_null_actor_without_task_lookup() {
    let (runtime, dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("roles");
    // Replace the attached role store with an owner token so the trusted caller has no pane.
    let owner_token = roles
        .issue_token("owner", Some("fixture-owner"), Some("api test"))
        .expect("owner token");
    flock.attach_roles(roles, None);
    let request = RequestCtx {
        service_token: Some(owner_token),
        ..RequestCtx::default()
    };

    let noted = dispatch_task_mutation_request(
        &dispatcher,
        &flock,
        &runtime,
        "task_note",
        json!({"id": "t-missing-is-allowed", "event": true, "reason": {"source": ["x"]}}),
        &request,
    )
    .expect("note may refer to a missing task");
    assert_eq!(noted["noted"], "t-missing-is-allowed");
    assert_eq!(noted["event"], true, "echo the original JSON event value");
    assert_eq!(noted["actor"], Value::Null, "no pane means SQL NULL actor");
    assert_eq!(noted["meta"]["reason"], json!({"source": ["x"]}));

    let defaulted = dispatch_task_mutation_request(
        &dispatcher,
        &flock,
        &runtime,
        "task_note",
        json!({"id": "t-missing-is-allowed", "event": "", "reason": 7}),
        &request,
    )
    .expect("falsey event defaults and truthy numeric reason remains JSON");
    assert_eq!(defaulted["event"], "note");
    assert_eq!(defaulted["meta"]["reason"], 7);

    let numeric_event = dispatch_task_mutation_request(
        &dispatcher,
        &flock,
        &runtime,
        "task_note",
        json!({"id": "t-missing-is-allowed", "event": 5, "reason": []}),
        &request,
    )
    .expect("truthy numeric event binds as TEXT and falsey reason is discarded");
    assert_eq!(numeric_event["event"], 5);
    assert_eq!(numeric_event["meta"], json!({}));

    let connection = rusqlite::Connection::open(tasks.path()).expect("read task DB");
    let rows: Vec<(String, Option<String>, String)> = connection
        .prepare("SELECT event, actor, meta FROM task_events WHERE task_id = ? ORDER BY id")
        .unwrap()
        .query_map(["t-missing-is-allowed"], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows[0].0, "1", "SQLite TEXT affinity binds true as 1");
    assert_eq!(rows[0].1, None);
    assert_eq!(
        serde_json::from_str::<Value>(&rows[0].2).unwrap()["reason"],
        json!({"source": ["x"]})
    );
    assert_eq!(rows[1].0, "note");
    assert_eq!(rows[1].1, None);
    assert_eq!(
        serde_json::from_str::<Value>(&rows[1].2).unwrap()["reason"],
        7
    );
    assert_eq!(rows[2].0, "5");
    assert_eq!(rows[2].1, None);
    assert_eq!(
        serde_json::from_str::<Value>(&rows[2].2).unwrap(),
        json!({})
    );

    let before = rows.len();
    for (event, kind) in [(json!(["x"]), "list"), (json!({"x": 1}), "dict")] {
        let binding = dispatch_task_mutation_request(
            &dispatcher,
            &flock,
            &runtime,
            "task_note",
            json!({"id": "t-missing-is-allowed", "event": event}),
            &request,
        )
        .unwrap_err()
        .to_string();
        assert!(binding.contains(&format!(
            "Error binding parameter 2: type '{kind}' is not supported"
        )));
    }
    let after: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM task_events WHERE task_id = ?",
            ["t-missing-is-allowed"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after as usize, before, "failed note binding adds no event");
}

#[test]
fn api_t39_task_block_cancel_bind_raw_reasons_after_guards_and_return_dependents() {
    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    let task = runtime
        .block_on(tasks.add("global", "blocked root", ""))
        .expect("root");
    let dependent = runtime
        .block_on(tasks.add_with(
            "global",
            "dependent",
            "",
            crate::tasks::AddOptions {
                depends_on: vec![task.id.clone()],
                ..Default::default()
            },
        ))
        .expect("dependent");
    let private = runtime
        .block_on(tasks.add("w1:p3", "private task", ""))
        .expect("private task");

    let blocked = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_block",
        json!({"id": task.id, "reason": false}),
        Some("w1:p2"),
        None,
    )
    .expect("raw false reason uses SQLite scalar affinity");
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["block_reason"], "0");
    let connection = rusqlite::Connection::open(tasks.path()).expect("read task DB");
    let meta: Option<String> = connection
        .query_row(
            "SELECT meta FROM task_events WHERE task_id = ? AND event = 'blocked' ORDER BY id DESC LIMIT 1",
            [&task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(meta, None, "status event adds no synthetic reason meta");

    let events_before = runtime.block_on(tasks.events(&task.id));
    for (reason, kind) in [
        (json!(["bad binding"]), "list"),
        (json!({"bad": true}), "dict"),
    ] {
        let composite = dispatch_task_mutation(
            &dispatcher,
            &flock,
            &runtime,
            "task_cancel",
            json!({"id": task.id, "reason": reason}),
            Some("w1:p2"),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(composite.contains(&format!(
            "Error binding parameter 3: type '{kind}' is not supported"
        )));
        assert_eq!(
            runtime.block_on(tasks.get(&task.id)).unwrap().status,
            "blocked"
        );
        assert_eq!(runtime.block_on(tasks.events(&task.id)), events_before);
    }

    let visibility = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_cancel",
        json!({"id": private.id, "reason": []}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(visibility.contains("belongs to another pane's private queue"));

    let cancelled = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_cancel",
        json!({"id": task.id, "reason": 0}),
        Some("w1:p2"),
        None,
    )
    .expect("cancel response uses post-commit full wire");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["block_reason"], "0");
    assert_eq!(cancelled["blocked_dependents"], json!([dependent.id]));
    assert!(cancelled.get("children").is_some());

    let terminal_binding = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_block",
        json!({"id": task.id, "reason": []}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(terminal_binding.contains("finished task is not reopened"));
}

#[test]
fn api_t39_task_next_heartbeat_release_preserve_caller_and_lease_envelopes() {
    assert_eq!(
        python_float(&json!(" 30 ")).expect("Python whitespace float"),
        30.0
    );
    assert_eq!(
        python_float(&json!("3_0")).expect("Python underscore float"),
        30.0
    );
    assert_eq!(
        python_float(&json!("٣٠")).expect("Python Unicode digit float"),
        30.0
    );
    assert!(python_float(&json!("3__0")).is_err());
    assert!(python_float(&json!("Infinity"))
        .expect("Python nonfinite float")
        .is_infinite());

    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    let first = runtime
        .block_on(tasks.add("global", "first claim", ""))
        .expect("first task");
    let second = runtime
        .block_on(tasks.add("global", "second claim", ""))
        .expect("second task");
    let caller = task_test_caller("w1:p1", 4321, "98765", "codex-agent");

    let claimed = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_next",
        json!({"scope": "global", "lease_seconds": 0, "allow_busy": false}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("falsey numeric zero uses the default lease");
    assert_eq!(claimed["task"]["id"], first.id);
    assert_eq!(claimed["scope"], "global");
    assert_eq!(claimed["pane_id"], "w1:p1");
    assert_eq!(claimed["lease_seconds"], 1800.0);
    assert_eq!(claimed["caller"]["pid"], 4321);
    assert_eq!(claimed["caller"]["start"], "98765");
    assert_eq!(claimed["caller"]["name"], "codex-agent");
    assert!(claimed.get("following").is_none());
    assert!(claimed["task"]["children"].is_array());
    assert!(claimed["task"]["leases"].is_array());

    let bad_scope_precedes_lease_conversion = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_next",
        json!({"scope": "not a scope!", "lease_seconds": "not a float"}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .unwrap_err()
    .to_string();
    assert!(bad_scope_precedes_lease_conversion.contains("scope must be 'global' or a pane id"));

    let lease = runtime
        .block_on(tasks.leases_for_pane("w1:p1", None))
        .expect("live lease")
        .into_iter()
        .find(|lease| lease.task_id == first.id)
        .expect("lease for first task");
    assert_eq!(lease.caller_pid, Some(4321));
    assert_eq!(lease.caller_start.as_deref(), Some("98765"));

    let busy = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_next",
        json!({"scope": "global"}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("busy lease is a refusal envelope");
    assert_eq!(busy["task"], Value::Null);
    assert_eq!(busy["refused"]["reason"], "busy");
    assert_eq!(busy["refused"]["task_ids"], json!([first.id]));
    assert_eq!(busy["following"], true);

    let parallel = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_next",
        json!({"scope": "global", "allow_busy": " YES ", "lease_seconds": " 30 "}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("truthy allow_busy permits an explicit second claim");
    assert_eq!(parallel["task"]["id"], second.id);
    assert_eq!(parallel["lease_seconds"], 30.0);
    assert!(parallel.get("following").is_none());

    let heartbeat = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        json!({"id": first.id, "lease_seconds": 0}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("heartbeat refreshes caller details");
    assert_eq!(heartbeat["task_id"], first.id);
    assert_eq!(heartbeat["pane_id"], "w1:p1");
    assert_eq!(heartbeat["lease_seconds"], 1800.0);
    let whitespace_heartbeat = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        json!({"id": first.id, "lease_seconds": " 30 "}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("heartbeat accepts Python float whitespace");
    assert_eq!(whitespace_heartbeat["lease_seconds"], 30.0);
    let refreshed = runtime
        .block_on(tasks.leases_for_pane("w1:p1", None))
        .expect("refreshed leases")
        .into_iter()
        .find(|lease| lease.task_id == first.id)
        .expect("refreshed first lease");
    assert_eq!(refreshed.caller_pid, Some(4321));
    assert_eq!(refreshed.caller_start.as_deref(), Some("98765"));

    let invalid_lease = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        json!({"id": first.id, "lease_seconds": "0"}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .unwrap_err()
    .to_string();
    assert!(invalid_lease.contains("lease_seconds must be positive"));

    let missing_heartbeat_precedes_lease_conversion = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        json!({"id": "t-missing", "lease_seconds": "not a float"}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .unwrap_err()
    .to_string();
    assert!(missing_heartbeat_precedes_lease_conversion.contains("no task with id"));

    let released = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_release",
        json!({"id": second.id}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("release returns full post-commit wire row");
    assert_eq!(released["id"], second.id);
    assert_eq!(released["status"], "open");
    assert!(released["children"].is_array());
    assert!(runtime
        .block_on(tasks.leases_for_pane("w1:p1", None))
        .unwrap()
        .iter()
        .all(|lease| lease.task_id != second.id));

    // The Flock's empty-queue subscription remains a refusal envelope, not a fake success.
    runtime
        .block_on(tasks.complete(&first.id))
        .expect("complete first");
    runtime
        .block_on(tasks.complete(&second.id))
        .expect("complete second");
    let empty = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_next",
        json!({"scope": "global", "allow_busy": true}),
        Some("w1:p1"),
        Some(caller),
    )
    .expect("empty queue refusal");
    assert_eq!(empty["task"], Value::Null);
    assert_eq!(empty["refused"]["reason"], "empty");
    assert_eq!(empty["following"], true);
    assert!(flock
        .task_followers()
        .iter()
        .any(|(pane, _)| pane == "w1:p1"));
}

#[test]
fn api_t39_nonfinite_leases_remain_numeric_through_task_wire_and_events() {
    fn client_roundtrip(command: &str, input: Value, result: Value) -> Value {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("client socket directory");
        let path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&path).expect("temporary peer");
        std::thread::scope(|scope| {
            let peer = scope.spawn(|| {
                let (mut socket, _) = listener.accept().expect("client connected");
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&socket).read_line(&mut line).unwrap();
                let request: crate::protocol::Request =
                    crate::protocol::decode_request(&line).unwrap();
                assert_eq!(request.cmd, command);
                assert_eq!(
                    crate::wire_json::to_string(&request.args).unwrap(),
                    crate::wire_json::to_string(&input).unwrap()
                );
                assert_eq!(request.self_pane_id.as_deref(), Some("w1:p1"));
                let response = crate::protocol::Response::ok(request.id, result);
                socket
                    .write_all(crate::protocol::encode_line(&response).as_bytes())
                    .unwrap();
            });
            let mut client = crate::client::FlockClient::new(
                crate::client::UnixTransport { path },
                Some("w1:p1".into()),
                "test pane".into(),
                Duration::from_secs(5),
                None,
            );
            let decoded = client.call(command, input.as_object().unwrap().clone());
            peer.join().expect("temporary peer completed");
            decoded.expect("actual client decodes dispatcher result")
        })
    }
    let raw_float: Value = serde_json::from_str("1e309").expect("standard JSON float overflow");
    assert!(python_float(&raw_float)
        .expect("JSON float exponent converts to Python infinity")
        .is_infinite());
    let raw_larger_float: Value =
        serde_json::from_str("1e999").expect("arbitrary-precision JSON float");
    assert!(python_float(&raw_larger_float)
        .expect("larger exponent remains a Python float")
        .is_infinite());
    let raw_large_integer: Value = serde_json::from_str(&format!("1{}", "0".repeat(400)))
        .expect("arbitrary-precision JSON integer");
    assert!(matches!(
        python_float(&raw_large_integer),
        Err(DunkError::Refused(message)) if message == "int too large to convert to float"
    ));

    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    let task = runtime
        .block_on(tasks.add("global", "literal Infinity", ""))
        .expect("task");
    let caller = task_test_caller("w1:p1", 4321, "98765", "codex-agent");
    let next_args: Value = serde_json::from_str(r#"{"scope":"global","lease_seconds":"Infinity"}"#)
        .expect("string Infinity is a JSON string");
    let next = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_next",
        next_args.clone(),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("positive infinite lease is stored and returned");
    let next = client_roundtrip("task_next", next_args, next);
    assert_eq!(next["task"]["id"], task.id);
    assert_eq!(next["task"]["title"], "literal Infinity");
    for field in ["lease_seconds", "expires_at"] {
        let token = next[field]
            .as_number()
            .expect("nonfinite lease remains a JSON number")
            .to_string();
        assert_eq!(token, "1e+999");
        assert!(token.parse::<f64>().unwrap().is_infinite());
    }

    let full = runtime
        .block_on(tasks.get_full_wire(&task.id, Some("w1:p1")))
        .expect("full read after claim");
    assert_eq!(full.wire["title"], "literal Infinity");
    assert_eq!(
        full.wire["lease_expires_at"]
            .as_number()
            .unwrap()
            .to_string(),
        "1e+999"
    );
    assert_eq!(full.wire["leases"].as_array().unwrap().len(), 1);
    assert_eq!(
        full.wire["leases"][0]["expires_at"]
            .as_number()
            .unwrap()
            .to_string(),
        "1e+999"
    );

    let connection = rusqlite::Connection::open(tasks.path()).expect("read task DB");
    let (storage_type, expiry): (String, f64) = connection
        .query_row(
            "SELECT typeof(expires_at), expires_at FROM task_leases WHERE task_id = ?",
            [&task.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("infinite lease row");
    assert_eq!(storage_type, "real");
    assert!(expiry.is_infinite());
    let event_meta: String = connection
        .query_row(
            "SELECT meta FROM task_events WHERE task_id = ? AND event = 'claimed'",
            [&task.id],
            |row| row.get(0),
        )
        .expect("claim event");
    let event_meta: Value = serde_json::from_str(&event_meta).expect("valid event JSON");
    assert_eq!(
        event_meta["lease_seconds"].as_number().unwrap().to_string(),
        "1e+999"
    );

    let heartbeat_args: Value =
        serde_json::from_str(&format!(r#"{{"id":"{}","lease_seconds":1e309}}"#, task.id))
            .expect("numeric exponent lease argument");
    let heartbeat = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        heartbeat_args.clone(),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .expect("numeric exponent heartbeat stores infinity");
    let heartbeat = client_roundtrip("task_heartbeat", heartbeat_args, heartbeat);
    assert_eq!(
        heartbeat["lease_seconds"].as_number().unwrap().to_string(),
        "1e+999"
    );
    assert_eq!(
        heartbeat["expires_at"].as_number().unwrap().to_string(),
        "1e+999"
    );

    let negative_infinity = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        json!({"id": task.id, "lease_seconds": "-Infinity"}),
        Some("w1:p1"),
        Some(caller.clone()),
    )
    .unwrap_err()
    .to_string();
    assert!(negative_infinity.contains("lease_seconds must be positive"));

    let nan = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_heartbeat",
        json!({"id": task.id, "lease_seconds": "NaN"}),
        Some("w1:p1"),
        Some(caller),
    )
    .unwrap_err()
    .to_string();
    assert!(nan.contains("NOT NULL constraint failed: task_leases.expires_at"));
    let expiry_after_nan: f64 = connection
        .query_row(
            "SELECT expires_at FROM task_leases WHERE task_id = ?",
            [&task.id],
            |row| row.get(0),
        )
        .expect("failed NaN heartbeat leaves lease untouched");
    assert!(expiry_after_nan.is_infinite());
}

#[test]
fn h_task_add_matches_python_defaults_and_role_gate() {
    let (runtime, _dir, flock, _tasks, dispatcher) = task_api_fixture();
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
    let (runtime, _dir, flock, tasks, dispatcher) = task_api_fixture();
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
        Err(DunkError::Refused(message))
            if message == format!("task {id} belongs to another pane's private queue (w1:p1)")
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
    let (runtime, _dir, flock, tasks, dispatcher) = task_api_fixture();
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

fn task_delete_fixture() -> (
    tokio::runtime::Runtime,
    tempfile::TempDir,
    Flock,
    Arc<crate::tasks::Tasks>,
    Dispatcher,
) {
    let (dir, flock) = flock();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
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
    dispatcher.register_task_delete();
    (runtime, dir, flock, tasks, dispatcher)
}

fn dispatch_task_delete(
    dispatcher: &Dispatcher,
    flock: &Flock,
    runtime: &tokio::runtime::Runtime,
    gate: &impl CallerGate,
    args: Option<Args>,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx::default();
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    dispatcher.dispatch(&context, gate, "task_delete", args)
}

#[test]
fn h_task_delete_matches_python_and_store_guards() {
    let (runtime, _dir, flock, tasks, dispatcher) = task_delete_fixture();
    let blocked = runtime
        .block_on(tasks.add("global", "blocked by dependency", ""))
        .expect("blocked task");
    let dependent = runtime
        .block_on(tasks.add_with(
            "global",
            "dependent",
            "",
            crate::tasks::AddOptions {
                depends_on: vec![blocked.id.clone()],
                ..Default::default()
            },
        ))
        .expect("dependent task");
    let removable = runtime
        .block_on(tasks.add("global", "removable", ""))
        .expect("removable task");

    assert!(matches!(
        dispatch_task_delete(
            &dispatcher,
            &flock,
            &runtime,
            &FixedRole("worker", false),
            args(json!({"id": removable.id})),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(runtime.block_on(tasks.get(&removable.id)).is_ok());

    assert!(matches!(
        dispatch_task_delete(
            &dispatcher,
            &flock,
            &runtime,
            &FixedRole("owner", true),
            None,
        ),
        Err(DunkError::Refused(message)) if message == "task_delete: missing required argument(s): id"
    ));
    assert!(matches!(
        dispatch_task_delete(
            &dispatcher,
            &flock,
            &runtime,
            &FixedRole("owner", true),
            args(json!({"id": "t999999"})),
        ),
        Err(DunkError::Refused(message)) if message.contains("no task with id")
    ));
    assert!(matches!(
        dispatch_task_delete(
            &dispatcher,
            &flock,
            &runtime,
            &FixedRole("owner", true),
            args(json!({"id": blocked.id})),
        ),
        Err(DunkError::Refused(message)) if message.contains(&format!("task {} is a dependency of {}", blocked.id, dependent.id))
    ));

    let deleted = dispatch_task_delete(
        &dispatcher,
        &flock,
        &runtime,
        &FixedRole("owner", true),
        args(json!({"id": removable.id})),
    )
    .expect("delete task");
    assert_eq!(deleted, json!({"deleted": removable.id}));
    assert!(runtime.block_on(tasks.get(&removable.id)).is_err());
}

struct TaskCompleteApiEnv<'a> {
    terminal: Mutex<Box<dyn Terminal>>,
    jobs: &'a crate::tasks::next_job_runtime::NextJobs,
}

impl PaneInfoEnv for TaskCompleteApiEnv<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for TaskCompleteApiEnv<'_> {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }

    fn next_jobs(&self) -> Result<&crate::tasks::next_job_runtime::NextJobs, DunkError> {
        Ok(self.jobs)
    }
}

fn task_complete_fixture() -> (
    tokio::runtime::Runtime,
    tempfile::TempDir,
    Arc<Flock>,
    Arc<crate::tasks::Tasks>,
    crate::tasks::next_job_runtime::NextJobs,
    Dispatcher,
) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let tasks = Arc::new(
        runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .expect("task store"),
    );
    runtime
        .block_on(tasks.allocation_init())
        .expect("allocation init");
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    ));
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("roles");
    roles
        .register("w1:p1", "worker", None, None, None, None, None, None)
        .expect("register worker");
    flock.attach_roles(roles, None);
    flock.attach_stores(Stores {
        tasks: Some(Arc::clone(&tasks)),
        ..Stores::default()
    });
    let gate_env = Env::from_pairs(&[
        ("DUNKINGSHEEP_DIR", dir.path().to_str().unwrap()),
        ("DUNKINGSHEEP_TURN_END_DISPATCH", "0"),
    ]);
    let jobs = crate::tasks::next_job_runtime::NextJobs::with_providers(
        Arc::clone(&flock),
        Arc::new(move || Ok(gate_env.clone())),
        None,
    );
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_task_complete();
    (runtime, dir, flock, tasks, jobs, dispatcher)
}

fn add_in_progress_task(
    runtime: &tokio::runtime::Runtime,
    tasks: &crate::tasks::Tasks,
    title: &str,
) -> crate::tasks::Task {
    let task = runtime
        .block_on(tasks.add_with(
            "global",
            title,
            "",
            crate::tasks::AddOptions {
                assignees: vec!["w1:p1".into()],
                ..Default::default()
            },
        ))
        .expect("add task");
    runtime
        .block_on(tasks.update_with_status(
            &task.id,
            Default::default(),
            Some("in_progress"),
            Some("w1:p1"),
        ))
        .expect("claim task");
    task
}

fn dispatch_task_complete(
    dispatcher: &Dispatcher,
    flock: &Flock,
    runtime: &tokio::runtime::Runtime,
    jobs: &crate::tasks::next_job_runtime::NextJobs,
    role: &impl CallerGate,
    args: Option<Args>,
) -> Result<Value, DunkError> {
    let env = TaskCompleteApiEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        jobs,
    };
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..Default::default()
    };
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    dispatcher.dispatch(&context, role, "task_complete", args)
}

#[test]
fn h_task_complete_dispatcher_matches_defaults_and_landed_boolean() {
    let (runtime, _dir, flock, tasks, jobs, dispatcher) = task_complete_fixture();
    let default_landed = add_in_progress_task(&runtime, &tasks, "default landed");
    let default_result = dispatch_task_complete(
        &dispatcher,
        &flock,
        &runtime,
        &jobs,
        &FixedRole("worker", false),
        args(json!({"id": default_landed.id})),
    )
    .expect("complete with Python defaults");
    assert_eq!("done", default_result["status"]);
    assert!(default_result["next_job"].is_object());
    assert_eq!(
        "done",
        runtime
            .block_on(tasks.get(&default_landed.id))
            .unwrap()
            .status
    );
    assert_eq!(
        Some("w1:p1"),
        runtime
            .block_on(tasks.get(&default_landed.id))
            .unwrap()
            .completed_by
            .as_deref()
    );

    let stacked = add_in_progress_task(&runtime, &tasks, "stacked landed");
    let stacked_result = dispatch_task_complete(
        &dispatcher,
        &flock,
        &runtime,
        &jobs,
        &FixedRole("worker", false),
        args(json!({"id": stacked.id, "by": "spoofed label", "landed": "false"})),
    )
    .expect("complete a stacked task");
    assert_eq!(false, stacked_result["landed"]);
    assert_eq!("done", stacked_result["status"]);
    assert_eq!(
        Some("w1:p1"),
        runtime
            .block_on(tasks.get(&stacked.id))
            .unwrap()
            .completed_by
            .as_deref()
    );
}

#[test]
fn h_task_complete_dispatcher_pins_role_and_argument_errors() {
    let (runtime, _dir, flock, tasks, jobs, dispatcher) = task_complete_fixture();
    let task = add_in_progress_task(&runtime, &tasks, "must remain open");

    assert!(matches!(
        dispatch_task_complete(
            &dispatcher,
            &flock,
            &runtime,
            &jobs,
            &FixedRole("desk", false),
            args(json!({"id": task.id})),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role desk")
    ));
    assert!(matches!(
        dispatch_task_complete(
            &dispatcher,
            &flock,
            &runtime,
            &jobs,
            &FixedRole("worker", false),
            args(json!({})),
        ),
        Err(DunkError::Refused(message)) if message == "task_complete: missing required argument(s): id"
    ));
    assert!(matches!(
        dispatch_task_complete(
            &dispatcher,
            &flock,
            &runtime,
            &jobs,
            &FixedRole("worker", false),
            args(json!({"id": task.id, "extra": true})),
        ),
        Err(DunkError::Refused(message)) if message == "task_complete: unknown argument(s): extra"
    ));
    assert!(matches!(
        dispatch_task_complete(
            &dispatcher,
            &flock,
            &runtime,
            &jobs,
            &FixedRole("worker", false),
            args(json!({"id": task.id, "verdict": "accept"})),
        ),
        Err(DunkError::Refused(message)) if message == "--verdict applies to review tasks only"
    ));
    assert_eq!(
        "in_progress",
        runtime.block_on(tasks.get(&task.id)).unwrap().status
    );
}

fn owner_api_fixture(tag: &str) -> (tempfile::TempDir, Flock, OwnerApiEnv) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join(format!("{tag}-launches.json")),
        dir.path().join(format!("{tag}-lanes.json")),
        false,
    );
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
        .register("w1:p2", "worker", None, None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    let inbox = Arc::new(
        crate::inbox_store::Store::open(
            dir.path().join(format!("{tag}-inbox.db")),
            dir.path().join(format!("{tag}-blobs")),
        )
        .unwrap(),
    );
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });
    let env = OwnerApiEnv::new(
        &dir.path().join(format!("{tag}-owner-push.db")),
        1_800_000_100.0,
    );
    (dir, flock, env)
}

fn dispatch_owner_api(
    dispatcher: &Dispatcher,
    flock: &Flock,
    env: &dyn CommandEnv,
    command: &str,
    args: Option<Args>,
    pane: Option<&str>,
) -> Result<Value, DunkError> {
    let request = RequestCtx {
        self_pane_id: pane.map(str::to_owned),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = HandlerContext {
        flock,
        request: &request,
        env,
        runtime: runtime.handle(),
    };
    dispatcher.dispatch(&context, &FlockGate(&context), command, args)
}

#[derive(Default)]
struct FakeSendCommands {
    calls: Mutex<Vec<Value>>,
    failure: Mutex<Option<String>>,
}

impl FakeSendCommands {
    fn record(&self, call: Value) -> Result<Value, DunkError> {
        self.calls.lock().unwrap().push(call.clone());
        if let Some(message) = self.failure.lock().unwrap().clone() {
            Err(DunkError::Refused(message))
        } else {
            Ok(json!({"operation": call["operation"], "pane_id": call["target"]["pane_id"]}))
        }
    }
}

impl SendCommandEnv for FakeSendCommands {
    fn send_text(
        &self,
        target: &Value,
        text: &str,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
        press_enter: bool,
    ) -> Result<Value, DunkError> {
        self.record(
            json!({"operation":"send_text", "target":target, "text":text,
            "self_pane_id":self_pane_id, "hold_while_typing":hold_while_typing,
            "wait_s":wait_s, "press_enter":press_enter}),
        )
    }

    fn send_keys(
        &self,
        target: &Value,
        keys: &str,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
    ) -> Result<Value, DunkError> {
        self.record(
            json!({"operation":"send_keys", "target":target, "keys":keys,
            "self_pane_id":self_pane_id, "hold_while_typing":hold_while_typing,
            "wait_s":wait_s}),
        )
    }

    fn send_keys_raw(
        &self,
        target: &Value,
        keys: &Value,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
    ) -> Result<Value, DunkError> {
        self.record(
            json!({"operation":"send_keys", "target":target, "keys":keys,
            "self_pane_id":self_pane_id, "hold_while_typing":hold_while_typing, "wait_s":wait_s}),
        )
    }

    fn flush_box(&self, target: &Value, self_pane_id: Option<&str>) -> Result<Value, DunkError> {
        self.record(json!({"operation":"flush_box", "target":target,
            "self_pane_id":self_pane_id}))
    }
}

struct SendApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    send: FakeSendCommands,
    panes: Vec<Value>,
}

impl SendApiEnv {
    fn new() -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            send: FakeSendCommands::default(),
            panes: vec![
                json!({"pane_id":"w1:p1", "agent":"owner", "tab_label":"owner"}),
                json!({"pane_id":"w2:p2", "agent":"worker", "tab_label":"worker"}),
                json!({"pane_id":"w3:p3", "agent":"reviewer", "tab_label":"reviewer"}),
                json!({"pane_id":"w4:p4", "agent":"planner", "tab_label":"planner"}),
            ],
        }
    }
}

impl PaneInfoEnv for SendApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id))
            .cloned()
    }
}

impl CommandEnv for SendApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }

    fn send_commands(&self) -> Result<&dyn SendCommandEnv, DunkError> {
        Ok(&self.send)
    }
}

fn send_api_fixture(tag: &str) -> (tempfile::TempDir, Flock, SendApiEnv, Dispatcher) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    for (pane, role) in [
        ("w1:p1", "owner"),
        ("w2:p2", "worker"),
        ("w3:p3", "reviewer"),
        ("w4:p4", "planner"),
    ] {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .unwrap();
    }
    flock.attach_roles(roles, None);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_send_text_keys_flush_box();
    (dir, flock, SendApiEnv::new(), dispatcher)
}

const API_CLEAR_OWNER: &str = "w1:p1";
const API_CLEAR_WORKER: &str = "w2:p2";

struct ApiClearBoxBackend {
    panes: Vec<Value>,
    screens: BTreeMap<String, Option<String>>,
    statuses: BTreeMap<String, String>,
    keys: Vec<String>,
    fail_at: Option<usize>,
}

impl ApiClearBoxBackend {
    fn new(panes: Vec<Value>) -> Self {
        Self {
            panes,
            screens: BTreeMap::from([(
                API_CLEAR_WORKER.to_owned(),
                Some("Context is full - please /clear\n".into()),
            )]),
            statuses: BTreeMap::new(),
            keys: Vec::new(),
            fail_at: None,
        }
    }
}

impl crate::flock::sends::SendEnv for ApiClearBoxBackend {
    fn read_pane(&self, pane_id: &str, _: i64, _: bool) -> Option<String> {
        self.screens.get(pane_id).cloned().flatten()
    }

    fn pane_harness(&self, _: &str) -> Option<String> {
        None
    }

    fn session_process_gone(&self, _: &str) -> bool {
        false
    }

    fn read_status(&self, pane_id: &str) -> Option<String> {
        self.statuses.get(pane_id).cloned()
    }

    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.keys.push(key.to_owned());
        let ok = self.fail_at != Some(self.keys.len());
        if ok && key == "Escape" {
            self.screens.insert(
                pane_id.to_owned(),
                Some("› Ask Codex to do anything\n".into()),
            );
            self.statuses.insert(pane_id.to_owned(), "idle".into());
        }
        (
            ok,
            if ok {
                "sent".into()
            } else {
                "terminal refused key".into()
            },
        )
    }

    fn federated_input(&self) -> bool {
        false
    }
}

impl PaneInfoEnv for ApiClearBoxBackend {
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

impl crate::flock::clear_box::ClearBoxEnv for ApiClearBoxBackend {
    fn server_error_hint(&self) -> String {
        "test terminal unavailable".into()
    }
}

struct SendHandlerApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    panes: Vec<Value>,
    clear_box: RefCell<ApiClearBoxBackend>,
    clear_box_calls: Cell<usize>,
    delivery_env_calls: Cell<usize>,
    owner_push: crate::owner::OwnerPush,
    now_s: f64,
}

impl SendHandlerApiEnv {
    fn new(path: &Path) -> Self {
        let panes = vec![
            json!({"pane_id":API_CLEAR_OWNER, "agent":"owner", "tab_label":"owner"}),
            json!({"pane_id":API_CLEAR_WORKER, "agent":"claude", "label":"worker", "tab_label":"worker"}),
        ];
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            clear_box: RefCell::new(ApiClearBoxBackend::new(panes.clone())),
            panes,
            clear_box_calls: Cell::new(0),
            delivery_env_calls: Cell::new(0),
            owner_push: crate::owner::OwnerPush::open(path.join("owner-push.db")).unwrap(),
            now_s: 10_000.0,
        }
    }
}

impl PaneInfoEnv for SendHandlerApiEnv {
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

impl crate::flock::mailbox::MailboxEnv for SendHandlerApiEnv {
    fn list_panes(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}

impl crate::flock::delivery::DeliveryEnv for SendHandlerApiEnv {
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|pane| pane["pane_id"] == pane_id)
            .cloned()
    }

    fn agent_alive(&self, _: &str) -> Option<bool> {
        Some(true)
    }

    fn has_web_token(&self) -> Result<bool, DunkError> {
        Ok(false)
    }

    fn server_error_hint(&self) -> String {
        "test terminal unavailable".into()
    }
}

impl CommandEnv for SendHandlerApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        self.now_s
    }

    fn delivery_env(&self) -> Result<&dyn crate::flock::delivery::DeliveryEnv, DunkError> {
        self.delivery_env_calls
            .set(self.delivery_env_calls.get() + 1);
        Ok(self)
    }

    fn with_clear_box_env(
        &self,
        operation: &mut dyn FnMut(
            &mut dyn crate::flock::clear_box::ClearBoxEnv,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        self.clear_box_calls.set(self.clear_box_calls.get() + 1);
        operation(&mut *self.clear_box.borrow_mut())
    }

    fn owner_push(&self) -> Result<&crate::owner::OwnerPush, DunkError> {
        Ok(&self.owner_push)
    }
}

fn send_handler_api_fixture(
    tag: &str,
) -> (tempfile::TempDir, Flock, SendHandlerApiEnv, Dispatcher) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    roles
        .register(API_CLEAR_OWNER, "owner", None, None, None, None, None, None)
        .unwrap();
    roles
        .register(
            API_CLEAR_WORKER,
            "worker",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    flock.attach_roles(roles, None);
    let env = SendHandlerApiEnv::new(dir.path());
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_clear_box_delivery_capability();
    (dir, flock, env, dispatcher)
}

#[test]
fn api_t39_clear_box_dispatch_preserves_raw_reason_and_authorization_order() {
    let (_dir, flock, env, dispatcher) = send_handler_api_fixture("api-clear-box");
    assert_eq!(
        dispatcher.registration_attempts(),
        &["clear_box", "delivery", "delivery_capability"]
    );

    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some(API_CLEAR_OWNER),
        "clear_box",
        args(json!({
            "target":{"pane_id":API_CLEAR_WORKER, "extra":7},
            "reason":17
        })),
    )
    .expect("owner clear handler delegates to the real Flock operation");
    assert_eq!(result["reason"], 17);
    assert_eq!(result["before"]["state"], "blocked");
    assert_eq!(result["after"]["state"], "ready");
    assert_eq!(result["flushed"], true);
    assert_eq!(env.clear_box.borrow().keys, ["C-u", "Escape"]);
    assert_eq!(env.clear_box_calls.get(), 1);

    let missing = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some(API_CLEAR_OWNER),
        "clear_box",
        args(json!({})),
    )
    .unwrap_err();
    assert!(missing
        .to_string()
        .contains("missing required argument(s): target"));
    assert_eq!(
        env.clear_box_calls.get(),
        1,
        "required args precede the adapter"
    );

    let denied = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some(API_CLEAR_WORKER),
        "clear_box",
        args(json!({"target":"unknown target"})),
    )
    .unwrap_err();
    assert!(denied.to_string().contains("not permitted for role worker"));
    assert_eq!(
        env.clear_box_calls.get(),
        1,
        "role gate precedes the adapter"
    );
}

#[test]
fn api_t39_delivery_dispatch_preserves_target_and_falsey_status_semantics() {
    let (_dir, flock, env, dispatcher) = send_handler_api_fixture("api-delivery");
    {
        let mut backend = env.clear_box.borrow_mut();
        backend.screens.insert(
            API_CLEAR_WORKER.into(),
            Some("› Ask Codex to do anything\n".into()),
        );
        backend
            .statuses
            .insert(API_CLEAR_WORKER.into(), "busy".into());
    }

    for (label, args_value, expected) in [
        (
            "omitted status falls back",
            json!({"target":{"pane_id":API_CLEAR_WORKER, "extra":7}}),
            "working",
        ),
        (
            "null status falls back",
            json!({"target":"worker", "status":null}),
            "working",
        ),
        (
            "false suppresses fallback",
            json!({"target":"worker", "status":false}),
            "ready",
        ),
        (
            "zero suppresses fallback",
            json!({"target":"worker", "status":0}),
            "ready",
        ),
        (
            "empty string suppresses fallback",
            json!({"target":"worker", "status":""}),
            "ready",
        ),
    ] {
        let result = dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some(API_CLEAR_WORKER),
            "delivery",
            args(args_value),
        )
        .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(result["state"], expected, "{label}");
    }
    assert_eq!(env.clear_box_calls.get(), 5);

    let invalid_dict = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some(API_CLEAR_WORKER),
        "delivery",
        args(json!({"target":{"extra":7}})),
    )
    .unwrap_err();
    assert_eq!(invalid_dict.to_string(), "target dict has no pane_id");
    assert_eq!(
        env.clear_box_calls.get(),
        5,
        "target resolution precedes screen reads"
    );

    let missing = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some(API_CLEAR_WORKER),
        "delivery",
        args(json!({})),
    )
    .unwrap_err();
    assert!(missing
        .to_string()
        .contains("missing required argument(s): target"));
}

#[test]
fn api_t39_delivery_capability_dispatch_preserves_python_falsey_and_type_errors() {
    let (_dir, flock, env, dispatcher) = send_handler_api_fixture("api-delivery-capability");
    for (label, args_value) in [
        ("absent", json!({})),
        ("null", json!({"target":null})),
        ("false", json!({"target":false})),
        ("zero", json!({"target":0})),
        ("empty string", json!({"target":""})),
        ("whitespace", json!({"target":"  \t"})),
        ("empty list", json!({"target":[]})),
        ("empty object", json!({"target":{}})),
    ] {
        let error = dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some(API_CLEAR_WORKER),
            "delivery_capability",
            args(args_value),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "delivery_capability must be given a target (a pane id like %9 or a pinned lane name)",
            "{label}"
        );
    }
    for (value, expected) in [
        (json!(1), "'int' object has no attribute 'strip'"),
        (json!(true), "'bool' object has no attribute 'strip'"),
        (json!(["worker"]), "'list' object has no attribute 'strip'"),
    ] {
        let error = dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some(API_CLEAR_WORKER),
            "delivery_capability",
            args(json!({"target":value})),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    assert_eq!(
        env.delivery_env_calls.get(),
        0,
        "invalid targets fail before lookup"
    );

    let capability = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some(API_CLEAR_WORKER),
        "delivery_capability",
        args(json!({"target":format!("  {API_CLEAR_WORKER}  ")})),
    )
    .expect("trimmed pane id resolves through Flock delivery capability");
    assert_eq!(capability["pane_id"], API_CLEAR_WORKER);
    assert_eq!(capability["send"], "guarded");
    assert_eq!(env.delivery_env_calls.get(), 1);
}

#[test]
fn send_text_dispatch_preserves_defaults_and_adapter_result() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-text");
    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "send_text",
        args(json!({"target":"worker", "text":"hello"})),
    )
    .unwrap();
    assert_eq!(result, json!({"operation":"send_text", "pane_id":"w2:p2"}));
    let calls = env.send.calls.lock().unwrap();
    assert_eq!(calls[0]["hold_while_typing"], true);
    assert_eq!(calls[0]["wait_s"], 3.0);
    assert_eq!(calls[0]["press_enter"], true);
    assert_eq!(calls[0]["self_pane_id"], "w1:p1");
}

#[test]
fn send_text_and_keys_forward_explicit_options() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-options");
    dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "send_text",
        args(
            json!({"target":"worker", "text":"draft", "hold_while_typing":false,
            "wait_s":"0.25", "press_enter":false}),
        ),
    )
    .unwrap();
    dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "send_keys",
        args(
            json!({"target":"worker", "keys":"ctrl+c", "hold_while_typing":false,
            "wait_s":0}),
        ),
    )
    .unwrap();
    let calls = env.send.calls.lock().unwrap();
    assert_eq!(calls[0]["press_enter"], false);
    assert_eq!(calls[0]["wait_s"], 0.25);
    assert_eq!(calls[1]["operation"], "send_keys");
    assert_eq!(calls[1]["hold_while_typing"], false);
    assert_eq!(calls[1]["wait_s"], 0.0);
}

#[test]
fn send_keys_dispatch_forwards_raw_values_without_schema_coercion() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-raw-keys");
    for keys in [
        json!(["ctrl+c Enter", false, null]),
        json!({"z":1,"a":2}),
        json!(false),
        json!(0),
    ] {
        dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w2:p2"),
            "send_keys",
            args(json!({"target":"self", "keys":keys,"wait_s":0})),
        )
        .unwrap();
        assert_eq!(env.send.calls.lock().unwrap().last().unwrap()["keys"], keys);
    }
}

#[test]
fn send_keys_and_flush_dispatch_to_their_named_operations() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-operations");
    for (command, call) in [
        ("send_keys", json!({"target":"worker", "keys":"Enter"})),
        ("flush_box", json!({"target":"worker"})),
    ] {
        dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            command,
            args(call),
        )
        .unwrap();
    }
    let calls = env.send.calls.lock().unwrap();
    assert_eq!(calls[0]["operation"], "send_keys");
    assert_eq!(calls[0]["hold_while_typing"], true);
    assert_eq!(calls[0]["wait_s"], 3.0);
    assert_eq!(calls[1]["operation"], "flush_box");
}

#[test]
fn worker_and_reviewer_send_gates_allow_self_and_refuse_other_panes() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-role-gate");
    dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "send_text",
        args(json!({"target":"self", "text":"hello"})),
    )
    .unwrap();
    let error = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "send_keys",
        args(json!({"target":"owner", "keys":"Enter"})),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("send_keys to another pane requires a superuser"));
    let reviewer_error = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w3:p3"),
        "flush_box",
        args(json!({"target":"owner"})),
    )
    .unwrap_err()
    .to_string();
    assert!(reviewer_error.contains("flush_box to another pane requires a superuser"));
    assert_eq!(env.send.calls.lock().unwrap().len(), 1);
}

#[test]
fn dispatcher_role_gate_precedes_retired_token_handler_gate() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-role-order");
    let error = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w4:p4"),
        "send_text",
        args(json!({"target":"worker", "text":"hello", "desk_token":"old"})),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("send_text is not permitted for role planner"));
    assert!(!error.contains("desk_token is retired"));
    assert!(env.send.calls.lock().unwrap().is_empty());
}

#[test]
fn send_rejects_retired_desk_token_invalid_wait_and_propagates_operation_error() {
    let (_dir, flock, env, dispatcher) = send_api_fixture("send-errors");
    let retired = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "send_text",
        args(json!({"target":"worker", "text":"hello", "desk_token":"old"})),
    )
    .unwrap_err()
    .to_string();
    assert!(retired.contains("desk_token is retired"));

    let retired_before_self_gate = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w2:p2"),
        "send_text",
        args(json!({"target":"owner", "text":"hello", "desk_token":"old"})),
    )
    .unwrap_err()
    .to_string();
    assert!(retired_before_self_gate.contains("desk_token is retired"));

    let invalid_wait = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "send_keys",
        args(json!({"target":"worker", "keys":"Enter", "wait_s":3601})),
    )
    .unwrap_err()
    .to_string();
    assert!(invalid_wait.contains("wait_s must be between 0 and 3600"));
    assert!(env.send.calls.lock().unwrap().is_empty());

    *env.send.failure.lock().unwrap() = Some("send backend failed".into());
    for (command, call) in [
        ("send_text", json!({"target":"worker", "text":"hello"})),
        ("send_keys", json!({"target":"worker", "keys":"Enter"})),
        ("flush_box", json!({"target":"worker"})),
    ] {
        let operation_error = dispatch_as_flock_role(
            &dispatcher,
            &flock,
            &env,
            Some("w1:p1"),
            command,
            args(call),
        )
        .unwrap_err()
        .to_string();
        assert!(operation_error.contains("send backend failed"));
    }
    assert_eq!(env.send.calls.lock().unwrap().len(), 3);
}

#[test]
fn h_owner_stats_matches_python() {
    let (_dir, flock, env) = owner_api_fixture("owner-stats");
    env.owner_push
        .push("w1:p2", "pending", None, None, 1_800_000_090.0)
        .unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_stats_receipts_conversation();

    let out = dispatch_owner_api(&dispatcher, &flock, &env, "owner_stats", None, None)
        .expect("owner stats via dispatcher");
    assert_eq!(out["stats"]["pushed"], 1);
    assert_eq!(out["stats"]["pending"], 1);
    assert_eq!(out["pending"][0]["pane_id"], "w1:p2");
}

#[test]
fn h_owner_receipts_matches_python() {
    let (_dir, flock, env) = owner_api_fixture("owner-receipts");
    let id = env
        .owner_push
        .push("w1:p2", "pending", None, None, 1_800_000_090.0)
        .unwrap()
        .id;
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_stats_receipts_conversation();

    let out = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_receipts",
        args(json!({"push_ids": format!("{id}, , nope")})),
        None,
    )
    .expect("owner receipts via dispatcher");
    assert_eq!(out["requested"], 1);
    assert_eq!(out["returned"], 1);
    assert_eq!(out["receipts"][0]["id"], id);
    assert_eq!(out["receipts"][0].get("body"), None);
    assert_eq!(env.owner_push.pending(None).unwrap().len(), 1);
}

#[test]
fn h_owner_delivered_matches_python() {
    let (_dir, flock, env) = owner_api_fixture("owner-delivered");
    let inbox = flock.stores().inbox.unwrap();
    let message_id = inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: crate::flock::mailbox::OWNER_MAILBOX,
            body: Some("question"),
            mailbox_label: Some(crate::flock::mailbox::OWNER_MAILBOX),
            transport: Some("telegram"),
            ..crate::inbox_store::Outgoing::default()
        })
        .unwrap();
    let dispatcher = owner_view_dispatcher();

    let out = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_delivered",
        args(json!({
            "message_id": message_id,
            "transport": "telegram",
            "external_id": "telegram-123"
        })),
        Some("w1:p1"),
    )
    .expect("owner delivery receipt via dispatcher");

    assert_eq!(out["message"]["message_id"], message_id);
    assert_eq!(out["message"]["external_id"], "telegram-123");
    assert_eq!(out["message"]["delivery_transport"], "telegram");
    assert!(out["delivered_at"].as_f64().is_some_and(|at| at > 0.0));
    assert_eq!(out["transport"], "telegram");

    let delivered_at = out["delivered_at"].clone();
    let retry = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_delivered",
        args(json!({
            "message_id": message_id,
            "transport": "telegram",
            "external_id": "telegram-retry"
        })),
        Some("w1:p1"),
    )
    .expect("idempotent owner delivery receipt");
    assert_eq!(retry["delivered_at"], delivered_at);
    assert_eq!(retry["message"]["external_id"], "telegram-123");

    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_delivered",
            args(json!({
                "message_id": message_id,
                "transport": "telegram"
            })),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
}

struct FakeOwnerViews {
    app_db: PathBuf,
    inbox_rows: std::collections::HashMap<String, Value>,
    stats: Vec<Value>,
    latencies: std::collections::HashMap<String, Vec<f64>>,
    now: f64,
    inbox_error: Option<String>,
    stats_error: Option<String>,
    latency_error: Option<String>,
    latency_calls: Mutex<Vec<(String, usize)>>,
}

impl Default for FakeOwnerViews {
    fn default() -> Self {
        Self {
            app_db: PathBuf::from("/nonexistent/application-messages.sqlite3"),
            inbox_rows: std::collections::HashMap::new(),
            stats: Vec::new(),
            latencies: std::collections::HashMap::new(),
            now: 2_000.0,
            inbox_error: None,
            stats_error: None,
            latency_error: None,
            latency_calls: Mutex::new(Vec::new()),
        }
    }
}

impl OwnerViewEnv for FakeOwnerViews {
    fn app_db_path(&self) -> Result<PathBuf, DunkError> {
        Ok(self.app_db.clone())
    }

    fn inbox_status(&self, message_id: &str, _now: f64) -> Result<Option<Value>, DunkError> {
        if let Some(error) = &self.inbox_error {
            return Err(DunkError::Refused(error.clone()));
        }
        Ok(self.inbox_rows.get(message_id).cloned())
    }

    fn inbox_stats(&self, _now: f64) -> Result<Vec<Value>, DunkError> {
        if let Some(error) = &self.stats_error {
            return Err(DunkError::Refused(error.clone()));
        }
        Ok(self.stats.clone())
    }

    fn seen_latencies(&self, mailbox: &str, limit: usize) -> Result<Vec<f64>, DunkError> {
        self.latency_calls
            .lock()
            .unwrap()
            .push((mailbox.to_string(), limit));
        if let Some(error) = &self.latency_error {
            return Err(DunkError::Refused(error.clone()));
        }
        Ok(self.latencies.get(mailbox).cloned().unwrap_or_default())
    }

    fn now(&self) -> f64 {
        self.now
    }
}

struct OwnerViewApiEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    views: FakeOwnerViews,
}

impl OwnerViewApiEnv {
    fn new(views: FakeOwnerViews) -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            views,
        }
    }
}

impl PaneInfoEnv for OwnerViewApiEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for OwnerViewApiEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        self.views.now
    }

    fn owner_views(&self) -> Result<&dyn OwnerViewEnv, DunkError> {
        Ok(&self.views)
    }
}

fn owner_view_dispatcher() -> Dispatcher {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_delivered_message_state_reply_stats();
    dispatcher
}

#[test]
fn h_message_state_matches_python() {
    let (_dir, flock) = staffed_flock("message-state");
    let dir = tempfile::tempdir().unwrap();
    let app_db = dir.path().join("messages.sqlite3");
    let connection = rusqlite::Connection::open(&app_db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, role TEXT, channel TEXT, body TEXT, delivery TEXT, delivery_detail TEXT, created_at TEXT, answer_id INTEGER, answered_at TEXT);
             INSERT INTO messages VALUES (10, 'owner', 'vp', 'question', 'sent', NULL, '2026-11-03T01:00:00Z', NULL, NULL);",
        )
        .unwrap();
    drop(connection);

    let mut views = FakeOwnerViews {
        app_db,
        ..FakeOwnerViews::default()
    };
    views.inbox_rows.insert(
        "i42".into(),
        json!({"id":"i42", "mailbox":"worker", "sender":"owner", "created_at":1900.0,
            "notify_state":"notified", "notify_reason":null, "read_at":1950.0,
            "acked_at":1960.0, "state":"acked"}),
    );
    let env = OwnerViewApiEnv::new(views);
    let dispatcher = owner_view_dispatcher();
    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "message_state",
        args(json!({"message_ids":"10, i42, 99, absent"})),
    )
    .unwrap();
    let messages = result["messages"].as_array().unwrap();
    assert_eq!(messages[0]["source"], "application");
    assert_eq!(messages[0]["id"], 10);
    assert_eq!(messages[0]["delivered"]["transport"], "sent");
    assert_eq!(messages[1]["source"], "inbox");
    assert_eq!(messages[1]["delivered"]["ok"], true);
    assert_eq!(messages[1]["seen"]["acked_at"], "1970-01-01T00:32:40Z");
    assert_eq!(messages[2]["found"], false);
    assert_eq!(messages[2]["source"], "application");
    assert_eq!(messages[3], json!({"id":"absent", "found":false}));
}

#[test]
fn h_message_state_keeps_missing_unreadable_and_nonnumeric_ids_distinct() {
    let (_dir, flock) = staffed_flock("message-state-missing");
    let env = OwnerViewApiEnv::new(FakeOwnerViews::default());
    let dispatcher = owner_view_dispatcher();
    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "message_state",
        args(json!({"message_ids":"7, no-such-inbox"})),
    )
    .unwrap();
    assert_eq!(result["messages"][0]["found"], false);
    assert_eq!(result["messages"][0]["source"], "application");
    assert_eq!(
        result["messages"][0],
        json!({"id":"7", "found":false, "source":"application", "reason":"no application row with this id"})
    );
    assert_eq!(
        result["messages"][1],
        json!({"id":"no-such-inbox", "found":false})
    );

    let dir = tempfile::tempdir().unwrap();
    let unreadable = dir.path().join("not-a-database.sqlite3");
    std::fs::write(&unreadable, "not sqlite").unwrap();
    let env = OwnerViewApiEnv::new(FakeOwnerViews {
        app_db: unreadable,
        ..FakeOwnerViews::default()
    });
    let unreadable_result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "message_state",
        args(json!({"message_ids":"8"})),
    )
    .unwrap();
    assert_eq!(
        unreadable_result["messages"][0],
        json!({"id":"8", "found":false, "source":"application", "reason":"no application row with this id"})
    );

    let query_error = dir.path().join("wrong-schema.sqlite3");
    rusqlite::Connection::open(&query_error)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (id INTEGER);")
        .unwrap();
    let env = OwnerViewApiEnv::new(FakeOwnerViews {
        app_db: query_error,
        ..FakeOwnerViews::default()
    });
    let query_error_result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "message_state",
        args(json!({"message_ids":"9"})),
    )
    .unwrap();
    assert_eq!(
        query_error_result["messages"][0],
        json!({"id":"9", "found":false, "source":"application", "reason":"no application row with this id"})
    );
}

#[test]
fn h_reply_stats_matches_python() {
    let (_dir, flock) = staffed_flock("reply-stats");
    let mut views = FakeOwnerViews {
        stats: vec![
            json!({"mailbox":"b"}),
            json!({"mailbox":"a"}),
            json!({"mailbox":"c"}),
        ],
        latencies: std::collections::HashMap::from([
            ("a".into(), vec![1.0, 3.0, 5.0]),
            ("c".into(), vec![7.0]),
        ]),
        ..FakeOwnerViews::default()
    };
    views.now = 2_000.0;
    let env = OwnerViewApiEnv::new(views);
    let dispatcher = owner_view_dispatcher();
    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "reply_stats",
        args(json!({"mailboxes":"b, a"})),
    )
    .unwrap();
    let boxes = result["mailboxes"].as_array().unwrap();
    assert_eq!(boxes.len(), 2);
    assert_eq!(boxes[0]["mailbox"], "a");
    assert_eq!(boxes[0]["seen_latency_s"]["value"], 3.0);
    assert_eq!(boxes[0]["seen_latency_last_s"]["value"], 5.0);
    assert_eq!(boxes[1]["mailbox"], "b");
    assert_eq!(
        boxes[1]["seen_latency_s"]["reason"],
        "no message has been read yet"
    );
    assert_eq!(
        env.views.latency_calls.lock().unwrap().as_slice(),
        [("b".into(), 200), ("a".into(), 200)]
    );
}

#[test]
fn message_state_maps_inbox_lookup_failure_to_missing_row() {
    let (_dir, flock) = staffed_flock("owner-view-errors");
    let dispatcher = owner_view_dispatcher();
    let env = OwnerViewApiEnv::new(FakeOwnerViews {
        inbox_error: Some("inbox lookup failed".into()),
        ..FakeOwnerViews::default()
    });
    let result = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "message_state",
        args(json!({"message_ids":"i1"})),
    )
    .unwrap();
    assert_eq!(result["messages"][0], json!({"id":"i1", "found":false}));
}

#[test]
fn reply_stats_propagates_inbox_query_failures() {
    let (_dir, flock) = staffed_flock("reply-stats-errors");
    let dispatcher = owner_view_dispatcher();
    let env = OwnerViewApiEnv::new(FakeOwnerViews {
        stats_error: Some("inbox stats failed".into()),
        ..FakeOwnerViews::default()
    });
    let error = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "reply_stats",
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("inbox stats failed"));

    let env = OwnerViewApiEnv::new(FakeOwnerViews {
        stats: vec![json!({"mailbox":"a"})],
        latency_error: Some("latency query failed".into()),
        ..FakeOwnerViews::default()
    });
    let error = dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        Some("w1:p1"),
        "reply_stats",
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("latency query failed"));
}

#[test]
fn h_owner_conversation_matches_python() {
    let (_dir, flock, env) = owner_api_fixture("owner-conversation");
    let inbox = flock.stores().inbox.unwrap();
    let question = inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: crate::flock::mailbox::OWNER_MAILBOX,
            body: Some("question"),
            sender: Some("agent:%7"),
            mailbox_label: Some(crate::flock::mailbox::OWNER_MAILBOX),
            transport: Some("telegram"),
            ..crate::inbox_store::Outgoing::default()
        })
        .unwrap();
    inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: crate::flock::mailbox::OWNER_MAILBOX,
            body: Some("answer"),
            sender: Some("owner"),
            mailbox_label: Some(crate::flock::mailbox::OWNER_MAILBOX),
            transport: Some("web"),
            reply_to: Some(&question),
            ..crate::inbox_store::Outgoing::default()
        })
        .unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_stats_receipts_conversation();

    let out = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_conversation",
        args(json!({"limit": 0, "before": "9999999999"})),
        Some("w1:p1"),
    )
    .expect("owner conversation via dispatcher");
    assert_eq!(out["mailbox"], crate::flock::mailbox::OWNER_MAILBOX);
    assert_eq!(out["count"], 2);
    assert_eq!(out["messages"][0]["sender"], "agent:%7");
    assert_eq!(out["messages"][0]["transport"], "telegram");
    assert_eq!(out["messages"][1]["sender"], "owner");
    assert_eq!(out["messages"][1]["reply_to"], question);
    assert_eq!(out["messages"][0]["answered"], true);
    assert_eq!(out["waiting"], json!([]));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_conversation",
            None,
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_conversation",
            args(json!({"before": "not a timestamp"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "could not convert string to float: 'not a timestamp'"
    ));
}

/// Python `float(before)` on the owner conversation cursor, over a real Unix socket.
/// dunk_inbox.conversation binds `created_at < float(before)`: an absent cursor, Infinity
/// and 1e309 keep every row; NaN, -Infinity and the string "NaN" keep none. An integer past
/// the float range raises OverflowError, which the daemon answers as an internal error.
#[test]
fn owner_conversation_before_cursor_matches_python_over_real_socket() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;
    // `Dispatcher` is Send + Sync, and OwnerApiEnv holds a terminal that is not Sync, so the
    // peer keeps only the owner-push path and clock and builds its env for each request.
    struct OwnerPeer<'a> {
        dispatcher: &'a Dispatcher,
        flock: &'a Flock,
        owner_push: PathBuf,
        now_s: f64,
    }
    impl crate::server::Dispatcher for OwnerPeer<'_> {
        fn dispatch(&self, cmd: &str, input: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
            let env = OwnerApiEnv::new(&self.owner_push, self.now_s);
            dispatch_owner_api(
                self.dispatcher,
                self.flock,
                &env,
                cmd,
                input.as_object().cloned(),
                ctx.self_pane_id.as_deref(),
            )
        }
    }
    let (dir, flock, _env) = owner_api_fixture("owner-before-socket");
    let inbox = flock.stores().inbox.unwrap();
    for body in ["question", "answer"] {
        inbox
            .send(&crate::inbox_store::Outgoing {
                mailbox: crate::flock::mailbox::OWNER_MAILBOX,
                body: Some(body),
                sender: Some("agent:%7"),
                mailbox_label: Some(crate::flock::mailbox::OWNER_MAILBOX),
                transport: Some("telegram"),
                ..crate::inbox_store::Outgoing::default()
            })
            .unwrap();
    }
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_stats_receipts_conversation();
    let peer = OwnerPeer {
        dispatcher: &dispatcher,
        flock: &flock,
        owner_push: dir.path().join("owner-before-socket-owner-push.db"),
        now_s: 1_800_000_100.0,
    };
    let path = dir.path().join("owner-before.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // The fixture env is not Sync, so the server stays on this thread and only the client is
    // spawned; its panics are re-raised so the assertion message reaches the test output.
    std::thread::scope(|scope| {
        let client = scope.spawn(|| {
            let mut socket = UnixStream::connect(&path).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut call = |args: &str| -> Value {
                let request = format!(
                    r#"{{"id":1,"cmd":"owner_conversation","args":{args},"self_pane_id":"w1:p1"}}"#
                );
                socket.write_all(format!("{request}\n").as_bytes()).unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                crate::wire_json::from_str(&line).unwrap()
            };
            let result = |reply: &Value| -> Value {
                assert_eq!(reply["ok"], json!(true), "{reply}");
                reply["result"].clone()
            };
            assert_eq!(result(&call("{}"))["count"], 2);
            assert_eq!(result(&call(r#"{"before":Infinity}"#))["count"], 2);
            assert_eq!(result(&call(r#"{"before":1e309}"#))["count"], 2);
            assert_eq!(
                result(&call(r#"{"before":NaN}"#)),
                json!({
                    "mailbox": crate::flock::mailbox::OWNER_MAILBOX,
                    "messages": [],
                    "count": 0,
                    "waiting": [],
                })
            );
            assert_eq!(result(&call(r#"{"before":-Infinity}"#))["count"], 0);
            assert_eq!(result(&call(r#"{"before":"NaN"}"#))["count"], 0);
            assert_eq!(result(&call(r#"{"before":0}"#))["count"], 0);
            // Python float(int) overflows past the float range; dispatch answers the
            // OverflowError as "internal error: ..." (dunk_server.handle_line). A 309-digit
            // integer is still finite and keeps every row.
            let huge = format!("1{}", "0".repeat(400));
            let overflow = json!({
                "id": 1,
                "ok": false,
                "error": "internal error: int too large to convert to float",
            });
            assert_eq!(call(&format!(r#"{{"before":{huge}}}"#)), overflow);
            assert_eq!(call(&format!(r#"{{"before":-{huge}}}"#)), overflow);
            let finite = format!("1{}", "0".repeat(308));
            assert_eq!(
                result(&call(&format!(r#"{{"before":{finite}}}"#)))["count"],
                2
            );
        });
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let peer_pid = crate::server::peer_pid(&socket);
        let reader = BufReader::new(socket.try_clone().unwrap());
        for line in reader.lines() {
            let (response, _) = crate::server::handle_line(&peer, &line.unwrap(), peer_pid);
            socket
                .write_all(crate::protocol::encode_line(&response).as_bytes())
                .unwrap();
        }
        client
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    });
}

#[test]
fn h_owner_reply_matches_python() {
    let (_dir, flock, env) = owner_api_fixture("owner-reply-api");
    let inbox = flock.stores().inbox.unwrap();
    let question = inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: crate::flock::mailbox::OWNER_MAILBOX,
            body: Some("question"),
            sender: Some("web"),
            mailbox_label: Some(crate::flock::mailbox::OWNER_MAILBOX),
            transport: Some("telegram"),
            ..crate::inbox_store::Outgoing::default()
        })
        .unwrap();
    let other = inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: "other-mailbox",
            body: Some("not an owner question"),
            ..crate::inbox_store::Outgoing::default()
        })
        .unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_reply_ingest_outbox();

    let out = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_reply",
        args(json!({"message_id": question, "body": 42})),
        Some("w1:p1"),
    )
    .expect("owner reply through dispatcher");
    assert_eq!(out["question"], question);
    assert_eq!(out["acked"], question);
    assert_eq!(out["transport"], "telegram");
    assert_eq!(out["reply"]["body"], "42");
    assert_eq!(out["reply"]["sender"], "w1:p1");
    assert_eq!(out["reply"]["transport"], "telegram");
    assert_eq!(out["reply"]["reply_to"], question);
    assert_eq!(inbox.get_message(&question).unwrap().summary.state, "acked");

    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_reply",
            args(json!({"message_id": question, "body": "again"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message.contains("was already answered by")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_reply",
            args(json!({"message_id": other, "body": "answer"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message.contains("is not an owner message")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_reply",
            None,
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_reply",
            None,
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "owner_reply: missing required argument(s): message_id, body"
    ));
}

#[test]
fn h_owner_ingest_matches_python() {
    let (dir, flock, env) = owner_api_fixture("owner-ingest-api");
    let attachment = dir.path().join("attachment with spaces.txt");
    std::fs::write(&attachment, b"fixture attachment").unwrap();
    let attach = format!(",{}, ,{}", attachment.display(), attachment.display());
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_reply_ingest_outbox();

    let out = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_ingest",
        args(json!({
            "body": "incoming question",
            "transport": " TELEGRAM ",
            "external_id": "telegram:88",
            "subject": " original subject ",
            "attach": attach,
        })),
        Some("w1:p1"),
    )
    .expect("owner ingest through dispatcher");
    let id = out["message"]["message_id"].as_str().unwrap().to_string();
    assert_eq!(out["duplicate"], false);
    assert_eq!(out["transport"], "telegram");
    assert_eq!(out["message"]["body"], "incoming question");
    assert_eq!(out["message"]["subject"], " original subject ");
    assert_eq!(out["message"]["external_id"], "telegram:88");
    assert_eq!(out["message"]["transport"], "telegram");
    assert_eq!(out["message"]["sender"], Value::Null);
    assert_eq!(out["message"]["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(
        out["message"]["attachments"][0]["filename"],
        "attachment with spaces.txt"
    );

    for (subject, expected) in [
        (json!(42), json!("42")),
        (json!(true), json!("1")),
        (json!(false), json!("0")),
        (Value::Null, Value::Null),
    ] {
        let scalar = dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({
                "body": "scalar subject",
                "transport": "web",
                "external_id": format!("scalar:{}", expected),
                "subject": subject,
            })),
            Some("w1:p1"),
        )
        .expect("scalar SQLite subject binding");
        assert_eq!(scalar["message"]["subject"], expected);
    }
    let omitted = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_ingest",
        args(json!({"body": "no subject", "transport": "web", "external_id": "scalar:omitted"})),
        Some("w1:p1"),
    )
    .expect("omitted subject");
    assert_eq!(omitted["message"]["subject"], Value::Null);

    let oversized: Value = serde_json::from_str(
        r#"{"body":"too large","transport":"web","external_id":"raw-overflow-subject","subject":18446744073709551616}"#,
    )
    .unwrap();
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(oversized),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "Python int too large to convert to SQLite INTEGER"
    ));
    let oversized_negative: Value = serde_json::from_str(
        r#"{"body":"too small","transport":"web","external_id":"raw-overflow-negative-subject","subject":-9223372036854775809}"#,
    )
    .unwrap();
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(oversized_negative),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "Python int too large to convert to SQLite INTEGER"
    ));
    for (wire, expected) in [
        (
            r#"{"body":"float","transport":"web","external_id":"float-decimal-pos","subject":18446744073709551616.0}"#,
            "1.84467440737096e+19",
        ),
        (
            r#"{"body":"float","transport":"web","external_id":"float-exponent-pos","subject":1.8446744073709552e19}"#,
            "1.84467440737096e+19",
        ),
        (
            r#"{"body":"float","transport":"web","external_id":"float-decimal-neg","subject":-9223372036854775809.0}"#,
            "-9.22337203685478e+18",
        ),
        (
            r#"{"body":"float","transport":"web","external_id":"float-exponent-neg","subject":-9.223372036854776e18}"#,
            "-9.22337203685478e+18",
        ),
    ] {
        let wire: Value = serde_json::from_str(wire).unwrap();
        let out = dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(wire),
            Some("w1:p1"),
        )
        .expect("floating-point subjects remain SQLite REAL binds");
        assert_eq!(out["message"]["subject"], expected);
    }
    assert!(flock
        .stores()
        .inbox
        .unwrap()
        .find_by_external_id("owner", Some("raw-overflow-subject"))
        .unwrap()
        .is_none());
    assert!(flock
        .stores()
        .inbox
        .unwrap()
        .find_by_external_id("owner", Some("raw-overflow-negative-subject"))
        .unwrap()
        .is_none());

    let duplicate = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_ingest",
        args(json!({
            "body": "redelivery",
            "transport": "telegram",
            "external_id": "telegram:88",
            "attach": "missing-file-must-not-be-opened",
        })),
        Some("w1:p1"),
    )
    .expect("duplicate returns the original message before attachment IO");
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["message"]["message_id"], id);
    assert_eq!(duplicate["message"]["body"], "incoming question");

    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({"body": "x", "transport": "bogus"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "transport must be one of web, telegram, report; got 'bogus'"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({"body": "x", "transport": "web", "attach": [attachment, 17]})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "no such file to attach: 17"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({"body": "x", "transport": "web", "attach": 17})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "'int' object is not iterable"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({"body": "denied", "transport": "web"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            None,
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "owner_ingest: missing required argument(s): body, transport"
    ));
}

#[test]
fn h_owner_outbox_matches_python() {
    let (_dir, flock, env) = owner_api_fixture("owner-outbox-api");
    let inbox = flock.stores().inbox.unwrap();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_reply_ingest_outbox();
    let mut telegram_questions = Vec::new();
    for body in ["first", "second"] {
        let question = inbox
            .send(&crate::inbox_store::Outgoing {
                mailbox: crate::flock::mailbox::OWNER_MAILBOX,
                body: Some(body),
                transport: Some("telegram"),
                ..crate::inbox_store::Outgoing::default()
            })
            .unwrap();
        telegram_questions.push(question);
    }
    let web_question = inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: crate::flock::mailbox::OWNER_MAILBOX,
            body: Some("web question"),
            transport: Some("web"),
            ..crate::inbox_store::Outgoing::default()
        })
        .unwrap();
    for question in telegram_questions
        .iter()
        .chain(std::iter::once(&web_question))
    {
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_reply",
            args(json!({"message_id": question, "body": "answer"})),
            Some("w1:p1"),
        )
        .unwrap();
    }

    let one = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_outbox",
        args(json!({"transport": " TELEGRAM ", "limit": "1"})),
        Some("w1:p1"),
    )
    .unwrap();
    assert_eq!(one["transport"], " TELEGRAM ");
    assert_eq!(one["count"], 1);
    assert_eq!(one["messages"][0]["reply_to"], telegram_questions[0]);
    assert_eq!(one["messages"][0]["sender"], "w1:p1");
    assert_eq!(one["messages"][0]["transport"], "telegram");
    assert_eq!(one["messages"][0]["read_at"], Value::Null);

    let all = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_outbox",
        args(json!({"transport": "telegram", "limit": -1})),
        Some("w1:p1"),
    )
    .unwrap();
    assert_eq!(all["count"], 2);
    let zero = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_outbox",
        args(json!({"transport": "telegram", "limit": 0})),
        Some("w1:p1"),
    )
    .unwrap();
    assert_eq!(zero["count"], 0);
    let defaulted = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_outbox",
        args(json!({"transport": "telegram", "limit": null})),
        Some("w1:p1"),
    )
    .unwrap();
    assert_eq!(defaulted["count"], 2);
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": "telegram", "limit": "bad"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "invalid literal for int() with base 10: 'bad'"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": "telegram", "limit": 9223372036854775808_u64})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "Python int too large to convert to SQLite INTEGER"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(serde_json::from_str(
                r#"{"transport":"web","limit":-9223372036854775809}"#
            )
            .unwrap()),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "Python int too large to convert to SQLite INTEGER"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": "bogus", "limit": 18446744073709551616_u128})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "transport must be one of web, telegram, report; got 'bogus'"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": null, "limit": 18446744073709551616_u128})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "owner_outbox: missing required argument(s): transport"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": null, "limit": "bad"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "owner_outbox: missing required argument(s): transport"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": "web", "limit": 18446744073709551616_u128})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": "web", "limit": "bad"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": null, "limit": 20})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "owner_outbox: missing required argument(s): transport"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            args(json!({"transport": "telegram", "limit": "9223372036854775808"})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "Python int too large to convert to SQLite INTEGER"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            None,
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_outbox",
            None,
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "owner_outbox: missing required argument(s): transport"
    ));
}

#[test]
fn h_owner_ingest_duplicate_preserves_raw_wire_order() {
    let (_dir, flock, env) = owner_api_fixture("owner-ingest-duplicate-wire-order");
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_reply_ingest_outbox();
    let first = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_ingest",
        args(json!({"body": "original", "transport": "web", "external_id": "wire-duplicate"})),
        Some("w1:p1"),
    )
    .unwrap();
    let message_id = first["message"]["message_id"].clone();

    for subject in [json!(["unsupported"]), json!({"unsupported": true})] {
        let duplicate = dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({
                "body": "redelivery",
                "transport": "web",
                "external_id": "wire-duplicate",
                "attach": 17,
                "subject": subject,
            })),
            Some("w1:p1"),
        )
        .expect("duplicate must return before raw attachment/subject handling");
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(duplicate["message"]["message_id"], message_id);
    }
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({
                "body": "redelivery",
                "transport": "bogus",
                "external_id": "wire-duplicate",
                "attach": 17,
                "subject": [],
            })),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "transport must be one of web, telegram, report; got 'bogus'"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_ingest",
            args(json!({"body": "redelivery", "transport": "bogus", "external_id": "wire-duplicate", "attach": 17})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
}

#[test]
fn h_owner_ingest_raw_errors_match_python_order() {
    let (dir, flock, env) = owner_api_fixture("owner-ingest-raw-errors");
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_owner_reply_ingest_outbox();
    let inbox = flock.stores().inbox.unwrap();
    let missing = dir.path().join("missing-before-subject-bind.txt");
    let attachment = dir.path().join("staged-before-subject-bind.txt");
    std::fs::write(&attachment, b"attachment fixture").unwrap();

    assert!(matches!(
        dispatch_owner_api(
            &dispatcher, &flock, &env, "owner_ingest",
            args(json!({"body": "  ", "transport": "web", "external_id": "raw-empty-body", "attach": []})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "a message needs a body or an attachment"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher, &flock, &env, "owner_ingest",
            args(json!({"body": "  ", "transport": "web", "external_id": "raw-noniterable", "attach": 17})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "'int' object is not iterable"
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher, &flock, &env, "owner_ingest",
            args(json!({"body": "x", "transport": "web", "external_id": "raw-missing-path", "attach": [missing.to_string_lossy()], "subject": []})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message.starts_with("no such file to attach:")
    ));
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher, &flock, &env, "owner_ingest",
            args(json!({"body": "x", "transport": "web", "external_id": "raw-python-dict-path", "attach": [{"x": true}]})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message == "no such file to attach: {'x': True}"
    ));
    for (external_id, subject, expected) in [
        (
            "raw-list-subject",
            json!([]),
            "Error binding parameter 6: type 'list' is not supported",
        ),
        (
            "raw-dict-subject",
            json!({"x": 1}),
            "Error binding parameter 6: type 'dict' is not supported",
        ),
    ] {
        assert!(matches!(
            dispatch_owner_api(
                &dispatcher, &flock, &env, "owner_ingest",
                args(json!({"body": "x", "transport": "web", "external_id": external_id, "attach": [attachment.to_string_lossy()], "subject": subject})),
                Some("w1:p1"),
            ),
            Err(DunkError::Refused(message)) if message == expected
        ));
    }
    for external_id in [
        "raw-empty-body",
        "raw-noniterable",
        "raw-missing-path",
        "raw-python-dict-path",
        "raw-list-subject",
        "raw-dict-subject",
    ] {
        assert!(
            inbox
                .find_by_external_id("owner", Some(external_id))
                .unwrap()
                .is_none(),
            "failed request inserted {external_id}"
        );
    }
}

#[test]
fn task_statistics_dispatch_tests_task_stats_metrics_reap_roles_and_outputs() {
    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    runtime
        .block_on(tasks.add("global", "visible", ""))
        .unwrap();
    runtime.block_on(tasks.add("w1:p1", "private", "")).unwrap();
    let stats = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_stats",
        json!({}),
        Some("w1:p1"),
        None,
    )
    .unwrap();
    assert_eq!(stats["totals"]["open"], 2);
    let metrics = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_metrics",
        json!({"window_s":"3600","bucket":""}),
        Some("w1:p1"),
        None,
    )
    .unwrap();
    assert_eq!(metrics["scope"], "visible");
    assert_eq!(metrics["window"]["seconds"], 3600);
    assert_eq!(metrics["window"]["bucket"], "hour");
    let denied = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_reap",
        json!({}),
        Some("w1:p1"),
        None,
    )
    .unwrap_err();
    assert!(
        denied.to_string().contains("task_reap is not permitted for role worker (pane w1:p1); see docs/FLAT_FLEET.md section 4.3"),
        "{denied}"
    );
    let fixture_refusal = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_reap",
        json!({}),
        Some("w1:p2"),
        None,
    )
    .unwrap_err();
    assert!(
        fixture_refusal.to_string().contains("alive_source='live'"),
        "{fixture_refusal}"
    );
    // These roles are not registerable pane identities in the real Roles store, so exercise
    // their registry policy through the same Dispatcher with the existing deterministic gate.
    for role in ["system", "web"] {
        let env = TestEnv::default();
        let request = RequestCtx::default();
        let context = HandlerContext {
            flock: &flock,
            request: &request,
            env: &env,
            runtime: runtime.handle(),
        };
        let gate = FixedRole(role, true);
        let refused = dispatcher
            .dispatch(&context, &gate, "task_reap", args(json!({})))
            .unwrap_err();
        assert!(
            refused.to_string().contains("alive_source='live'"),
            "{role}: {refused}"
        );
    }
}

#[test]
fn task_stats_defaults_scope_and_privacy_match_python_dispatch() {
    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    runtime.block_on(tasks.add("global", "global", "")).unwrap();
    runtime.block_on(tasks.add("w1:p1", "mine", "")).unwrap();
    runtime.block_on(tasks.add("w1:p3", "other", "")).unwrap();
    let visible = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_stats",
        json!({}),
        Some("w1:p1"),
        None,
    )
    .unwrap();
    assert_eq!(visible["totals"]["open"], 2);
    let global = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_stats",
        json!({"scope":"global"}),
        Some("w1:p1"),
        None,
    )
    .unwrap();
    assert_eq!(global["totals"]["open"], 1);
    let denied = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_stats",
        json!({"scope":"w1:p3"}),
        Some("w1:p1"),
        None,
    )
    .unwrap_err();
    assert!(
        denied.to_string().contains("per-pane tasks are private"),
        "{denied}"
    );
}

#[test]
fn task_metrics_defaults_coercions_and_scope_match_python_dispatch() {
    let (runtime, _dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    runtime.block_on(tasks.add("global", "global", "")).unwrap();
    runtime.block_on(tasks.add("w1:p1", "mine", "")).unwrap();
    runtime.block_on(tasks.add("w1:p3", "other", "")).unwrap();
    let default = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_metrics",
        json!({"window_s":null,"since":null,"until":null,"bucket":false}),
        Some("w1:p1"),
        None,
    )
    .unwrap();
    assert_eq!(default["scope"], "visible");
    assert_eq!(default["window"]["seconds"], 3600);
    assert_eq!(default["window"]["bucket"], "hour");
    let converted = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_metrics",
        json!({"window_s":"10","since":"90","until":"100","scope":"global","bucket":"DAY"}),
        Some("w1:p1"),
        None,
    )
    .unwrap();
    assert_eq!(converted["scope"], "global");
    assert_eq!(converted["window"]["seconds"], 10);
    assert_eq!(converted["window"]["bucket"], "day");
    let precedence = dispatch_task_mutation(
        &dispatcher,
        &flock,
        &runtime,
        "task_metrics",
        json!({"scope":"w1:p3", "since":{"bad":"type"}}),
        Some("w1:p1"),
        None,
    )
    .unwrap_err();
    assert!(
        precedence
            .to_string()
            .contains("per-pane tasks are private"),
        "{precedence}"
    );
}

#[test]
fn task_reap_live_tmux_listing_failure_reopens_expired_claim() {
    let (runtime, dir, flock, tasks, mut dispatcher) = task_mutation_api_fixture();
    let task = runtime
        .block_on(tasks.add("global", "expired", ""))
        .unwrap();
    runtime
        .block_on(tasks.set_status(&task.id, "in_progress", None, None))
        .unwrap();
    runtime.block_on(tasks.insert_lease(&task.id, "w9:p9", 1.0, 1.0, 2.0));
    let missing_tmux = dir.path().join("missing-tmux-command");
    let env = TestEnv(Mutex::new(Box::new(
        crate::terminal::tmux::TmuxBackend::new(missing_tmux),
    )));
    let request = RequestCtx {
        self_pane_id: Some("w1:p2".into()),
        ..RequestCtx::default()
    };
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    dispatcher.register_task_statistics();
    let reaped = dispatcher
        .dispatch(&context, &gate, "task_reap", args(json!({})))
        .unwrap();
    assert_eq!(
        reaped["requeued"],
        json!([{"task_id":task.id,"pane_id":"w9:p9","reason":"expired"}])
    );
    assert_eq!(
        runtime.block_on(tasks.get(&task.id)).unwrap().status,
        "open"
    );
    assert_eq!(runtime.block_on(tasks.leases(&task.id)), 0);
    let again = dispatcher
        .dispatch(&context, &gate, "task_reap", args(json!({})))
        .unwrap();
    assert_eq!(again["requeued"], json!([]));
}

#[test]
fn api_python_repr_preserves_mixed_quotes_and_controls() {
    assert_eq!(py_repr("a'\"\\\n\r\tb"), r###"'a\'"\\\n\r\tb'"###);
}

#[test]
fn socket_nonfinite_leases_reach_real_task_sql_and_preserve_error_order() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;

    struct TaskPeer<'a> {
        dispatcher: &'a Dispatcher,
        flock: &'a Flock,
        runtime: &'a tokio::runtime::Runtime,
    }
    impl crate::server::Dispatcher for TaskPeer<'_> {
        fn dispatch(&self, cmd: &str, input: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
            assert_eq!(ctx.socket_peer, Some(true));
            dispatch_task_mutation_request(
                self.dispatcher,
                self.flock,
                self.runtime,
                cmd,
                input.clone(),
                ctx,
            )
        }
    }
    let (runtime, dir, flock, tasks, dispatcher) = task_mutation_api_fixture();
    let task = runtime
        .block_on(tasks.add("global", "literal Infinity", ""))
        .unwrap();
    let path = dir.path().join("numeric.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let peer = TaskPeer {
        dispatcher: &dispatcher,
        flock: &flock,
        runtime: &runtime,
    };
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let peer_pid = crate::server::peer_pid(&socket);
            let reader = BufReader::new(socket.try_clone().unwrap());
            for line in reader.lines() {
                let (response, _) = crate::server::handle_line(&peer, &line.unwrap(), peer_pid);
                socket
                    .write_all(crate::protocol::encode_line(&response).as_bytes())
                    .unwrap();
            }
        });
        let mut socket = UnixStream::connect(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut call = |cmd: &str, raw_args: &str, pane: &str| {
            let request =
                format!(r#"{{"id":1,"cmd":"{cmd}","args":{raw_args},"self_pane_id":"{pane}"}}"#);
            socket.write_all(format!("{request}\n").as_bytes()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let value = crate::wire_json::from_str(&line).unwrap();
            (line, value)
        };
        let (line, claimed) = call(
            "task_next",
            r#"{"scope":"global","lease_seconds":Infinity}"#,
            "w1:p1",
        );
        assert_eq!(claimed["ok"], true, "{line}");
        assert_eq!(claimed["result"]["task"]["id"], task.id);
        assert!(line.contains(r#""lease_seconds":Infinity"#), "{line}");
        assert!(line.contains(r#""expires_at":Infinity"#), "{line}");
        assert_eq!(claimed["result"]["task"]["title"], "literal Infinity");
        let db = rusqlite::Connection::open(tasks.path()).unwrap();
        let before: (f64, i64) = db.query_row(
            "SELECT expires_at, (SELECT COUNT(*) FROM task_events WHERE task_id=?1) FROM task_leases WHERE task_id=?1",
            [&task.id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(before.0, f64::INFINITY);
        let meta: String = db
            .query_row(
                "SELECT meta FROM task_events WHERE task_id=?1 AND event='claimed'",
                [&task.id],
                |row| row.get(0),
            )
            .unwrap();
        let event = crate::wire_json::from_str(&meta).unwrap();
        assert_eq!(
            crate::github::parse_python_json_float(&event["lease_seconds"].to_string()).unwrap(),
            f64::INFINITY
        );
        let raw = format!(r#"{{"id":"{}","lease_seconds":NaN}}"#, task.id);
        let (_, denied) = call("task_heartbeat", &raw, "w1:p3");
        assert_eq!(denied["ok"], false);
        assert!(!denied["error"].as_str().unwrap().contains("NOT NULL"));
        let (_, missing) = call(
            "task_heartbeat",
            r#"{"id":"t-missing","lease_seconds":NaN}"#,
            "w1:p1",
        );
        assert!(missing["error"]
            .as_str()
            .unwrap()
            .contains("no task with id"));
        let (_, nan) = call("task_heartbeat", &raw, "w1:p1");
        assert!(
            nan["error"]
                .as_str()
                .unwrap()
                .contains("NOT NULL constraint failed: task_leases.expires_at"),
            "{nan}"
        );
        let after: (f64, i64) = db.query_row(
            "SELECT expires_at, (SELECT COUNT(*) FROM task_events WHERE task_id=?1) FROM task_leases WHERE task_id=?1",
            [&task.id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(
            before, after,
            "failed NaN heartbeat must not change lease or events"
        );
        drop(reader);
        drop(socket);
        worker.join().unwrap();
    });
}

#[path = "context_mutation_tests.rs"]
mod context_mutation_tests;
