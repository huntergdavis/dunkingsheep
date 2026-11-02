use std::sync::{Arc, Mutex};

use super::*;
use crate::flock::{panes::Stores, sends::StatusBaseline, Flock};
use crate::metrics::MetricsStore;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use serde_json::json;

struct Fixture {
    flock: Flock,
    terminal: Mutex<Box<dyn Terminal>>,
    metrics: Arc<MetricsStore>,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new(mut fake: FakeTerminal) -> Self {
        fake.panes[0].command = "codex".into();
        let dir = crate::testsupport::dir("submit-runtime");
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
        let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        flock.attach_stores(Stores {
            metrics: Some(Arc::clone(&metrics)),
            ..Stores::default()
        });
        Self {
            flock,
            terminal: Mutex::new(Box::new(fake)),
            metrics,
            _dir: dir,
        }
    }
    fn adapter(&self) -> TerminalCallAdapter<'_> {
        TerminalCallAdapter {
            flock: &self.flock,
            terminal: &self.terminal,
        }
    }
    fn events(&self) -> Vec<Event> {
        self.metrics
            .events_between(0.0, crate::clock::now_s() + 1.0)
            .unwrap()
    }
}

#[test]
fn production_env_submit_helpers_uses_concrete_services() {
    let f = Fixture::new(FakeTerminal::new());
    let mut a = f.adapter();
    let before = a.helper_submitenv_now_s();
    a.helper_submitenv_sleep_s(0.001);
    assert!(a.helper_submitenv_now_s() >= before);
    assert_eq!(a.helper_submitenv_remembered_submit_key("w1:p1"), None);
    a.helper_submitenv_remember_submit_key("w1:p1", "C-m");
    assert_eq!(
        a.helper_submitenv_remembered_submit_key("w1:p1").as_deref(),
        Some("C-m")
    );
    assert_eq!(a.helper_submitenv_remembered_submit_key("other"), None);
    assert_eq!(
        a.helper_submitenv_send_submit_key_guarded("w1:p1", "C-m", true),
        (true, "sent".into(), Some("Tab".into()))
    );
    // A prior busy Tab cannot become an idle retry key. The accepted key decision re-reads.
    assert_eq!(
        a.helper_submitenv_send_submit_key_guarded("w1:p1", "Tab", false),
        (true, "sent".into(), Some("Enter".into()))
    );
    assert!(f.terminal.try_lock().is_ok());
    a.helper_submitenv_record_submit_attempt("w1:p1", "ok", 2, "雪".len());
    let events = f.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "submit.attempt");
    assert_eq!(events[0].attempts, Some(2));
    assert_eq!(events[0].bytes, Some(3));
    assert_eq!(events[0].outcome.as_deref(), Some("ok"));
    assert_eq!(events[0].pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(events[0].source, None);
}

#[test]
fn production_submission_adapters_are_complete_and_guard_retry_keys() {
    fn complete<T: SubmitEnv + FlushEnv>() {}
    complete::<TerminalCallAdapter<'_>>();
    let f = Fixture::new(FakeTerminal::new());
    let mut a = f.adapter();
    // Exercise the accepted full ladder with the concrete adapter, real backend calls,
    // monotonic windows, real sleeps and actual metrics. The cleared composer confirms it.
    let result = sends::press_until_submitted(
        &mut a,
        "w1:p1",
        "already typed",
        false,
        false,
        StatusBaseline::ReadNow,
    );
    assert_eq!(result, (true, "sent".into()));
    assert_eq!(f.events().last().unwrap().attempts, Some(1));
    let empty = sends::flush_box(&mut a, &json!({"pane_id":"w1:p1"}), Some("w1:p1")).unwrap();
    assert_eq!(empty["flushed"], false);
    assert!(empty["message"]
        .as_str()
        .unwrap()
        .contains("nothing to submit"));

    let mut modal = FakeTerminal::new();
    modal.screens.insert("w1:p1".into(),"Would you like to run the following command?\n❯ 1. Yes\n  2. No\n\n      Press Enter to confirm or Esc to cancel\n".into());
    *f.terminal.lock().unwrap() = Box::new(modal);
    let refused = SubmitEnv::send_submit_key_guarded(&mut a, "w1:p1", "Enter", false);
    assert_eq!(
        refused,
        (
            false,
            "submission held: a dialog opened before the key".into(),
            None
        )
    );
    *f.terminal.lock().unwrap() = Box::new(FakeTerminal::unavailable());
    assert_eq!(
        SubmitEnv::send_submit_key_guarded(&mut a, "w1:p1", "Enter", false),
        (
            false,
            "submission unconfirmed: pane unreadable before key".into(),
            None
        )
    );
    assert_eq!(
        FlushEnv::server_error_hint(&a),
        "tmux server not running - start it with `tmux new -d`"
    );
    // Both refusal paths released the real fleet-wide typing guard.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _guard = f.flock.typing_guard("other").unwrap();
            })
            .join()
            .unwrap();
    });
}

#[test]
fn production_submission_metrics_and_learned_keys_survive_adapter_recreation() {
    let f = Fixture::new(FakeTerminal::new());
    {
        let mut a = f.adapter();
        SubmitEnv::remember_submit_key(&mut a, "w1:p1", "C-m");
        SubmitEnv::record_submit_attempt(&mut a, "w1:p1", "ok", 3, 7);
    }
    let mut next = f.adapter();
    assert_eq!(
        SubmitEnv::remembered_submit_key(&next, "w1:p1").as_deref(),
        Some("C-m")
    );
    assert_eq!(SubmitEnv::remembered_submit_key(&next, "w1:p2"), None);
    assert_eq!(
        SubmitEnv::send_submit_key_guarded(&mut next, "w1:p1", "C-m", false),
        (true, "sent".into(), Some("C-m".into()))
    );
    let rows = f.events();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attempts, Some(3));
    assert_eq!(rows[0].bytes, Some(7));
    // Metrics disabled by configuration is a real store policy, not a fake adapter sink.
    let mut disabled = MetricsStore::open(f._dir.path().join("disabled.db")).unwrap();
    disabled.enabled = false;
    let disabled = Arc::new(disabled);
    f.flock.attach_stores(Stores {
        metrics: Some(Arc::clone(&disabled)),
        ..Stores::default()
    });
    SubmitEnv::record_submit_attempt(&mut next, "w1:p1", "ok", 1, 2);
    assert!(disabled
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap()
        .is_empty());
}
