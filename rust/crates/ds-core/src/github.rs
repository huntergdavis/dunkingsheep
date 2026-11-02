//! GitHub App configuration, RS256 authentication, REST requests and token cache.
//!
//! This module ports the small authentication layer in `dunk_github.py`. Paths,
//! time and HTTP transport are injectable so its credential behavior can be
//! exercised without touching a user's config or making a live request.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::Value;

use crate::error::{refusal, DunkError};

pub const KINDS: [&str; 2] = ["author", "reviewer"];
pub const DEFAULT_API: &str = "https://api.github.com";
pub const REFRESH_MARGIN_S: f64 = 600.0;

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
    let now = now.filter(|v| *v != 0).unwrap_or_else(unix_time);
    let mut key_file = File::open(&app.pem).map_err(|e| GitHubError(e.to_string()))?;
    let mut pem = Vec::new();
    key_file
        .read_to_end(&mut pem)
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
            let status = response.status();
            let reason = status.canonical_reason().unwrap_or("unknown").to_owned();
            let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
            Ok(GitHubResponse {
                status: status.as_u16(),
                reason,
                body,
            })
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
    transport: Box<dyn GitHubTransport>,
    api_root: String,
}

impl Tokens {
    pub fn new(
        path: Option<PathBuf>,
        config_dir: PathBuf,
        transport: Box<dyn GitHubTransport>,
        api_root: Option<String>,
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
            transport,
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
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|e| GitHubError(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| GitHubError(e.to_string()))?;
        }
        temporary
            .as_file_mut()
            .write_all(&bytes)
            .map_err(|e| GitHubError(e.to_string()))?;
        temporary
            .as_file_mut()
            .flush()
            .map_err(|e| GitHubError(e.to_string()))?;
        temporary
            .persist(&self.path)
            .map_err(|e| GitHubError(e.error.to_string()))?;
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
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
