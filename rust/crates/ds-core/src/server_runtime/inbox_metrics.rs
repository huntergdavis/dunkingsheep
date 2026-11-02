//! Concrete metrics and task-view sources for the inbox runtime adapter.

use super::RequestRuntimeAdapter;
use crate::error::DunkError;
use crate::metrics::Event;
use serde_json::Value;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete InboxEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_inboxenv_record_metric(
        &self,
        kind: &str,
        pane_id: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        duration_ms: Option<i64>,
    ) {
        let Some(metrics) = self.terminal.flock.stores().metrics else {
            return;
        };
        metrics.record(&Event {
            kind: kind.to_owned(),
            pane_id: pane_id.map(str::to_owned),
            mailbox: Some(mailbox.to_owned()),
            sender: sender.map(str::to_owned),
            source: Some("inbox".to_owned()),
            outcome: Some("ok".to_owned()),
            duration_ms,
            ..Event::default()
        });
    }

    #[allow(dead_code)] // The complete InboxEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_inboxenv_record_sent(
        &self,
        pane_id: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        bytes: usize,
        attachments: usize,
    ) {
        let Some(metrics) = self.terminal.flock.stores().metrics else {
            return;
        };
        metrics.record(&Event {
            kind: "inbox.sent".to_owned(),
            pane_id: pane_id.map(str::to_owned),
            mailbox: Some(mailbox.to_owned()),
            sender: sender.map(str::to_owned),
            source: Some("inbox".to_owned()),
            outcome: Some("ok".to_owned()),
            attempts: (attachments != 0).then(|| i64::try_from(attachments).unwrap_or(i64::MAX)),
            bytes: Some(i64::try_from(bytes).unwrap_or(i64::MAX)),
            ..Event::default()
        });
    }

    #[allow(dead_code)] // The complete InboxEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_inboxenv_tasks_for_pane(&self, pane_id: &str) -> Result<Value, DunkError> {
        let tasks = self
            .terminal
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store is not attached".to_owned()))?;
        self.runtime.block_on(tasks.for_pane(pane_id, None))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::flock::panes::Stores;
    use crate::flock::Flock;
    use crate::metrics::MetricsStore;
    use crate::server::RequestCtx;
    use crate::tasks::Tasks;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    #[test]
    fn production_env_inbox_metrics_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-inbox-metrics");
        let metrics =
            Arc::new(MetricsStore::open(dir.path().join("metrics.db")).expect("metrics store"));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .expect("tasks store"),
        );
        runtime
            .block_on(tasks.add("global", "visible task", "body"))
            .expect("add task");
        let expected = runtime
            .block_on(tasks.for_pane("%17", None))
            .expect("render pane task view");
        assert_eq!(json!("%17"), expected["pane_id"]);
        assert!(expected["global"].is_array());
        assert!(expected["pane"].is_array());
        assert!(expected["next"].is_object());

        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            metrics: Some(Arc::clone(&metrics)),
            ..Stores::default()
        });
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        adapter.helper_inboxenv_record_metric(
            "inbox.read",
            Some("%17"),
            "%17",
            Some("%sender"),
            Some(42),
        );
        adapter.helper_inboxenv_record_sent(Some("%17"), "%17", Some("%sender"), 33, 2);
        let events = metrics
            .events_between(0.0, crate::clock::now_s() + 1.0)
            .expect("read recorded metrics");
        assert_eq!(2, events.len());
        let read = events
            .iter()
            .find(|event| event.kind == "inbox.read")
            .expect("read metric");
        assert_eq!(Some("%17".into()), read.pane_id);
        assert_eq!(Some("%17".into()), read.mailbox);
        assert_eq!(Some("%sender".into()), read.sender);
        assert_eq!(Some("inbox".into()), read.source);
        assert_eq!(Some("ok".into()), read.outcome);
        assert_eq!(Some(42), read.duration_ms);
        let sent = events
            .iter()
            .find(|event| event.kind == "inbox.sent")
            .expect("sent metric");
        assert_eq!(Some("%17".into()), sent.pane_id);
        assert_eq!(Some("%17".into()), sent.mailbox);
        assert_eq!(Some("%sender".into()), sent.sender);
        assert_eq!(Some("inbox".into()), sent.source);
        assert_eq!(Some("ok".into()), sent.outcome);
        assert_eq!(Some(2), sent.attempts);
        assert_eq!(Some(33), sent.bytes);
        assert_eq!(
            expected,
            adapter.helper_inboxenv_tasks_for_pane("%17").unwrap()
        );
        assert!(adapter
            .helper_inboxenv_tasks_for_pane("not a pane")
            .is_err());

        let without_metrics = Flock::new(
            dir.path().join("no-metrics-launches.json"),
            dir.path().join("no-metrics-lanes.json"),
            false,
        );
        without_metrics.attach_stores(Stores {
            tasks: Some(tasks),
            ..Stores::default()
        });
        let no_metrics_request = RequestCtx::default();
        let no_metrics_adapter = RequestRuntimeAdapter::new(
            &without_metrics,
            &no_metrics_request,
            runtime.handle(),
            &terminal,
        );
        no_metrics_adapter.helper_inboxenv_record_metric("inbox.read", None, "%17", None, None);
        no_metrics_adapter.helper_inboxenv_record_sent(None, "%17", None, 0, 0);
        assert!(without_metrics.stores().metrics.is_none());
    }
}
