use super::*;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[derive(Default)]
struct FixtureFs {
    data: Mutex<BTreeMap<PathBuf, Result<String, std::io::ErrorKind>>>,
    keys: Mutex<Vec<PathBuf>>,
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
    let public_key = jsonwebtoken::DecodingKey::from_rsa_pem(TEST_RSA_KEY.as_bytes()).unwrap();
    let claims = jsonwebtoken::decode::<Value>(&jwt, &public_key, &validation)
        .unwrap()
        .claims;
    assert_eq!(claims["iat"], 9_940);
    assert_eq!(claims["exp"], 10_540);
    assert_eq!(claims["iss"], 123);
}

struct FakeTransport {
    next: Mutex<Result<GitHubResponse, String>>,
    seen: Mutex<Vec<GitHubRequest>>,
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
        }
    }
}
impl GitHubTransport for FakeTransport {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        self.seen.lock().unwrap().push(request);
        let result = self.next.lock().unwrap().clone();
        Box::pin(async move { result })
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
        "POST",
        "https://api.github.com/repos/o/r",
        Some("tok"),
        Some("jwt"),
        Some(&payload),
        7,
        DEFAULT_API,
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
        "GET",
        "https://api.github.com/app",
        None,
        None,
        None,
        20,
        DEFAULT_API,
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), "GitHub GET /app -> 403: denied");
    *fake.next.lock().unwrap() = Err("timed out".into());
    let error = futuresless_block_on(request(
        &fake,
        "GET",
        "url",
        None,
        None,
        None,
        20,
        DEFAULT_API,
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), "GitHub unreachable: timed out");
}

fn futuresless_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
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
        serde_json::json!({"expires_at":123,"token":"fake"}),
    );
    tokens.save().unwrap();
    assert!(!dir.path().join("cache.json.tmp").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["tokens"]["fixture"]["token"], "fake");
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
