//! The daemon log: `dunk_server.configure_logging` for the Rust daemon.
//!
//! One [`DaemonLog`] per daemon owns the sink: the `daemon.log` file under the config directory
//! plus stderr when it is a terminal, or stderr alone with `--no-log-file` or an unwritable
//! file. Lines follow the Python `%(asctime)s %(levelname)s %(name)s: %(message)s` format, and
//! every `tracing` event at INFO and above becomes such a line.
//!
//! ROUTED, NOT FANNED OUT: each thread carries the sink of the daemon that spawned it, and an
//! event on a thread that carries none goes nowhere. The daemon thread adopts its sink in
//! [`DaemonLog::configure`]; every thread the daemon's work spawns inherits it through
//! [`ThreadLog::capture`] and [`ThreadLog::adopt`]: the connection workers, signal waiters and
//! parent watcher (server.rs), the tokio runtime's threads (`on_thread_start`), and the outbox,
//! dunk timer and manual fire workers. A server that is not a daemon (a fixture in this crate's
//! tests) logs nowhere, so one daemon's lines never land in another daemon's file. The crate's
//! remaining thread spawns (store actors, the memory ingestor, the reaper's probes, the pid
//! probe) emit no `tracing` events.
//!
//! DIFFERENCE: the file is created mode 0600 (the Python `FileHandler` uses the umask).

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) const SERVER_LOGGER: &str = "dunkingsheep.server";
pub(crate) const CORE_LOGGER: &str = "dunkingsheep";

struct LogSink {
    file: Option<Mutex<std::fs::File>>,
    stderr: bool,
}

impl LogSink {
    fn write(&self, level: &str, name: &str, message: &str) {
        let line = format_log_line(chrono::Local::now(), level, name, message);
        if let Some(file) = &self.file {
            let _ = file
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .write_all(line.as_bytes());
        }
        if self.stderr {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

thread_local! {
    static THREAD_LOG: RefCell<Option<Arc<LogSink>>> = const { RefCell::new(None) };
}

static BRIDGE: std::sync::Once = std::sync::Once::new();

fn sink_for_current_thread() -> Option<Arc<LogSink>> {
    THREAD_LOG.with(|slot| slot.borrow().clone())
}

/// The daemon sink the calling thread logs to, cloneable into a thread it is about to spawn.
#[derive(Clone)]
pub struct ThreadLog(Option<Arc<LogSink>>);

impl ThreadLog {
    /// The calling thread's sink (none outside a daemon).
    pub fn capture() -> Self {
        Self(sink_for_current_thread())
    }

    /// Make the calling thread log to the captured daemon's sink.
    pub fn adopt(&self) {
        THREAD_LOG.with(|slot| *slot.borrow_mut() = self.0.clone());
    }
}

/// One daemon's log: configured first thing on the daemon thread, dropped last.
pub struct DaemonLog {
    sink: Arc<LogSink>,
}

impl DaemonLog {
    pub fn configure(config_dir: &Path, no_log_file: bool) -> Self {
        use std::io::IsTerminal;
        use std::os::unix::fs::OpenOptionsExt;
        let file = if no_log_file {
            None
        } else {
            std::fs::create_dir_all(config_dir)
                .and_then(|()| {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .mode(0o600)
                        .open(config_dir.join("daemon.log"))
                })
                .ok()
                .map(Mutex::new)
        };
        let stderr = file.is_none() || std::io::stderr().is_terminal();
        let sink = Arc::new(LogSink { file, stderr });
        BRIDGE.call_once(|| {
            // A process that already chose a subscriber keeps it; the daemon's events then go
            // where that subscriber sends them.
            let _ = tracing::subscriber::set_global_default(TracingBridge);
        });
        let log = Self { sink };
        log.thread_log().adopt();
        log
    }

    /// `log.error(...)` on the daemon's own logger.
    pub fn error(&self, message: &str) {
        self.sink.write("ERROR", SERVER_LOGGER, message);
    }

    /// This daemon's sink, for the threads and runtimes the daemon starts itself.
    pub fn thread_log(&self) -> ThreadLog {
        ThreadLog(Some(Arc::clone(&self.sink)))
    }
}

impl Drop for DaemonLog {
    /// The daemon thread stops logging with its daemon; threads that inherited the sink keep
    /// their own clone until they exit, which the daemon's joins order before this drop.
    fn drop(&mut self) {
        THREAD_LOG.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|sink| Arc::ptr_eq(sink, &self.sink))
            {
                *slot = None;
            }
        });
    }
}

/// One Python logging line with the default `asctime` (local time, milliseconds after a comma).
fn format_log_line(
    at: chrono::DateTime<chrono::Local>,
    level: &str,
    name: &str,
    message: &str,
) -> String {
    format!(
        "{} {level} {name}: {message}\n",
        at.format("%Y-%m-%d %H:%M:%S,%3f")
    )
}

/// Every `tracing` event is one log line: WARN is Python's WARNING; events from the server and
/// the daemon assembly (`dunk_server.py`) carry the `dunkingsheep.server` logger name and the
/// rest of the port (`dunk_core.py` and its modules) the `dunkingsheep` name.
struct TracingBridge;

impl tracing::Subscriber for TracingBridge {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_event() && *metadata.level() <= tracing::Level::INFO
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let Some(sink) = sink_for_current_thread() else {
            return;
        };
        let mut message = EventMessage::default();
        event.record(&mut message);
        let message = message.finish();
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARNING",
            _ => "INFO",
        };
        let target = event.metadata().target();
        let name = if target == "ds_core::server"
            || target.starts_with("ds_core::server_runtime::daemon")
        {
            SERVER_LOGGER
        } else {
            CORE_LOGGER
        };
        sink.write(level, name, &message.text);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// The event's `message` field first, then any other fields as `name=value`.
#[derive(Default)]
struct EventMessage {
    text: String,
    fields: String,
}

impl EventMessage {
    fn finish(mut self) -> Self {
        if !self.fields.is_empty() {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(&self.fields);
            self.fields.clear();
        }
        self
    }
}

impl tracing::field::Visit for EventMessage {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.text = format!("{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            self.fields.push_str(&format!("{}={value:?}", field.name()));
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS,mmm ` as logging's default `asctime` prints it.
#[cfg(test)]
pub(crate) fn has_python_timestamp(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() > 24
        && bytes[23] == b' '
        && bytes[..23]
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                4 | 7 => *byte == b'-',
                10 => *byte == b' ',
                13 | 16 => *byte == b':',
                19 => *byte == b',',
                _ => byte.is_ascii_digit(),
            })
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    #[test]
    fn log_lines_use_the_python_logging_format() {
        use chrono::TimeZone;
        let at = chrono::Local
            .with_ymd_and_hms(2026, 11, 6, 11, 16, 31)
            .single()
            .expect("unambiguous local time")
            + chrono::Duration::milliseconds(26);
        assert_eq!(
            "2026-11-06 11:16:31,026 INFO dunkingsheep.server: dunkingsheep 2.71.4 listening on /tmp/s.sock (pid 7)\n",
            format_log_line(
                at,
                "INFO",
                SERVER_LOGGER,
                "dunkingsheep 2.71.4 listening on /tmp/s.sock (pid 7)"
            )
        );
    }

    /// Two daemons in one process, alive at the same time: each file holds only the lines of
    /// its own daemon's threads (the daemon thread, a thread it spawned, its own refusals), and
    /// a thread neither daemon adopted reaches no file.
    #[test]
    fn events_reach_only_the_emitting_threads_daemon() {
        let dir = crate::testsupport::dir("daemon-log-routing");
        let gate = Arc::new(Barrier::new(3));
        let mut daemons = Vec::new();
        for name in ["a", "b"] {
            let config = dir.path().join(name);
            let gate = Arc::clone(&gate);
            daemons.push(std::thread::spawn(move || {
                let log = DaemonLog::configure(&config, false);
                gate.wait();
                tracing::warn!("daemon {name} itself");
                let inherited = ThreadLog::capture();
                std::thread::spawn(move || {
                    inherited.adopt();
                    tracing::info!("daemon {name} child");
                })
                .join()
                .expect("child thread");
                log.error(&format!("daemon {name} refusal"));
                gate.wait();
                drop(log);
            }));
        }
        gate.wait();
        tracing::info!("nobody's thread");
        gate.wait();
        for daemon in daemons {
            daemon.join().expect("daemon thread");
        }
        for (name, other) in [("a", "b"), ("b", "a")] {
            let text = std::fs::read_to_string(dir.path().join(name).join("daemon.log"))
                .expect("daemon.log");
            let lines: Vec<&str> = text.lines().collect();
            assert!(
                lines.iter().all(|line| has_python_timestamp(line)),
                "{text}"
            );
            let suffixes: Vec<String> = lines.iter().map(|line| line[24..].to_owned()).collect();
            assert_eq!(
                vec![
                    format!("WARNING dunkingsheep: daemon {name} itself"),
                    format!("INFO dunkingsheep: daemon {name} child"),
                    format!("ERROR dunkingsheep.server: daemon {name} refusal"),
                ],
                suffixes,
                "daemon {name} file holds exactly its own lines, none of {other}: {text}"
            );
        }
    }

    /// A thread that did not inherit a sink logs nowhere, even while exactly one daemon is
    /// configured: a server that is not a daemon must not borrow a daemon's file. After the
    /// daemon's log is dropped its own thread logs nowhere too.
    #[test]
    fn a_thread_no_daemon_adopted_logs_nowhere() {
        let dir = crate::testsupport::dir("daemon-log-unadopted");
        let config = dir.path().join("config");
        let log = DaemonLog::configure(&config, false);
        std::thread::spawn(|| tracing::warn!("adopted by nobody"))
            .join()
            .expect("unadopted thread");
        tracing::info!("the daemon thread itself");
        drop(log);
        tracing::warn!("after the daemon log is gone");
        let text = std::fs::read_to_string(config.join("daemon.log")).expect("daemon.log");
        let suffixes: Vec<&str> = text.lines().map(|line| &line[24..]).collect();
        assert_eq!(
            vec!["INFO dunkingsheep: the daemon thread itself"],
            suffixes,
            "{text}"
        );
    }
}
