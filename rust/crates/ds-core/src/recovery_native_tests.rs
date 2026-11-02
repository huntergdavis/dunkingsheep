//! Identity-only recovery session reader tests ported from the named wiring cases in Python.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::*;

const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    hooks: PathBuf,
    rollout: PathBuf,
    bindings: PathBuf,
    cwd: PathBuf,
    pane: Value,
    entries: Vec<PaneProcess>,
    environment: BTreeMap<String, String>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ds-recovery-native-")
            .tempdir()
            .unwrap();
        let home = dir.path().join("codex-home");
        let hooks = home.join("hooks");
        let cwd = dir.path().join("workspace");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
        std::fs::write(
            &rollout,
            format!(
                "{}\n",
                json!({
                    "type": "session_meta",
                    "payload": {"session_id": SESSION, "cwd": cwd},
                })
            ),
        )
        .unwrap();
        let db = Connection::open(home.join("state_5.sqlite")).unwrap();
        db.execute(
            "create table threads (id text primary key, rollout_path text, cwd text)",
            [],
        )
        .unwrap();
        db.execute(
            "insert into threads values (?,?,?)",
            (SESSION, rollout.to_str().unwrap(), cwd.to_str().unwrap()),
        )
        .unwrap();

        let pid = i64::from(std::process::id());
        let start = proc::process_start(pid).expect("the test process has /proc identity");
        let pane = json!({
            "pane_id": "w1:p2",
            "pane_pid": pid,
            "agent": "codex",
            "cwd": cwd,
        });
        let entries = vec![PaneProcess {
            pid,
            comm: "codex".into(),
            state: "S".into(),
            start: Some(start),
            argv: vec!["codex".into()],
        }];
        let bindings = dir.path().join("bindings.json");

        Self {
            _dir: dir,
            home: home.clone(),
            hooks: hooks.clone(),
            rollout,
            bindings,
            cwd,
            pane,
            entries,
            environment: BTreeMap::new(),
        }
    }

    fn pid(&self) -> i64 {
        self.entries[0].pid
    }

    fn start(&self) -> &str {
        self.entries[0].start.as_deref().unwrap()
    }

    fn binding_record(&self) -> Value {
        json!({
            "pid": self.pid(),
            "start": self.start(),
            "cwd": self.cwd,
            "codex_home": self.home,
            "session_id": SESSION,
        })
    }

    fn hook_record(&self) -> Value {
        json!({
            "session_id": SESSION,
            "pane_id": self.pane["pane_id"],
            "root_pid": self.pid(),
            "root_start": self.start(),
            "cwd": self.cwd,
        })
    }

    fn resolve(
        &self,
        binding_path: Option<&Path>,
    ) -> Result<PromptSessionBinding, NativeSessionError> {
        resolve_prompt_session_from(&self.pane, &self.entries, &self.environment, binding_path)
    }

    fn binding(&self) -> PromptSessionBinding {
        std::fs::write(
            &self.bindings,
            json!({"w1:p2": self.binding_record()}).to_string(),
        )
        .unwrap();
        self.resolve(Some(&self.bindings)).unwrap()
    }

    fn write_rollout(&self, events: &[Value]) {
        let mut lines = vec![json!({
            "type": "session_meta",
            "payload": {"session_id": SESSION, "cwd": self.cwd},
        })];
        lines.extend(events.iter().cloned());
        let text = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&self.rollout, format!("{text}\n")).unwrap();
    }
}

#[test]
fn test_legacy_bare_codex_requires_generation_bound_registration() {
    let fixture = Fixture::new();
    let refusal = fixture.resolve(None).unwrap_err();
    assert!(refusal
        .to_string()
        .contains("no process-bound native session id"));

    std::fs::write(
        &fixture.bindings,
        json!({"w1:p2": fixture.binding_record()}).to_string(),
    )
    .unwrap();
    let session = fixture.resolve(Some(&fixture.bindings)).unwrap();
    assert_eq!(fixture.pid(), session.pid);
    assert_eq!(fixture.start(), session.start);
    assert_eq!(SESSION, session.session_id);
    assert_eq!(fixture.cwd.to_string_lossy(), session.cwd);

    let mut stale = fixture.binding_record();
    stale["start"] = json!("stale");
    std::fs::write(&fixture.bindings, json!({"w1:p2": stale}).to_string()).unwrap();
    assert!(fixture
        .resolve(Some(&fixture.bindings))
        .unwrap_err()
        .to_string()
        .contains("wrong process generation/cwd"));
}

#[test]
fn test_different_native_process_binding_not_adopted() {
    let fixture = Fixture::new();
    let mut record = fixture.binding_record();
    record["pid"] = json!(fixture.pid() + 1);
    std::fs::write(&fixture.bindings, json!({"w1:p2": record}).to_string()).unwrap();
    let error = fixture.resolve(Some(&fixture.bindings)).unwrap_err();
    assert!(error.to_string().contains("wrong process generation/cwd"));
}

#[test]
fn test_hook_session_bound_to_root_pid_start_and_pane() {
    let mut fixture = Fixture::new();
    fixture.environment.insert(
        CODEX_HOME.into(),
        fixture.home.to_string_lossy().into_owned(),
    );
    fixture.environment.insert(
        STATE_DIR.into(),
        fixture.hooks.to_string_lossy().into_owned(),
    );
    let hook = fixture.hooks.join(format!("{SESSION}.json"));
    std::fs::write(&hook, fixture.hook_record().to_string()).unwrap();

    let resolved = fixture.resolve(None).unwrap();
    assert_eq!(SESSION, resolved.session_id);
    assert_eq!(fixture.pid(), resolved.pid);

    let mut stale = fixture.hook_record();
    stale["root_start"] = json!("stale");
    std::fs::write(&hook, stale.to_string()).unwrap();
    assert!(fixture
        .resolve(None)
        .unwrap_err()
        .to_string()
        .contains("no unique process-bound native session id"));

    // An unrelated hook/session file is never adopted based on recency or filename.
    std::fs::write(
        fixture.hooks.join("newest.json"),
        json!({"session_id":"newest"}).to_string(),
    )
    .unwrap();
    assert!(fixture.resolve(None).is_err());
}

#[test]
fn test_read_prompt_session_decodes_python_message_bodies_in_source_order() {
    let fixture = Fixture::new();
    fixture.write_rollout(&[
        json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "content": [
                    {"type": "input_text", "text": "quoted \"escape\"\nline Ω"},
                    {"type": "output_text", "text": "second"}
                ]
            }
        }),
        json!({"type":"response_item", "payload":{"type":"function_call","name":"ignored"}}),
        json!({"type":"event_msg", "payload":{"type":"user_message", "message":"question"}}),
        json!({"type":"event_msg", "payload":{"type":"agent_message", "message":"answer"}}),
        json!({"type":"event_msg", "payload":{"type":"token_count"}}),
    ]);
    let binding = fixture.binding();
    let cache = Mutex::new(PromptSessionCache::default());
    let session = read_prompt_session_from_binding(&binding, &cache).unwrap();
    assert_eq!(
        "quoted \"escape\"\nline Ω\nsecond\nquestion\nanswer",
        session.text
    );
    assert_eq!(binding.pid, session.pid);
    assert_eq!(binding.start, session.start);
    session.assert_unchanged().unwrap();
}

#[test]
fn test_prompt_text_conversion_matches_python_truthiness_and_str_fixtures() {
    assert_eq!("", prompt_str_or_empty(None));
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!(0.0),
        json!(""),
        json!([]),
        json!({}),
    ] {
        assert_eq!("", prompt_str_or_empty(Some(&value)), "{value}");
    }
    let cases = [
        (json!(true), "True"),
        (json!(7), "7"),
        (json!(1.5), "1.5"),
        (json!("escaped\nΩ"), "escaped\nΩ"),
        (json!(["x", null, true]), "['x', None, True]"),
        (json!({"ok": true}), "{'ok': True}"),
    ];
    for (value, expected) in cases {
        assert_eq!(expected, prompt_str_or_empty(Some(&value)), "{value}");
    }
}

#[test]
fn test_prompt_python_repr_escapes_nonprintable_unicode_like_python() {
    let cases = [
        ("\u{00a0}", "'\\xa0'"),
        ("\u{2028}", "'\\u2028'"),
        ("\u{200b}", "'\\u200b'"),
        ("\u{e000}", "'\\ue000'"),
        ("\u{0378}", "'\\u0378'"),
        ("\u{e0001}", "'\\U000e0001'"),
        ("plain space", "'plain space'"),
        ("cafe\u{0301} ☕", "'cafe\u{0301} ☕'"),
        ("a'b", "\"a'b\""),
        ("a\"b'c", "'a\"b\\'c'"),
        ("\u{007f}", "'\\x7f'"),
    ];
    for (input, expected) in cases {
        assert_eq!(expected, prompt_python_string_repr(input), "{input:?}");
    }

    let list = json!(["\u{00a0}"]);
    assert_eq!("['\\xa0']", prompt_str_or_empty(Some(&list)));
    let dictionary = json!({"\u{00a0}": "value"});
    assert_eq!("{'\\xa0': 'value'}", prompt_str_or_empty(Some(&dictionary)));
}

#[test]
fn test_prompt_python_printability_table_matches_all_unicode_scalars() {
    assert_eq!("3.14.4", python_printability::PYTHON_VERSION);
    assert_eq!("16.0.0", python_printability::UNICODE_VERSION);

    let mut digest = Sha256::new();
    for codepoint in 0..=0x10ffff {
        if (0xd800..=0xdfff).contains(&codepoint) {
            continue;
        }
        let character = char::from_u32(codepoint).expect("Unicode scalar");
        digest.update([u8::from(python_printability::is_printable(character))]);
    }
    let actual = format!("{:x}", digest.finalize());
    assert_eq!(python_printability::PRINTABILITY_SHA256, actual);
}

#[test]
fn test_read_prompt_session_refuses_active_turns_and_untrusted_rollouts() {
    let fixture = Fixture::new();
    let binding = fixture.binding();
    let cache = Mutex::new(PromptSessionCache::default());

    fixture.write_rollout(&[json!({
        "type":"event_msg",
        "payload":{"type":"task_started", "turn_id":"active"}
    })]);
    assert!(read_prompt_session_from_binding(&binding, &cache)
        .unwrap_err()
        .to_string()
        .contains("active turn"));

    std::fs::write(&fixture.rollout, "{\"type\":\"session_meta\"").unwrap();
    assert!(read_prompt_session_from_binding(&binding, &cache).is_err());

    std::fs::write(&fixture.rollout, "{not-json}\n").unwrap();
    assert!(read_prompt_session_from_binding(&binding, &cache).is_err());

    fixture.write_rollout(&[]);
    let mismatched_session = format!(
        "{}\n",
        json!({"type":"session_meta", "payload":{"session_id":"foreign", "cwd":fixture.cwd}})
    );
    std::fs::write(&fixture.rollout, mismatched_session).unwrap();
    assert!(read_prompt_session_from_binding(&binding, &cache).is_err());

    fixture.write_rollout(&[]);
    let mismatched_cwd = format!(
        "{}\n",
        json!({"type":"session_meta", "payload":{"session_id":SESSION, "cwd":"/foreign"}})
    );
    std::fs::write(&fixture.rollout, mismatched_cwd).unwrap();
    assert!(read_prompt_session_from_binding(&binding, &cache).is_err());
}

#[test]
fn test_read_prompt_session_refuses_file_and_process_changes_between_checks() {
    let fixture = Fixture::new();
    fixture.write_rollout(&[json!({
        "type":"event_msg", "payload":{"type":"user_message", "message":"before"}
    })]);
    let binding = fixture.binding();
    let cache = Mutex::new(PromptSessionCache::default());
    let mut checks = 0;
    let result = read_prompt_session_from_binding_with(
        &binding,
        &cache,
        || {},
        || {
            checks += 1;
            if checks == 2 {
                let mut text = std::fs::read_to_string(&fixture.rollout).unwrap();
                text.push_str(
                    &format!(
                        "{}\n",
                        json!({"type":"event_msg", "payload":{"type":"agent_message", "message":"after"}})
                    ),
                );
                std::fs::write(&fixture.rollout, text).unwrap();
            }
            true
        },
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("changed after the quiescence check"));

    fixture.write_rollout(&[]);
    let mut checks = 0;
    let result = read_prompt_session_from_binding_with(
        &binding,
        &Mutex::new(PromptSessionCache::default()),
        || {},
        || {
            checks += 1;
            checks == 1
        },
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("bound Codex process changed"));
}

#[test]
fn test_prompt_session_cache_hit_revalidates_process_file_and_evicts_ninth_entry() {
    let fixture = Fixture::new();
    let binding = fixture.binding();
    let cache = Mutex::new(PromptSessionCache::default());
    let first = read_prompt_session_from_binding(&binding, &cache).unwrap();
    assert_eq!(
        first,
        read_prompt_session_from_binding(&binding, &cache).unwrap()
    );

    let mut process_checks = 0;
    let result = read_prompt_session_from_binding_with(
        &binding,
        &cache,
        || {},
        || {
            process_checks += 1;
            false
        },
    );
    assert_eq!(1, process_checks);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("bound Codex process changed"));

    let result = read_prompt_session_from_binding_with(
        &binding,
        &cache,
        || {
            let mut text = std::fs::read_to_string(&fixture.rollout).unwrap();
            text.push_str(&format!(
                "{}\n",
                json!({"type":"event_msg", "payload":{"type":"agent_message", "message":"changed"}})
            ));
            std::fs::write(&fixture.rollout, text).unwrap();
        },
        || true,
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("changed after the quiescence check"));

    let mut keys = Vec::new();
    for index in 0..9 {
        let session_id = format!("session-{index}");
        let path = fixture._dir.path().join(format!("rollout-{index}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"session_meta", "payload":{"session_id":session_id, "cwd":fixture.cwd}})
            ),
        )
        .unwrap();
        let other = PromptSessionBinding {
            session_id,
            rollout_path: path.to_string_lossy().into_owned(),
            ..binding.clone()
        };
        keys.push(cache_key(
            &other,
            native_session::signature(&std::fs::metadata(&path).unwrap()),
        ));
        read_prompt_session_from_binding(&other, &cache).unwrap();
    }
    let cache = cache.lock().unwrap();
    assert_eq!(8, cache.entries.len());
    assert!(!cache.entries.iter().any(|(key, _)| *key == keys[0]));
    assert!(cache.entries.iter().any(|(key, _)| *key == keys[8]));
}

#[test]
fn test_prompt_session_assert_unchanged_checks_process_start_identity() {
    let fixture = Fixture::new();
    let binding = fixture.binding();
    let snapshot = native_session::read_rollout(
        &binding.rollout_path,
        &binding.session_id,
        Some(&binding.cwd),
    )
    .unwrap();
    let evidence = PromptSession {
        snapshot: NativeSessionSnapshot {
            session_id: binding.session_id,
            rollout_path: binding.rollout_path,
            signature: snapshot.signature,
            active_turns: snapshot.active,
            started_turns: snapshot.started,
            completed_turns: snapshot.completed,
        },
        text: String::new(),
        pid: binding.pid,
        start: "replaced-start-time".into(),
    };
    assert!(evidence
        .assert_unchanged()
        .unwrap_err()
        .to_string()
        .contains("bound Codex process changed"));
}
