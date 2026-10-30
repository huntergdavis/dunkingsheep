//! Roles: who a caller is, and what that role may call (T10, `dunk_roles.py`).
//!
//! Ported 1:1 from `dunk_roles.py`: the same role names, the same read-only set, the same
//! per-role allowlists and the same `WEB_DENIED` exception, because the daemon, the CLI and
//! the MCP server all check `allowed()` and a port that "tidied" a table would change who
//! may do what.
//!
//! What this module is NOT: it does not bind a role to a process (that is `dunk_callers`,
//! T03) and it does not read the registry file (the `Roles` store is the other half of T10,
//! still to port). `allowed()` is the pure rule those two feed.

use crate::error::DunkError;

/// Every role the daemon knows.
pub const ROLES: &[&str] = &[
    "owner",
    "system",
    "web",
    "desk",
    "planner",
    "worker",
    "reviewer",
    "anonymous",
];

/// The roles a lane may be registered with.
pub const LANE_ROLES: &[&str] = &["desk", "planner", "worker", "reviewer", "owner"];

/// Read-only commands: every role may call these.
pub const READ_ONLY: &[&str] = &[
    "delivery",
    "delivery_capability",
    "dispatch",
    "doctor",
    "get_dunk",
    "get_message",
    "health",
    "help",
    "inbox_count",
    "inbox_stats",
    "inbox_status",
    "lane_register",
    "lanes",
    "list_dunks",
    "list_messages",
    "list_panes",
    "list_workspaces",
    "memory_context",
    "memory_recall",
    "memory_search",
    "memory_stats",
    "memory_status",
    "message_state",
    "metrics",
    "owner_receipts",
    "owner_stats",
    "pm_digest",
    "pm_routing",
    "poll",
    "poolrad_progress",
    "project_summary",
    "reply_stats",
    "report_list",
    "review_list",
    "roster",
    "state_latest",
    "status",
    "superuser_recover",
    "superuser_status",
    "task_followers",
    "task_list",
    "task_metrics",
    "task_read",
    "task_stats",
    "turn_end",
    "turn_start",
    "whoami",
];

/// Verbs the owner's transport may NOT call: closing, shaping and stopping the fleet stay with a human.
pub const WEB_DENIED: &[&str] = &[
    "add_dunk",
    "clear_box",
    "close",
    "create_tab",
    "create_workspace",
    "fire_dunk",
    "focus",
    "gh_token",
    "herdr",
    "lane_register",
    "lane_set",
    "memory_backfill",
    "memory_ingest",
    "remove_dunk",
    "rename",
    "respawn_pane",
    "retire_pane",
    "shutdown",
    "split_pane",
    "start_agent",
    "start_dunk",
    "stop_all",
    "stop_dunk",
    "superuser_add",
    "superuser_recover",
    "superuser_remove",
    "toggle_dunk",
    "token_issue",
    "token_revoke",
    "update_dunk",
];

/// A lane that does work: the read-only set plus its own verbs.
pub const WORKER: &[&str] = &[
    "context_add",
    "context_supersede",
    "delivery",
    "delivery_capability",
    "dispatch",
    "doctor",
    "flush_box",
    "get_dunk",
    "get_message",
    "gh_token",
    "health",
    "help",
    "inbox_ack",
    "inbox_archive",
    "inbox_cancel",
    "inbox_count",
    "inbox_delete",
    "inbox_fetch",
    "inbox_list",
    "inbox_read",
    "inbox_recover",
    "inbox_send",
    "inbox_stats",
    "inbox_status",
    "lane_register",
    "lanes",
    "list_dunks",
    "list_messages",
    "list_panes",
    "list_workspaces",
    "memory_context",
    "memory_recall",
    "memory_search",
    "memory_stats",
    "memory_status",
    "message_state",
    "metrics",
    "owner_receipts",
    "owner_stats",
    "pm_digest",
    "pm_routing",
    "poll",
    "poolrad_progress",
    "project_summary",
    "read_pane",
    "reply_stats",
    "report_list",
    "review_list",
    "roster",
    "send_keys",
    "send_text",
    "state_capture",
    "state_latest",
    "status",
    "superuser_recover",
    "superuser_status",
    "task_add",
    "task_block",
    "task_complete",
    "task_follow",
    "task_followers",
    "task_heartbeat",
    "task_list",
    "task_metrics",
    "task_next",
    "task_note",
    "task_read",
    "task_release",
    "task_stats",
    "task_unblock",
    "task_unfollow",
    "task_update",
    "turn_end",
    "turn_start",
    "whoami",
];

/// Timers and bridges: task shaping, delivery, the owner mailbox transports.
pub const SYSTEM: &[&str] = &[
    "context_add",
    "delivery",
    "delivery_capability",
    "dispatch",
    "doctor",
    "flush_box",
    "get_dunk",
    "get_message",
    "gh_token",
    "health",
    "help",
    "inbox_count",
    "inbox_fetch",
    "inbox_read",
    "inbox_send",
    "inbox_stats",
    "inbox_status",
    "lane_register",
    "lanes",
    "list_dunks",
    "list_messages",
    "list_panes",
    "list_workspaces",
    "memory_context",
    "memory_recall",
    "memory_search",
    "memory_stats",
    "memory_status",
    "message_state",
    "metrics",
    "owner_conversation",
    "owner_delivered",
    "owner_ingest",
    "owner_outbox",
    "owner_poll",
    "owner_push",
    "owner_receipts",
    "owner_reply",
    "owner_stats",
    "owner_take",
    "pm_digest",
    "pm_routing",
    "poll",
    "poolrad_progress",
    "poolrad_sync",
    "project_summary",
    "read_pane",
    "recover",
    "reply_stats",
    "report_list",
    "report_run",
    "respawn_pane",
    "review_list",
    "review_sync",
    "roster",
    "send_keys",
    "state_capture",
    "state_latest",
    "status",
    "superuser_recover",
    "superuser_status",
    "task_add",
    "task_assign",
    "task_block",
    "task_cancel",
    "task_followers",
    "task_list",
    "task_metrics",
    "task_note",
    "task_read",
    "task_reap",
    "task_stats",
    "task_unblock",
    "task_update",
    "turn_end",
    "turn_start",
    "whoami",
];

/// The front desk: reads the board and the owner mailbox, files work, answers the owner.
pub const DESK: &[&str] = &[
    "delivery",
    "delivery_capability",
    "dispatch",
    "doctor",
    "get_dunk",
    "get_message",
    "health",
    "help",
    "inbox_ack",
    "inbox_count",
    "inbox_fetch",
    "inbox_read",
    "inbox_stats",
    "inbox_status",
    "lane_register",
    "lanes",
    "list_dunks",
    "list_messages",
    "list_panes",
    "list_workspaces",
    "memory_context",
    "memory_recall",
    "memory_search",
    "memory_stats",
    "memory_status",
    "message_state",
    "metrics",
    "owner_conversation",
    "owner_outbox",
    "owner_poll",
    "owner_receipts",
    "owner_reply",
    "owner_stats",
    "owner_take",
    "pm_digest",
    "pm_routing",
    "poll",
    "poolrad_progress",
    "project_summary",
    "read_pane",
    "reply_stats",
    "report_list",
    "report_run",
    "review_list",
    "roster",
    "state_latest",
    "status",
    "superuser_recover",
    "superuser_status",
    "task_add",
    "task_followers",
    "task_list",
    "task_metrics",
    "task_note",
    "task_read",
    "task_stats",
    "turn_end",
    "turn_start",
    "whoami",
];

/// The planner: files task bodies, reads, and nothing else.
pub const PLANNER: &[&str] = &[
    "delivery",
    "delivery_capability",
    "dispatch",
    "doctor",
    "get_dunk",
    "get_message",
    "health",
    "help",
    "inbox_count",
    "inbox_read",
    "inbox_stats",
    "inbox_status",
    "lane_register",
    "lanes",
    "list_dunks",
    "list_messages",
    "list_panes",
    "list_workspaces",
    "memory_context",
    "memory_recall",
    "memory_search",
    "memory_stats",
    "memory_status",
    "message_state",
    "metrics",
    "owner_receipts",
    "owner_stats",
    "pm_digest",
    "pm_routing",
    "poll",
    "poolrad_progress",
    "project_summary",
    "reply_stats",
    "report_list",
    "review_list",
    "roster",
    "state_latest",
    "status",
    "superuser_recover",
    "superuser_status",
    "task_add",
    "task_followers",
    "task_list",
    "task_metrics",
    "task_note",
    "task_read",
    "task_stats",
    "turn_end",
    "turn_start",
    "whoami",
];

/// May `role` call `command`? The single choke point (`dunk_api.dispatch` in Python).
///
/// `owner` may call everything; `web` may call everything except `WEB_DENIED`; every other
/// role gets its table. An unknown role is treated as `anonymous`, which is `READ_ONLY` -
/// fail closed, because the alternative is a typo in a role name granting write access.
pub fn allowed(role: &str, command: &str) -> bool {
    match role {
        "owner" => true,
        "web" => !WEB_DENIED.contains(&command),
        "system" => SYSTEM.contains(&command),
        "desk" => DESK.contains(&command),
        "planner" => PLANNER.contains(&command),
        "worker" | "reviewer" => WORKER.contains(&command),
        _ => READ_ONLY.contains(&command),
    }
}

/// Resolve a role name, refusing an unknown one.
///
/// The registry file and `--role` both take a name; a name that is not in `ROLES` must be a
/// refusal, not a default, or a typo silently becomes `anonymous` at registration time.
pub fn parse_role(name: &str) -> Result<&'static str, DunkError> {
    let name = name.trim();
    ROLES
        .iter()
        .find(|role| **role == name)
        .copied()
        .ok_or_else(|| {
            DunkError::Message(format!(
                "role must be one of {}; got {name:?}",
                ROLES.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_may_call_everything_and_an_unknown_role_gets_read_only() {
        assert!(allowed("owner", "shutdown"));
        assert!(allowed("owner", "task_assign"));
        assert!(allowed("nonsense", "status"), "fail closed: read-only");
        assert!(!allowed("nonsense", "task_next"));
    }

    #[test]
    fn the_owner_transport_is_denied_the_fleet_destructive_verbs() {
        assert!(allowed("web", "task_add"));
        assert!(!allowed("web", "close"));
        assert!(!allowed("web", "start_agent"));
        assert!(!allowed("web", "respawn_pane"));
    }

    #[test]
    fn a_worker_pulls_work_but_never_assigns_it() {
        assert!(allowed("worker", "task_next"));
        assert!(allowed("worker", "inbox_send"));
        assert!(
            !allowed("worker", "task_assign"),
            "lanes pull, they are not handed work"
        );
        assert!(!allowed("worker", "start_agent"));
    }

    #[test]
    fn the_desk_answers_the_owner_and_does_not_ingest() {
        assert!(allowed("desk", "owner_outbox"));
        assert!(allowed("desk", "task_add"));
        assert!(
            !allowed("desk", "owner_ingest"),
            "ingest belongs to the transport"
        );
        assert!(!allowed("desk", "send_keys"));
    }

    #[test]
    fn the_planner_files_bodies_and_does_nothing_else() {
        assert!(allowed("planner", "task_add"));
        assert!(!allowed("planner", "task_next"));
        assert!(!allowed("planner", "inbox_send"));
    }

    #[test]
    fn a_role_name_must_be_one_of_the_eight() {
        assert_eq!(Ok("worker"), parse_role("worker"));
        let error = parse_role("wroker").expect_err("a typo is a refusal, not a default");
        assert!(error.to_string().contains("role must be one of"));
    }

    #[test]
    fn the_tables_are_the_python_tables_sizes() {
        // Guards a silent shrink: the Python sets are these sizes at the port's base.
        assert_eq!(47, READ_ONLY.len());
        assert_eq!(75, WORKER.len());
        assert_eq!(30, WEB_DENIED.len());
        assert_eq!(8, ROLES.len());
    }
}

// -- the registry store -------------------------------------------------------

/// One lane's registration, exactly as `roles.json` holds it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct LaneEntry {
    /// The role bound to the pane.
    pub role: String,
    /// The pane's root process at registration time.
    #[serde(default)]
    pub pane_pid: Option<u32>,
    /// That process's start time, so a reused pid is a different lane.
    #[serde(default)]
    pub start: Option<String>,
    /// The harness (`pi`, `claude`, `codex`).
    #[serde(default)]
    pub provider: Option<String>,
    /// The model, for the record.
    #[serde(default)]
    pub model: Option<String>,
    /// Who registered it.
    #[serde(default)]
    pub by: Option<String>,
    /// The display label.
    #[serde(default)]
    pub label: Option<String>,
}

/// The registry file, as the Python daemon writes it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Registry {
    /// The role an unregistered pane gets.
    #[serde(default = "default_role")]
    pub default: String,
    /// pane id -> registration.
    #[serde(default)]
    pub lanes: std::collections::BTreeMap<String, LaneEntry>,
    /// Providers a reviewer lane may run on.
    #[serde(default)]
    pub reviewer_providers: Vec<String>,
}

fn default_role() -> String {
    "anonymous".to_string()
}

/// A READ-ONLY view of the registry (owner decision 2026-10-30).
///
/// The Rust store reads the SAME `~/.config/dunkingsheep/roles.json` the Python daemon
/// writes, same schema and same path, because the differential harness needs identical
/// input on both sides. It is read-only: **the Python daemon stays the only writer** until
/// a separate task moves the writer, so this struct has no save path on purpose - a second
/// writer is how two processes overwrite each other's lane bindings.
#[derive(Debug)]
pub struct Roles {
    path: std::path::PathBuf,
    registry: Registry,
    stamp: Option<(u64, u64)>,
}

impl Roles {
    /// Read the registry from `path`. A missing file is the empty registry, not an error:
    /// a fresh install has no registrations yet, and the daemon seeds it on first use.
    pub fn load(path: std::path::PathBuf) -> Result<Roles, DunkError> {
        let registry = match crate::atomic_json::read_json(&path)? {
            Some(value) => serde_json::from_value(value)?,
            None => Registry {
                default: default_role(),
                lanes: Default::default(),
                reviewer_providers: Vec::new(),
            },
        };
        let stamp = file_stamp(&path);
        Ok(Roles {
            path,
            registry,
            stamp,
        })
    }

    /// Re-read the file when it changed under us.
    ///
    /// The Python's `refresh()` exists because the VP edited the file and the daemon kept
    /// serving its in-memory copy, so a pane that WAS on the list was not a superuser until
    /// the next restart (2026-10-24). A stat per check costs nothing; a restart does not.
    pub fn refresh(&mut self) -> Result<bool, DunkError> {
        let stamp = file_stamp(&self.path);
        if stamp == self.stamp {
            return Ok(false);
        }
        let registry = match crate::atomic_json::read_json(&self.path)? {
            Some(value) => serde_json::from_value(value)?,
            None => Registry {
                default: default_role(),
                lanes: Default::default(),
                reviewer_providers: Vec::new(),
            },
        };
        self.registry = registry;
        self.stamp = stamp;
        Ok(true)
    }

    /// The registry as read.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The role bound to a pane, or the file's default.
    pub fn role_of(&mut self, pane_id: &str) -> Result<String, DunkError> {
        self.refresh()?;
        Ok(self
            .registry
            .lanes
            .get(pane_id)
            .map(|entry| entry.role.clone())
            .unwrap_or_else(|| self.registry.default.clone()))
    }

    /// The provider of a registered pane, lowercased.
    pub fn provider_of(&mut self, pane_id: &str) -> Result<Option<String>, DunkError> {
        self.refresh()?;
        Ok(self
            .registry
            .lanes
            .get(pane_id)
            .and_then(|entry| entry.provider.clone())
            .map(|provider| provider.trim().to_lowercase())
            .filter(|provider| !provider.is_empty()))
    }

    /// Reviewer role AND a provider on the reviewer list (free models never review).
    pub fn may_review(&mut self, pane_id: &str) -> Result<bool, DunkError> {
        if self.role_of(pane_id)? != "reviewer" {
            return Ok(false);
        }
        let Some(provider) = self.provider_of(pane_id)? else {
            return Ok(false);
        };
        Ok(self
            .registry
            .reviewer_providers
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&provider)))
    }
}

fn file_stamp(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    // mtime in nanoseconds plus the size: the pair changes when the file is rewritten,
    // which is all `refresh()` needs (the Python compares st_mtime_ns and st_size too).
    std::fs::metadata(path).ok().map(|meta| {
        let nanos = (meta.mtime() as u64) * 1_000_000_000 + (meta.mtime_nsec() as u64);
        (nanos, meta.len())
    })
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ds-roles-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("roles.json")
    }

    fn write(path: &std::path::Path, json: &str) {
        let mut handle = std::fs::File::create(path).unwrap();
        handle.write_all(json.as_bytes()).unwrap();
        handle.sync_all().unwrap();
    }

    const REGISTRY: &str = r#"{
      "default": "anonymous",
      "lanes": {
        "%100": {"role": "worker", "provider": "pi", "pane_pid": 1234, "label": "ds-worker-2"},
        "%101": {"role": "reviewer", "provider": "claude"},
        "%102": {"role": "reviewer", "provider": "pi"}
      },
      "reviewer_providers": ["claude", "codex"]
    }"#;

    #[test]
    fn a_registered_lane_gets_its_role_and_an_unknown_one_gets_the_default() {
        let path = fixture("roles");
        write(&path, REGISTRY);
        let mut roles = Roles::load(path).unwrap();
        assert_eq!("worker", roles.role_of("%100").unwrap());
        assert_eq!("reviewer", roles.role_of("%101").unwrap());
        assert_eq!("anonymous", roles.role_of("%999").unwrap());
    }

    #[test]
    fn only_a_reviewer_on_a_listed_provider_may_review() {
        let path = fixture("review");
        write(&path, REGISTRY);
        let mut roles = Roles::load(path).unwrap();
        assert!(
            roles.may_review("%101").unwrap(),
            "claude is a reviewer provider"
        );
        assert!(
            !roles.may_review("%102").unwrap(),
            "a pi reviewer is a worker-tier lane and must not review"
        );
        assert!(
            !roles.may_review("%100").unwrap(),
            "a worker is not a reviewer"
        );
    }

    #[test]
    fn a_missing_registry_is_the_empty_one_not_an_error() {
        let path = fixture("missing");
        let _ = std::fs::remove_file(&path);
        let mut roles = Roles::load(path).unwrap();
        assert_eq!("anonymous", roles.role_of("%1").unwrap());
        assert!(!roles.may_review("%1").unwrap());
    }

    #[test]
    fn a_changed_file_is_picked_up_without_a_restart() {
        // The 2026-10-24 incident: the file was edited and the daemon kept its copy.
        let path = fixture("refresh");
        write(&path, REGISTRY);
        let mut roles = Roles::load(path.clone()).unwrap();
        assert_eq!("anonymous", roles.role_of("%200").unwrap());
        std::thread::sleep(std::time::Duration::from_millis(5));
        write(
            &path,
            &REGISTRY.replace(
                r#""%102": {"role": "reviewer", "provider": "pi"}"#,
                r#""%102": {"role": "reviewer", "provider": "pi"}, "%200": {"role": "desk"}"#,
            ),
        );
        assert_eq!(
            "desk",
            roles.role_of("%200").unwrap(),
            "refresh picks it up"
        );
    }
}
