//! T44, slice 14: the LOCAL (no daemon) context writes: `context init`, `add`, `promote`,
//! `supersede`, `retire` with `--local`, and `ledger adopt` (`run_context_command`). The store,
//! the ledger and the write rules are `ds-core::context`; this is the command body.
//!
//! As in the Python, the four daemon-write subcommands run with NO ledger when `--local` (a
//! ground truth written this way is not injected until the owner runs `context ledger adopt`:
//! the daemon is the only path that records).
//!
//! Failure kinds, as the Python has three: `SystemExit("message")` (stderr, exit 1), `Refused`
//! (stderr, exit 2) and a store `ContextError`, which `main()` turns into `Refused("context: ...")`.
//!
//! DIFFERENCE: Python's `supersede <slug> FILE` with a missing FILE dies with a traceback
//! (exit 1); here it is a one-line `SystemExit` with the OS error.

use std::io::Read;
use std::path::{Path, PathBuf};

use ds_core::context::ledger::Ledger;
use ds_core::context::store::{AddArgs, Store, SupersedeMeta};
use serde_json::{json, Map, Value};

use crate::context_cmd::Out;

/// A failed local write and how the process exits.
#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    /// `SystemExit("message")` (or `SystemExit(1)` with an empty message): exit 1.
    SystemExit(String),
    /// `Refused(message)`: the message as is, exit 2.
    Refused(String),
    /// A store `ContextError`: `context: <message>`, exit 2.
    Context(String),
}

impl Fail {
    pub fn code(&self) -> i32 {
        match self {
            Fail::SystemExit(_) => 1,
            Fail::Refused(_) | Fail::Context(_) => 2,
        }
    }

    /// What goes to stderr (empty for `SystemExit(1)`).
    pub fn message(&self) -> String {
        match self {
            Fail::SystemExit(m) | Fail::Refused(m) => m.clone(),
            Fail::Context(m) => format!("context: {m}"),
        }
    }
}

fn ctx(error: ds_core::context::ContextError) -> Fail {
    Fail::Context(error.0)
}

fn text<'a>(ns: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ns.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn superuser() -> bool {
    std::env::var("DUNKINGSHEEP_SUPERUSER").as_deref() == Ok("1")
}

fn global(ns: &Map<String, Value>) -> bool {
    matches!(ns.get("global_store"), Some(Value::Bool(true)))
}

/// `add`'s body: `--text`, else stdin for `-`, else the file.
fn add_body(ns: &Map<String, Value>, stdin: &mut dyn Read) -> Result<String, Fail> {
    if let Some(t) = text(ns, "text") {
        return Ok(t.to_string());
    }
    let file = ns.get("file").and_then(Value::as_str).unwrap_or("-");
    if file == "-" {
        let mut buf = String::new();
        let _ = stdin.read_to_string(&mut buf);
        return Ok(buf);
    }
    std::fs::read_to_string(file).map_err(|_| {
        Fail::Refused(format!(
            "no such file: {} - the body comes from --text, stdin or a file; for text inline use: \
             dunkingsheep context add --text '<claim>' --class ... --subject ...",
            ds_core::context::store::py_repr(file)
        ))
    })
}

fn added(a: &ds_core::context::store::Added) -> Value {
    json!({"slug": a.slug, "path": a.path.display().to_string(), "class": a.cls})
}

/// Run one local context write. `ledger_path` is `None` for the default ledger (only `ledger
/// adopt` uses it).
pub fn context_write_local(
    sub: &str,
    ns: &Map<String, Value>,
    stdin: &mut dyn Read,
    ledger_path: Option<&Path>,
) -> Result<Out, Fail> {
    let path: Option<PathBuf> = text(ns, "path").map(PathBuf::from);
    let store = Store::new(path.as_deref(), global(ns));
    match sub {
        "init" => {
            let root = store.ensure().map_err(ctx)?.display().to_string();
            Ok(Out {
                text: format!("store at {root}\n"),
                value: json!({"root": root}),
            })
        }
        "ledger" => {
            // Only `adopt` writes; `status` is a read (context_cmd).
            if !superuser() {
                return Err(Fail::SystemExit(
                    "refused: `context ledger adopt` is the owner's bootstrap; run it with \
                     DUNKINGSHEEP_SUPERUSER=1 from the owner's terminal"
                        .into(),
                ));
            }
            let mut ledger = Ledger::new(ledger_path);
            let adopted = store.adopt(&mut ledger, "adopt");
            let ledger_file = ledger_path
                .map(Path::to_path_buf)
                .unwrap_or_else(ds_core::context::ledger::default_path);
            Ok(Out {
                text: format!(
                    "adopted {} entries into {}\n",
                    adopted.len(),
                    ledger_file.display()
                ),
                value: json!({"adopted": adopted, "ledger": ledger_file.display().to_string()}),
            })
        }
        "add" => {
            let body = add_body(ns, stdin)?;
            let args = AddArgs {
                cls: text(ns, "cls").unwrap_or(""),
                subject: text(ns, "subject").unwrap_or(""),
                type_: text(ns, "type_").unwrap_or("reference"),
                authority: text(ns, "authority"),
                source: text(ns, "source").unwrap_or(""),
                stated: text(ns, "stated"),
                recheck_after: text(ns, "recheck_after"),
                supersedes: text(ns, "supersedes"),
                slug: None,
                by: None,
            };
            let result = store.add(&body, &args, None).map_err(ctx)?;
            Ok(Out {
                text: format!("added {}/{}\n", result.cls, result.slug),
                value: added(&result),
            })
        }
        "promote" => {
            let slug = text(ns, "slug").unwrap_or("");
            let Some(entry) = store.find(slug) else {
                return Err(Fail::SystemExit(String::new()));
            };
            if !superuser() {
                return Err(Fail::SystemExit(
                    "refused: only an owner act promotes a ground truth".into(),
                ));
            }
            let source = entry.get("source").unwrap_or("").to_string();
            let args = AddArgs {
                cls: "ground-truth",
                subject: entry.get("subject").unwrap_or(""),
                type_: entry
                    .get("type")
                    .filter(|t| !t.is_empty())
                    .unwrap_or("principle"),
                authority: Some("owner"),
                source: &source,
                stated: None,
                recheck_after: None,
                supersedes: Some(&entry.slug),
                slug: None,
                by: None,
            };
            let result = store.add(&entry.body, &args, None).map_err(ctx)?;
            Ok(Out {
                text: format!("promoted {}\n", result.slug),
                value: added(&result),
            })
        }
        "supersede" => {
            let file = ns.get("file").and_then(Value::as_str).unwrap_or("-");
            let body = if file == "-" {
                let mut buf = String::new();
                let _ = stdin.read_to_string(&mut buf);
                buf
            } else {
                std::fs::read_to_string(file)
                    .map_err(|e| Fail::SystemExit(format!("{file}: {e}")))?
            };
            let slug = text(ns, "slug").unwrap_or("");
            let result = store
                .supersede(slug, &body, None, &SupersedeMeta::default(), None)
                .map_err(ctx)?;
            Ok(Out {
                text: format!("superseded by {}\n", result.slug),
                value: added(&result),
            })
        }
        "retire" => {
            let slug = text(ns, "slug").unwrap_or("");
            let reason = ns.get("reason").and_then(Value::as_str).unwrap_or("");
            let (slug, reason) = store.retire(slug, reason, None, None).map_err(ctx)?;
            Ok(Out {
                text: format!("retired {slug}\n"),
                value: json!({"slug": slug, "retired": true, "reason": reason}),
            })
        }
        other => Err(Fail::SystemExit(format!(
            "`context {other}` is not a write subcommand"
        ))),
    }
}
