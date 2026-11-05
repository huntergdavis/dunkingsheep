//! Persistent authentication for the desk's existing credential/session format.
//!
//! Paths are supplied by the startup owner. This module never reads the process
//! environment or prints credentials. All filesystem/SQLite work runs off the
//! async executor through a blocking worker.
use crate::routes::auth::{AuthBackend, AuthError, AuthFuture, Identity};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    digest, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    num::NonZeroU32,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const ROUNDS: u32 = 400_000;
const SESSION_S: f64 = 7.0 * 24.0 * 3600.0;
#[derive(Clone)]
pub struct PersistentAuth {
    inner: Arc<Inner>,
}
struct Inner {
    data: PathBuf,
    gate: Mutex<()>,
    attempts: Mutex<HashMap<String, Arc<Mutex<Vec<f64>>>>>,
    login_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    kdf_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Credentials {
    salt: String,
    digest: String,
    token: String,
}
fn internal(_: impl std::fmt::Display) -> AuthError {
    AuthError::Internal("authentication storage failed".into())
}
fn now() -> Result<f64, AuthError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_secs_f64())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>, AuthError> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(internal("invalid hex"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(internal))
        .collect()
}
fn secret(n: usize) -> Result<String, AuthError> {
    let mut bytes = vec![0; n];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| internal("randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn salt() -> Result<Vec<u8>, AuthError> {
    let mut bytes = vec![0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| internal("randomness"))?;
    Ok(bytes)
}
fn derive(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(ROUNDS).unwrap(),
        salt,
        password.as_bytes(),
        &mut out,
    );
    out
}
fn password_matches(password: &str, record: &Credentials) -> Result<bool, AuthError> {
    let salt = unhex(&record.salt)?;
    let expected = unhex(&record.digest)?;
    Ok(pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(ROUNDS).unwrap(),
        &salt,
        password.as_bytes(),
        &expected,
    )
    .is_ok())
}
fn session_key(cookie: &str) -> String {
    hex(digest::digest(&digest::SHA256, cookie.as_bytes()).as_ref())
}
fn read_record(path: &Path) -> Result<Credentials, AuthError> {
    serde_json::from_slice(&fs::read(path).map_err(internal)?).map_err(internal)
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let parent = path.parent().ok_or_else(|| internal("parent"))?;
    let temporary = parent.join(format!(".auth-{}", secret(18)?));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(internal)?;
        file.write_all(bytes).map_err(internal)?;
        file.sync_all().map_err(internal)?;
        fs::rename(&temporary, path).map_err(internal)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
impl PersistentAuth {
    /// Explicit existing desk data directory. Initializes only auth and session storage.
    pub fn open(data: impl AsRef<Path>) -> Result<Self, AuthError> {
        Self::open_with_password(data, None)
    }

    /// `initial_password` is captured by startup from VP_PASSWORD; empty means absent.
    pub fn open_with_password(
        data: impl AsRef<Path>,
        initial_password: Option<&str>,
    ) -> Result<Self, AuthError> {
        let data = data.as_ref().to_path_buf();
        fs::create_dir_all(&data).map_err(internal)?;
        let db = Connection::open(data.join("messages.sqlite3")).map_err(internal)?;
        db.busy_timeout(Duration::from_secs(10)).map_err(internal)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS sessions(token_hash TEXT PRIMARY KEY, expires_at REAL NOT NULL); CREATE INDEX IF NOT EXISTS sessions_expires_at ON sessions(expires_at);").map_err(internal)?;
        let auth = data.join("auth.json");
        if !auth.exists() {
            let password = match initial_password.filter(|p| !p.is_empty()) {
                Some(password) => password.to_owned(),
                None => secret(18)?,
            };
            let token = secret(32)?;
            let salt = salt()?;
            let record = Credentials {
                salt: hex(&salt),
                digest: hex(&derive(&password, &salt)),
                token,
            };
            private_write(&auth, &serde_json::to_vec(&record).map_err(internal)?)?;
            private_write(
                &data.join("owner-credentials"),
                format!("Password: {password}\nAPI token: {}\n", record.token).as_bytes(),
            )?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                data,
                gate: Mutex::new(()),
                attempts: Mutex::new(HashMap::new()),
                login_slots: Arc::new(tokio::sync::Semaphore::new(8)),
                #[cfg(test)]
                kdf_hook: Mutex::new(None),
            }),
        })
    }
    fn db(&self) -> Result<Connection, AuthError> {
        let db = Connection::open(self.inner.data.join("messages.sqlite3")).map_err(internal)?;
        db.busy_timeout(Duration::from_secs(10)).map_err(internal)?;
        Ok(db)
    }
    fn record(&self) -> Result<Credentials, AuthError> {
        read_record(&self.inner.data.join("auth.json"))
    }
    fn sync_credentials(&self) -> Result<(), AuthError> {
        loop {
            let credentials = self.inner.data.join("owner-credentials");
            let auth = self.inner.data.join("auth.json");
            let (text, record, modified) = {
                let _guard = self.inner.gate.lock().map_err(internal)?;
                let metadata = match fs::metadata(&credentials) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(internal(error)),
                };
                let modified = metadata.modified().map_err(internal)?;
                if modified
                    <= fs::metadata(&auth)
                        .map_err(internal)?
                        .modified()
                        .map_err(internal)?
                {
                    return Ok(());
                }
                (
                    fs::read_to_string(&credentials).map_err(internal)?,
                    self.record()?,
                    modified,
                )
            };
            let Some(password) = text
                .lines()
                .find_map(|line| line.strip_prefix("Password: ").filter(|p| !p.is_empty()))
            else {
                return Ok(());
            };
            let matches = password_matches(password, &record)?;
            let replacement = if matches {
                None
            } else {
                let fresh = salt()?;
                Some((hex(&fresh), hex(&derive(password, &fresh))))
            };
            let _guard = self.inner.gate.lock().map_err(internal)?;
            if self.record()? != record
                || fs::metadata(&credentials)
                    .map_err(internal)?
                    .modified()
                    .map_err(internal)?
                    != modified
                || fs::read_to_string(&credentials).map_err(internal)? != text
            {
                continue;
            }
            if let Some((fresh_salt, fresh_digest)) = replacement {
                let mut updated = record;
                updated.salt = fresh_salt;
                updated.digest = fresh_digest;
                private_write(&auth, &serde_json::to_vec(&updated).map_err(internal)?)?;
                self.db()?
                    .execute("DELETE FROM sessions", [])
                    .map_err(internal)?;
            }
            return Ok(());
        }
    }
    fn login_sync(&self, address: &str, password: &str) -> Result<String, AuthError> {
        let address_attempts = {
            let mut all = self.inner.attempts.lock().map_err(internal)?;
            Arc::clone(all.entry(address.to_owned()).or_default())
        };
        let mut attempts = address_attempts.lock().map_err(internal)?;
        loop {
            self.sync_credentials()?;
            let timestamp = now()?;
            attempts.retain(|t| timestamp - *t < 300.0);
            if attempts.len() >= 8 {
                return Err(AuthError::TooManyAttempts);
            }
            let record = {
                let _guard = self.inner.gate.lock().map_err(internal)?;
                self.record()?
            };
            #[cfg(test)]
            {
                let hook = self.inner.kdf_hook.lock().map_err(internal)?.clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            let matches = password_matches(password, &record)?;
            let _guard = self.inner.gate.lock().map_err(internal)?;
            if self.record()? != record {
                continue;
            }
            if !matches {
                attempts.push(timestamp);
                return Err(AuthError::IncorrectPassword);
            }
            attempts.clear();
            let session = secret(32)?;
            let mut db = self.db()?;
            let tx = db.transaction().map_err(internal)?;
            tx.execute("DELETE FROM sessions WHERE expires_at<=?", [timestamp])
                .map_err(internal)?;
            tx.execute(
                "INSERT INTO sessions(token_hash,expires_at) VALUES(?,?)",
                params![session_key(&session), timestamp + SESSION_S],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)?;
            return Ok(session);
        }
    }
    fn identity_sync(
        &self,
        bearer: Option<&str>,
        cookie: Option<&str>,
    ) -> Result<Identity, AuthError> {
        let _guard = self.inner.gate.lock().map_err(internal)?;
        if let Some(bearer) = bearer {
            let record = self.record()?;
            if bearer.as_bytes().ct_eq(record.token.as_bytes()).into() {
                return Ok(Identity::Api);
            }
        }
        if let Some(cookie) = cookie.filter(|s| !s.is_empty()) {
            let db = self.db()?;
            let key = session_key(cookie);
            let expires: Option<f64> = db
                .query_row(
                    "SELECT expires_at FROM sessions WHERE token_hash=?",
                    [&key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(internal)?;
            if let Some(expires) = expires {
                if expires > now()? {
                    return Ok(Identity::Owner);
                }
                db.execute("DELETE FROM sessions WHERE token_hash=?", [&key])
                    .map_err(internal)?;
            }
        }
        Err(AuthError::SignInRequired)
    }
    fn logout_sync(&self, cookie: Option<&str>) -> Result<(), AuthError> {
        let _guard = self.inner.gate.lock().map_err(internal)?;
        if let Some(cookie) = cookie.filter(|s| !s.is_empty()) {
            self.db()?
                .execute(
                    "DELETE FROM sessions WHERE token_hash=?",
                    [session_key(cookie)],
                )
                .map_err(internal)?;
        }
        Ok(())
    }
    fn change_sync(&self, current: &str, new: &str, keep: Option<&str>) -> Result<(), AuthError> {
        loop {
            let record = {
                let _guard = self.inner.gate.lock().map_err(internal)?;
                self.record()?
            };
            let matches = password_matches(current, &record)?;
            let fresh = if matches { Some(salt()?) } else { None };
            let replacement = fresh
                .as_ref()
                .map(|fresh| (hex(fresh), hex(&derive(new, fresh))));
            let _guard = self.inner.gate.lock().map_err(internal)?;
            if self.record()? != record {
                continue;
            }
            let Some((fresh_salt, fresh_digest)) = replacement else {
                return Err(AuthError::IncorrectPassword);
            };
            let mut updated = record;
            updated.salt = fresh_salt;
            updated.digest = fresh_digest;
            private_write(
                &self.inner.data.join("auth.json"),
                &serde_json::to_vec(&updated).map_err(internal)?,
            )?;
            let credentials = self.inner.data.join("owner-credentials");
            if credentials.exists() {
                private_write(
                    &credentials,
                    format!("Password: {new}\nAPI token: {}\n", updated.token).as_bytes(),
                )?;
            }
            self.db()?
                .execute(
                    "DELETE FROM sessions WHERE token_hash<>?",
                    [session_key(keep.unwrap_or(""))],
                )
                .map_err(internal)?;
            return Ok(());
        }
    }
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(Self) -> Result<T, AuthError> + Send + 'static,
    ) -> Result<T, AuthError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || f(this))
            .await
            .map_err(internal)?
    }
}
impl AuthBackend for PersistentAuth {
    fn login<'a>(
        &'a self,
        address: &'a str,
        password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>> {
        let (a, p) = (address.to_owned(), password.to_owned());
        Box::pin(async move {
            let permit = Arc::clone(&self.inner.login_slots)
                .acquire_owned()
                .await
                .map_err(internal)?;
            self.blocking(move |s| {
                let _permit = permit;
                s.login_sync(&a, &p)
            })
            .await
        })
    }
    fn identity<'a>(
        &'a self,
        bearer: Option<&'a str>,
        cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        let (b, c) = (bearer.map(str::to_owned), cookie.map(str::to_owned));
        Box::pin(async move {
            self.blocking(move |s| s.identity_sync(b.as_deref(), c.as_deref()))
                .await
        })
    }
    fn logout<'a>(&'a self, cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
        let c = cookie.map(str::to_owned);
        Box::pin(async move { self.blocking(move |s| s.logout_sync(c.as_deref())).await })
    }
    fn change_password<'a>(
        &'a self,
        current: &'a str,
        new: &'a str,
        keep_cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>> {
        let (c, n, k) = (
            current.to_owned(),
            new.to_owned(),
            keep_cookie.map(str::to_owned),
        );
        Box::pin(async move {
            let permit = Arc::clone(&self.inner.login_slots)
                .acquire_owned()
                .await
                .map_err(internal)?;
            self.blocking(move |s| {
                let _permit = permit;
                s.change_sync(&c, &n, k.as_deref())
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn password_change_waits_for_shared_kdf_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let auth =
            PersistentAuth::open_with_password(dir.path(), Some("original-password")).unwrap();
        let permits = Arc::clone(&auth.inner.login_slots)
            .acquire_many_owned(8)
            .await
            .unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            auth.change_password("original-password", "replacement-password", None)
        )
        .await
        .is_err());
        assert!(password_matches("original-password", &auth.record().unwrap()).unwrap());
        drop(permits);
        auth.change_password("original-password", "replacement-password", None)
            .await
            .unwrap();
        assert!(password_matches("replacement-password", &auth.record().unwrap()).unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_login_does_not_block_identity_and_rechecks_changed_password() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let auth =
            PersistentAuth::open_with_password(dir.path(), Some("original-password")).unwrap();
        let token = auth.record().unwrap().token;
        let first = Arc::new(AtomicBool::new(true));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        *auth.inner.kdf_hook.lock().unwrap() = Some(Arc::new(move || {
            if first.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
        }));
        let worker = auth.clone();
        let login = tokio::spawn(async move { worker.login("one", "original-password").await });
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), auth.identity(Some(&token), None))
                .await
                .unwrap()
                .unwrap(),
            Identity::Api
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            auth.change_password("original-password", "replacement-password", None),
        )
        .await
        .unwrap()
        .unwrap();
        release_tx.send(()).unwrap();
        assert!(matches!(
            login.await.unwrap(),
            Err(AuthError::IncorrectPassword)
        ));
    }

    #[test]
    fn password_digest_matches_python_pbkdf2() {
        let salt: Vec<u8> = (0..16).collect();
        assert_eq!(
            hex(&derive("fixture-🔒", &salt)),
            "b54996009e6c640e70353b694757189cbdb3ae2ed570c351e63a7251659089f3"
        );
    }

    #[tokio::test]
    async fn real_login_route_uses_persistent_provider() {
        use axum::{
            body::Body,
            extract::{ConnectInfo, Extension},
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let backend: Arc<dyn AuthBackend> = Arc::new(
            PersistentAuth::open_with_password(dir.path(), Some("original-password")).unwrap(),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/api/login")
            .header("content-type", "application/json")
            .extension(ConnectInfo(
                "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(r#"{"password":"original-password"}"#))
            .unwrap();
        let response = crate::routes::auth::router()
            .layer(Extension(backend))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("vp_session="));
    }

    #[tokio::test]
    async fn python_compatible_credentials_sessions_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let auth =
            PersistentAuth::open_with_password(dir.path(), Some("test-password-123")).unwrap();
        assert_eq!(
            fs::metadata(dir.path().join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let record = auth.record().unwrap();
        assert!(password_matches("test-password-123", &record).unwrap());
        let cookie = auth.login("first", "test-password-123").await.unwrap();
        assert_eq!(
            auth.identity(None, Some(&cookie)).await.unwrap(),
            Identity::Owner
        );
        assert_eq!(
            auth.identity(Some(&record.token), None).await.unwrap(),
            Identity::Api
        );
        let db = auth.db().unwrap();
        let stored: String = db
            .query_row("SELECT token_hash FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, session_key(&cookie));
        assert_ne!(stored, cookie);
        let restarted = PersistentAuth::open(dir.path()).unwrap();
        assert_eq!(
            restarted.identity(None, Some(&cookie)).await.unwrap(),
            Identity::Owner
        );
        restarted.logout(Some(&cookie)).await.unwrap();
        assert!(matches!(
            restarted.identity(None, Some(&cookie)).await,
            Err(AuthError::SignInRequired)
        ));
    }

    #[tokio::test]
    async fn password_change_keeps_current_session_and_invalidates_others() {
        let dir = tempfile::tempdir().unwrap();
        let auth =
            PersistentAuth::open_with_password(dir.path(), Some("original-password")).unwrap();
        let first = auth.login("one", "original-password").await.unwrap();
        let second = auth.login("two", "original-password").await.unwrap();
        assert!(matches!(
            auth.change_password("wrong", "replacement-password", Some(&first))
                .await,
            Err(AuthError::IncorrectPassword)
        ));
        auth.change_password("original-password", "replacement-password", Some(&first))
            .await
            .unwrap();
        assert_eq!(
            auth.identity(None, Some(&first)).await.unwrap(),
            Identity::Owner
        );
        assert!(matches!(
            auth.identity(None, Some(&second)).await,
            Err(AuthError::SignInRequired)
        ));
        assert!(matches!(
            auth.login("three", "original-password").await,
            Err(AuthError::IncorrectPassword)
        ));
        auth.login("four", "replacement-password").await.unwrap();
    }

    #[tokio::test]
    async fn edited_owner_credentials_revoke_sessions_at_next_login() {
        let dir = tempfile::tempdir().unwrap();
        let auth =
            PersistentAuth::open_with_password(dir.path(), Some("original-password")).unwrap();
        let cookie = auth.login("one", "original-password").await.unwrap();
        let path = dir.path().join("owner-credentials");
        fs::write(&path, format!("Password: {}\n", "changed-password")).unwrap();
        let modified = fs::metadata(dir.path().join("auth.json"))
            .unwrap()
            .modified()
            .unwrap()
            + Duration::from_secs(2);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        auth.login("two", "changed-password").await.unwrap();
        assert!(matches!(
            auth.identity(None, Some(&cookie)).await,
            Err(AuthError::SignInRequired)
        ));
    }

    #[tokio::test]
    async fn rate_limit_and_storage_errors_are_not_successes() {
        let dir = tempfile::tempdir().unwrap();
        let auth =
            PersistentAuth::open_with_password(dir.path(), Some("original-password")).unwrap();
        for _ in 0..8 {
            assert!(matches!(
                auth.login("same", "wrong").await,
                Err(AuthError::IncorrectPassword)
            ));
        }
        assert!(matches!(
            auth.login("same", "original-password").await,
            Err(AuthError::TooManyAttempts)
        ));
        fs::write(dir.path().join("auth.json"), b"not json").unwrap();
        assert!(matches!(
            auth.login("other", "original-password").await,
            Err(AuthError::Internal(_))
        ));
    }
}
