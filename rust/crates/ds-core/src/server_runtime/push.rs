//! Concrete sources used by the background task push loop.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use super::LoopRuntimeAdapter;
use crate::error::{refusal, DunkError};
use crate::flock::lanes::pane_to_json;
use crate::flock::loop_env::LoopFuture;
use crate::roles::DEFAULT_ROLE;

impl LoopRuntimeAdapter {
    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_list_panes<'a>(
        &'a self,
    ) -> LoopFuture<'a, Result<Vec<Value>, DunkError>> {
        let worker = self.with_loop_terminal(|_, terminal| {
            terminal.with_terminal(|backend| {
                backend
                    .try_list_panes_grouped()
                    .map(|grouped| {
                        grouped
                            .into_values()
                            .flatten()
                            .map(|pane| pane_to_json(&pane, None))
                            .collect()
                    })
                    .map_err(|error| DunkError::Refused(error.to_string()))
            })
        });
        Box::pin(async move {
            worker
                .await
                .map_err(|error| refusal(format!("terminal pane-list worker failed: {error}")))?
        })
    }

    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_caller_role(
        &self,
        pane_id: &str,
        pane_info: Option<&Value>,
    ) -> String {
        self.flock
            .with_roles(|roles, legacy| {
                let role = roles.role_of(Some(pane_id), pane_info)?;
                Ok(
                    if role == roles.default
                        && legacy.is_some_and(|allowlist| allowlist.panes.contains(pane_id))
                    {
                        "owner".to_owned()
                    } else {
                        role
                    },
                )
            })
            .unwrap_or_else(|_| DEFAULT_ROLE.to_owned())
    }

    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_registered_lanes(&self) -> Option<BTreeSet<String>> {
        self.flock
            .with_roles(|roles, _| {
                let described = roles.describe()?;
                let lanes = described.get("lanes").and_then(Value::as_object);
                Ok(lanes.map(|lanes| lanes.keys().cloned().collect()))
            })
            .ok()
            .flatten()
    }

    #[allow(dead_code)] // The async PushEnv assembly belongs to its dependent leaf.
    pub(crate) fn helper_pushenv_unread_mail<'a>(
        &'a self,
        pane_id: &'a str,
    ) -> LoopFuture<'a, bool> {
        let pane_id = pane_id.to_owned();
        let worker_pane_id = pane_id.clone();
        let worker = self.with_loop_terminal(move |flock, terminal| {
            let Some(store) = flock.stores().inbox.clone() else {
                return false;
            };
            let mailbox = match flock.own_mailbox_from_sources(
                terminal,
                || store.read_pool(),
                &worker_pane_id,
                || {
                    std::env::var("DUNKINGSHEEP_MAILBOX")
                        .ok()
                        .map(|value| {
                            value
                                .trim_matches(|c: char| {
                                    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
                                })
                                .to_owned()
                        })
                        .filter(|value| !value.is_empty())
                },
                super::configured_backend_name,
            ) {
                Ok(mailbox) => mailbox,
                Err(error) => {
                    eprintln!("push: unread mail check failed pane={worker_pane_id}: {error}");
                    return false;
                }
            };
            let now = crate::clock::now_s();
            let unread = match crate::inbox::count(store.read_pool(), &mailbox, &[], now) {
                Ok(counts) => counts.unread,
                Err(error) => {
                    eprintln!("push: unread mail count failed pane={worker_pane_id}: {error}");
                    return false;
                }
            };
            if unread == 0 {
                return false;
            }
            let messages = match store.read(&mailbox, 50, true, false) {
                Ok(messages) => messages
                    .into_iter()
                    .map(|message| crate::push::Mail {
                        subject: message.summary.subject,
                        body: Some(message.body),
                        created_at: Some(message.summary.created_at),
                    })
                    .collect::<Vec<_>>(),
                Err(error) => {
                    eprintln!("push: unread mail read failed pane={worker_pane_id}: {error}");
                    return false;
                }
            };
            let grace_s = std::env::var("DUNKINGSHEEP_UNREAD_GRACE_S")
                .ok()
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(crate::push::UNREAD_MAIL_GRACE_S);
            crate::push::unread_mail(unread, &messages, now, grace_s)
        });
        Box::pin(async move {
            match worker.await {
                Ok(unread) => unread,
                Err(error) => {
                    eprintln!("push: unread mail worker failed pane={pane_id}: {error}");
                    false
                }
            }
        })
    }

    #[allow(dead_code)] // The async PushEnv assembly belongs to its dependent leaf.
    pub(crate) fn helper_pushenv_announce_task<'a>(
        &'a self,
        pane_id: &'a str,
        task: &'a crate::tasks::Task,
        lease_seconds: f64,
    ) -> LoopFuture<'a, ()> {
        let pane_id = pane_id.to_owned();
        let task = task.clone();
        let task_id = task.id.clone();
        let worker_task_id = task_id.clone();
        let subject = format!("TASK {}: {}", task.id, task.title);
        let dependencies =
            serde_json::from_str::<Vec<String>>(&task.depends_on).unwrap_or_default();
        let mut lines = vec![
            format!("TASK {} [{}] {}", task.id, task.priority, task.title),
            format!(
                "scope {} | status {} | lease {}s",
                task.scope, task.status, lease_seconds as i64
            ),
        ];
        if !task.body.is_empty() {
            lines.extend([String::new(), task.body.clone()]);
        }
        if !dependencies.is_empty() {
            lines.extend([
                String::new(),
                format!("depends on: {}", dependencies.join(", ")),
            ]);
        }
        lines.extend([
            String::new(),
            format!("heartbeat: dunkingsheep task heartbeat {}", task.id),
            format!("complete:  dunkingsheep task complete {}", task.id),
            "next:      dunkingsheep task next".to_owned(),
        ]);
        let body = lines.join("\n");
        let flock = Arc::clone(&self.flock);
        let terminal_factory = Arc::clone(&self.terminal_factory);
        let worker = self.with_loop_terminal(move |flock_ref, terminal| {
            let result = (|| {
                let runtime = terminal.runtime.clone();
                let request = crate::server::RequestCtx::default();
                let request_adapter = super::RequestRuntimeAdapter::new(
                    flock_ref,
                    &request,
                    &runtime,
                    terminal.terminal,
                );
                let outbox = super::outbox_runtime::OutboxRuntime::new(
                    Arc::clone(&flock),
                    runtime.clone(),
                    terminal_factory,
                )?;
                let inbox = super::inbox_data::InboxRuntimeView::new(
                    request_adapter,
                    Arc::clone(&flock),
                    Arc::clone(&outbox.pool),
                )?;
                let args = crate::flock::commands_inbox::SendArgs {
                    to: &pane_id,
                    body: &body,
                    subject: Some(&subject),
                    notify: true,
                    ..Default::default()
                };
                flock_ref.inbox_send(&inbox, &args).map(|_| ())
            })();
            if let Err(error) = result {
                eprintln!("could not announce task {worker_task_id} to {pane_id}: {error}");
            }
            flock_ref.set_last_message(format!("Pushed task {worker_task_id} to {pane_id}"));
        });
        Box::pin(async move {
            if let Err(error) = worker.await {
                eprintln!("push: announce task worker failed for {task_id}: {error}");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::Env;
    use crate::flock::{loops_push::PushEnv, panes::Stores, Flock};
    use crate::inbox_store::{Outgoing, Store};
    use crate::roles::Roles;
    use crate::tasks::{Task, Tasks};
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    fn fake_push_terminal() -> Box<dyn Terminal> {
        let mut terminal = FakeTerminal::new();
        terminal.panes[0].label = "w1:p1".into();
        Box::new(terminal)
    }

    struct PushClaimEnv {
        pane_id: String,
        unread: bool,
    }

    impl PushEnv for PushClaimEnv {
        fn list_panes(&self) -> Result<Vec<Value>, crate::error::DunkError> {
            Ok(vec![serde_json::json!({
                "pane_id": self.pane_id,
                "agent_status": "idle"
            })])
        }

        fn caller_role(&self, _: &str, _: Option<&Value>) -> String {
            "worker".into()
        }

        fn registered_lanes(&self) -> Option<BTreeSet<String>> {
            Some(BTreeSet::from([self.pane_id.clone()]))
        }

        fn may_review(&self, _: &str) -> bool {
            false
        }

        fn provider_of(&self, _: &str) -> Option<String> {
            None
        }

        fn lane_agent_alive(&self, _: &str) -> Option<bool> {
            Some(true)
        }

        fn unread_mail(&self, _: &str) -> bool {
            self.unread
        }

        fn announce_task(&self, _: &str, _: &Task, _: f64) {}
    }

    #[test]
    fn production_env_push_delivery_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-push-delivery");
        let store = Arc::new(
            Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .expect("inbox store"),
        );
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&store)),
            ..Stores::default()
        });
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&flock), Arc::new(fake_push_terminal));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let task = Task {
            id: "t42".into(),
            scope: "global".into(),
            title: "review API".into(),
            body: "Check the response shape.".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            kind: "work".into(),
            assignees: "[]".into(),
            depends_on: "[\"t40\", \"t41\"]".into(),
            block_reason: None,
            created_at: 0.0,
            updated_at: 0.0,
            completed_by: None,
            completed_at: None,
        };

        runtime.block_on(async {
            adapter
                .helper_pushenv_announce_task("w1:p1", &task, 12.8)
                .await
        });
        assert_eq!(flock.last_message(), "Pushed task t42 to w1:p1");
        let messages = store
            .read("w1:p1", 50, true, false)
            .expect("read task announcement");
        let announcement = messages
            .iter()
            .find(|message| message.summary.subject.as_deref() == Some("TASK t42: review API"))
            .expect("announcement stored in inbox");
        assert!(announcement.body.contains("TASK t42 [high] review API"));
        assert!(announcement.body.contains("lease 12s"));
        assert!(announcement.body.contains("depends on: t40, t41"));

        // Task announcement mail is ignored by the unread gate; actionable mail holds it.
        assert!(!runtime.block_on(async { adapter.helper_pushenv_unread_mail("w1:p1").await }));
        store
            .send(&Outgoing {
                mailbox: "w1:p1",
                body: Some("Please review this change."),
                subject: Some("Action needed"),
                sender: Some("w2:p2"),
                pane_id: Some("w1:p1"),
                ..Outgoing::default()
            })
            .expect("store actionable mail");
        assert!(runtime.block_on(async { adapter.helper_pushenv_unread_mail("w1:p1").await }));
        store
            .read("w1:p1", 50, false, true)
            .expect("read queued mail");
        assert!(!runtime.block_on(async { adapter.helper_pushenv_unread_mail("w1:p1").await }));
    }

    #[test]
    fn production_env_push_delivery_unread_mail_blocks_claims() {
        let dir = crate::testsupport::dir("runtime-push-delivery-claim-gate");
        let store = Arc::new(
            Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .expect("inbox store"),
        );
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&store)),
            tasks: Some(Arc::clone(&tasks)),
            ..Stores::default()
        });
        store
            .send(&Outgoing {
                mailbox: "w1:p1",
                body: Some("Please respond before taking more work."),
                subject: Some("Action needed"),
                sender: Some("w2:p2"),
                pane_id: Some("w1:p1"),
                ..Outgoing::default()
            })
            .expect("store actionable mail");
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&flock), Arc::new(fake_push_terminal));
        let unread = runtime.block_on(async { adapter.helper_pushenv_unread_mail("w1:p1").await });
        assert!(unread, "the production helper sees actionable unread mail");

        let task = runtime
            .block_on(tasks.add("global", "waiting work", ""))
            .unwrap();
        let env = PushClaimEnv {
            pane_id: "w1:p1".into(),
            unread,
        };
        let decisions = runtime.block_on(flock.push_tasks(&env));
        assert_eq!(
            runtime.block_on(tasks.get(&task.id)).unwrap().status,
            "open"
        );
        assert_eq!(
            decisions
                .iter()
                .find(|decision| decision.pane_id == "w1:p1")
                .map(|decision| decision.detail.as_str()),
            Some("unread_mail")
        );
    }

    #[test]
    fn production_env_push_core_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-push-core");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .expect("roles store");
        roles
            .register(
                "w2:p2",
                "reviewer",
                Some(2202),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("register reviewer");
        roles
            .register("w1:p1", "worker", Some(1101), None, None, None, None, None)
            .expect("register worker");
        roles
            .register("w3:p3", "worker", Some(9999), None, None, None, None, None)
            .expect("register worker");
        flock.attach_roles(roles, None);

        let adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let panes = runtime.block_on(async {
            let panes = adapter
                .helper_pushenv_list_panes()
                .await
                .expect("concrete terminal pane list");
            assert!(!panes.is_empty());
            panes
        });
        assert_eq!(
            adapter.helper_pushenv_caller_role(
                "w2:p2",
                panes.iter().find(|p| p["pane_id"] == "w2:p2")
            ),
            "reviewer"
        );
        assert_eq!(
            adapter.helper_pushenv_caller_role(
                "w1:p1",
                panes.iter().find(|p| p["pane_id"] == "w1:p1")
            ),
            "worker"
        );
        assert_eq!(
            adapter.helper_pushenv_registered_lanes(),
            Some(BTreeSet::from([
                "w1:p1".into(),
                "w2:p2".into(),
                "w3:p3".into()
            ]))
        );
        assert_eq!(
            adapter.helper_pushenv_caller_role(
                "w3:p3",
                panes.iter().find(|p| p["pane_id"] == "w3:p3")
            ),
            DEFAULT_ROLE
        );
        assert_eq!(
            adapter.helper_pushenv_caller_role("w1:p1", None),
            DEFAULT_ROLE
        );

        let unavailable = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| {
                let mut terminal = FakeTerminal::new();
                terminal.fail_grouped_panes = Some(crate::terminal::TerminalError::unavailable(
                    "pane listing failed",
                ));
                Box::new(terminal) as Box<dyn Terminal>
            }),
        );
        let error = runtime
            .block_on(async { unavailable.helper_pushenv_list_panes().await })
            .expect_err("listing failure is surfaced");
        assert!(error.to_string().contains("pane listing failed"));

        let no_roles_flock = Arc::new(Flock::new(
            dir.path().join("no-roles-launches.json"),
            dir.path().join("no-roles.json"),
            false,
        ));
        let without_registry = LoopRuntimeAdapter::new(
            Arc::clone(&no_roles_flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        assert_eq!(without_registry.helper_pushenv_registered_lanes(), None);
        assert_eq!(
            without_registry.helper_pushenv_caller_role("%unregistered", None),
            DEFAULT_ROLE
        );
        let followers = std::collections::BTreeMap::from([("%follow".into(), "global".into())]);
        let targets = crate::push::push_targets(&followers, None, |_| {
            panic!("unavailable registry must not discover registered lanes")
        });
        assert_eq!(targets.len(), 1);
        assert!(targets.contains_key("%follow"));
    }
}
