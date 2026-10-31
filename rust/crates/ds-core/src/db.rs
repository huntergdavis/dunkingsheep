//! `db`: one writer actor and a read pool per database (RUST_PORT.md 5.2).
//!
//! Ported from `dunk_sqlite.py` plus the lock discipline the three Python stores
//! keep by hand (`with self._lock: BEGIN IMMEDIATE … commit`). Two rules, and they
//! are the whole module:
//!
//! * **One writer per database.** A tokio task owns the only writable
//!   `Connection`; callers send it a closure and await a oneshot reply. A
//!   transaction therefore cannot interleave with another writer, and a caller
//!   waiting on SQLite does not hold a tokio worker thread.
//! * **Readers never wait on a writer.** WAL means a read sees the last committed
//!   snapshot while a write transaction is open, so the pool's connections are
//!   opened `query_only` and are never handed to the writer.
//!
//! The Python's connection rules are kept exactly: `busy_timeout 5000`, WAL for
//! owned stores only (never change the journal mode of a database we merely read),
//! `query_only` for readers, and a filesystem path is required - a hand-built
//! `file:` URI is refused, because `Path::as_uri()` is what escapes spaces and
//! `?`/`#` correctly and `readonly` must not be able to create a missing database.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::error::DunkError;

/// How many times opening a writer retries the WAL switch on a busy database.
///
/// Bounded on purpose: a genuinely locked database must still fail, and a caller must
/// not wait for ever on an open (t1006).
const WAL_OPEN_ATTEMPTS: u32 = 5;
/// The first backoff for that retry, doubled per attempt (5, 10, 20, 40 ms).
const WAL_OPEN_BACKOFF_MS: u64 = 5;

/// Is this refusal the retryable busy/locked one?
fn error_is_busy(error: &DunkError) -> bool {
    matches!(error, DunkError::Busy)
}

/// The Python's `BUSY_TIMEOUT_MS` (`dunk_sqlite.py`).
pub const BUSY_TIMEOUT_MS: u64 = 5000;

/// How many read-only connections a pool keeps by default (RUST_PORT.md 5.2: N).
pub const DEFAULT_READ_POOL: usize = 4;

/// The one place SQLite errors become [`DunkError`].
///
/// Busy/locked is its own variant because the caller may retry and the wire
/// protocol says `retryable: true`; everything else is a refusal carrying SQLite's
/// own sentence, which the Python also surfaces verbatim.
pub fn map_sqlite(error: rusqlite::Error) -> DunkError {
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        if inner.code == rusqlite::ErrorCode::DatabaseBusy
            || inner.code == rusqlite::ErrorCode::DatabaseLocked
        {
            return DunkError::Busy;
        }
    }
    DunkError::Refused(error.to_string())
}

/// Refuse a hand-built URI, exactly as `dunk_sqlite.connect` does.
///
/// A caller that passes `file:…?mode=ro` has already decided the flags behind the
/// API's back; accepting it is how a "read-only" connection ends up writable.
fn checked_path(path: &Path) -> Result<PathBuf, DunkError> {
    let text = path.to_string_lossy();
    if text.starts_with("file:") || text.contains("?mode=ro") || text.contains("?mode=rw") {
        return Err(DunkError::Refused(
            "SQLite connection requires a filesystem path; use readonly=True for reads".to_string(),
        ));
    }
    Ok(path.to_path_buf())
}

/// A writable connection with the Python's pragmas: busy timeout, then WAL.
pub fn open_writer(path: &Path) -> Result<Connection, DunkError> {
    open_writer_with_timeout(path, BUSY_TIMEOUT_MS)
}

/// [`open_writer`] with an explicit busy timeout.
///
/// The knob exists for tests: proving that a locked database maps to
/// [`DunkError::Busy`] should take milliseconds, not the production five seconds,
/// and a test that waits out the real timeout hides a hang in the busy path.
pub fn open_writer_with_timeout(path: &Path, busy_ms: u64) -> Result<Connection, DunkError> {
    let path = checked_path(path)?;
    let connection = Connection::open(&path).map_err(map_sqlite)?;
    connection
        .busy_timeout(std::time::Duration::from_millis(busy_ms))
        .map_err(map_sqlite)?;
    // An in-memory database has no journal file to switch.
    //
    // Switching to WAL needs a lock the OTHER writer may hold at this instant, so
    // `PRAGMA journal_mode = WAL` can answer SQLITE_BUSY even though the busy timeout
    // is already set (the timeout covers statements, not this pragma's exclusive
    // moment). Measured: `two_writers_serialize_their_transactions` failed 1 run in 20
    // with `writer: Busy` here, in a place nothing in the test could influence - and
    // two daemons (or a timer process starting while the daemon writes) hit exactly
    // this. The retry module T02 landed exists for this sentence: a busy answer is
    // RETRYABLE, and failing the open instead is the bug.
    if path.to_string_lossy() != ":memory:" {
        let mut last = None;
        for attempt in 0..WAL_OPEN_ATTEMPTS {
            match connection.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(error) => {
                    let mapped = map_sqlite(error);
                    if error_is_busy(&mapped) && attempt + 1 < WAL_OPEN_ATTEMPTS {
                        std::thread::sleep(std::time::Duration::from_millis(
                            WAL_OPEN_BACKOFF_MS << attempt.min(4),
                        ));
                        last = Some(mapped);
                        continue;
                    }
                    last = Some(mapped);
                    break;
                }
            }
        }
        if let Some(error) = last {
            return Err(error);
        }
    }
    Ok(connection)
}

/// A read-only connection: never creates the file, never writes, never changes the
/// journal mode of a database that belongs to someone else.
pub fn open_reader(path: &Path) -> Result<Connection, DunkError> {
    let path = checked_path(path)?;
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(map_sqlite)?;
    connection
        .busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(map_sqlite)?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(map_sqlite)?;
    Ok(connection)
}

/// One write job: a closure the actor runs with the connection it owns.
type Job = Box<dyn FnOnce(&mut Connection) -> Result<Value, DunkError> + Send + 'static>;

/// The single writer of one database (RUST_PORT.md 5.2).
///
/// `call` sends a closure and awaits its reply, so a caller's transaction is the
/// actor's next job: never interleaved, never on a worker thread.
pub struct Writer {
    path: PathBuf,
    /// `Option` so `shutdown` can DROP the sender before awaiting the actor: the
    /// actor's loop ends when the channel closes, so awaiting it while still holding
    /// a sender is a deadlock (found by the two-writer test hanging, not spinning).
    jobs: Mutex<Option<tokio::sync::mpsc::Sender<Job>>>,
    actor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Writer {
    /// Start the actor. The connection is opened HERE, on the caller's thread, so a
    /// bad path or a busy open is reported to the caller rather than lost in a task.
    pub fn spawn(path: impl AsRef<Path>) -> Result<Self, DunkError> {
        let path = checked_path(path.as_ref())?;
        let mut connection = open_writer(&path)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Job>(64);
        let actor = tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                // A job that panics must not take the writer with it: the panic is
                // caught, the connection is still valid, and the next job runs.
                let _ =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut connection)));
            }
        });
        Ok(Writer {
            path,
            jobs: Mutex::new(Some(tx)),
            actor: Mutex::new(Some(actor)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `work` on the writer's connection and return its result.
    pub async fn call<T, F>(&self, work: F) -> Result<T, DunkError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, DunkError> + Send + 'static,
    {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |connection| {
            let result = work(connection);
            // The caller may have gone away; that is not the writer's problem.
            let _ = reply.send(result);
            Ok(Value::Null)
        });
        let sender = {
            self.jobs
                .lock()
                .expect("writer sender lock")
                .clone()
                .ok_or_else(|| {
                    DunkError::Unavailable("the writer has been shut down".to_string())
                })?
        };
        sender
            .send(job)
            .await
            .map_err(|_| DunkError::Unavailable("the writer actor has stopped".to_string()))?;
        answer
            .await
            .map_err(|_| DunkError::Unavailable("the writer dropped the reply".to_string()))?
    }

    /// Stop the actor and wait for it, closing the connection.
    ///
    /// The sender is dropped FIRST: the actor's loop ends when the channel closes,
    /// so awaiting the task while a sender is alive waits forever.
    pub async fn shutdown(&self) {
        drop(self.jobs.lock().expect("writer sender lock").take());
        let actor = { self.actor.lock().expect("writer actor lock").take() };
        if let Some(actor) = actor {
            let _ = actor.await;
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.actor.lock() {
            if let Some(actor) = slot.take() {
                actor.abort();
            }
        }
    }
}

/// A SYNCHRONOUS handle over the async writer actor (t978).
///
/// `Writer::call` is `async`, but every ported store method is a plain blocking call -
/// that is the Python's shape, and keeping the signatures identical is the port's
/// premise. This handle owns a current-thread runtime, starts the actor inside it, and
/// `block_on`s each call, so a store reads exactly like the Python's:
///
/// ```ignore
/// let writer = SyncWriter::spawn(path)?;
/// writer.call(|connection| {
///     connection.execute("INSERT INTO events (at, kind) VALUES (?1, ?2)", (1.0, "x"))?;
///     Ok(())
/// })?;
/// ```
///
/// WHY THIS SHAPE (the t978 decision): the alternatives were a runtime per store (the
/// same thing with the runtime visible in every store), or making the stores async
/// (which changes every caller signature including the CLI's synchronous paths). This
/// keeps the async surface where it already is and confines the runtime to one place.
///
/// Callers must NOT call this from inside a tokio runtime: `block_on` from within a
/// runtime panics. The store layer runs on the daemon's synchronous command handlers,
/// which is where the Python's blocking methods already ran.
pub struct SyncWriter {
    writer: std::sync::Arc<Writer>,
    runtime: tokio::runtime::Runtime,
}

impl std::fmt::Debug for SyncWriter {
    /// The writer actor holds a channel and a task handle, neither of which is
    /// useful in a debug line; the path is.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyncWriter")
            .field("path", &self.path())
            .finish()
    }
}

impl SyncWriter {
    /// Open the database and start the writer actor inside a fresh runtime.
    pub fn spawn(path: impl AsRef<Path>) -> Result<Self, DunkError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| DunkError::Refused(format!("tokio runtime: {error}")))?;
        let writer = {
            let _entered = runtime.enter(); // the actor task lives in this runtime
            Writer::spawn(path)?
        };
        Ok(SyncWriter {
            writer: std::sync::Arc::new(writer),
            runtime,
        })
    }

    pub fn path(&self) -> &Path {
        self.writer.path()
    }

    /// Run one write on the writer's connection, blocking until it is done.
    pub fn call<T, F>(&self, work: F) -> Result<T, DunkError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, DunkError> + Send + 'static,
    {
        self.runtime.block_on(self.writer.call(work))
    }

    /// Drain the actor and stop it.
    pub fn shutdown(self) {
        let SyncWriter { writer, runtime } = self;
        runtime.block_on(writer.shutdown());
    }
}

/// A small pool of read-only connections (RUST_PORT.md 5.2).
///
/// Reads are synchronous in `rusqlite`, so the pool is a `Mutex<Vec<Connection>>`:
/// a read borrows one connection for the duration of the closure and returns it.
/// WAL is why this never waits for the writer.
pub struct ReadPool {
    path: PathBuf,
    size: usize,
    connections: Mutex<Vec<Connection>>,
    available: Condvar,
}

/// Puts a borrowed connection back (and wakes a waiter) even if the read panics.
struct Borrowed<'a> {
    pool: &'a ReadPool,
    connection: Option<Connection>,
}

impl Drop for Borrowed<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Ok(mut idle) = self.pool.connections.lock() {
                idle.push(connection);
                self.pool.available.notify_one();
            }
        }
    }
}

impl ReadPool {
    pub fn open(path: impl AsRef<Path>, size: usize) -> Result<Self, DunkError> {
        let path = checked_path(path.as_ref())?;
        let size = size.max(1);
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            connections.push(open_reader(&path)?);
        }
        Ok(ReadPool {
            path,
            size,
            connections: Mutex::new(connections),
            available: Condvar::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Borrow one connection for a read.
    pub fn with<T, F>(&self, read: F) -> Result<T, DunkError>
    where
        F: FnOnce(&Connection) -> Result<T, DunkError>,
    {
        // t1241: the pool used to hold its mutex across the whole read and always use the
        // first connection, so N threads ran one at a time and p95 grew with N (measured
        // under load: 8 readers, p95 350-910 ms, max 2.1 s). A read now takes a connection
        // OUT of the pool, releases the lock, reads, and puts it back; callers wait only
        // when all `size` connections are in use.
        let connection = {
            let mut idle = self.connections.lock().expect("read pool lock");
            loop {
                if let Some(connection) = idle.pop() {
                    break connection;
                }
                if self.size == 0 {
                    return Err(DunkError::Unavailable("the read pool is empty".to_string()));
                }
                idle = self.available.wait(idle).expect("read pool lock");
            }
        };
        let guard = Borrowed {
            pool: self,
            connection: Some(connection),
        };
        match guard.connection.as_ref() {
            Some(connection) => read(connection),
            None => Err(DunkError::Unavailable("the read pool is empty".to_string())),
        }
    }
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod db_tests;
