use super::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[derive(Default)]
struct FixtureFs {
    data: Mutex<BTreeMap<PathBuf, Result<String, std::io::ErrorKind>>>,
    keys: Mutex<Vec<PathBuf>>,
    key_contents: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
}

impl AppFileSystem for FixtureFs {
    fn read_config(&self, path: &Path) -> std::io::Result<String> {
        match self.data.lock().unwrap().get(path) {
            Some(Ok(value)) => Ok(value.clone()),
            Some(Err(kind)) => Err(std::io::Error::from(*kind)),
            None => Err(std::io::ErrorKind::NotFound.into()),
        }
    }
    fn key_exists(&self, path: &Path) -> bool {
        self.keys.lock().unwrap().contains(&path.to_path_buf())
    }
    fn read_key(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.key_contents
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

fn configured(kind: &str, json: &str, key: &str) -> (PathBuf, FixtureFs) {
    let dir = PathBuf::from("/isolated/config");
    let fs = FixtureFs::default();
    fs.data
        .lock()
        .unwrap()
        .insert(app_file(kind, &dir), Ok(json.into()));
    fs.keys.lock().unwrap().push(PathBuf::from(key));
    (dir, fs)
}

#[test]
fn load_app_supports_both_kinds_numeric_string_id_and_defaults() {
    for kind in KINDS {
        let key = format!("/isolated/config/github-app-{kind}.pem");
        let (dir, fs) = configured(kind, r#"{"id":" 42 ","slug":"","pem":""}"#, &key);
        let app = load_app(kind, &dir, &fs).unwrap();
        assert_eq!(app.id, 42);
        assert_eq!(app.slug, format!("dunkingsheep-{kind}"));
        assert_eq!(app.pem, PathBuf::from(key));
    }
}

#[test]
fn load_app_preserves_explicit_slug_and_pem_and_refusal_order() {
    let (dir, fs) = configured(
        "author",
        r#"{"id":7,"slug":"custom","pem":"/keys/test.pem"}"#,
        "/keys/test.pem",
    );
    let app = load_app("author", &dir, &fs).unwrap();
    assert_eq!(
        app,
        AppConfig {
            id: 7,
            slug: "custom".into(),
            pem: "/keys/test.pem".into()
        }
    );
    let error = load_app("unknown", &dir, &fs).unwrap_err();
    assert_eq!(
        error.to_string(),
        "app kind must be one of author, reviewer; got 'unknown'"
    );
}

#[test]
fn load_app_distinguishes_missing_unreadable_malformed_and_missing_key() {
    let dir = PathBuf::from("/cfg");
    let fs = FixtureFs::default();
    assert!(load_app("reviewer", &dir, &fs)
        .unwrap_err()
        .to_string()
        .contains(
            "no GitHub App registered as 'reviewer': /cfg/github-app-reviewer.json is missing"
        ));
    fs.data.lock().unwrap().insert(
        app_file("author", &dir),
        Err(std::io::ErrorKind::PermissionDenied),
    );
    assert!(load_app("author", &dir, &fs)
        .unwrap_err()
        .to_string()
        .contains("no GitHub App registered as 'author'"));
    fs.data
        .lock()
        .unwrap()
        .insert(app_file("author", &dir), Ok("{".into()));
    assert!(load_app("author", &dir, &fs)
        .unwrap_err()
        .to_string()
        .starts_with("invalid app config JSON:"));
    fs.data
        .lock()
        .unwrap()
        .insert(app_file("author", &dir), Ok(r#"{"id":1}"#.into()));
    assert_eq!(
        load_app("author", &dir, &fs).unwrap_err().to_string(),
        "GitHub App 'author' private key missing: /cfg/github-app-author.pem"
    );
}

const TEST_RSA_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCdd4z+Ptd6LbDm\nrtFbHknrfBcEdlMsEfbSc08K89MVFSqbr/SmjGvwEc0+yyL1VO60lpRWU3ukjwbt\nPtaevcB8h/alfFlo1G5LOtm/UK8DZ8cqObeUIc8TzS8tfqVZiQWPj2/wDmaPlSxV\nV8l6JcHyyXH7lyXdYn4CYX9QT+ZBNWv/mVHOO0lrb9AZFgiBHXM3TYpI1+eFPwri\npjUD2FWczTZRxfsK9XyoC4rIHi1ZY8SURLnTHQ6jltYuXKjTcmgn3FnD6ZmRcx8S\nZkW8NTmjlsLy6TG2q3QoEDoD2I5//ERa7sGKFPy2j6qW/D/B/c1c/Gl5Rw9dcGkO\nUc2TamHXAgMBAAECggEABQzuksEFPD/26o80281O5aSnQFdjN4ij+AAGyv0jCgwR\nv8iW+jiKxbpscNUnG+NLfLNPsq0ezBpiSH25eIHxiIz71jb0zy3xE6yhi0dVNbDA\nbJRJVRD4iVQNbVIgj4ybhw08s+BiXbyVHW+hYK+RVu4rH7yrVDxyejUSVksZwYGe\nPCRcnotAw+cOzBV72W2uFJUabYHK1xS47LHnlv9x+5QNBR0Qx+eXblOyvwK5Wrx7\ngv2Ipfn8q2jnopTsHA+Pl4WKDZzhqomBQ/1TBMkMqwavmJEScRlxH9ZDcdvvDnCu\nQxF23NE8PJBQVnvtv8GTNuUMOmD2gT+Cfg6an3sGkQKBgQDVHjXCSoUhuvVZuNQ7\nHziXnNDCX0Pn2PWg0i317xSSLoO1kuZO4xqiB0OOQfdMVsC1Q1tNiHYETnhDDZIV\noeBmeyHLp2Jez8qpmL4CvXGGkKqyJXvPwHPU289vz3ET16ypdtEDt/7SbwFlhn3S\n9IhiPlqp5B8C6FGNUMKmsFPf8wKBgQC9JrxfqbmmNX5GQI7H6Z+92uQtp0xYOpbf\nDXIfTl3UHO2Z75QtIjUPAdTD7X5HYOvb/ENY3XPPdc+HQVzhgYD/mCFliYmCcGdy\nnSXTLgKb7MJWjoC+/M7bTka5I6xM0cBLwkrPz6OOK7iK377x1ZQ8DU5O4EbgGcb1\nXm4vpUMTjQKBgG2eDU6zH8karcoJbHhAdc5d5W6kq3vcN3MRyHwgDfemny5MsdNP\nsso2bEQGoBmO4a4aSCMYFoPejLMIjuQ7j/2GRfrHTKyxum8DVnB4WqU5UeMxrn5M\nYgfRS+AZ1fK7016S860y/YnleNVsmNlh16iYo++Qm3bWYeQK7yPnvCyLAoGAGTvc\nr4pt+O1z/h1IrwTMycA05p5GAhM242Tgz6pdvBlr3Thn9Sl2GJ1u7AB38z/L11ot\nBeLtO0UFPY8LvA3RzsdV1Tzm+l7SfijWzfh6YqDQQEnchR4m3nzv1G0c5FarJ7Aw\n2M5jjGjqwqQczhhnISfJRZ6H9geaUkuZuYN7klUCgYBaFWs/K1bI4uc10ijs8kM5\ntIYAacFAjiKAQvsC4TAQJbmO99aFN0B5KuE/Zdhpz/mAMLPjSBjcxRA2Rpp+Mi3D\nJTM8xpZY+9b3ORfNM1sdkE11d5SJeDOK66DT65XGbTAC98ABjf206hwTFYudwDM5\nkDe/RCKYtGMQebnrDowfWA==\n-----END PRIVATE KEY-----\n";

#[test]
fn app_jwt_uses_rs256_and_exact_claim_times() {
    let dir = tempdir().unwrap();
    let pem = dir.path().join("fixture.pem");
    fs::write(&pem, TEST_RSA_KEY).unwrap();
    let jwt = app_jwt(
        &AppConfig {
            id: 123,
            slug: "fixture".into(),
            pem,
        },
        Some(10_000),
    )
    .unwrap();
    let header = jsonwebtoken::decode_header(&jwt).unwrap();
    assert_eq!(header.alg, Algorithm::RS256);
    let mut validation = jsonwebtoken::Validation::new(Algorithm::RS256);
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.required_spec_claims.clear();
    validation.insecure_disable_signature_validation();
    let claims = jsonwebtoken::decode::<Value>(
        &jwt,
        &jsonwebtoken::DecodingKey::from_secret(b"fixture"),
        &validation,
    )
    .unwrap()
    .claims;
    assert_eq!(claims["iat"], 9_940);
    assert_eq!(claims["exp"], 10_540);
    assert_eq!(claims["iss"], 123);
}

struct FakeTransport {
    next: Mutex<Result<GitHubResponse, String>>,
    seen: Mutex<Vec<GitHubRequest>>,
    queued: Mutex<VecDeque<Result<GitHubResponse, String>>>,
}
impl Default for FakeTransport {
    fn default() -> Self {
        Self {
            next: Mutex::new(Ok(GitHubResponse {
                status: 200,
                reason: "OK".into(),
                body: Vec::new(),
            })),
            seen: Mutex::new(Vec::new()),
            queued: Mutex::new(VecDeque::new()),
        }
    }
}
impl GitHubTransport for FakeTransport {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        self.seen.lock().unwrap().push(request);
        let result = self
            .queued
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.next.lock().unwrap().clone());
        Box::pin(async move { result })
    }
}

impl FakeTransport {
    fn enqueue(&self, response: GitHubResponse) {
        self.queued.lock().unwrap().push_back(Ok(response));
    }
}

#[test]
fn request_preserves_headers_body_timeout_and_bearer_precedence() {
    let fake = FakeTransport::default();
    *fake.next.lock().unwrap() = Ok(GitHubResponse {
        status: 201,
        reason: "Created".into(),
        body: br#"{"id":4}"#.to_vec(),
    });
    let payload = serde_json::json!({"name":"x"});
    let result = futuresless_block_on(request(
        &fake,
        RequestOptions {
            method: "POST",
            url: "https://api.github.com/repos/o/r",
            token: Some("tok"),
            bearer: Some("jwt"),
            payload: Some(&payload),
            timeout_s: 7,
            api_root: DEFAULT_API,
        },
    ))
    .unwrap();
    assert_eq!(result, (201, Some(serde_json::json!({"id":4}))));
    let seen = fake.seen.lock().unwrap();
    let request = &seen[0];
    assert_eq!(request.headers["Accept"], "application/vnd.github+json");
    assert_eq!(request.headers["User-Agent"], "dunkingsheep");
    assert_eq!(request.headers["X-GitHub-Api-Version"], "2022-11-28");
    assert_eq!(request.headers["Authorization"], "Bearer jwt");
    assert_eq!(request.headers["Content-Type"], "application/json");
    assert_eq!(request.body.as_deref(), Some(br#"{"name":"x"}"#.as_slice()));
    assert_eq!(request.timeout_s, 7);
}

#[test]
fn request_formats_api_errors_and_unreachable_errors() {
    let fake = FakeTransport::default();
    *fake.next.lock().unwrap() = Ok(GitHubResponse {
        status: 403,
        reason: "Forbidden".into(),
        body: br#"{"message":"denied"}"#.to_vec(),
    });
    let error = futuresless_block_on(request(
        &fake,
        RequestOptions {
            method: "GET",
            url: "https://api.github.com/app",
            token: None,
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root: DEFAULT_API,
        },
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), "GitHub GET /app -> 403: denied");
    *fake.next.lock().unwrap() = Err("timed out".into());
    let error = futuresless_block_on(request(
        &fake,
        RequestOptions {
            method: "GET",
            url: "url",
            token: None,
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root: DEFAULT_API,
        },
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), "GitHub unreachable: timed out");
}

fn futuresless_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut context = Context::from_waker(Waker::noop());
    let mut future = Box::pin(future);
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return result;
        }
    }
}

#[test]
fn token_load_filters_bad_entries_and_save_is_private_and_atomic() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.json");
    let stale_temp = dir.path().join("cache.json.tmp");
    fs::write(&stale_temp, "stale sentinel, not token data").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stale_temp, fs::Permissions::from_mode(0o644)).unwrap();
    }
    fs::write(&path, r#"{"tokens":{"good":{"expires_at":9,"token":"x"},"missing":{},"false":{"expires_at":0},"scalar":2}}"#).unwrap();
    let fake = Box::new(FakeTransport::default());
    let mut tokens = Tokens::new(
        Some(path.clone()),
        dir.path().into(),
        fake,
        Some(DEFAULT_API.into()),
    );
    assert!(tokens.cached("good").is_some());
    assert!(tokens.cached("missing").is_none());
    assert!(tokens.cached("false").is_none());
    assert!(tokens.cached("scalar").is_none());
    tokens.cache.get_mut().unwrap().insert(
        "fixture".into(),
        serde_json::json!({"expires_at":123,"token":"fixture-secret"}),
    );
    tokens.save().unwrap();
    assert_eq!(
        fs::read_to_string(&stale_temp).unwrap(),
        "stale sentinel, not token data"
    );
    assert!(!fs::read_to_string(&stale_temp)
        .unwrap()
        .contains("fixture-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&stale_temp).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
        let name = entry.unwrap().file_name();
        !name.to_string_lossy().starts_with(".cache.json.tmp.")
    }));
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["tokens"]["fixture"]["token"], "fixture-secret");
}

#[test]
fn token_cache_save_removes_temporary_file_when_publish_fails() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.json");
    fs::create_dir(&path).unwrap();
    let fake = Box::new(FakeTransport::default());
    let mut tokens = Tokens::new(
        Some(path.clone()),
        dir.path().into(),
        fake,
        Some(DEFAULT_API.into()),
    );
    tokens.cache.get_mut().unwrap().insert(
        "fixture".into(),
        serde_json::json!({"expires_at":123,"token":"fixture-secret"}),
    );

    assert!(tokens.save().is_err());
    assert!(path.is_dir());
    assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".cache.json.tmp.")
    }));
}

#[test]
fn installation_id_returns_first_installation_or_exact_empty_refusal() {
    let app = AppConfig {
        id: 1,
        slug: "fixture".into(),
        pem: "unused".into(),
    };
    let fake = FakeTransport::default();
    *fake.next.lock().unwrap() = Ok(GitHubResponse {
        status: 200,
        reason: "OK".into(),
        body: br#"[{"id":"19"},{"id":20}]"#.to_vec(),
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let transport = Box::new(RecordingTransport {
        inner: fake,
        recorded: Arc::clone(&seen),
    });
    let tokens = Tokens::new(
        None,
        PathBuf::from("/isolated"),
        transport,
        Some(DEFAULT_API.into()),
    );
    assert_eq!(
        futuresless_block_on(tokens.installation_id(&app, "fake.jwt")).unwrap(),
        19
    );
    assert_eq!(seen.lock().unwrap()[0].method, "GET");
    assert_eq!(
        seen.lock().unwrap()[0].url,
        "https://api.github.com/app/installations"
    );

    let empty = Box::new(FakeTransport::default());
    let tokens = Tokens::new(
        None,
        PathBuf::from("/isolated"),
        empty,
        Some(DEFAULT_API.into()),
    );
    assert_eq!(
        futuresless_block_on(tokens.installation_id(&app, "fake.jwt"))
            .unwrap_err()
            .to_string(),
        "GitHub App fixture is not installed on any account; install it on the three repos"
    );
}

struct RecordingTransport {
    inner: FakeTransport,
    recorded: Arc<Mutex<Vec<GitHubRequest>>>,
}
impl GitHubTransport for RecordingTransport {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        self.recorded.lock().unwrap().push(request.clone());
        self.inner.send(request)
    }
}

fn response(body: Vec<u8>) -> GitHubResponse {
    GitHubResponse {
        status: 200,
        reason: "OK".into(),
        body,
    }
}

fn recorded_transport(
    fake: FakeTransport,
) -> (Box<dyn GitHubTransport>, Arc<Mutex<Vec<GitHubRequest>>>) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    (
        Box::new(RecordingTransport {
            inner: fake,
            recorded: Arc::clone(&recorded),
        }),
        recorded,
    )
}

#[test]
fn kind_for_role_matches_owner_worker_and_refusal_rules() {
    assert_eq!(kind_for_role("worker", None).unwrap(), Some("author"));
    assert_eq!(kind_for_role("reviewer", None).unwrap(), Some("reviewer"));
    assert_eq!(kind_for_role("owner", None).unwrap(), None);
    assert_eq!(
        kind_for_role("owner", Some("reviewer")).unwrap(),
        Some("reviewer")
    );
    assert_eq!(kind_for_role("desk", Some("author")).unwrap(), None);
    assert_eq!(
        kind_for_role("worker", Some("reviewer"))
            .unwrap_err()
            .to_string(),
        "role worker may only act as the author App, not reviewer"
    );
    assert_eq!(
        kind_for_role("owner", Some("unknown"))
            .unwrap_err()
            .to_string(),
        "unknown app kind 'unknown'"
    );
}

#[test]
fn tokens_get_mints_once_then_returns_fresh_cache_and_forget_persists() {
    let dir = tempdir().unwrap();
    let config_dir = dir.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let app_config = app_file("author", &config_dir);
    let key_path = config_dir.join("github-app-author.pem");
    let file_system = FixtureFs::default();
    file_system.data.lock().unwrap().insert(
        app_config,
        Ok(r#"{"id":"123","slug":"author-fixture"}"#.into()),
    );
    file_system.keys.lock().unwrap().push(key_path.clone());
    file_system
        .key_contents
        .lock()
        .unwrap()
        .insert(key_path, TEST_RSA_KEY.as_bytes().to_vec());

    let fake = FakeTransport::default();
    fake.enqueue(response(br#"[{"id":44}]"#.to_vec()));
    fake.enqueue(response(
        br#"{"token":"fake-install-token","expires_at":"1970-01-02T03:46:39Z","permissions":{"contents":"write"}}"#.to_vec(),
    ));
    let (transport, recorded) = recorded_transport(fake);
    let cache_path = dir.path().join("github-tokens.json");
    let tokens = Tokens::with_app_file_system(
        Some(cache_path.clone()),
        config_dir,
        transport,
        Some(DEFAULT_API.into()),
        Box::new(file_system),
    );

    let minted = futuresless_block_on(tokens.get("author", Some(1_000.0))).unwrap();
    assert_eq!(minted["token"], "fake-install-token");
    assert_eq!(minted["installation_id"], 44);
    assert_eq!(minted["slug"], "author-fixture");
    assert_eq!(minted["kind"], "author");
    assert_eq!(minted["cached"], false);
    assert_eq!(minted["expires_at"], 99_999);
    let cache_hit = futuresless_block_on(tokens.get("author", Some(1_001.0))).unwrap();
    assert_eq!(cache_hit["cached"], true);
    assert_eq!(recorded.lock().unwrap().len(), 2);
    assert_eq!(recorded.lock().unwrap()[0].method, "GET");
    assert_eq!(recorded.lock().unwrap()[1].method, "POST");

    let serialized: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(
        serialized["tokens"]["author"]["token"],
        "fake-install-token"
    );
    futuresless_block_on(tokens.forget(Some("author"))).unwrap();
    assert!(tokens.cached("author").is_none());
    assert!(fs::read_to_string(cache_path).unwrap().contains("tokens"));
}

#[test]
fn tokens_forget_without_kind_clears_all_cached_entries() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cache.json");
    fs::write(
        &path,
        r#"{"tokens":{"author":{"expires_at":9,"token":"a"},"reviewer":{"expires_at":10,"token":"r"}}}"#,
    )
    .unwrap();
    let tokens = Tokens::new(
        Some(path.clone()),
        dir.path().into(),
        Box::new(FakeTransport::default()),
        Some(DEFAULT_API.into()),
    );
    futuresless_block_on(tokens.forget(None)).unwrap();
    assert!(tokens.cached("author").is_none());
    assert!(tokens.cached("reviewer").is_none());
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["tokens"], serde_json::json!({}));
}

#[test]
fn open_pulls_paginates_and_maps_python_output_fields() {
    let fake = FakeTransport::default();
    let first_page = (1..=100)
        .map(|number| serde_json::json!({"number":number,"title":"bulk"}))
        .collect::<Vec<_>>();
    fake.enqueue(response(serde_json::to_vec(&first_page).unwrap()));
    fake.enqueue(response(
        br#"[{"number":"101","title":"","html_url":"https://p/101","draft":"false","head":{"sha":"head-sha","ref":"feature"},"base":{"ref":"main"},"user":{},"created_at":"c","updated_at":"u","mergeable":null,"mergeable_state":"dirty"}]"#.to_vec(),
    ));
    let (transport, recorded) = recorded_transport(fake);
    let pulls = futuresless_block_on(open_pulls(
        &*transport,
        DEFAULT_API,
        "owner/repo",
        "fake-token",
    ))
    .unwrap();
    assert_eq!(pulls.len(), 101);
    assert_eq!(pulls[100]["number"], 101);
    assert_eq!(pulls[100]["title"], "");
    assert_eq!(pulls[100]["draft"], true);
    assert_eq!(pulls[100]["head"], "head-sha");
    assert_eq!(pulls[100]["branch"], "feature");
    assert_eq!(pulls[100]["base"], "main");
    assert_eq!(pulls[100]["login"], "");
    assert_eq!(pulls[100]["mergeable"], Value::Null);
    let seen = recorded.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].url.ends_with("?state=open&per_page=100&page=1"));
    assert!(seen[1].url.ends_with("?state=open&per_page=100&page=2"));
    assert_eq!(seen[0].headers["Authorization"], "token fake-token");
}

#[test]
fn pull_detail_preserves_mergeability_and_post_status_limits_description() {
    let fake = FakeTransport::default();
    fake.enqueue(response(
        br#"{"additions":8,"deletions":2,"changed_files":3,"mergeable":false,"mergeable_state":"dirty","body":""}"#.to_vec(),
    ));
    fake.enqueue(response(br#"{"id":7}"#.to_vec()));
    let (transport, recorded) = recorded_transport(fake);
    let detail =
        futuresless_block_on(pull_detail(&*transport, DEFAULT_API, "o/r", 7, "token")).unwrap();
    assert_eq!(detail["additions"], 8);
    assert_eq!(detail["mergeable"], false);
    assert_eq!(detail["mergeable_state"], "dirty");
    assert_eq!(detail["body"], "");

    let description = "é".repeat(150);
    futuresless_block_on(post_status(
        &*transport,
        DEFAULT_API,
        "token",
        StatusUpdate {
            repo: "o/r",
            sha: "abc",
            state: "success",
            description: &description,
            target_url: Some("https://example.test/review"),
        },
    ))
    .unwrap();
    let seen = recorded.lock().unwrap();
    let payload: Value = serde_json::from_slice(seen[1].body.as_ref().unwrap()).unwrap();
    assert_eq!(seen[1].method, "POST");
    assert!(seen[1].url.ends_with("/repos/o/r/statuses/abc"));
    assert_eq!(seen[1].headers["Authorization"], "token token");
    assert_eq!(payload["context"], "dunkingsheep/review");
    assert_eq!(
        payload["description"].as_str().unwrap().chars().count(),
        140
    );
    assert_eq!(payload["target_url"], "https://example.test/review");
}

#[test]
fn merge_review_and_graphql_requests_keep_methods_payloads_and_errors() {
    let fake = FakeTransport::default();
    fake.enqueue(response(br#"{"merged":true}"#.to_vec()));
    fake.enqueue(response(br#"{"id":9}"#.to_vec()));
    fake.enqueue(response(
        br#"{"data":{"viewer":{"login":"fixture"}}}"#.to_vec(),
    ));
    fake.enqueue(response(
        br#"{"errors":[{"message":"bad query"},"second error"]}"#.to_vec(),
    ));
    let (transport, recorded) = recorded_transport(fake);
    futuresless_block_on(merge_pull(
        &*transport,
        DEFAULT_API,
        "o/r",
        5,
        "reviewer-token",
        Some("expected-sha"),
    ))
    .unwrap();
    futuresless_block_on(review_pull(
        &*transport,
        DEFAULT_API,
        "o/r",
        5,
        "APPROVE",
        "looks good",
        "reviewer-token",
    ))
    .unwrap();
    let variables = serde_json::json!({"id":"node"});
    let data = futuresless_block_on(graphql(
        &*transport,
        DEFAULT_API,
        "query Viewer { viewer { login } }",
        "author-token",
        Some(&variables),
    ))
    .unwrap();
    assert_eq!(data["viewer"]["login"], "fixture");
    let error = futuresless_block_on(graphql(
        &*transport,
        DEFAULT_API,
        "query Bad { nope }",
        "author-token",
        None,
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), "GitHub GraphQL: bad query; second error");

    let seen = recorded.lock().unwrap();
    assert_eq!(seen[0].method, "PUT");
    assert!(seen[0].url.ends_with("/repos/o/r/pulls/5/merge"));
    let merge_payload: Value = serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(
        merge_payload,
        serde_json::json!({"merge_method":"merge","sha":"expected-sha"})
    );
    assert_eq!(seen[1].method, "POST");
    let review_payload: Value = serde_json::from_slice(seen[1].body.as_ref().unwrap()).unwrap();
    assert_eq!(
        review_payload,
        serde_json::json!({"event":"APPROVE","body":"looks good"})
    );
    assert_eq!(seen[2].url, "https://api.github.com/graphql");
    let query_payload: Value = serde_json::from_slice(seen[2].body.as_ref().unwrap()).unwrap();
    assert_eq!(query_payload["variables"], variables);
    assert_eq!(seen[3].url, "https://api.github.com/graphql");
}

#[test]
fn git_ref_helpers_use_python_paths_and_only_404_409_mean_absent() {
    let fake = FakeTransport::default();
    fake.enqueue(response(br#"{"object":{"sha":"ref-sha"}}"#.to_vec()));
    fake.enqueue(response(br#"{"object":{"sha":"present-sha"}}"#.to_vec()));
    fake.enqueue(GitHubResponse {
        status: 404,
        reason: "Not Found".into(),
        body: br#"{"message":"Not Found"}"#.to_vec(),
    });
    fake.enqueue(GitHubResponse {
        status: 409,
        reason: "Conflict".into(),
        body: br#"{"message":"Conflict"}"#.to_vec(),
    });
    fake.enqueue(GitHubResponse {
        status: 403,
        reason: "Forbidden".into(),
        body: br#"{"message":"denied"}"#.to_vec(),
    });
    fake.enqueue(response(br#"{"tree":{"sha":"tree-sha"}}"#.to_vec()));
    let (transport, recorded) = recorded_transport(fake);

    assert_eq!(
        futuresless_block_on(get_ref(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "feature/topic",
            "fixture-token",
        ))
        .unwrap(),
        "ref-sha"
    );
    assert!(futuresless_block_on(ref_exists(
        &*transport,
        DEFAULT_API,
        "owner/repo",
        "present",
        "fixture-token",
    ))
    .unwrap());
    assert!(!futuresless_block_on(ref_exists(
        &*transport,
        DEFAULT_API,
        "owner/repo",
        "missing",
        "fixture-token",
    ))
    .unwrap());
    assert!(!futuresless_block_on(ref_exists(
        &*transport,
        DEFAULT_API,
        "owner/repo",
        "conflicting",
        "fixture-token",
    ))
    .unwrap());
    assert_eq!(
        futuresless_block_on(ref_exists(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "forbidden",
            "fixture-token",
        ))
        .unwrap_err()
        .to_string(),
        "GitHub GET /repos/owner/repo/git/ref/heads/forbidden -> 403: denied"
    );
    assert_eq!(
        futuresless_block_on(commit_tree(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "commit-sha",
            "fixture-token",
        ))
        .unwrap(),
        "tree-sha"
    );

    let seen = recorded.lock().unwrap();
    assert_eq!(seen.len(), 6);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].url,
        "https://api.github.com/repos/owner/repo/git/ref/heads/feature/topic"
    );
    assert_eq!(
        seen[5].url,
        "https://api.github.com/repos/owner/repo/git/commits/commit-sha"
    );
    assert_eq!(seen[0].headers["Authorization"], "token fixture-token");
}

#[test]
fn file_text_decodes_python_base64_and_handles_missing_directories_and_encoding() {
    let fake = FakeTransport::default();
    fake.enqueue(response(
        br#"{"encoding":"base64","content":"SGVsbG8g4pyT\n","sha":"blob-sha"}"#.to_vec(),
    ));
    fake.enqueue(GitHubResponse {
        status: 404,
        reason: "Not Found".into(),
        body: br#"{"message":"Not Found"}"#.to_vec(),
    });
    fake.enqueue(response(br#"[{"name":"child"}]"#.to_vec()));
    fake.enqueue(response(br#"{"encoding":"utf-8"}"#.to_vec()));
    let (transport, recorded) = recorded_transport(fake);

    assert_eq!(
        futuresless_block_on(file_text(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "docs/readme.md",
            "main",
            "fixture-token",
        ))
        .unwrap(),
        (
            Some("Hello ✓".into()),
            Some(Value::String("blob-sha".into()))
        )
    );
    assert_eq!(
        futuresless_block_on(file_text(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "missing.md",
            "main",
            "fixture-token",
        ))
        .unwrap(),
        (None, None)
    );
    assert_eq!(
        futuresless_block_on(file_text(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "docs",
            "main",
            "fixture-token",
        ))
        .unwrap_err()
        .to_string(),
        "owner/repo/docs is a directory, not a file"
    );
    assert_eq!(
        futuresless_block_on(file_text(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "binary.dat",
            "main",
            "fixture-token",
        ))
        .unwrap_err()
        .to_string(),
        "owner/repo/binary.dat: unexpected encoding 'utf-8'"
    );

    let seen = recorded.lock().unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(
        seen[0].url,
        "https://api.github.com/repos/owner/repo/contents/docs/readme.md?ref=main"
    );
    assert_eq!(seen[0].headers["Authorization"], "token fixture-token");
}

#[test]
fn pr_files_paginates_and_projects_python_file_fields() {
    let fake = FakeTransport::default();
    let first_page = (0..100)
        .map(|number| serde_json::json!({"filename":format!("file-{number}"),"status":"modified"}))
        .collect::<Vec<_>>();
    fake.enqueue(response(serde_json::to_vec(&first_page).unwrap()));
    fake.enqueue(response(
        br#"[{"filename":"renamed.md","status":"renamed","previous_filename":"old.md"},{"filename":null,"status":null}]"#.to_vec(),
    ));
    let (transport, recorded) = recorded_transport(fake);

    let files = futuresless_block_on(pr_files(
        &*transport,
        DEFAULT_API,
        "owner/repo",
        42,
        "fixture-token",
    ))
    .unwrap();
    assert_eq!(files.len(), 102);
    assert_eq!(files[0]["filename"], "file-0");
    assert_eq!(files[100]["previous_filename"], "old.md");
    assert_eq!(files[101]["filename"], "");
    assert_eq!(files[101]["status"], "");
    assert_eq!(files[101]["previous_filename"], Value::Null);

    let seen = recorded.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].url,
        "https://api.github.com/repos/owner/repo/pulls/42/files?per_page=100&page=1"
    );
    assert_eq!(
        seen[1].url,
        "https://api.github.com/repos/owner/repo/pulls/42/files?per_page=100&page=2"
    );
}

#[test]
fn git_write_helpers_send_exact_payloads_and_project_open_pull() {
    let fake = FakeTransport::default();
    fake.enqueue(response(br#"{"sha":"tree-sha"}"#.to_vec()));
    fake.enqueue(response(br#"{"sha":"commit-sha"}"#.to_vec()));
    fake.enqueue(response(br#"{"object":{"sha":"branch-sha"}}"#.to_vec()));
    fake.enqueue(response(
        br#"{"number":"17","html_url":"https://github.test/pull/17","node_id":"PR_node","head":{"sha":"head-sha"}}"#.to_vec(),
    ));
    fake.enqueue(GitHubResponse {
        status: 422,
        reason: "Unprocessable Entity".into(),
        body: br#"{"message":"Reference already exists"}"#.to_vec(),
    });
    let (transport, recorded) = recorded_transport(fake);
    let entries = vec![serde_json::json!({
        "path":"docs/credit.csv",
        "mode":"100644",
        "type":"blob",
        "content":"row\n",
    })];
    let parents = vec!["parent-sha".to_owned()];

    assert_eq!(
        futuresless_block_on(create_tree(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "base-tree",
            &entries,
            "reviewer-token",
        ))
        .unwrap(),
        "tree-sha"
    );
    assert_eq!(
        futuresless_block_on(create_commit(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "Add accounting row",
            "tree-sha",
            &parents,
            "reviewer-token",
        ))
        .unwrap(),
        "commit-sha"
    );
    assert_eq!(
        futuresless_block_on(create_ref(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "accounting/pr42",
            "commit-sha",
            "reviewer-token",
        ))
        .unwrap(),
        "branch-sha"
    );
    assert_eq!(
        futuresless_block_on(open_pull(
            &*transport,
            DEFAULT_API,
            OpenPullRequest {
                repo: "owner/repo",
                title: "Account for accepted row",
                head: "accounting/pr42",
                base: "main",
                body: "Generated from the merged evidence.",
            },
            "reviewer-token",
        ))
        .unwrap(),
        serde_json::json!({
            "number":17,
            "url":"https://github.test/pull/17",
            "node_id":"PR_node",
            "head":"head-sha",
        })
    );
    assert_eq!(
        futuresless_block_on(create_ref(
            &*transport,
            DEFAULT_API,
            "owner/repo",
            "accounting/pr42",
            "new-sha",
            "reviewer-token",
        ))
        .unwrap_err()
        .to_string(),
        "GitHub POST /repos/owner/repo/git/refs -> 422: Reference already exists"
    );

    let seen = recorded.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(
        seen[0].url,
        "https://api.github.com/repos/owner/repo/git/trees"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(seen[0].body.as_ref().unwrap()).unwrap(),
        serde_json::json!({"base_tree":"base-tree","tree":entries})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(seen[1].body.as_ref().unwrap()).unwrap(),
        serde_json::json!({"message":"Add accounting row","tree":"tree-sha","parents":["parent-sha"]})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(seen[2].body.as_ref().unwrap()).unwrap(),
        serde_json::json!({"ref":"refs/heads/accounting/pr42","sha":"commit-sha"})
    );
    assert_eq!(seen[3].url, "https://api.github.com/repos/owner/repo/pulls");
    assert_eq!(
        serde_json::from_slice::<Value>(seen[3].body.as_ref().unwrap()).unwrap(),
        serde_json::json!({
            "title":"Account for accepted row",
            "head":"accounting/pr42",
            "base":"main",
            "body":"Generated from the merged evidence.",
        })
    );
    assert_eq!(seen[0].headers["Authorization"], "token reviewer-token");
    assert!(seen.iter().all(|request| request.timeout_s == 20));
}
