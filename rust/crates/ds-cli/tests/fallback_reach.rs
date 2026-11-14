//! t3385 batch 2: the dispatcher's catch-all arms (a `not_ported(..)` behind a required
//! subcommand group, a `choices` argument, a routing match or the top-level command match) are
//! unreachable through the generated parser. This pins both halves: the parser refuses the
//! inputs that would reach them (exit 2, as argparse), and the generated table accepts exactly
//! the names the dispatcher routes, so a regenerated table that adds a name fails here instead
//! of printing "parsed but not ported yet" at a user.

use std::collections::BTreeSet;

use ds_cli::args::try_parse;
use ds_cli::cli_table::ROOT;
use ds_cli::spec::Cmd;
use serde_json::Value;

fn code(argv: &[&str]) -> Option<i32> {
    let mut full = vec!["dunkingsheep"];
    full.extend_from_slice(argv);
    try_parse(full).err().map(|e| e.exit_code())
}

fn command(path: &[&str]) -> &'static Cmd {
    let mut cmd: &'static Cmd = &ROOT;
    for name in path {
        cmd = cmd
            .subs
            .iter()
            .find(|sub| sub.name == *name)
            .unwrap_or_else(|| panic!("the table has no command {path:?}"));
    }
    cmd
}

fn sub_names(path: &[&str]) -> BTreeSet<&'static str> {
    command(path).subs.iter().map(|sub| sub.name).collect()
}

fn choices(path: &[&str], dest: &str) -> BTreeSet<&'static str> {
    command(path)
        .args
        .iter()
        .find(|arg| arg.dest == dest)
        .unwrap_or_else(|| panic!("{path:?} has no argument {dest}"))
        .choices
        .iter()
        .copied()
        .collect()
}

fn names(list: &[&'static str]) -> BTreeSet<&'static str> {
    list.iter().copied().collect()
}

/// memory, superuser, github, pm, poolrad, review, token, turn, inbox, task, context: the
/// Python groups are `required = True`, so a bare command or an unknown subcommand never
/// reaches the dispatcher's `None` / `Some(other)` arms.
#[test]
fn required_groups_refuse_a_bare_command_and_an_unknown_subcommand() {
    for group in [
        "memory",
        "superuser",
        "github",
        "pm",
        "poolrad",
        "review",
        "token",
        "turn",
        "inbox",
        "task",
        "context",
    ] {
        assert!(
            command(&[group]).sub_required,
            "{group}: the Python group is required"
        );
        assert_eq!(code(&[group]), Some(2), "bare {group}");
        assert_eq!(
            code(&[group, "no-such-subcommand"]),
            Some(2),
            "{group} no-such-subcommand"
        );
    }
}

/// `guard <action>` and `context ledger <action>` carry `choices`; an unknown value is refused
/// by the parser, so the dispatcher's `other =>` arms for them are unreachable.
#[test]
fn choice_arguments_refuse_an_unknown_value() {
    assert_eq!(
        choices(&["guard"], "action"),
        names(&[
            "status",
            "install",
            "install-tmux",
            "uninstall",
            "check",
            "shadow",
            "unshadow"
        ])
    );
    assert_eq!(code(&["guard", "no-such-action"]), Some(2));
    assert!(
        try_parse(["dunkingsheep", "guard"]).is_ok(),
        "guard's action defaults to status"
    );
    assert_eq!(
        choices(&["context", "ledger"], "action"),
        names(&["adopt", "status"])
    );
    assert_eq!(code(&["context", "ledger", "no-such-action"]), Some(2));
    assert_eq!(
        code(&["context", "ledger"]),
        Some(2),
        "ledger's action is a required positional"
    );
}

/// owner, report, state: the Python groups are NOT required, so the bare command parses
/// (the dispatcher handles it: owner as `owner stats`, report and state as Python's uncaught
/// exception) and only an unknown subcommand is refused.
#[test]
fn optional_groups_accept_the_bare_command_and_refuse_an_unknown_subcommand() {
    for (group, dest) in [
        ("owner", "owner_command"),
        ("report", "report_command"),
        ("state", "state_command"),
    ] {
        assert!(
            !command(&[group]).sub_required,
            "{group}: the Python group is not required"
        );
        let ns = try_parse(["dunkingsheep", group]).unwrap_or_else(|e| panic!("bare {group}: {e}"));
        // argparse leaves the dest as None; the generated table omits an absent subcommand's
        // dest, which the dispatcher reads the same way (`text(ns, dest)` is None).
        assert!(
            ns.get(dest).is_none_or(Value::is_null),
            "bare {group}: {:?}",
            ns.get(dest)
        );
        assert_eq!(
            code(&[group, "no-such-subcommand"]),
            Some(2),
            "{group} no-such-subcommand"
        );
    }
}

/// The names each group accepts are exactly the names `dispatch::run` and its helpers route.
/// A regenerated table that adds one fails here, not at a user with "not ported".
#[test]
fn the_table_accepts_exactly_the_subcommands_the_dispatcher_routes() {
    assert_eq!(
        sub_names(&["memory"]),
        names(&[
            "recall",
            "search",
            "context",
            "hook-context",
            "stats",
            "status",
            "ingest",
            "backfill",
            "queue"
        ])
    );
    assert_eq!(
        sub_names(&["superuser"]),
        names(&["status", "add", "remove", "recover"])
    );
    assert_eq!(sub_names(&["github"]), names(&["protect", "wrappers"]));
    assert_eq!(sub_names(&["pm"]), names(&["digest", "routing"]));
    assert_eq!(
        sub_names(&["poolrad"]),
        names(&["backlog-sync", "progress"])
    );
    assert_eq!(sub_names(&["review"]), names(&["sync", "list"]));
    assert_eq!(sub_names(&["token"]), names(&["issue", "revoke", "list"]));
    assert_eq!(sub_names(&["turn"]), names(&["start", "end"]));
    assert_eq!(
        sub_names(&["inbox"]),
        names(&[
            "count", "send", "list", "read", "recover", "archive", "status", "ack", "cancel",
            "delete", "stats", "fetch"
        ])
    );
    assert_eq!(
        sub_names(&["task"]),
        names(&[
            "add",
            "list",
            "read",
            "update",
            "assign",
            "complete",
            "block",
            "unblock",
            "cancel",
            "delete",
            "note",
            "next",
            "heartbeat",
            "release",
            "follow",
            "unfollow",
            "followers",
            "stats",
            "metrics",
            "reap",
            "backlog"
        ])
    );
    assert_eq!(
        sub_names(&["context"]),
        names(&[
            "init",
            "add",
            "promote",
            "supersede",
            "retire",
            "ledger",
            "get",
            "search",
            "digest",
            "snapshot",
            "check",
            "index",
            "stats"
        ])
    );
    // `run` routes owner actions to three helpers; every action the table accepts has a home.
    assert_eq!(
        sub_names(&["owner"]),
        names(&[
            "push",
            "poll",
            "take",
            "conversation",
            "handoff",
            "reply",
            "ingest",
            "outbox",
            "delivered",
            "stats"
        ])
    );
    assert_eq!(sub_names(&["report"]), names(&["run", "list"]));
    assert_eq!(sub_names(&["state"]), names(&["capture", "latest"]));
}

/// The top-level table is exactly the set `dispatch::run` matches (aliases aside), so the
/// final `other => not_ported(other)` arm is unreachable: an unknown command is the parser's
/// usage error.
#[test]
fn the_top_level_table_is_exactly_what_run_dispatches() {
    let expected = [
        "allocation",
        "status",
        "turn",
        "poll",
        "list",
        "get",
        "add",
        "update",
        "remove",
        "start",
        "stop",
        "toggle",
        "stop-all",
        "fire",
        "send",
        "send-keys",
        "install",
        "guard",
        "inbox",
        "context",
        "load",
        "pm",
        "task",
        "gh-token",
        "secrets-check",
        "git-credential",
        "review",
        "poolrad",
        "github",
        "whoami",
        "token",
        "superuser",
        "memory",
        "health",
        "watchdog",
        "watchdog-deadman",
        "usage",
        "usage-provider",
        "recover",
        "doctor",
        "owner",
        "report",
        "state",
        "roster",
        "message-state",
        "reply-stats",
        "project-summary",
        "api",
        "capability",
        "lane",
        "retired",
        "dispatch",
        "clear-box",
        "delivery",
        "flush",
        "messages",
        "cancel-message",
        "metrics",
        "panes",
        "workspaces",
        "new-workspace",
        "new-tab",
        "split",
        "start-agent",
        "respawn-pane",
        "rename",
        "focus",
        "close",
        "herdr",
        "herdr-help",
        "read",
        "shutdown",
        "serve",
        "tui",
        "mcp",
        "mcp-setup",
        "guide",
        "commands",
        "mcp-map",
    ];
    assert_eq!(
        ROOT.subs
            .iter()
            .map(|sub| sub.name)
            .collect::<BTreeSet<_>>(),
        names(&expected)
    );
    assert_eq!(code(&["no-such-command"]), Some(2));
}
