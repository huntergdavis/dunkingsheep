//! GitHub App configuration, RS256 authentication, REST requests and token cache.
//!
//! This module ports the small authentication layer in `dunk_github.py`. Paths,
//! time and HTTP transport are injectable so its credential behavior can be
//! exercised without touching a user's config or making a live request.

mod mints;
mod required_ci;
pub use mints::{
    author_from_mints, author_mint_path, record_author_mint, record_author_mint_outcome,
    repos_config, AuthorMintRecord,
};
pub(crate) use mints::{python_number_string, DEFAULT_REPOS};
pub use required_ci::require_merge_ci;

/// Parse a Python-compatible float string using the existing mint decoder.
pub(crate) fn parse_python_float(raw: &str) -> Option<f64> {
    mints::parse_float(raw).ok()
}

/// Parse a raw JSON number with Python's int-versus-float overflow distinction.
pub(crate) fn parse_python_json_float(raw: &str) -> Result<f64, &'static str> {
    mints::parse_json_number_float(raw)
}

/// Keep nonfinite task floats numeric in arbitrary-precision JSON Values.
pub(crate) fn python_float_json_value(value: f64) -> Option<Value> {
    mints::float_json_value(value)
}

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::Value;

use crate::error::{refusal, DunkError};

pub const KINDS: [&str; 2] = ["author", "reviewer"];
pub const DEFAULT_API: &str = "https://api.github.com";
pub const REFRESH_MARGIN_S: f64 = 600.0;
pub const REVIEW_CONTEXT: &str = "dunkingsheep/review";
pub const DEFAULT_REQUIRED_VERDICTS: usize = 1;
pub const POOLRAD_REQUIRED_VERDICTS: usize = 2;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TokenCacheTempFile {
    path: PathBuf,
    file: Option<File>,
    cleanup: bool,
}

impl TokenCacheTempFile {
    fn new(path: PathBuf, file: File) -> Self {
        Self {
            path,
            file: Some(file),
            cleanup: true,
        }
    }

    fn file(&self) -> &File {
        self.file.as_ref().expect("temporary file is still open")
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is still open")
    }

    fn close(&mut self) {
        drop(self.file.take());
    }

    fn disarm(&mut self) {
        self.cleanup = false;
    }
}

impl Drop for TokenCacheTempFile {
    fn drop(&mut self) {
        self.close();
        if self.cleanup {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubError(pub String);

impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitHubError {}

impl From<GitHubError> for DunkError {
    fn from(value: GitHubError) -> Self {
        refusal(value.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub id: i64,
    pub slug: String,
    pub pem: PathBuf,
}

/// The narrow file seam lets tests model read failures even when running as root.
pub trait AppFileSystem: Send + Sync {
    fn read_config(&self, path: &Path) -> std::io::Result<String>;
    fn key_exists(&self, path: &Path) -> bool;
    fn read_key(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        fs::read(path)
    }
}

#[derive(Debug, Default)]
pub struct OsAppFileSystem;

impl AppFileSystem for OsAppFileSystem {
    fn read_config(&self, path: &Path) -> std::io::Result<String> {
        fs::read_to_string(path)
    }

    fn key_exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

pub fn app_file(kind: &str, config_dir: &Path) -> PathBuf {
    config_dir.join(format!("github-app-{kind}.json"))
}

pub fn load_app(
    kind: &str,
    config_dir: &Path,
    fs: &dyn AppFileSystem,
) -> Result<AppConfig, GitHubError> {
    if !KINDS.contains(&kind) {
        return Err(GitHubError(format!(
            "app kind must be one of author, reviewer; got {}",
            python_repr(kind)
        )));
    }
    let path = app_file(kind, config_dir);
    let raw = fs.read_config(&path).map_err(|_| {
        GitHubError(format!(
            "no GitHub App registered as {}: {} is missing (create it with the manifest page, docs/FLAT_FLEET.md 6.1)",
            python_repr(kind),
            path.display()
        ))
    })?;
    let data: Value = serde_json::from_str(&raw)
        .map_err(|e| GitHubError(format!("invalid app config JSON: {e}")))?;
    let object = data
        .as_object()
        .ok_or_else(|| GitHubError("'{}' object has no attribute 'get'".into()))?;
    let id_value = object.get("id").ok_or_else(|| GitHubError("'id'".into()))?;
    let id = python_int(id_value)?;
    let default_slug = format!("dunkingsheep-{kind}");
    let slug = object
        .get("slug")
        .filter(|v| !is_falsey(v))
        .and_then(Value::as_str)
        .unwrap_or(&default_slug)
        .to_owned();
    let default_pem = config_dir.join(format!("github-app-{kind}.pem"));
    let pem = object
        .get("pem")
        .filter(|v| !is_falsey(v))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or(default_pem);
    if !fs.key_exists(&pem) {
        return Err(GitHubError(format!(
            "GitHub App {} private key missing: {}",
            python_repr(kind),
            pem.display()
        )));
    }
    Ok(AppConfig { id, slug, pem })
}

fn python_repr(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn is_falsey(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64().unwrap_or(1.0) == 0.0,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn python_int(value: &Value) -> Result<i64, GitHubError> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| GitHubError("int() argument must be a string, a bytes-like object or a real number, not 'float'".into())),
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| {
            GitHubError(format!("invalid literal for int() with base 10: {s:?}"))
        }),
        Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
        Value::Null => Err(GitHubError("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into())),
        _ => Err(GitHubError("int() argument must be a string, a bytes-like object or a real number".into())),
    }
}

#[derive(Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: i64,
}

pub fn app_jwt(app: &AppConfig, now: Option<i64>) -> Result<String, GitHubError> {
    app_jwt_with_fs(app, now, &OsAppFileSystem)
}

fn app_jwt_with_fs(
    app: &AppConfig,
    now: Option<i64>,
    file_system: &dyn AppFileSystem,
) -> Result<String, GitHubError> {
    let now = now.filter(|v| *v != 0).unwrap_or_else(unix_time);
    let pem = file_system
        .read_key(&app.pem)
        .map_err(|e| GitHubError(e.to_string()))?;
    let key = EncodingKey::from_rsa_pem(&pem).map_err(|e| GitHubError(e.to_string()))?;
    encode(
        &Header::new(Algorithm::RS256),
        &AppClaims {
            iat: now - 60,
            exp: now + 540,
            iss: app.id,
        },
        &key,
    )
    .map_err(|e| GitHubError(e.to_string()))
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitHubRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
    pub timeout_s: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitHubResponse {
    pub status: u16,
    pub reason: String,
    pub body: Vec<u8>,
}

pub trait GitHubTransport: Send + Sync {
    /// Optional response ETag for conditional, read-only consumers. Existing transports
    /// need not implement caching; callers must handle a missing ETag.
    fn send_with_etag<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<(GitHubResponse, Option<String>), String>> {
        Box::pin(async move { self.send(request).await.map(|response| (response, None)) })
    }
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>>;
}

pub type PinBoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

pub struct RequestOptions<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub token: Option<&'a str>,
    pub bearer: Option<&'a str>,
    pub payload: Option<&'a Value>,
    pub timeout_s: u64,
    pub api_root: &'a str,
}

#[derive(Clone, Default)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl GitHubTransport for ReqwestTransport {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        Box::pin(async move {
            self.send_with_etag(request)
                .await
                .map(|(response, _)| response)
        })
    }
    fn send_with_etag<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<(GitHubResponse, Option<String>), String>> {
        Box::pin(async move {
            let mut builder = self
                .client
                .request(
                    request.method.parse().map_err(|e| format!("{e}"))?,
                    &request.url,
                )
                .timeout(std::time::Duration::from_secs(request.timeout_s));
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(|e| e.to_string())?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            let status = response.status();
            let reason = status.canonical_reason().unwrap_or("unknown").to_owned();
            let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
            Ok((
                GitHubResponse {
                    status: status.as_u16(),
                    reason,
                    body,
                },
                etag,
            ))
        })
    }
}

pub async fn request(
    transport: &dyn GitHubTransport,
    options: RequestOptions<'_>,
) -> Result<(u16, Option<Value>), GitHubError> {
    let RequestOptions {
        method,
        url,
        token,
        bearer,
        payload,
        timeout_s,
        api_root,
    } = options;
    let mut headers = BTreeMap::from([
        ("Accept".into(), "application/vnd.github+json".into()),
        ("User-Agent".into(), "dunkingsheep".into()),
        ("X-GitHub-Api-Version".into(), "2022-11-28".into()),
    ]);
    if let Some(bearer) = bearer.filter(|s| !s.is_empty()) {
        headers.insert("Authorization".into(), format!("Bearer {bearer}"));
    } else if let Some(token) = token.filter(|s| !s.is_empty()) {
        headers.insert("Authorization".into(), format!("token {token}"));
    }
    let body = payload.map(|p| serde_json::to_vec(p).unwrap_or_default());
    if body.is_some() {
        headers.insert("Content-Type".into(), "application/json".into());
    }
    let response = transport
        .send(GitHubRequest {
            method: method.to_owned(),
            url: url.to_owned(),
            headers,
            body,
            timeout_s,
        })
        .await
        .map_err(|e| GitHubError(format!("GitHub unreachable: {e}")))?;
    if !(200..300).contains(&response.status) {
        let detail: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        let message = detail
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&response.reason);
        return Err(GitHubError(format!(
            "GitHub {method} {} -> {}: {message}",
            url.replace(api_root, ""),
            response.status
        )));
    }
    if response.body.is_empty() {
        return Ok((response.status, None));
    }
    let parsed = serde_json::from_slice(&response.body)
        .map_err(|e| GitHubError(format!("invalid JSON response: {e}")))?;
    Ok((response.status, Some(parsed)))
}

pub struct Tokens {
    path: PathBuf,
    config_dir: PathBuf,
    cache: Mutex<BTreeMap<String, Value>>,
    operation_lock: tokio::sync::Mutex<()>,
    transport: Box<dyn GitHubTransport>,
    app_file_system: Box<dyn AppFileSystem>,
    api_root: String,
}

impl Tokens {
    pub fn new(
        path: Option<PathBuf>,
        config_dir: PathBuf,
        transport: Box<dyn GitHubTransport>,
        api_root: Option<String>,
    ) -> Self {
        Self::with_app_file_system(
            path,
            config_dir,
            transport,
            api_root,
            Box::new(OsAppFileSystem),
        )
    }

    pub fn with_app_file_system(
        path: Option<PathBuf>,
        config_dir: PathBuf,
        transport: Box<dyn GitHubTransport>,
        api_root: Option<String>,
        app_file_system: Box<dyn AppFileSystem>,
    ) -> Self {
        let path = path.unwrap_or_else(|| {
            std::env::var_os("DUNKINGSHEEP_GITHUB_TOKENS_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| config_dir.join("github-tokens.json"))
        });
        let mut this = Self {
            path,
            config_dir,
            cache: Mutex::new(BTreeMap::new()),
            operation_lock: tokio::sync::Mutex::new(()),
            transport,
            app_file_system,
            api_root: api_root.unwrap_or_else(|| {
                std::env::var("DUNKINGSHEEP_GITHUB_API").unwrap_or_else(|_| DEFAULT_API.into())
            }),
        };
        this.load();
        this
    }

    pub fn load(&mut self) {
        let loaded = fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let mut cache = BTreeMap::new();
        if let Some(tokens) = loaded.and_then(|v| v.get("tokens").cloned()) {
            if let Some(entries) = tokens.as_object() {
                for (kind, value) in entries {
                    if value.is_object() && value.get("expires_at").is_some_and(|v| !is_falsey(v)) {
                        cache.insert(kind.clone(), value.clone());
                    }
                }
            }
        }
        *self.cache.get_mut().unwrap_or_else(|e| e.into_inner()) = cache;
    }

    pub fn save(&self) -> Result<(), GitHubError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| GitHubError(e.to_string()))?;
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({"tokens": &*cache}))
            .map_err(|e| GitHubError(e.to_string()))?;
        #[cfg(unix)]
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let filename = self
            .path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("github-tokens.json"));
        let (temporary, file) = (0..128)
            .find_map(|_| {
                let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let candidate = parent.join(format!(
                    ".{}.tmp.{}.{}",
                    filename.to_string_lossy(),
                    std::process::id(),
                    sequence
                ));
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                match options.open(&candidate) {
                    Ok(file) => Some(Ok((candidate, file))),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(GitHubError(error.to_string()))),
                }
            })
            .unwrap_or_else(|| {
                Err(GitHubError(
                    "could not create a unique GitHub token cache file".into(),
                ))
            })?;
        let mut temporary = TokenCacheTempFile::new(temporary, file);
        #[cfg(unix)]
        temporary
            .file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| GitHubError(e.to_string()))?;
        temporary
            .file_mut()
            .write_all(&bytes)
            .map_err(|e| GitHubError(e.to_string()))?;
        temporary
            .file_mut()
            .flush()
            .map_err(|e| GitHubError(e.to_string()))?;
        temporary.close();
        fs::rename(&temporary.path, &self.path).map_err(|e| GitHubError(e.to_string()))?;
        temporary.disarm();
        Ok(())
    }

    pub async fn installation_id(&self, app: &AppConfig, bearer: &str) -> Result<i64, GitHubError> {
        let url = format!("{}/app/installations", self.api_root);
        let (_, result) = request(
            &*self.transport,
            RequestOptions {
                method: "GET",
                url: &url,
                token: None,
                bearer: Some(bearer),
                payload: None,
                timeout_s: 20,
                api_root: &self.api_root,
            },
        )
        .await?;
        let installs = result
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let first = installs.first().ok_or_else(|| {
            GitHubError(format!(
                "GitHub App {} is not installed on any account; install it on the three repos",
                app.slug
            ))
        })?;
        let id = first.get("id").ok_or_else(|| GitHubError("'id'".into()))?;
        python_int(id)
    }

    /// Return a cached installation token or mint one when it is within the
    /// source refresh window. The operation mutex covers cache check, mint,
    /// update and save just as Python's `threading.Lock` does.
    pub async fn get(&self, kind: &str, now: Option<f64>) -> Result<Value, GitHubError> {
        let now = now.filter(|v| *v != 0.0).unwrap_or_else(unix_time_f64);
        let _operation = self.operation_lock.lock().await;
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(kind)
            .cloned();
        if let Some(entry) = cached.as_ref().filter(|v| !is_falsey(v)) {
            let expires_at = python_float(entry.get("expires_at"), "expires_at")?;
            if expires_at - REFRESH_MARGIN_S > now {
                return Ok(with_token_metadata(entry, kind, true));
            }
        }

        let app = load_app(kind, &self.config_dir, &*self.app_file_system)?;
        let bearer = app_jwt_with_fs(&app, Some(now as i64), &*self.app_file_system)?;
        let installation = cached
            .as_ref()
            .filter(|v| !is_falsey(v))
            .and_then(|v| v.get("installation_id"))
            .filter(|v| !is_falsey(v))
            .cloned();
        let installation = match installation {
            Some(id) => id,
            None => Value::from(self.installation_id(&app, &bearer).await?),
        };
        let installation_text = python_string(&installation);
        let url = format!(
            "{}/app/installations/{installation_text}/access_tokens",
            self.api_root
        );
        let (_, data) = request(
            &*self.transport,
            RequestOptions {
                method: "POST",
                url: &url,
                token: None,
                bearer: Some(&bearer),
                payload: None,
                timeout_s: 20,
                api_root: &self.api_root,
            },
        )
        .await?;
        let data = data.ok_or_else(|| GitHubError("GitHub token response was empty".into()))?;
        let expires = data.get("expires_at");
        let expires_at = parse_token_expiry(expires, now);
        let token = data
            .get("token")
            .cloned()
            .ok_or_else(|| GitHubError("'token'".into()))?;
        let permissions = data
            .get("permissions")
            .filter(|v| !is_falsey(v))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let entry = serde_json::json!({
            "token": token,
            "expires_at": expires_at,
            "installation_id": installation,
            "slug": app.slug,
            "minted_at": now,
            "permissions": permissions,
        });
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(kind.to_owned(), entry.clone());
        self.save()?;
        tracing::info!(
            "minted {} installation token (expires {})",
            kind,
            python_string_opt(expires)
        );
        Ok(with_token_metadata(&entry, kind, false))
    }

    pub async fn forget(&self, kind: Option<&str>) -> Result<(), GitHubError> {
        let _operation = self.operation_lock.lock().await;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(kind) = kind.filter(|value| !value.is_empty()) {
            cache.remove(kind);
        } else {
            cache.clear();
        }
        drop(cache);
        self.save()
    }

    pub fn cached(&self, kind: &str) -> Option<Value> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(kind)
            .cloned()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
    pub fn api_root(&self) -> &str {
        &self.api_root
    }
}

fn unix_time_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn python_float(value: Option<&Value>, name: &str) -> Result<f64, GitHubError> {
    let Some(value) = value else {
        return Err(GitHubError(format!("KeyError: {name:?}")));
    };
    match value {
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| GitHubError("float value out of range".into())),
        Value::String(text) => text
            .parse::<f64>()
            .map_err(|_| GitHubError(format!("could not convert string to float: {text:?}"))),
        Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::Null => Err(GitHubError(
            "float() argument must be a string or a real number, not 'NoneType'".into(),
        )),
        _ => Err(GitHubError(
            "float() argument must be a string or a real number".into(),
        )),
    }
}

fn parse_token_expiry(value: Option<&Value>, now: f64) -> Value {
    let parsed = value
        .and_then(Value::as_str)
        .and_then(|text| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ").ok())
        .map(|date| date.and_utc().timestamp());
    parsed.map(Value::from).unwrap_or_else(|| {
        serde_json::Number::from_f64(now + 3600.0)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    })
}

fn with_token_metadata(entry: &Value, kind: &str, cached: bool) -> Value {
    let mut result = entry.as_object().cloned().unwrap_or_default();
    result.insert("kind".into(), Value::String(kind.to_owned()));
    result.insert("cached".into(), Value::Bool(cached));
    Value::Object(result)
}

fn python_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        _ => value.to_string(),
    }
}

fn python_string_opt(value: Option<&Value>) -> String {
    value.map(python_string).unwrap_or_else(|| "None".into())
}

fn python_truthy(value: &Value) -> bool {
    !is_falsey(value)
}

const ROLE_KINDS: &[(&str, Option<&str>)] = &[
    ("worker", Some("author")),
    ("planner", None),
    ("desk", None),
    ("reviewer", Some("reviewer")),
    ("system", Some("reviewer")),
    ("web", None),
    ("owner", None),
    ("anonymous", None),
];

/// The GitHub App identity a caller may request for its role.
pub fn kind_for_role(
    role: &str,
    requested: Option<&str>,
) -> Result<Option<&'static str>, GitHubError> {
    let requested = requested.filter(|value| !value.is_empty());
    if matches!(role, "owner" | "system") {
        if let Some(requested) = requested {
            if !KINDS.contains(&requested) {
                return Err(GitHubError(format!(
                    "unknown app kind {}",
                    python_repr(requested)
                )));
            }
            return Ok(Some(if requested == "author" {
                "author"
            } else {
                "reviewer"
            }));
        }
        return Ok(ROLE_KINDS
            .iter()
            .find(|(known, _)| *known == role)
            .and_then(|(_, kind)| *kind));
    }
    let implied = ROLE_KINDS
        .iter()
        .find(|(known, _)| *known == role)
        .and_then(|(_, kind)| *kind);
    let Some(implied) = implied else {
        return Ok(None);
    };
    if let Some(requested) = requested {
        if requested != implied {
            return Err(GitHubError(format!(
                "role {role} may only act as the {implied} App, not {requested}"
            )));
        }
    }
    Ok(Some(if implied == "author" {
        "author"
    } else {
        "reviewer"
    }))
}

/// All open pull requests for a repository, preserving GitHub's page order.
pub async fn open_pulls(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    token: &str,
) -> Result<Vec<Value>, GitHubError> {
    let mut pulls = Vec::new();
    for page in 1..=10 {
        let url = format!("{api_root}/repos/{repo}/pulls?state=open&per_page=100&page={page}");
        let (_, batch) = request(
            transport,
            RequestOptions {
                method: "GET",
                url: &url,
                token: Some(token),
                bearer: None,
                payload: None,
                timeout_s: 20,
                api_root,
            },
        )
        .await?;
        let batch = batch
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let batch_len = batch.len();
        pulls.extend(batch);
        if batch_len < 100 {
            break;
        }
    }

    pulls
        .into_iter()
        .map(|pull| {
            let number = python_int(
                pull.get("number")
                    .ok_or_else(|| GitHubError("'number'".into()))?,
            )?;
            let head = pull.get("head").filter(|v| python_truthy(v));
            let base = pull.get("base").filter(|v| python_truthy(v));
            let user = pull.get("user").filter(|v| python_truthy(v));
            let login = user
                .and_then(|v| v.get("login"))
                .filter(|v| python_truthy(v))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            let title = pull
                .get("title")
                .filter(|v| python_truthy(v))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            Ok(serde_json::json!({
                "repo": repo,
                "number": number,
                "title": title,
                "url": pull.get("html_url").cloned().unwrap_or(Value::Null),
                "draft": pull.get("draft").is_some_and(python_truthy),
                "head": head.and_then(|v| v.get("sha")).cloned().unwrap_or(Value::Null),
                "branch": head.and_then(|v| v.get("ref")).cloned().unwrap_or(Value::Null),
                "base": base.and_then(|v| v.get("ref")).cloned().unwrap_or(Value::Null),
                "login": login,
                "created_at": pull.get("created_at").cloned().unwrap_or(Value::Null),
                "updated_at": pull.get("updated_at").cloned().unwrap_or(Value::Null),
                "mergeable": pull.get("mergeable").cloned().unwrap_or(Value::Null),
                "mergeable_state": pull.get("mergeable_state").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

/// Resolve a branch or ref name to the commit SHA GitHub currently reports.
pub async fn get_ref(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    reference: &str,
    token: &str,
) -> Result<String, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/git/ref/heads/{reference}");
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    data.as_ref()
        .and_then(|value| value.get("object"))
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| GitHubError("'sha'".into()))
}

/// Check for a branch, treating only GitHub's not-found/conflict responses as absent.
pub async fn ref_exists(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    branch: &str,
    token: &str,
) -> Result<bool, GitHubError> {
    match get_ref(transport, api_root, repo, branch, token).await {
        Ok(_) => Ok(true),
        Err(error)
            if error.to_string().contains("-> 404") || error.to_string().contains("-> 409") =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Resolve a commit SHA to its tree SHA.
pub async fn commit_tree(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    sha: &str,
    token: &str,
) -> Result<String, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/git/commits/{sha}");
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    data.as_ref()
        .and_then(|value| value.get("tree"))
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| GitHubError("'sha'".into()))
}

/// Return decoded UTF-8 file contents and its blob SHA, or `(None, None)` for a 404.
pub async fn file_text(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    path: &str,
    reference: &str,
    token: &str,
) -> Result<(Option<String>, Option<Value>), GitHubError> {
    let url = format!("{api_root}/repos/{repo}/contents/{path}?ref={reference}");
    let (_, data) = match request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await
    {
        Ok(result) => result,
        Err(error) if error.to_string().contains("-> 404") => return Ok((None, None)),
        Err(error) => return Err(error),
    };
    let data = data.unwrap_or(Value::Null);
    if data.is_null() {
        return Err(GitHubError(
            "'NoneType' object has no attribute 'get'".into(),
        ));
    }
    if data.is_array() {
        return Err(GitHubError(format!(
            "{repo}/{path} is a directory, not a file"
        )));
    }
    let encoding_value = data.get("encoding");
    let encoding = encoding_value.and_then(Value::as_str);
    if encoding != Some("base64") {
        let encoding_repr = match encoding_value {
            None | Some(Value::Null) => "None".to_owned(),
            Some(Value::String(value)) => python_repr(value),
            Some(value) => python_string(value),
        };
        return Err(GitHubError(format!(
            "{repo}/{path}: unexpected encoding {}",
            encoding_repr
        )));
    }
    let content_value = data.get("content").filter(|value| python_truthy(value));
    let content = match content_value {
        None => "",
        Some(Value::String(content)) => content,
        Some(_) => {
            return Err(GitHubError(
                "argument should be a bytes-like object or ASCII string".into(),
            ));
        }
    };
    let bytes = decode_github_base64(content)?;
    let text = String::from_utf8(bytes).map_err(|error| GitHubError(error.to_string()))?;
    let sha = data.get("sha").cloned();
    Ok((Some(text), sha))
}

fn decode_github_base64(content: &str) -> Result<Vec<u8>, GitHubError> {
    fn sextet(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    // Python's base64.b64decode defaults to discarding non-alphabet characters.
    let encoded: Vec<u8> = content
        .bytes()
        .filter(|byte| sextet(*byte).is_some() || *byte == b'=')
        .collect();
    let (groups, remainder) = encoded.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(GitHubError("Incorrect padding".into()));
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    let group_count = groups.len();
    for (index, group) in groups.iter().enumerate() {
        let first = sextet(group[0]).ok_or_else(|| GitHubError("Invalid base64 data".into()))?;
        let second = sextet(group[1]).ok_or_else(|| GitHubError("Invalid base64 data".into()))?;
        decoded.push((first << 2) | (second >> 4));
        if group[2] == b'=' {
            if group[3] != b'=' || index + 1 != group_count {
                return Err(GitHubError("Invalid base64 data".into()));
            }
            continue;
        }
        let third = sextet(group[2]).ok_or_else(|| GitHubError("Invalid base64 data".into()))?;
        decoded.push((second << 4) | (third >> 2));
        if group[3] == b'=' {
            if index + 1 != group_count {
                return Err(GitHubError("Invalid base64 data".into()));
            }
            continue;
        }
        let fourth = sextet(group[3]).ok_or_else(|| GitHubError("Invalid base64 data".into()))?;
        decoded.push((third << 6) | fourth);
    }
    Ok(decoded)
}

/// List every file changed by a pull request, preserving GitHub pagination order.
pub async fn pr_files(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    token: &str,
) -> Result<Vec<Value>, GitHubError> {
    let mut files = Vec::new();
    for page in 1..=10 {
        let url = format!("{api_root}/repos/{repo}/pulls/{number}/files?per_page=100&page={page}");
        let (_, batch) = request(
            transport,
            RequestOptions {
                method: "GET",
                url: &url,
                token: Some(token),
                bearer: None,
                payload: None,
                timeout_s: 20,
                api_root,
            },
        )
        .await?;
        let batch = batch
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let batch_len = batch.len();
        files.extend(batch.into_iter().map(|item| {
            let filename = item
                .get("filename")
                .filter(|value| python_truthy(value))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            let status = item
                .get("status")
                .filter(|value| python_truthy(value))
                .cloned()
                .unwrap_or_else(|| Value::String(String::new()));
            serde_json::json!({
                "filename": filename,
                "status": status,
                "previous_filename": item.get("previous_filename").cloned().unwrap_or(Value::Null),
            })
        }));
        if batch_len < 100 {
            break;
        }
    }
    Ok(files)
}

/// Create a Git tree on top of an existing tree SHA.
pub async fn create_tree(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    base_tree: &str,
    entries: &[Value],
    token: &str,
) -> Result<String, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/git/trees");
    let payload = serde_json::json!({"base_tree": base_tree, "tree": entries});
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&payload),
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    data.as_ref()
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| GitHubError("'sha'".into()))
}

/// Create a commit with the supplied parent SHAs.
pub async fn create_commit(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    message: &str,
    tree: &str,
    parents: &[String],
    token: &str,
) -> Result<String, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/git/commits");
    let payload = serde_json::json!({"message": message, "tree": tree, "parents": parents});
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&payload),
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    data.as_ref()
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| GitHubError("'sha'".into()))
}

/// Create a branch ref. GitHub's conflict response deliberately remains an error.
pub async fn create_ref(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    branch: &str,
    sha: &str,
    token: &str,
) -> Result<String, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/git/refs");
    let payload = serde_json::json!({"ref": format!("refs/heads/{branch}"), "sha": sha});
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&payload),
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    data.as_ref()
        .and_then(|value| value.get("object"))
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| GitHubError("'object' or 'sha'".into()))
}

/// Open a pull request and return Python's compact response projection.
pub async fn open_pull(
    transport: &dyn GitHubTransport,
    api_root: &str,
    pull: OpenPullRequest<'_>,
    token: &str,
) -> Result<Value, GitHubError> {
    let url = format!("{api_root}/repos/{}/pulls", pull.repo);
    let payload = serde_json::json!({
        "title": pull.title,
        "head": pull.head,
        "base": pull.base,
        "body": pull.body,
    });
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&payload),
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let data = data.unwrap_or(Value::Null);
    let number = python_int(
        data.get("number")
            .ok_or_else(|| GitHubError("'number'".into()))?,
    )?;
    let pull_head = data
        .get("head")
        .filter(|value| python_truthy(value))
        .and_then(|value| value.get("sha"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(serde_json::json!({
        "number": number,
        "url": data.get("html_url").cloned().unwrap_or(Value::Null),
        "node_id": data.get("node_id").cloned().unwrap_or(Value::Null),
        "head": pull_head,
    }))
}

pub struct OpenPullRequest<'a> {
    pub repo: &'a str,
    pub title: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    pub body: &'a str,
}

fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn github_object(value: Option<Value>) -> Result<Value, GitHubError> {
    let value = value.unwrap_or(Value::Null);
    if value.is_object() {
        Ok(value)
    } else {
        Err(GitHubError(format!(
            "'{}' object has no attribute 'get'",
            python_type_name(&value)
        )))
    }
}

/// Return open pull requests whose head belongs to the repository's owner and branch.
pub async fn pulls_for_head(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    branch: &str,
    token: &str,
) -> Result<Vec<Value>, GitHubError> {
    let owner = repo.split('/').next().unwrap_or("");
    let url = format!("{api_root}/repos/{repo}/pulls?state=open&head={owner}:{branch}&per_page=20");
    let (_, batch) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let pulls = batch
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    pulls
        .into_iter()
        .map(|pull| {
            let number = python_int(
                pull.get("number")
                    .ok_or_else(|| GitHubError("'number'".into()))?,
            )?;
            Ok(serde_json::json!({
                "number": number,
                "url": pull.get("html_url").cloned().unwrap_or(Value::Null),
                "node_id": pull.get("node_id").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

/// Queue a pull request for auto-merge, using GitHub's GraphQL mutation.
pub async fn enable_auto_merge(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    token: &str,
    method: Option<&str>,
) -> Result<Value, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/pulls/{number}");
    let (_, pull) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let pull = github_object(pull)?;
    let node_id = pull
        .get("node_id")
        .filter(|value| python_truthy(value))
        .cloned()
        .ok_or_else(|| {
            GitHubError(format!(
                "{repo}#{number}: no node id for the auto-merge mutation"
            ))
        })?;
    let method = method.unwrap_or("MERGE").to_uppercase();
    let variables = serde_json::json!({"id":node_id,"method":method});
    let result = graphql(
        transport,
        api_root,
        "mutation($id: ID!, $method: PullRequestMergeMethod!) { enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: $method}) { pullRequest { number } } }",
        token,
        Some(&variables),
    )
    .await?;
    Ok(result
        .get("enablePullRequestAutoMerge")
        .filter(|value| python_truthy(value))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({})))
}

/// Disable a pull request's queued auto-merge through GitHub GraphQL.
pub async fn disable_auto_merge(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    token: &str,
) -> Result<Value, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/pulls/{number}");
    let (_, pull) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let pull = github_object(pull)?;
    let node_id = pull
        .get("node_id")
        .filter(|value| python_truthy(value))
        .cloned()
        .ok_or_else(|| {
            GitHubError(format!(
                "{repo}#{number}: no node id for the auto-merge mutation"
            ))
        })?;
    let variables = serde_json::json!({"id":node_id});
    graphql(
        transport,
        api_root,
        "mutation($id: ID!) { disablePullRequestAutoMerge(input: {pullRequestId: $id}) { pullRequest { number } } }",
        token,
        Some(&variables),
    )
    .await
}

/// Read a pull request's state fields and normalize its Python truth values.
pub async fn pull_state(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    token: &str,
) -> Result<Value, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/pulls/{number}");
    let (_, response) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let pull = github_object(response)?;
    Ok(serde_json::json!({
        "state": pull.get("state").cloned().unwrap_or(Value::Null),
        "merged": pull.get("merged").is_some_and(python_truthy),
        "merged_at": pull.get("merged_at").cloned().unwrap_or(Value::Null),
        "mergeable_state": pull.get("mergeable_state").cloned().unwrap_or(Value::Null),
        "auto_merge": pull.get("auto_merge").is_some_and(python_truthy),
    }))
}

/// Build GitHub's branch-protection request body for the review app check.
pub fn protection_payload(
    approvals: &Value,
    reviewer_app_id: &Value,
    bypass_app_slug: Option<&str>,
) -> Result<Value, GitHubError> {
    let approvals = python_int(approvals)?;
    let reviewer_app_id = python_int(reviewer_app_id)?;
    let mut reviews = serde_json::json!({
        "dismiss_stale_reviews": true,
        "require_code_owner_reviews": false,
        "required_approving_review_count": approvals,
    });
    if let Some(slug) = bypass_app_slug.filter(|value| !value.is_empty()) {
        reviews["bypass_pull_request_allowances"] = serde_json::json!({"apps":[slug]});
    }
    Ok(serde_json::json!({
        "required_status_checks": {
            "strict": false,
            "checks": [{"context": REVIEW_CONTEXT, "app_id": reviewer_app_id}],
        },
        "enforce_admins": true,
        "required_pull_request_reviews": reviews,
        "restrictions": null,
        "allow_force_pushes": false,
        "allow_deletions": false,
        "required_linear_history": false,
    }))
}

/// Resolve the required distinct reviewer lanes, using the optional JSON config
/// when supplied and `github-repos.json` otherwise.
pub fn required_verdicts(repo: &str, config: Option<&Value>) -> Result<usize, GitHubError> {
    required_verdicts_with_fs(repo, config, &default_config_dir(), &OsAppFileSystem)
}

fn required_verdicts_with_fs(
    repo: &str,
    config: Option<&Value>,
    config_dir: &Path,
    file_system: &dyn AppFileSystem,
) -> Result<usize, GitHubError> {
    let config = config
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| {
            file_system
                .read_config(&config_dir.join("github-repos.json"))
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
        });
    let table = config
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|object| object.get("required_verdicts"))
        .filter(|value| python_truthy(value));
    let fallback = if repo == "huntergdavis/poolrad-decomp" {
        POOLRAD_REQUIRED_VERDICTS
    } else {
        DEFAULT_REQUIRED_VERDICTS
    };
    let fallback_value = Value::from(fallback as i64);
    let configured = match table {
        None => &fallback_value,
        Some(Value::Object(values)) => values.get(repo).unwrap_or(&fallback_value),
        Some(other) => {
            return Err(GitHubError(format!(
                "'{}' object has no attribute 'get'",
                python_type_name(other)
            )));
        }
    };
    let value = python_int(configured).unwrap_or(DEFAULT_REQUIRED_VERDICTS as i64);
    Ok(value.max(1) as usize)
}

fn default_config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("DUNKINGSHEEP_DIR") {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/dunkingsheep")
}

pub async fn pull_detail(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    token: &str,
) -> Result<Value, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/pulls/{number}");
    let (_, pull) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let pull = pull.unwrap_or(Value::Null);
    Ok(serde_json::json!({
        "additions": pull.get("additions").cloned().unwrap_or(Value::Null),
        "deletions": pull.get("deletions").cloned().unwrap_or(Value::Null),
        "changed_files": pull.get("changed_files").cloned().unwrap_or(Value::Null),
        "mergeable": pull.get("mergeable").cloned().unwrap_or(Value::Null),
        "mergeable_state": pull.get("mergeable_state").cloned().unwrap_or(Value::Null),
        "body": pull.get("body").filter(|v| python_truthy(v)).cloned().unwrap_or_else(|| Value::String(String::new())),
    }))
}

pub struct StatusUpdate<'a> {
    pub repo: &'a str,
    pub sha: &'a str,
    pub state: &'a str,
    pub description: &'a str,
    pub target_url: Option<&'a str>,
}

pub async fn post_status(
    transport: &dyn GitHubTransport,
    api_root: &str,
    token: &str,
    update: StatusUpdate<'_>,
) -> Result<(u16, Option<Value>), GitHubError> {
    let mut payload = serde_json::Map::from_iter([
        ("state".into(), Value::String(update.state.to_owned())),
        (
            "context".into(),
            Value::String("dunkingsheep/review".into()),
        ),
        (
            "description".into(),
            Value::String(update.description.chars().take(140).collect()),
        ),
    ]);
    if let Some(target_url) = update.target_url.filter(|value| !value.is_empty()) {
        payload.insert("target_url".into(), Value::String(target_url.to_owned()));
    }
    let url = format!("{api_root}/repos/{}/statuses/{}", update.repo, update.sha);
    request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&Value::Object(payload)),
            timeout_s: 20,
            api_root,
        },
    )
    .await
}

/// Return the latest `dunkingsheep/review` commit status, or `None` when the
/// commit has no status for that context. Validate the whole combined response
/// before a caller uses it to reconcile a terminal review verdict.
pub async fn review_status(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    sha: &str,
    token: &str,
) -> Result<Option<String>, GitHubError> {
    let url = format!("{api_root}/repos/{repo}/commits/{sha}/status");
    let (_, combined) = request(
        transport,
        RequestOptions {
            method: "GET",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root,
        },
    )
    .await
    .map_err(|error| {
        if let Some(detail) = error.0.strip_prefix("invalid JSON response: ") {
            GitHubError(format!("GitHub commit status: invalid JSON: {detail}"))
        } else {
            error
        }
    })?;
    let combined = combined.unwrap_or(Value::Null);
    let statuses = combined
        .get("statuses")
        .and_then(Value::as_array)
        .ok_or_else(|| GitHubError("GitHub commit status: expected a statuses list".into()))?;

    let mut review = None;
    for status in statuses {
        let Some(status) = status.as_object() else {
            return Err(GitHubError(
                "GitHub commit status: expected a status object".into(),
            ));
        };
        let context = status
            .get("context")
            .and_then(Value::as_str)
            .filter(|context| !context.is_empty())
            .ok_or_else(|| GitHubError("GitHub commit status: missing status context".into()))?;
        let state = status
            .get("state")
            .and_then(Value::as_str)
            .filter(|state| matches!(*state, "pending" | "success" | "failure" | "error"))
            .ok_or_else(|| GitHubError("GitHub commit status: invalid status state".into()))?;
        if context == REVIEW_CONTEXT {
            if review.is_some() {
                return Err(GitHubError(
                    "GitHub commit status: duplicate review context".into(),
                ));
            }
            review = Some(state.to_owned());
        }
    }
    Ok(review)
}

pub async fn merge_pull(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    token: &str,
    sha: Option<&str>,
) -> Result<(u16, Option<Value>), GitHubError> {
    let mut payload =
        serde_json::Map::from_iter([("merge_method".into(), Value::String("merge".into()))]);
    if let Some(sha) = sha.filter(|value| !value.is_empty()) {
        payload.insert("sha".into(), Value::String(sha.to_owned()));
    }
    let url = format!("{api_root}/repos/{repo}/pulls/{number}/merge");
    request(
        transport,
        RequestOptions {
            method: "PUT",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&Value::Object(payload)),
            timeout_s: 20,
            api_root,
        },
    )
    .await
}

pub async fn review_pull(
    transport: &dyn GitHubTransport,
    api_root: &str,
    repo: &str,
    number: i64,
    event: &str,
    body: &str,
    token: &str,
) -> Result<(u16, Option<Value>), GitHubError> {
    let payload = serde_json::json!({"event": event, "body": body});
    let url = format!("{api_root}/repos/{repo}/pulls/{number}/reviews");
    request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&payload),
            timeout_s: 20,
            api_root,
        },
    )
    .await
}

pub async fn graphql(
    transport: &dyn GitHubTransport,
    api_root: &str,
    query: &str,
    token: &str,
    variables: Option<&Value>,
) -> Result<Value, GitHubError> {
    let mut payload =
        serde_json::Map::from_iter([("query".into(), Value::String(query.to_owned()))]);
    if let Some(variables) = variables.filter(|value| python_truthy(value)) {
        payload.insert("variables".into(), variables.clone());
    }
    let url = format!("{}/graphql", api_root.trim_end_matches('/'));
    let (_, data) = request(
        transport,
        RequestOptions {
            method: "POST",
            url: &url,
            token: Some(token),
            bearer: None,
            payload: Some(&Value::Object(payload)),
            timeout_s: 20,
            api_root,
        },
    )
    .await?;
    let Some(data) = data else {
        return Ok(serde_json::json!({}));
    };
    if let Some(errors) = data
        .as_object()
        .and_then(|object| object.get("errors"))
        .filter(|value| python_truthy(value))
    {
        let values = errors.as_array().cloned().unwrap_or_default();
        let message = values
            .iter()
            .map(|error| {
                let value = error
                    .as_object()
                    .map(|object| object.get("message").cloned().unwrap_or(Value::Null))
                    .unwrap_or_else(|| error.clone());
                python_string(&value)
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(GitHubError(format!("GitHub GraphQL: {message}")));
    }
    Ok(data
        .as_object()
        .and_then(|object| object.get("data"))
        .filter(|value| python_truthy(value))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({})))
}

#[cfg(test)]
#[path = "github_tests.rs"]
pub(crate) mod tests;
