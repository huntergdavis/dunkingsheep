//! t3241 tests for the production GitHub provider. Every App and GitHub REST call goes to an
//! in-process fake endpoint: no socket is opened, and no real App, key or token is involved.
//! The App registrations are temporary fixtures that reuse the public test key from
//! `github_tests.rs`. The adapter tests dispatch through the production dispatcher, so each
//! request runs through the same `RequestRuntimeAdapter` the daemon builds.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::runtime::Builder;

use super::GitHubProvider;
use crate::api::CommandEnv;
use crate::config::{self, Env};
use crate::error::DunkError;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::panes::Stores;
use crate::github::{
    GitHubRequest, GitHubResponse, GitHubTransport, OsAppFileSystem, PinBoxFuture, DEFAULT_API,
};
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::server_runtime::{
    RequestRuntimeAdapter, RuntimeObservability, RuntimeServiceInputs, RuntimeServices,
};
use crate::tasks::{AddOptions, Tasks};
use crate::terminal::{fakes::FakeTerminal, Terminal};

const FAKE_API: &str = "https://fake-app.invalid/api";
const NOW: f64 = 1_800_000_000.0;

fn reply(status: u16, body: Value) -> GitHubResponse {
    GitHubResponse {
        status,
        reason: "OK".into(),
        body: serde_json::to_vec(&body).unwrap(),
    }
}

/// One open pull request, `o/r#7`, as the review sync lists it.
fn pull_seven() -> Value {
    json!({
        "number": 7,
        "title": "candidate",
        "html_url": "https://example.test/pull/7",
        "draft": false,
        "head": {"sha": "head-7", "ref": "work/topic"},
        "base": {"ref": "main"},
        "user": {"login": "author"}
    })
}

/// One object answers both reads of the pull: the mergeable state and the PR state.
fn pull_seven_detail() -> Value {
    let mut detail = pull_seven();
    detail["mergeable"] = json!(true);
    detail["mergeable_state"] = json!("clean");
    detail["state"] = json!("open");
    detail["body"] = json!("");
    detail
}

/// Answers every request the fixture expects. The App installation and token routes are
/// always served, and the GitHub REST routes cover only `o/r#7`. Anything else answers 404,
/// so a stray call shows up as an error instead of passing silently.
fn github_answer(request: &GitHubRequest) -> GitHubResponse {
    let url = request.url.as_str();
    match request.method.as_str() {
        "GET" if url.ends_with("/app/installations") => reply(200, json!([{"id": 4242}])),
        "POST" if url.ends_with("/access_tokens") => reply(
            201,
            json!({
                "token": "ghs_t3241_fixture",
                "expires_at": "2099-01-01T00:00:00Z",
                "permissions": {"contents": "read"}
            }),
        ),
        "GET" if url.ends_with("/repos/o/r/pulls?state=open&per_page=100&page=1") => {
            reply(200, json!([pull_seven()]))
        }
        "GET" if url.ends_with("/repos/o/r/pulls/7") => reply(200, pull_seven_detail()),
        _ => reply(404, json!({"message": "not stubbed by the t3241 fixture"})),
    }
}

/// The fake App endpoint. It records every request and answers through `github_answer`.
struct FakeAppEndpoint {
    requests: Mutex<Vec<GitHubRequest>>,
}

impl FakeAppEndpoint {
    fn requests(&self) -> Vec<GitHubRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn urls(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.url)
            .collect()
    }
}

/// A transport handle onto the shared fake endpoint. The provider holds one for token minting
/// and one for the review service, and both record into the same log.
struct SharedEndpoint(Arc<FakeAppEndpoint>);

impl GitHubTransport for SharedEndpoint {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        let answer = github_answer(&request);
        self.0.requests.lock().unwrap().push(request);
        Box::pin(async move { Ok(answer) })
    }
}

/// A registered App whose key is the public fixture key from `github_tests.rs`.
fn write_app(config_dir: &Path, kind: &str) {
    std::fs::write(
        config_dir.join(format!("github-app-{kind}.json")),
        json!({"id": 4242, "slug": format!("dunkingsheep-{kind}")}).to_string(),
    )
    .unwrap();
    std::fs::write(
        config_dir.join(format!("github-app-{kind}.pem")),
        crate::github::tests::TEST_RSA_KEY,
    )
    .unwrap();
}

/// The lanes the adapter tests use: a worker, the owner, and a reviewer on the OpenAI
/// provider, which is the only reviewer provider the completion gate accepts here.
fn lanes() -> Value {
    json!({
        "lanes": {
            "%worker": {"role": "worker", "provider": "codex"},
            "%owner": {"role": "owner", "provider": "claude"},
            "%reviewer": {"role": "reviewer", "provider": "openai"}
        },
        "reviewer_providers": ["openai"]
    })
}

fn mint_urls() -> Vec<String> {
    vec![
        format!("{FAKE_API}/app/installations"),
        format!("{FAKE_API}/app/installations/4242/access_tokens"),
    ]
}

fn read_cache(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[cfg(unix)]
fn assert_private_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the token cache is written owner-only");
}

#[cfg(not(unix))]
fn assert_private_mode(_path: &Path) {}

/// Inputs for a standard-thread runtime with temporary stores, the shape the existing
/// `RuntimeServices` fixtures use. The lanes are written as the role registry's roles file.
fn runtime_inputs(root: &Path, env: Env, lanes: Value) -> RuntimeServiceInputs {
    let runtime = Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(Tasks::open(root.join("tasks.db")))
            .unwrap(),
    );
    runtime.block_on(tasks.allocation_init()).unwrap();
    let inbox = Arc::new(
        crate::inbox_store::Store::open(root.join("inbox.db"), root.join("blobs")).unwrap(),
    );
    std::fs::write(root.join("roles.json"), lanes.to_string()).unwrap();
    let roles = Roles::new(
        &env,
        Some(root.join("roles.json")),
        Some(root.join("roles-tokens.json")),
        None,
    )
    .unwrap();
    let executable = root.join("daemon");
    std::fs::write(&executable, b"test daemon identity").unwrap();
    let terminal_factory: LoopTerminalFactory =
        Arc::new(|| -> Box<dyn Terminal> { Box::new(FakeTerminal::new()) });
    RuntimeServiceInputs {
        launches_file: root.join("launches.json"),
        lane_registry_file: root.join("lanes.json"),
        persist_enabled: false,
        stores: Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        },
        roles,
        legacy_superusers: None,
        release_version: "test".into(),
        state_file: root.join("state.json"),
        executable,
        env,
        observability: Some(RuntimeObservability {
            state_dir: root.join("state"),
            memory_db: None,
        }),
        runtime,
        terminal_factory,
    }
}

/// A runtime whose GitHub Apps and lanes are set per test. Every App call goes to the fake
/// endpoint, and the provider's token cache and review transport both point there.
struct Fixture {
    services: Arc<RuntimeServices>,
    endpoint: Arc<FakeAppEndpoint>,
    config: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// The author App and no lanes: enough for the provider-level tests.
    fn new() -> Self {
        Self::with(&["author"], json!({"lanes": {}}))
    }

    fn with(apps: &[&str], lanes: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        for kind in apps {
            write_app(&config, kind);
        }
        std::fs::write(config.join("github-repos.json"), r#"{"repos":["o/r"]}"#).unwrap();
        let endpoint = Arc::new(FakeAppEndpoint {
            requests: Mutex::new(Vec::new()),
        });
        let github = GitHubProvider::with_parts(
            config.join("github-tokens.json"),
            config.clone(),
            Box::new(SharedEndpoint(Arc::clone(&endpoint))),
            Box::new(SharedEndpoint(Arc::clone(&endpoint))),
            FAKE_API,
            Box::new(OsAppFileSystem),
        );
        let env = Env::from_pairs(&[("DUNKINGSHEEP_DIR", config.to_str().unwrap())]);
        let services =
            RuntimeServices::new_with_github(runtime_inputs(dir.path(), env, lanes), None, github)
                .unwrap();
        Self {
            services,
            endpoint,
            config,
            _dir: dir,
        }
    }

    fn tasks(&self) -> Arc<Tasks> {
        self.services
            .flock()
            .stores()
            .tasks
            .expect("the runtime has a task store")
    }

    /// Runs one future on the runtime's own handle, as the daemon's handlers do.
    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.services.runtime.handle().block_on(future)
    }

    /// Dispatches one command through the production dispatcher as `pane`. The adapter built
    /// for the call is the same one the daemon builds per request.
    fn dispatch(&self, cmd: &str, args: Value, pane: &str) -> Result<Value, DunkError> {
        let request = RequestCtx {
            self_pane_id: Some(pane.into()),
            ..RequestCtx::default()
        };
        self.services
            .production_dispatcher()
            .dispatch(cmd, &args, &request)
    }
}

#[test]
fn production_provider_resolves_config_dir_cache_and_api_root_from_the_captured_env() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::from_pairs(&[
        ("DUNKINGSHEEP_DIR", dir.path().to_str().unwrap()),
        ("DUNKINGSHEEP_GITHUB_API", FAKE_API),
    ]);
    let provider = GitHubProvider::production(&env);
    let tokens = provider.tokens().unwrap();
    assert_eq!(tokens.config_dir(), dir.path());
    assert_eq!(
        tokens.path(),
        dir.path().join("github-tokens.json").as_path()
    );
    assert_eq!(tokens.api_root(), FAKE_API);
}

#[test]
fn blank_overrides_fall_back_to_the_home_config_directory_and_the_public_api() {
    let home = tempfile::tempdir().unwrap();
    let env = Env::from_pairs(&[
        ("DUNKINGSHEEP_DIR", "   "),
        ("HOME", home.path().to_str().unwrap()),
        ("DUNKINGSHEEP_GITHUB_API", ""),
    ]);
    let provider = GitHubProvider::production(&env);
    let tokens = provider.tokens().unwrap();
    let expected_dir = home.path().join(".config").join("dunkingsheep");
    assert_eq!(tokens.config_dir(), expected_dir.as_path());
    assert_eq!(
        tokens.path(),
        expected_dir.join("github-tokens.json").as_path()
    );
    assert_eq!(tokens.api_root(), DEFAULT_API);
}

#[test]
fn explicit_token_cache_override_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("elsewhere").join("cache.json");
    let env = Env::from_pairs(&[
        ("DUNKINGSHEEP_DIR", dir.path().to_str().unwrap()),
        ("DUNKINGSHEEP_GITHUB_TOKENS_FILE", cache.to_str().unwrap()),
    ]);
    let provider = GitHubProvider::production(&env);
    let tokens = provider.tokens().unwrap();
    assert_eq!(tokens.path(), cache.as_path());
    assert_eq!(tokens.config_dir(), dir.path());
}

#[test]
fn unresolvable_config_dir_is_a_stored_refusal_and_the_daemon_still_constructs() {
    let root = tempfile::tempdir().unwrap();
    // Neither DUNKINGSHEEP_DIR nor HOME is set, so the config directory cannot be found.
    let env = Env::from_pairs(&[]);
    let expected = config::config_dir(&env).unwrap_err().to_string();
    let provider = GitHubProvider::production(&env);
    let refused = provider.tokens().err().expect("token use is refused");
    assert!(matches!(refused, DunkError::Refused(ref message) if *message == expected));

    let services =
        RuntimeServices::new(runtime_inputs(root.path(), env, json!({"lanes": {}}))).unwrap();
    let refused = services
        .github_tokens()
        .err()
        .expect("the runtime refuses token use");
    assert!(matches!(refused, DunkError::Refused(ref message) if *message == expected));
}

#[test]
fn request_adapter_borrows_the_runtime_token_cache_and_refuses_when_detached() {
    let fixture = Fixture::new();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let request = RequestCtx::default();
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));

    let detached = RequestRuntimeAdapter::new(
        fixture.services.flock(),
        &request,
        runtime.handle(),
        &terminal,
    );
    let refused = detached
        .github_tokens()
        .err()
        .expect("a detached adapter cannot reach a token cache");
    assert!(matches!(refused, DunkError::Refused(ref message) if message.contains("not attached")));

    let attached = RequestRuntimeAdapter::new(
        fixture.services.flock(),
        &request,
        runtime.handle(),
        &terminal,
    )
    .with_dunk_runtime(&fixture.services)
    .unwrap();
    let borrowed = attached.github_tokens().unwrap();
    assert!(std::ptr::eq(
        borrowed,
        fixture.services.github_tokens().unwrap()
    ));
}

#[test]
fn author_token_mints_once_then_serves_from_the_runtime_cache_and_forgets_on_revocation() {
    let fixture = Fixture::new();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let tokens = fixture.services.github_tokens().unwrap();
    let now = Some(NOW);

    let minted = runtime.block_on(tokens.get("author", now)).unwrap();
    assert_eq!(minted["cached"], json!(false));
    assert_eq!(minted["slug"], json!("dunkingsheep-author"));
    assert!(
        minted.get("token").is_some(),
        "the mint carries an installation token"
    );
    assert_eq!(fixture.endpoint.urls(), mint_urls());

    let again = runtime.block_on(tokens.get("author", now)).unwrap();
    assert_eq!(again["cached"], json!(true));
    assert_eq!(
        fixture.endpoint.urls().len(),
        2,
        "a fresh cached token makes no App call"
    );

    let cache_path = fixture.config.join("github-tokens.json");
    assert!(
        read_cache(&cache_path)["tokens"]["author"].is_object(),
        "the mint is persisted beside the App registrations"
    );
    assert_private_mode(&cache_path);

    runtime.block_on(tokens.forget(Some("author"))).unwrap();
    assert!(
        tokens.cached("author").is_none(),
        "revocation clears the in-memory entry"
    );
    assert!(
        read_cache(&cache_path)["tokens"].get("author").is_none(),
        "revocation is persisted"
    );

    let reminted = runtime.block_on(tokens.get("author", now)).unwrap();
    assert_eq!(
        reminted["cached"],
        json!(false),
        "after revocation the next use mints again"
    );
    assert_eq!(fixture.endpoint.urls().len(), 4);
}

#[test]
fn missing_app_registration_reports_its_real_refusal_without_an_app_call() {
    let fixture = Fixture::new();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let tokens = fixture.services.github_tokens().unwrap();
    let error = runtime
        .block_on(tokens.get("reviewer", Some(NOW)))
        .expect_err("no reviewer App is registered");
    let message = error.to_string();
    assert!(
        message.contains("no GitHub App registered as 'reviewer'"),
        "the real registration refusal is reported: {message}"
    );
    assert!(
        !message.contains("not wired"),
        "the unwired default is gone"
    );
    assert!(
        fixture.endpoint.urls().is_empty(),
        "no App call is attempted without a registration"
    );
}

#[test]
fn gh_token_through_the_production_adapter_mints_the_author_token_and_reports_provenance() {
    let fixture = Fixture::with(&["author"], lanes());
    let response = fixture
        .dispatch("gh_token", json!({"kind": "author"}), "%worker")
        .unwrap();
    assert_eq!(response["kind"], json!("author"));
    assert_eq!(response["role"], json!("worker"));
    assert!(
        response["token"].is_string(),
        "the author token is returned"
    );
    assert_eq!(response["provenance"], json!({"recorded": true}));
    assert!(
        crate::github::author_mint_path(None, &fixture.config).exists(),
        "the mint is recorded on disk"
    );
    assert_eq!(fixture.endpoint.urls(), mint_urls());
}

/// `git-credential` (ds-cli) asks the daemon for `gh_token` with no arguments, and the worker's
/// role implies the author App. This is that exact request through the production adapter.
#[test]
fn git_credential_request_for_a_worker_mints_through_the_same_adapter() {
    let fixture = Fixture::with(&["author"], lanes());
    let response = fixture.dispatch("gh_token", json!({}), "%worker").unwrap();
    assert_eq!(response["kind"], json!("author"));
    assert_eq!(response["role"], json!("worker"));
    assert!(
        response["token"].is_string(),
        "the author token is returned"
    );
    assert_eq!(response["provenance"]["recorded"], json!(true));
    assert_eq!(fixture.endpoint.urls(), mint_urls());
}

#[test]
fn author_mint_write_failure_is_reported_and_the_token_still_succeeds() {
    let fixture = Fixture::with(&["author"], lanes());
    // A directory where the mint record belongs makes the atomic rename fail.
    let blocked = crate::github::author_mint_path(None, &fixture.config);
    std::fs::create_dir_all(&blocked).unwrap();
    let response = fixture
        .dispatch("gh_token", json!({"kind": "author"}), "%worker")
        .unwrap();
    assert!(
        response["token"].is_string(),
        "a lost mint record never fails the token"
    );
    assert_eq!(response["provenance"]["recorded"], json!(false));
    assert!(
        response["provenance"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("could not write the author mint record")),
        "the lost record is reported with its reason"
    );
}

#[test]
fn review_sync_dry_run_through_the_production_adapter_reads_github_and_writes_nothing() {
    let fixture = Fixture::with(&["author", "reviewer"], lanes());
    let report = fixture
        .dispatch("review_sync", json!({"dry_run": true}), "%owner")
        .unwrap();
    assert_eq!(report["repos"][0]["repo"], json!("o/r"));
    assert_eq!(report["repos"][0]["open"], json!(1));
    let requests = fixture.endpoint.requests();
    assert!(
        requests.iter().any(|request| request
            .url
            .ends_with("/repos/o/r/pulls?state=open&per_page=100&page=1")),
        "the review sync listed the open pull request through the adapter's transport"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.method != "GET")
            .all(|request| request.url.ends_with("/access_tokens")),
        "a dry run may mint App tokens but must not write to GitHub"
    );
    assert!(
        fixture
            .block_on(fixture.tasks().list_review_rows_visible())
            .unwrap()
            .is_empty(),
        "a dry run files no review rows"
    );
}

#[test]
fn task_complete_verdict_error_is_reported_through_the_production_adapter() {
    // No reviewer App is registered, so the verdict's GitHub side cannot authenticate.
    let fixture = Fixture::with(&["author"], lanes());
    let tasks = fixture.tasks();
    let review = fixture
        .block_on(tasks.add_with(
            "global",
            "review",
            "",
            AddOptions {
                kind: Some("review".into()),
                assignees: vec!["%reviewer".into()],
                author: Some("%someone-else".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some("head-7".into()),
                ..Default::default()
            },
        ))
        .unwrap();
    fixture
        .block_on(tasks.claim_next("global", "%reviewer", Default::default()))
        .unwrap();
    fixture
        .block_on(tasks.note(&review.id, "note", "%reviewer", Some("reviewed")))
        .unwrap();

    let result = fixture
        .dispatch(
            "task_complete",
            json!({"id": review.id.clone(), "verdict": "accept", "landed": true}),
            "%reviewer",
        )
        .unwrap();

    let errors = result["github"]["errors"]
        .as_array()
        .expect("the verdict failure is reported under github.errors");
    assert!(
        errors.iter().any(|error| error
            .as_str()
            .is_some_and(|text| text.contains("no GitHub App registered as 'reviewer'"))),
        "the real registration refusal is reported"
    );
    assert_eq!(
        "done",
        fixture.block_on(tasks.get(&review.id)).unwrap().status
    );
    assert_eq!(
        Some("accept"),
        fixture
            .block_on(tasks.get_full_wire(&review.id, None))
            .unwrap()
            .wire["verdict"]
            .as_str()
    );
    assert!(
        fixture
            .endpoint
            .requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no write reaches GitHub when the reviewer App is missing"
    );
}

#[test]
fn pr_wait_production_registration_listing_and_cancellation_are_caller_scoped() {
    let f = Fixture::with(&["reviewer"], lanes());
    f.services
        .flock()
        .remember_lane("%worker", "wait-worker", true);
    f.services
        .flock()
        .remember_lane("%reviewer", "wait-reviewer", true);
    let wait = f
        .dispatch("pr_wait", json!({"target":"o/r#7"}), "%worker")
        .unwrap();
    assert!(f
        .dispatch(
            "pr_wait",
            json!({"target":"unregistered/repo#1"}),
            "%worker"
        )
        .is_err());
    assert!(f
        .dispatch(
            "pr_wait",
            json!({"target":"o/r#7","head":"short"}),
            "%worker"
        )
        .is_err());
    let listed = f
        .dispatch("pr_wait", json!({"list":true}), "%worker")
        .unwrap();
    assert_eq!(listed[0]["id"], wait["id"]);
    assert!(listed[0].get("last_poll_at").is_some());
    assert!(f
        .dispatch("pr_wait", json!({"list":true}), "%reviewer")
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        f.dispatch("pr_wait", json!({"cancel":wait["id"]}), "%reviewer")
            .unwrap()["cancelled"],
        0
    );
    assert_eq!(
        f.dispatch("pr_wait", json!({"cancel":wait["id"]}), "%worker")
            .unwrap()["cancelled"],
        1
    );
    assert!(
        f.endpoint.requests().is_empty(),
        "registration/list/cancel makes no API calls"
    );
}
