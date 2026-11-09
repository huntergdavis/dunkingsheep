//! Canonical memory producer. SQL mutations run on one SyncWriter; parsing never does.
use super::ingest::{self, MemoryWrite};
use super::jsonl::Throttle;
use crate::db::{map_sqlite, SyncWriter};
use crate::error::DunkError;
use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) struct MemoryWriter {
    path: PathBuf,
    state: Mutex<WriterState>,
}
struct WriterState {
    actor: Option<SyncWriter>,
    closed: bool,
}
impl MemoryWriter {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            state: Mutex::new(WriterState {
                actor: None,
                closed: false,
            }),
        }
    }
    // Private SQL-only boundary; callers expose typed mutations, never parser closures.
    pub(crate) fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Connection) -> Result<T, DunkError> + Send + 'static,
    ) -> Result<T, DunkError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(DunkError::Refused(
                "memory writes require a non-Tokio standard thread".into(),
            ));
        }
        let mut state = self.state.lock().expect("memory writer lock");
        if state.closed {
            return Err(DunkError::Unavailable("memory writer is closed".into()));
        }
        if state.actor.is_none() {
            state.actor = Some(SyncWriter::spawn(&self.path)?);
        }
        state.actor.as_ref().expect("initialized writer").call(job)
    }
    pub(crate) fn close(&self) -> Result<(), DunkError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(DunkError::Refused(
                "memory shutdown requires a non-Tokio standard thread".into(),
            ));
        }
        let mut state = self.state.lock().expect("memory writer lock");
        if let Some(actor) = state.actor.take() {
            // Close the actor even when committing the pending session fails.
            let result = actor.call(|connection| ingest::commit_pending(connection));
            actor.shutdown();
            state.closed = true;
            return result;
        }
        state.closed = true;
        Ok(())
    }
    pub(crate) fn enqueue(
        &self,
        path: &str,
        harness: Option<&str>,
        project: Option<&str>,
    ) -> Result<(i64, i64), DunkError> {
        let receipt = self.enqueue_receipt(path, harness, project)?;
        Ok((receipt.source_id, receipt.queue_id))
    }

    pub(crate) fn enqueue_receipt(
        &self,
        path: &str,
        harness: Option<&str>,
        project: Option<&str>,
    ) -> Result<ingest::EnqueueReceipt, DunkError> {
        let source = ingest::prepare_enqueue(path, harness, project)?;
        self.call(move |connection| ingest::enqueue_prepared_receipt(connection, source))
    }
}
impl Drop for MemoryWriter {
    fn drop(&mut self) {
        // Explicit service shutdown is the normal path. A synchronous standalone
        // ingestor still drains its actor when its last owner goes away.
        let _ = self.close();
    }
}

#[derive(Clone)]
pub(crate) struct WriterSink(pub(crate) Arc<MemoryWriter>);
impl MemoryWrite for WriterSink {
    fn upsert_session(
        &mut self,
        session_id: &str,
        harness: &str,
        project: Option<&str>,
        cwd: Option<&str>,
        title: Option<&str>,
        started_at: Option<f64>,
        updated_at: Option<f64>,
        metadata: Option<&Value>,
    ) -> Result<(), DunkError> {
        let (id, harness, project, cwd, title, metadata) = (
            session_id.to_owned(),
            harness.to_owned(),
            project.map(str::to_owned),
            cwd.map(str::to_owned),
            title.map(str::to_owned),
            metadata.cloned(),
        );
        self.0.call(move |connection| {
            // Python sqlite starts an implicit transaction for the session DML.
            // It remains pending until the next batch/queue/source/checkpoint commit.
            if connection.is_autocommit() {
                connection
                    .execute_batch("BEGIN DEFERRED")
                    .map_err(map_sqlite)?;
            }
            ingest::upsert_session(
                connection,
                &id,
                &harness,
                project.as_deref(),
                cwd.as_deref(),
                title.as_deref(),
                started_at,
                updated_at,
                metadata.as_ref(),
            )
        })
    }
    fn insert_chunks(
        &mut self,
        session_id: &str,
        chunks: &[String],
        role: Option<&str>,
        kind: Option<&str>,
        created_at: Option<f64>,
        metadata: Option<&Value>,
    ) -> Result<usize, DunkError> {
        let batch = ingest::prepare_chunks(session_id, chunks, role, kind, created_at, metadata);
        self.0
            .call(move |connection| ingest::insert_prepared(connection, batch))
    }

    fn upsert_source_row(
        &mut self,
        path: &str,
        harness: &str,
        project: Option<&str>,
    ) -> Result<(), DunkError> {
        let source = ingest::prepare_source(path, harness, project)?;
        self.0
            .call(move |connection| ingest::source_prepared(connection, source))
    }
}

use super::{
    ingestor::Ingestor,
    queries::{self, QueryStats},
    recall::{self, Filters, Hit},
};
use crate::db::ReadPool;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Explicit Float retains nonfinite typed inputs; JSON null never stands for NaN.
/// Typed f64 values do not carry Python object identity: NaN inputs receive distinct
/// cache identities on each request. Bare nonfinite protocol parsing belongs to t2982.
/// Neither numeric strings nor large integer digits are normalized through f64.
#[derive(Clone, Debug)]
pub enum QueryInput {
    Json(Value),
    Float(f64),
}
impl From<Value> for QueryInput {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}
impl QueryInput {
    fn truthy(&self) -> bool {
        match self {
            Self::Float(v) => *v != 0.0,
            Self::Json(v) => match v {
                Value::Null => false,
                Value::Bool(v) => *v,
                Value::String(v) => !v.is_empty(),
                Value::Array(v) => !v.is_empty(),
                Value::Object(v) => !v.is_empty(),
                Value::Number(v) => v.as_f64() != Some(0.0),
            },
        }
    }
    fn float(&self) -> Result<f64, DunkError> {
        match self {
            Self::Float(v) => Ok(*v),
            Self::Json(Value::Bool(v)) => Ok(if *v { 1.0 } else { 0.0 }),
            // Reuse the same Python-number conversion primitives as api.rs.
            Self::Json(Value::Number(v)) => crate::github::parse_python_json_float(&v.to_string())
                .map_err(|message| DunkError::Refused(message.into())),
            Self::Json(Value::String(v)) => crate::github::parse_python_float(v).ok_or_else(|| {
                DunkError::Refused(format!("could not convert string to float: {v:?}"))
            }),
            _ => Err(DunkError::Refused(
                "float() argument must be a string or a real number".into(),
            )),
        }
    }
    pub(crate) fn integer(&self) -> Result<String, DunkError> {
        match self {
            Self::Json(Value::Bool(v)) => Ok(if *v { "1" } else { "0" }.into()),
            Self::Json(Value::String(v)) => decimal_integer(v),
            Self::Json(Value::Number(v)) if !v.to_string().contains(['.', 'e', 'E']) => {
                decimal_integer(&v.to_string())
            }
            Self::Float(_) | Self::Json(Value::Number(_)) => {
                let value = self.float()?;
                if value.is_nan() {
                    return Err(DunkError::Refused(
                        "cannot convert float NaN to integer".into(),
                    ));
                }
                if !value.is_finite() {
                    return Err(DunkError::Refused(
                        "cannot convert float infinity to integer".into(),
                    ));
                }
                decimal_integer(&format!("{:.0}", value.trunc()))
            }
            _ => Err(DunkError::Refused(
                "int() argument must be a string, a bytes-like object or a real number".into(),
            )),
        }
    }
    fn atom(&self, nonce: u64) -> Result<KeyAtom, DunkError> {
        match self {
            Self::Json(Value::Null) => Ok(KeyAtom::Null),
            Self::Json(Value::String(v)) => Ok(KeyAtom::Text(v.clone())),
            Self::Json(Value::Array(_)) => {
                Err(DunkError::Refused("unhashable type: 'list'".into()))
            }
            Self::Json(Value::Object(_)) => {
                Err(DunkError::Refused("unhashable type: 'dict'".into()))
            }
            Self::Json(Value::Bool(_)) => Ok(KeyAtom::Integer(self.integer()?)),
            Self::Json(Value::Number(v)) if !v.to_string().contains(['.', 'e', 'E']) => {
                Ok(KeyAtom::Integer(self.integer()?))
            }
            _ => {
                let value = self.float()?;
                if value.is_nan() {
                    Ok(KeyAtom::Nan(nonce))
                } else if value.is_finite() && value.fract() == 0.0 {
                    Ok(KeyAtom::Integer(self.integer()?))
                } else {
                    Ok(KeyAtom::Float(value.to_bits()))
                }
            }
        }
    }
    fn sql_scalar(&self) -> Result<Option<recall::SqlScalar>, DunkError> {
        use rusqlite::types::Value as Sql;
        if !self.truthy() {
            return Ok(None);
        }
        Ok(Some(match self {
            Self::Json(Value::String(v)) => recall::SqlScalar::Value(Sql::Text(v.clone())),
            Self::Json(Value::Bool(v)) => recall::SqlScalar::Value(Sql::Integer(i64::from(*v))),
            Self::Json(Value::Number(v)) if !v.to_string().contains(['.', 'e', 'E']) => {
                recall::SqlScalar::Integer(self.integer()?)
            }
            _ => recall::SqlScalar::Value(Sql::Real(self.float()?)),
        }))
    }
}
fn sqlite_overflow() -> DunkError {
    DunkError::Refused("Python int too large to convert to SQLite INTEGER".into())
}
fn decimal_integer(text: &str) -> Result<String, DunkError> {
    let text = text.trim();
    let negative = text.starts_with('-');
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let mut digits = String::new();
    let mut previous = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(digit) = python_decimal_digit(c) {
            digits.push(char::from(b'0' + digit));
            previous = true;
        } else if c == '_'
            && previous
            && chars
                .peek()
                .and_then(|v| python_decimal_digit(*v))
                .is_some()
        {
            previous = false;
        } else {
            return Err(DunkError::Refused(format!(
                "invalid literal for int() with base 10: {text:?}"
            )));
        }
    }
    if !previous {
        return Err(DunkError::Refused(format!(
            "invalid literal for int() with base 10: {text:?}"
        )));
    }
    // Python's configured decimal string conversion limit applies before key creation.
    if digits.len() > 4300 {
        return Err(DunkError::Refused(
            "Exceeds the limit (4300 digits) for integer string conversion".into(),
        ));
    }
    let significant = digits.trim_start_matches('0');
    Ok(if significant.is_empty() {
        "0".into()
    } else if negative {
        format!("-{significant}")
    } else {
        significant.into()
    })
}
fn fetch_decimal(limit: &str) -> String {
    if limit.starts_with('-') {
        return "20".into();
    }
    let mut carry = 0u8;
    let mut bytes = Vec::new();
    for byte in limit.bytes().rev() {
        let n = (byte - b'0') * 3 + carry;
        bytes.push(b'0' + n % 10);
        carry = n / 10;
    }
    if carry > 0 {
        bytes.push(b'0' + carry);
    }
    bytes.reverse();
    let value = String::from_utf8(bytes).expect("decimal digits");
    if value.len() < 2 || (value.len() == 2 && value.as_str() < "20") {
        "20".into()
    } else {
        value
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyAtom {
    Null,
    Text(String),
    Integer(String),
    Float(u64),
    Nan(u64),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct QueryKey {
    query: String,
    limit: String,
    harness: KeyAtom,
    project: KeyAtom,
    since: KeyAtom,
}
struct CacheEntry {
    key: QueryKey,
    until: f64,
    rows: Vec<Hit>,
}

#[derive(Clone, Debug)]
pub struct RecallInput {
    pub query: QueryInput,
    pub limit: QueryInput,
    pub harness: QueryInput,
    pub project: QueryInput,
    pub since: QueryInput,
    pub max_chars: usize,
}
impl RecallInput {
    pub fn typed(query: &str, limit: usize, filters: &Filters, max_chars: usize) -> Self {
        Self {
            query: Value::String(query.into()).into(),
            limit: Value::from(limit).into(),
            harness: filters
                .harness
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null)
                .into(),
            project: filters
                .project
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null)
                .into(),
            since: filters
                .since
                .map(QueryInput::Float)
                .unwrap_or(QueryInput::Json(Value::Null)),
            max_chars,
        }
    }
}
/// Independent clocks: Python time.time controls cache expiry; perf_counter measures latency.
pub trait MemoryClock: Send + Sync {
    fn wall(&self) -> f64;
    fn performance(&self) -> f64;
}
struct SystemClock(Instant);
impl MemoryClock for SystemClock {
    fn wall(&self) -> f64 {
        crate::clock::now_s()
    }
    fn performance(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}
pub struct MemoryService {
    path: PathBuf,
    pool: Arc<ReadPool>,
    queries: Arc<QueryStats>,
    cache: Mutex<VecDeque<CacheEntry>>,
    clock: Arc<dyn MemoryClock>,
    nonce: AtomicU64,
    writer: Arc<MemoryWriter>,
    ingestor: Ingestor,
    stopping: AtomicBool,
    lifecycle: Mutex<()>,
}
impl MemoryService {
    pub fn open(
        path: impl AsRef<Path>,
        readers: usize,
        poll: Duration,
        throttle: Duration,
    ) -> Result<Self, DunkError> {
        Self::with_clock(
            path,
            readers,
            poll,
            throttle,
            Arc::new(SystemClock(Instant::now())),
        )
    }
    pub fn with_clock(
        path: impl AsRef<Path>,
        readers: usize,
        poll: Duration,
        throttle: Duration,
        clock: Arc<dyn MemoryClock>,
    ) -> Result<Self, DunkError> {
        let path = path.as_ref().to_owned();
        if !path.is_file() {
            return Err(DunkError::Refused(
                "memory service is not enabled; start the daemon with persistence".into(),
            ));
        }
        Self::create_with_clock(path, readers, poll, throttle.into(), clock)
    }

    /// Startup-only first-run creation, using the same writer for schema and ingestion.
    /// Unlike `open`, this creates missing parent directories and the database.
    pub fn create_at_start(
        path: impl AsRef<Path>,
        readers: usize,
        poll: Duration,
        throttle: impl Into<Throttle>,
    ) -> Result<Self, DunkError> {
        Self::create_with_clock(
            path.as_ref().to_owned(),
            readers,
            poll,
            throttle.into(),
            Arc::new(SystemClock(Instant::now())),
        )
    }

    fn create_with_clock(
        path: PathBuf,
        readers: usize,
        poll: Duration,
        throttle: Throttle,
        clock: Arc<dyn MemoryClock>,
    ) -> Result<Self, DunkError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(DunkError::Refused(
                "memory startup requires a non-Tokio standard thread".into(),
            ));
        }
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(DunkError::Refused(
                "memory startup requires an explicit file path".into(),
            ));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        // Canonicalize the existing parent before first creation; do not open a second writer.
        let path =
            if path.exists() {
                std::fs::canonicalize(path)?
            } else {
                std::fs::canonicalize(parent)?.join(path.file_name().ok_or_else(|| {
                    DunkError::Refused("memory startup requires a file name".into())
                })?)
            };
        let writer = Arc::new(MemoryWriter::new(&path));
        writer.call(|connection| ingest::apply_full_schema(connection))?;
        let pool = Arc::new(ReadPool::open(&path, readers)?);
        let ingestor = Ingestor::with_writer(Arc::clone(&writer), poll, throttle);
        Ok(Self {
            path,
            pool,
            writer,
            ingestor,
            clock,
            queries: Arc::new(QueryStats::default()),
            cache: Mutex::new(VecDeque::new()),
            nonce: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            lifecycle: Mutex::new(()),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn read_pool(&self) -> &Arc<ReadPool> {
        &self.pool
    }
    pub fn query_stats(&self) -> &Arc<QueryStats> {
        &self.queries
    }
    fn accepting(&self) -> Result<(), DunkError> {
        if self.stopping.load(Ordering::Acquire) {
            Err(DunkError::Unavailable("memory service is stopping".into()))
        } else {
            Ok(())
        }
    }
    pub fn recall(&self, input: &RecallInput) -> Result<Vec<Hit>, DunkError> {
        self.accepting()?;
        let started = self.clock.performance();
        let query = match &input.query {
            QueryInput::Json(Value::String(value)) => value.as_str(),
            value if !value.truthy() => "",
            _ => {
                return Err(DunkError::Refused(
                    "expected string or bytes-like object".into(),
                ))
            }
        };
        if super::pure::fts_query(query, "AND").is_none() {
            self.queries.record(0.0, false, false);
            return Ok(Vec::new());
        }
        let limit = input.limit.integer()?;
        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed);
        let key = QueryKey {
            query: query.into(),
            limit: limit.clone(),
            harness: input.harness.atom(nonce)?,
            project: input.project.atom(nonce)?,
            since: input.since.atom(nonce)?,
        };
        {
            let mut cache = self.cache.lock().expect("memory cache lock");
            if let Some(index) = cache
                .iter()
                .position(|entry| entry.key == key && entry.until >= self.clock.wall())
            {
                let entry = cache.remove(index).expect("cache index");
                let rows = entry.rows.clone();
                cache.push_back(entry);
                self.queries
                    .record((self.clock.performance() - started) * 1000.0, false, true);
                return Ok(rows);
            }
        }
        // No cache/stats lock is held during reads and no single-flight is imposed.
        let result = (|| {
            let sql_limit = fetch_decimal(&limit);
            let filters = recall::SqlFilters {
                harness: input.harness.sql_scalar()?,
                project: input.project.sql_scalar()?,
                since: if input.since.truthy() {
                    Some(recall::SqlScalar::Value(rusqlite::types::Value::Real(
                        input.since.float()?,
                    )))
                } else {
                    None
                },
            };
            // A huge integer is refused by fetch after read-pool acquisition, not here.
            let final_limit = self.pool.with(|_| {
                sql_limit.parse::<i64>().map_err(|_| sqlite_overflow())?;
                if limit.starts_with('-') || limit == "0" {
                    Ok(1)
                } else {
                    limit.parse::<usize>().map_err(|_| sqlite_overflow())
                }
            })?;
            recall::recall_sql(
                &self.pool,
                query,
                final_limit,
                &filters,
                self.clock.wall(),
                input.max_chars,
                &sql_limit,
            )
        })();
        match result {
            Ok(rows) => {
                let mut cache = self.cache.lock().expect("memory cache lock");
                let entry = CacheEntry {
                    key,
                    until: self.clock.wall() + 15.0,
                    rows: rows.clone(),
                };
                if let Some(index) = cache.iter().position(|old| old.key == entry.key) {
                    // OrderedDict assignment preserves an expired entry's position.
                    cache[index] = entry;
                } else {
                    cache.push_back(entry);
                }
                while cache.len() > 1024 {
                    cache.pop_front();
                }
                drop(cache);
                self.queries
                    .record((self.clock.performance() - started) * 1000.0, false, false);
                Ok(rows)
            }
            Err(error) => {
                self.queries
                    .record((self.clock.performance() - started) * 1000.0, true, false);
                Err(error)
            }
        }
    }
    pub fn recall_typed(
        &self,
        query: &str,
        limit: usize,
        filters: &Filters,
        max_chars: usize,
    ) -> Result<Vec<Hit>, DunkError> {
        self.recall(&RecallInput::typed(query, limit, filters, max_chars))
    }
    pub fn context(&self, query: &str, sessions: i64, snippets: usize) -> Result<Value, DunkError> {
        let limit = (i128::from(sessions) * 4).max(20).to_string();
        let mut input = RecallInput::typed(query, 20, &Filters::default(), 240);
        input.limit = Value::String(limit).into();
        let rows = self.recall(&input)?;
        Ok(queries::context_from_hits(
            query,
            sessions,
            snippets,
            self.clock.wall(),
            rows,
        ))
    }
    pub fn stats(&self) -> Result<Value, DunkError> {
        queries::stats(&self.pool, &self.path, &self.queries)
    }
    pub fn status(&self) -> Result<Value, DunkError> {
        let mut value = queries::status(&self.pool, &self.path, &self.queries)?;
        let status = self.ingestor.status();
        let mut ingestor = serde_json::json!({"running":status.running,"paused":status.paused,"last_error":status.last_error,"last_result":status.last_result});
        // Direct insertion: json! re-serialization cannot carry the codec's raw NaN Number.
        ingestor.as_object_mut().expect("status object").insert(
            "throttle_s".into(),
            crate::wire_json::float_value(status.throttle_s),
        );
        value["ingestor"] = ingestor;
        Ok(value)
    }

    pub fn queue_disposition(
        &self,
        id: i64,
        path: &str,
        reason: &str,
        actor: &str,
        restore: bool,
    ) -> Result<(), DunkError> {
        self.accepting()?;
        let (path, reason, actor) = (path.to_owned(), reason.to_owned(), actor.to_owned());
        self.writer.call(move |connection| {
            ingest::queue_disposition(connection, id, &path, &reason, &actor, restore)
        })
    }
    pub fn start(&self) -> Result<(), DunkError> {
        self.while_accepting(|| self.ingestor.start())?
    }
    /// Linearize service attachment/start against final stop without retaining a Flock lock.
    pub(crate) fn while_accepting<T>(&self, operation: impl FnOnce() -> T) -> Result<T, DunkError> {
        let _lifecycle = self.lifecycle.lock().expect("memory lifecycle lock");
        self.accepting()?;
        Ok(operation())
    }
    #[cfg(test)]
    pub(crate) fn set_parse_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.ingestor.set_parse_hook(hook);
    }
    #[cfg(test)]
    pub(crate) fn worker_id(&self) -> Option<std::thread::ThreadId> {
        self.ingestor.worker_id()
    }
    pub fn pause(&self) {
        self.ingestor.pause();
    }
    pub fn resume(&self) {
        self.ingestor.resume();
    }
    pub fn enqueue(
        &self,
        path: &str,
        harness: Option<&str>,
        project: Option<&str>,
    ) -> Result<(i64, i64), DunkError> {
        self.accepting()?;
        self.ingestor.enqueue(path, harness, project)
    }
    /// Enqueue and return the exact normalized source metadata committed with the receipt.
    pub fn enqueue_receipt(
        &self,
        path: &str,
        harness: Option<&str>,
        project: Option<&str>,
    ) -> Result<ingest::EnqueueReceipt, DunkError> {
        self.accepting()?;
        self.ingestor.enqueue_receipt(path, harness, project)
    }
    /// Python backfill catches only a vanished-source ValueError. Database and
    /// worker failures propagate, using the same owning writer as every enqueue.
    pub fn enqueue_discovered(&self, path: &str) -> Result<bool, DunkError> {
        self.accepting()?;
        if !std::path::Path::new(path).exists() {
            return Ok(false);
        }
        // The subsequent stat can fail independently of exists; Python propagates it.
        let metadata = std::fs::metadata(path)?;
        use std::os::unix::fs::MetadataExt;
        let source = ingest::PreparedSource {
            path: path.to_owned(),
            harness: ingest::harness_from_path(path).to_owned(),
            project: None,
            mtime: metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9,
            size: metadata.len() as i64,
            now: crate::clock::now_s(),
        };
        self.writer
            .call(move |connection| ingest::enqueue_prepared(connection, source))?;
        Ok(true)
    }

    pub fn paused(&self) -> bool {
        self.ingestor.paused()
    }

    pub fn backfill(
        &self,
        sources: &[(String, Option<String>, Option<String>)],
    ) -> Result<Vec<(i64, i64)>, DunkError> {
        sources
            .iter()
            .map(|(path, harness, project)| {
                self.enqueue(path, harness.as_deref(), project.as_deref())
            })
            .collect()
    }
    pub fn recover_leases(&self) -> Result<usize, DunkError> {
        self.accepting()?;
        self.writer.call(ingest::recover_leases)
    }
    pub fn requeue_partial(&self) -> Result<usize, DunkError> {
        self.accepting()?;
        self.writer.call(ingest::requeue_partial)
    }
    pub fn checkpoint(&self) -> Result<(i64, i64, i64), DunkError> {
        self.accepting()?;
        self.writer
            .call(|connection| ingest::checkpoint(connection))
    }
    /// Python-style worker stop. A busy worker retains its join handle and writer;
    /// a completed stop may be followed by start. Final store close is explicit.
    pub fn stop(&self) -> Result<bool, DunkError> {
        let _lifecycle = self.lifecycle.lock().expect("memory lifecycle lock");
        Ok(self.ingestor.stop_with_timeout(Duration::from_secs(5)))
    }
    /// Final teardown deliberately waits beyond Python's five-second stop bound when busy:
    /// never close/drop the writer while a parser still holds it.
    pub fn stop_and_join(&self) -> Result<(), DunkError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(DunkError::Refused(
                "memory shutdown requires a non-Tokio standard thread".into(),
            ));
        }
        let _lifecycle = self.lifecycle.lock().expect("memory lifecycle lock");
        self.stopping.store(true, Ordering::Release);
        self.ingestor.stop();
        self.writer.close()
    }
}
impl Drop for MemoryService {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

// Unicode decimal digit table reused from the existing API conversion primitive.
pub(crate) fn python_decimal_digit(character: char) -> Option<u8> {
    // Unicode 16.0 Decimal_Number ranges, matching the Python 3.14 runtime used by this port.
    // The two multi-set ranges are mathematical styled digits and segmented digits.
    const DECIMAL_RANGES: &[(u32, u32)] = &[
        (0x0030, 0x0039),
        (0x0660, 0x0669),
        (0x06F0, 0x06F9),
        (0x07C0, 0x07C9),
        (0x0966, 0x096F),
        (0x09E6, 0x09EF),
        (0x0A66, 0x0A6F),
        (0x0AE6, 0x0AEF),
        (0x0B66, 0x0B6F),
        (0x0BE6, 0x0BEF),
        (0x0C66, 0x0C6F),
        (0x0CE6, 0x0CEF),
        (0x0D66, 0x0D6F),
        (0x0DE6, 0x0DEF),
        (0x0E50, 0x0E59),
        (0x0ED0, 0x0ED9),
        (0x0F20, 0x0F29),
        (0x1040, 0x1049),
        (0x1090, 0x1099),
        (0x17E0, 0x17E9),
        (0x1810, 0x1819),
        (0x1946, 0x194F),
        (0x19D0, 0x19D9),
        (0x1A80, 0x1A89),
        (0x1A90, 0x1A99),
        (0x1B50, 0x1B59),
        (0x1BB0, 0x1BB9),
        (0x1C40, 0x1C49),
        (0x1C50, 0x1C59),
        (0xA620, 0xA629),
        (0xA8D0, 0xA8D9),
        (0xA900, 0xA909),
        (0xA9D0, 0xA9D9),
        (0xA9F0, 0xA9F9),
        (0xAA50, 0xAA59),
        (0xABF0, 0xABF9),
        (0xFF10, 0xFF19),
        (0x104A0, 0x104A9),
        (0x10D30, 0x10D39),
        (0x10D40, 0x10D49),
        (0x11066, 0x1106F),
        (0x110F0, 0x110F9),
        (0x11136, 0x1113F),
        (0x111D0, 0x111D9),
        (0x112F0, 0x112F9),
        (0x11450, 0x11459),
        (0x114D0, 0x114D9),
        (0x11650, 0x11659),
        (0x116C0, 0x116C9),
        (0x116D0, 0x116E3),
        (0x11730, 0x11739),
        (0x118E0, 0x118E9),
        (0x11950, 0x11959),
        (0x11BF0, 0x11BF9),
        (0x11C50, 0x11C59),
        (0x11D50, 0x11D59),
        (0x11DA0, 0x11DA9),
        (0x11F50, 0x11F59),
        (0x16130, 0x16139),
        (0x16A60, 0x16A69),
        (0x16AC0, 0x16AC9),
        (0x16B50, 0x16B59),
        (0x16D70, 0x16D79),
        (0x1CCF0, 0x1CCF9),
        (0x1D7CE, 0x1D7FF),
        (0x1E140, 0x1E149),
        (0x1E2F0, 0x1E2F9),
        (0x1E4F0, 0x1E4F9),
        (0x1E5F1, 0x1E5FA),
        (0x1E950, 0x1E959),
        (0x1FBF0, 0x1FBF9),
    ];

    let codepoint = character as u32;
    DECIMAL_RANGES.iter().find_map(|&(start, end)| {
        if (start..=end).contains(&codepoint) {
            Some(((codepoint - start) % 10) as u8)
        } else {
            None
        }
    })
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
