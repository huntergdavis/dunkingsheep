//! t3241 tests for the production GitHub provider. Every App call goes to an in-process fake
//! endpoint: no socket is opened, and no real App, key or token is involved. The App
//! registration is a temporary fixture that reuses the public test key from `github_tests.rs`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;
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
use crate::tasks::Tasks;
use crate::terminal::{fakes::FakeTerminal, Terminal};

const FAKE_API: &str = "https://fake-app.invalid/api";
const NOW: f64 = 1_800_000_000.0;

/// A scripted App endpoint: it records every request and answers from a queue.
#[derive(Default)]
struct FakeAppEndpoint {
    responses: Mutex<VecDeque<GitHubResponse>>,
    requests: Mutex<Vec<GitHubRequest>>,
}

impl FakeAppEndpoint {
    fn answer(&self, status: u16, body: serde_json::Value) {
        self.responses.lock().unwrap().push_back(GitHubResponse {
            status,
            reason: "OK".into(),
            body: serde_json::to_vec(&body).unwrap(),
        });
    }

    /// Queues one complete App mint: the installation lookup, then the installation token.
    fn answer_mint(&self) {
        self.answer(200, json!([{"id": 4242}]));
        self.answer(
            201,
            json!({
                "token": "ghs_t3241_fixture",
                "expires_at": "2099-01-01T00:00:00Z",
                "permissions": {"contents": "read"}
            }),
        );
    }

    fn urls(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.url.clone())
            .collect()
    }
}

/// The transport the provider holds: a shared handle onto the scripted endpoint.
struct SharedEndpoint(Arc<FakeAppEndpoint>);

impl GitHubTransport for SharedEndpoint {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        self.0.requests.lock().unwrap().push(request);
        let answer = self
            .0
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "fake App endpoint has no scripted answer".to_owned());
        Box::pin(async move { answer })
    }
}

/// A registered author App whose key is the public fixture key from `github_tests.rs`.
fn write_author_app(config_dir: &Path) {
    std::fs::write(
        config_dir.join("github-app-author.json"),
        json!({"id": 4242, "slug": "dunkingsheep-author"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        config_dir.join("github-app-author.pem"),
        crate::github::tests::TEST_RSA_KEY,
    )
    .unwrap();
}

fn mint_urls() -> Vec<String> {
    vec![
        format!("{FAKE_API}/app/installations"),
        format!("{FAKE_API}/app/installations/4242/access_tokens"),
    ]
}

fn read_cache(path: &Path) -> serde_json::Value {
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
/// `RuntimeServices` fixtures use.
fn runtime_inputs(root: &Path, env: Env) -> RuntimeServiceInputs {
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

/// A runtime whose only registered App is the author fixture, served by the fake endpoint.
struct Fixture {
    services: Arc<RuntimeServices>,
    endpoint: Arc<FakeAppEndpoint>,
    config: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        write_author_app(&config);
        let endpoint = Arc::new(FakeAppEndpoint::default());
        let github = GitHubProvider::with_parts(
            config.join("github-tokens.json"),
            config.clone(),
            Box::new(SharedEndpoint(Arc::clone(&endpoint))),
            FAKE_API,
            Box::new(OsAppFileSystem),
        );
        let env = Env::from_pairs(&[("DUNKINGSHEEP_DIR", config.to_str().unwrap())]);
        let services =
            RuntimeServices::new_with_github(runtime_inputs(dir.path(), env), None, github)
                .unwrap();
        Self {
            services,
            endpoint,
            config,
            _dir: dir,
        }
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

    let services = RuntimeServices::new(runtime_inputs(root.path(), env)).unwrap();
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

    fixture.endpoint.answer_mint();
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

    fixture.endpoint.answer_mint();
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
