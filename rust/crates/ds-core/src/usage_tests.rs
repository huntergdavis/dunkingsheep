//! Ported from tests/test_usage.py::UsageRunnerTests, same order and names.
use super::*;
use std::fs;

struct Tmp(tempfile::TempDir);

impl Tmp {
    fn script(&self, name: &str, body: &str) -> Value {
        let p = self.0.path().join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        json!({"command": format!("/bin/sh {}", p.display()), "timeout_s": 5})
    }
}

fn tmp() -> Tmp {
    Tmp(tempfile::tempdir().unwrap())
}

fn with_parse(mut spec: Value) -> Value {
    spec["parse"] =
        json!({"windows": "windows", "meaning": "meaning", "measured_at": "measured_at"});
    spec
}

#[test]
fn test_ok_result_and_parse() {
    let t = tmp();
    let spec = with_parse(t.script(
        "c.sh",
        r#"echo '{"provider":"x","status":"ok","meaning":"percent_used","measured_at":1,"windows":[{"name":"weekly","percent_used":15.0,"percent_remaining":85.0}]}'"#,
    ));
    let r = run_provider("x", &spec, None);
    assert_eq!(r["status"], "ok");
    assert_eq!(r["parsed"]["windows"][0]["percent_used"], 15.0);
    assert_eq!(r["parsed"]["measured_at"], 1);
}

#[test]
fn test_failed_command_surfaces_stderr() {
    let t = tmp();
    let r = run_provider(
        "x",
        &with_parse(t.script("c.sh", "printf boom >&2; exit 3")),
        None,
    );
    assert_eq!(r["status"], "error");
    assert_eq!(r["stderr"], "boom");
    assert_eq!(r["exit_code"], 3);
    assert!(render_text(&[r]).contains("UNKNOWN"));
}

#[test]
fn test_command_can_return_unknown_with_reason() {
    let t = tmp();
    let spec = with_parse(t.script(
        "c.sh",
        r#"echo '{"provider":"x","status":"unknown","meaning":"unknown","measured_at":1,"reason":"no endpoint"}'; exit 1"#,
    ));
    let r = run_provider("x", &spec, None);
    assert_eq!(r["status"], "unknown");
    let text = render_text(&[r]);
    assert!(text.contains("UNKNOWN") && text.contains("no endpoint"));
}

#[test]
fn test_timeout_is_an_error() {
    let t = tmp();
    let mut spec = t.script("slow.sh", "sleep 2");
    spec["timeout_s"] = json!(0.2);
    let r = run_provider("x", &spec, None);
    assert_eq!(r["status"], "error");
    assert!(r["error"].as_str().unwrap().contains("timed out"));
}

#[test]
fn test_invalid_json_is_an_error() {
    let t = tmp();
    let r = run_provider("x", &t.script("bad.sh", "echo 'not json'"), None);
    assert_eq!(r["status"], "error");
    assert!(r["error"].as_str().unwrap().contains("canonical JSON"));
}

#[test]
fn test_missing_parse_path_is_an_error() {
    let t = tmp();
    let mut spec = t.script(
        "c.sh",
        r#"echo '{"status":"ok","meaning":"percent_used","measured_at":1,"windows":[]}'"#,
    );
    spec["parse"] = json!({"missing": "not.there"});
    let r = run_provider("x", &spec, None);
    assert_eq!(r["status"], "error");
    assert!(r["error"].as_str().unwrap().contains("not.there"));
}

#[test]
fn test_no_cache_runs_the_command_each_time() {
    let t = tmp();
    let counter = t.0.path().join("count");
    let spec = t.script(
        "c.sh",
        &format!(
            r#"printf x >> {}; echo '{{"status":"ok","meaning":"percent_used","measured_at":1}}'"#,
            counter.display()
        ),
    );
    run_provider("x", &spec, None);
    run_provider("x", &spec, None);
    assert_eq!(fs::read_to_string(counter).unwrap().len(), 2);
}

#[test]
fn test_max_age_is_passed_to_the_command() {
    let t = tmp();
    let mut spec = t.script(
        "c.sh",
        r#"echo "{\"status\":\"ok\",\"meaning\":\"x\",\"measured_at\":1,\"age\":\"$DUNKINGSHEEP_USAGE_MAX_AGE_S\"}""#,
    );
    spec["max_age_s"] = json!(77);
    let r = run_provider("x", &spec, None);
    assert_eq!(r["age"].as_str().unwrap().parse::<f64>().unwrap(), 77.0);
}

#[test]
fn test_config_validation() {
    let t = tmp();
    let p = t.0.path().join("usage.json");
    let ps = p.to_str().unwrap();
    assert!(load_config(ps).is_err());
    for bad in [
        json!({"schema": 1, "providers": {}}),
        json!({"schema": 99, "providers": {"x": {"command": "true"}}}),
        json!({"schema": 1, "providers": {"x": {"command": 1}}}),
    ] {
        fs::write(&p, bad.to_string()).unwrap();
        assert!(load_config(ps).is_err(), "{bad}");
    }
    fs::write(
        &p,
        json!({"schema": 1, "providers": {"x": {"command": "true"}}}).to_string(),
    )
    .unwrap();
    assert!(load_config(ps).is_ok());
}

#[test]
fn test_unknown_provider_is_a_clear_error() {
    let config = json!({"schema": 1, "providers": {"a": {"command": "true"}}});
    let e = run(&config, Some("b"), None).unwrap_err();
    assert!(e.to_string().contains("unknown provider"));
}

#[test]
fn shlex_matches_python() {
    assert_eq!(
        shlex_split(r#"a "b c" 'd e' f\ g"#).unwrap(),
        ["a", "b c", "d e", "f g"]
    );
    assert!(shlex_split("a 'b").is_err());
}
