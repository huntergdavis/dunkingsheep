//! T44, slice 2: the READ side of `dunkingsheep context ...` (`run_context_command` in the
//! script): `get`, `search`, `digest`, `check`, `index`, `stats`, `ledger` (status). Local
//! files only: no socket, no daemon. The store, the ledger, the index and the digest are
//! `ds-core::context` (T50); this is the command body and its printers.
//!
//! Each command returns the JSON result AND the exact text the Python printer prints (so
//! `--json` and the human form come from one call, as in the Python).
//!
//! NOT here: `init`, `add`, `promote`, `supersede`, `retire`, `ledger adopt` (writes), and
//! `snapshot` (its human form is `json.dumps(indent=2)` of an insertion-ordered dict, and
//! serde_json's maps are key-sorted: held with the other json.dumps printers pending the
//! owner's `preserve_order` decision). Python's `SystemExit("message")` is `Err(Refused)`.

use std::path::{Path, PathBuf};

use ds_core::context::digest::{digest_both, select_stores};
use ds_core::context::ledger::Ledger;
use ds_core::context::store::{Entry, Store, CLASSES};
use ds_core::error::DunkError;
use serde_json::{json, Map, Value};

use crate::render_fleet::py_str;

/// The result of one command: JSON for `--json`, text for the human form.
#[derive(Debug, Clone, PartialEq)]
pub struct Out {
    pub value: Value,
    pub text: String,
}

fn refused<T>(text: String) -> Result<T, DunkError> {
    Err(DunkError::Refused(text))
}

fn ctx_err(error: ds_core::context::ContextError) -> DunkError {
    DunkError::Refused(error.0)
}

fn flag(ns: &Map<String, Value>, key: &str) -> bool {
    matches!(ns.get(key), Some(Value::Bool(true)))
}

fn text<'a>(ns: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ns.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `which = "global" if args.global_store else "project" if args.project else None`.
fn which(ns: &Map<String, Value>) -> Option<&'static str> {
    if flag(ns, "global_store") {
        Some("global")
    } else if flag(ns, "project") {
        Some("project")
    } else {
        None
    }
}

/// `entry` as the Python's dict: the front matter, then `slug`, `path`, `body`.
fn entry_value(entry: &Entry) -> Value {
    let mut map = Map::new();
    for (key, value) in &entry.meta {
        map.insert(
            key.clone(),
            value.as_ref().map_or(Value::Null, |v| json!(v)),
        );
    }
    map.insert("slug".into(), json!(entry.slug));
    map.insert("path".into(), json!(entry.path.display().to_string()));
    map.insert("body".into(), json!(entry.body));
    Value::Object(map)
}

fn ljust(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(s.chars().count())))
}

/// Python's `repr()` of the `counts` dict, in `CLASSES` order (the Python builds it that way).
fn counts_repr(counts: &Value) -> String {
    let parts: Vec<String> = CLASSES
        .iter()
        .map(|class| {
            format!(
                "'{class}': {}",
                py_str(counts.get(*class).unwrap_or(&json!(0)))
            )
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// Run one read-side `context` subcommand. `ns` is the argparse namespace as a dictionary
/// (`path`, `global_store`, `project`, `key`, `query`, `cls`, `scope`, `subject`,
/// `max_bytes`, `action`); `ledger_path` is `None` for the default ledger.
pub fn context_read(
    sub: &str,
    ns: &Map<String, Value>,
    ledger_path: Option<&Path>,
    now: chrono::NaiveDateTime,
) -> Result<Out, DunkError> {
    let path: Option<PathBuf> = text(ns, "path").map(PathBuf::from);
    let path = path.as_deref();
    match sub {
        "ledger" => {
            if text(ns, "action") == Some("adopt") {
                return refused(
                    "`context ledger adopt` is a write; not ported in this slice".into(),
                );
            }
            let store = Store::new(path, flag(ns, "global_store"));
            let mut ledger = Ledger::new(ledger_path);
            let mut rows = Vec::new();
            let mut out = String::new();
            for entry in store.live_entries() {
                let body = std::fs::read_to_string(&entry.path).unwrap_or_default();
                let state = ledger
                    .status(&store.root, &entry.slug, &body)
                    .map_err(|e| DunkError::Refused(e.to_string()))?;
                let class = entry.get("class").unwrap_or("").to_string();
                out.push_str(&format!(
                    "{} {} {}\n",
                    ljust(state.as_str(), 11),
                    ljust(&class, 13),
                    entry.slug
                ));
                rows.push(json!({"slug": entry.slug, "class": class, "state": state.as_str()}));
            }
            let count = ledger
                .count()
                .map_err(|e| DunkError::Refused(e.to_string()))?;
            Ok(Out {
                value: json!({"entries": rows, "count": count}),
                text: out,
            })
        }
        "get" => {
            let key = text(ns, "key").unwrap_or("");
            let stores = select_stores(None, which(ns));
            for (name, store) in &stores {
                if let Some(entry) = store.resolve(key).map_err(ctx_err)? {
                    store.log_access(&entry.slug, None, "read");
                    let class = entry.get("class").unwrap_or("None");
                    let authority = entry.get("authority").unwrap_or("None");
                    return Ok(Out {
                        text: format!(
                            "# {} ({class}, {authority}, {name})\n\n{}\n",
                            entry.slug, entry.body
                        ),
                        value: entry_value(&entry),
                    });
                }
            }
            let names: Vec<&str> = stores.iter().map(|(n, _)| n.as_str()).collect();
            refused(format!(
                "no entry {} in {}",
                ds_core::context::store::py_repr(key),
                if names.is_empty() {
                    "any store".to_string()
                } else {
                    names.join(", ")
                }
            ))
        }
        "search" => {
            let query = text(ns, "query").unwrap_or("");
            let stores = select_stores(None, which(ns));
            let mut rows = Vec::new();
            let mut out = String::new();
            for (name, store) in &stores {
                for mut row in store
                    .search(query, text(ns, "cls"), text(ns, "scope"), 10)
                    .map_err(ctx_err)?
                {
                    row["store"] = json!(name);
                    out.push_str(&format!(
                        "{} {} {} {}\n",
                        ljust(name, 8),
                        ljust(&py_str(&row["class"]), 13),
                        ljust(&py_str(&row["slug"]), 40),
                        py_str(&row["subject"])
                    ));
                    rows.push(row);
                }
            }
            Ok(Out {
                value: Value::Array(rows),
                text: out,
            })
        }
        "digest" => {
            let max_bytes = ns.get("max_bytes").and_then(Value::as_u64).unwrap_or(8192) as usize;
            let mut ledger = Ledger::new(ledger_path);
            let digest = digest_both(
                path,
                which(ns),
                max_bytes,
                text(ns, "subject"),
                true,
                &mut ledger,
                now,
            );
            Ok(Out {
                value: json!(digest),
                text: format!("{digest}\n"),
            })
        }
        "check" | "index" | "stats" => {
            let stores = select_stores(path, which(ns));
            let mut value = Map::new();
            let mut out = String::new();
            for (name, store) in &stores {
                let root = store.root.display().to_string();
                match sub {
                    "index" => {
                        let (indexed, index_path) = store.index().map_err(ctx_err)?;
                        let index_path = index_path.display().to_string();
                        value.insert(
                            name.clone(),
                            json!({"path": root, "result": {"indexed": indexed, "path": index_path}}),
                        );
                        out.push_str(&format!("{name}: indexed {indexed} -> {index_path}\n"));
                    }
                    "check" => {
                        let mut ledger = Ledger::new(ledger_path);
                        let result = store.check(&mut ledger).map_err(ctx_err)?;
                        let counts = result.get("counts").cloned().unwrap_or(json!({}));
                        let total: i64 = counts
                            .as_object()
                            .map(|o| o.values().filter_map(Value::as_i64).sum())
                            .unwrap_or(0);
                        let len =
                            |k: &str| result.get(k).and_then(Value::as_array).map_or(0, Vec::len);
                        value.insert(
                            name.clone(),
                            json!({"path": root, "counts": counts, "refusals": len("refusals"),
                                   "stale": len("stale"), "entries": total}),
                        );
                        out.push_str(&format!(
                            "{name} ({root}): entries {total}, refusals {}, stale {}\n",
                            len("refusals"),
                            len("stale")
                        ));
                    }
                    _ => {
                        let result = store.stats().map_err(ctx_err)?;
                        let get = |k: &str| result.get(k).cloned().unwrap_or(Value::Null);
                        out.push_str(&format!(
                            "{name} ({root}): counts {}, injected_7d {}, reads_7d {}, never_delivered {}, never_read {}\n",
                            counts_repr(&get("counts")),
                            py_str(&get("injected_7d")),
                            py_str(&get("reads_7d")),
                            py_str(&get("never_delivered_count")),
                            py_str(&get("never_read_count"))
                        ));
                        value.insert(
                            name.clone(),
                            json!({"path": root, "counts": get("counts"), "reads_7d": get("reads_7d"),
                                   "injected_7d": get("injected_7d"), "never_read_count": get("never_read_count"),
                                   "never_read": get("never_read"),
                                   "never_delivered_count": get("never_delivered_count")}),
                        );
                    }
                }
            }
            Ok(Out {
                value: Value::Object(value),
                text: out,
            })
        }
        other => refused(format!(
            "`context {other}` is not a read-side command (or not ported yet)"
        )),
    }
}
