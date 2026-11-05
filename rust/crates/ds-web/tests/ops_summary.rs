#[path = "../src/ops_summary.rs"]
mod ops_summary;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::{path::Path, process::Command};
use tempfile::tempdir;

const PY_ORACLE: &str = r#"
import ast, calendar, json, math, sqlite3, sys
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo
from pathlib import Path

repo, db_path, host_path, opencode_path = map(Path, sys.argv[1:5])
blob_payload = len(sys.argv) > 5 and sys.argv[5] == 'blob'
source = (repo / 'bridges/ops_metrics.py').read_text()
tree = ast.parse(source)
names = {'iso','parse_time','_unique_authored_trial','experiment_summary','host_health','opencode_usage','summary'}
nodes = [node for node in tree.body if isinstance(node, ast.Assign)
         and any(isinstance(target, ast.Name) and target.id == 'SCHEMA' for target in node.targets)]
nodes += [node for node in tree.body if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name in names]
module = ast.Module(body=nodes, type_ignores=[])
namespace = {'PROJECT':'poolrad','HOST_WATCH_STATE':host_path,'OPENCODE_DB':opencode_path,
             'datetime':datetime,'timedelta':timedelta,'timezone':timezone,
             'calendar':calendar,'math':math,'json':json,'sqlite3':sqlite3,
             'ZoneInfo':ZoneInfo,'Path':Path}
exec(compile(ast.fix_missing_locations(module), '<ops_metrics_ast_fixture>', 'exec'), namespace)
conn = sqlite3.connect(db_path)
conn.row_factory = sqlite3.Row
conn.executescript(namespace['SCHEMA'])
conn.execute("INSERT INTO collection_runs(id,observed_at,panes_state,github_state,vp_messages_state,dunk_metrics_state,accounting_state,agent_messages_state,errors_json) VALUES(1,'2025-04-15T11:59:00Z','ok','ok','ok','ok','ok','ok','{}')")
conn.executemany("INSERT INTO pane_samples(run_id,pane_id,project,agent_kind,agent_status,agent_name,lane_name,role) VALUES(1,?,?,?,?,?,?,?)", [
    ('%1','poolrad','codex','running','builder','lane-a','coder'),
    ('%2','poolrad','codex','idle','reviewer','lane-b','reviewer')])
conn.executemany("INSERT INTO quota_observations(project,provider,cohort,pane_id,observed_at,precision,weekly_percent_left,reset_at,evidence_ref) VALUES(?,?,?,?,?,?,?,?,?)", [
    ('poolrad','codex','weekly','%1','2025-04-15T10:00:00Z','reported',90,'2025-04-20T00:00:00Z','q1'),
    ('poolrad','codex','weekly','%1','2025-04-15T11:00:00Z','reported',80,'2025-04-20T00:00:00Z','q2')])
conn.execute("INSERT INTO subscription_costs(project,provider,monthly_usd_cents,observed_at,evidence_ref) VALUES('poolrad','codex',9999,'2025-04-01T00:00:00Z','sub')")
conn.execute("INSERT INTO goal_observations(project,scope,goal_key,observed_at,status,value,target,unit,evidence_ref,note) VALUES('poolrad','local','coverage','2025-04-15T11:00:00Z','in_progress',1.25,2.5,'bodies','goal','fixture')")
conn.executemany("INSERT INTO accounting_snapshots(project,observed_at,commit_oid,strict_full_c,verified_game_identities,ceiling,bound_kind,source_signature,evidence_ref) VALUES('poolrad',?,?,?,?,815,'upper_bound','sig','docs/re/test')", [
    ('2025-04-15T10:30:00Z','a'*40,50,60),('2025-04-15T11:30:00Z','b'*40,60,70)])
conn.execute("INSERT INTO pr_latest(project,number,state,head_oid,merged_at,first_seen_at,last_seen_at) VALUES('poolrad',42,'OPEN',NULL,NULL,'2025-04-01T00:00:00Z','2025-04-15T11:00:00Z')")
payload = sqlite3.Binary(b'{"events":3}') if blob_payload else '{"events":3}'
conn.execute("INSERT INTO dunk_metrics_snapshots(observed_at,generated_at,interval_start,interval_end,schema_version,payload_json) VALUES('2025-04-15T11:50:00Z','2025-04-15T11:50:00Z','2025-04-15T11:00:00Z','2025-04-15T11:50:00Z',1,?)", (payload,))
conn.execute("INSERT INTO vp_message_counts(run_id,channel,role,total) VALUES(1,'cos','pm',4)")
conn.executemany("INSERT INTO agent_message_events(source_event_id,occurred_at,pane_id,kind,delivery_source,source_ref,mailbox_alias) VALUES(?,?,?,?,?,?,?)", [
    (1,'2025-04-15T10:20:00Z','%1','inbox.sent','test','e1','builder'),
    (2,'2025-04-15T10:30:00Z','%1','message.sent','test','e2',None)])
conn.execute("INSERT INTO agent_trials(trial_id,project,pane_id,agent_type,provider,model,reasoning_effort,hypothesis,started_at,ended_at,input_tokens,output_tokens,metered_usd,evidence_ref) VALUES('trial-1','poolrad','%1','Codex','openai','gpt-6-sol','low','fixture','2025-04-15T10:00:00Z','2025-04-15T11:00:00Z',10,20,1.25,'trial')")
conn.executemany("INSERT INTO agent_trials(trial_id,project,pane_id,agent_type,provider,model,reasoning_effort,hypothesis,started_at,ended_at,metered_usd,evidence_ref) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)", [
    ('trial-ss1','poolrad','%1','Straße','openai','model-x','low','casefold','2025-04-15T09:00:00Z','2025-04-15T09:30:00Z',0.0,'casefold-1'),
    ('trial-ss2','poolrad','%1','STRASSE','openai','model-x','LOW','casefold','2025-04-15T08:00:00Z','2025-04-15T08:30:00Z',0.0,'casefold-2')])
conn.executemany("INSERT INTO function_body_events(project,function_id,status,pane_id,trial_id,source_head,evidence_ref,reviewed_by,occurred_at) VALUES('poolrad',?,?,?,?,?,?,?,?)", [
    ('f-one','authored','%193','trial-1','c'*40,'authored',None,'2025-04-15T10:10:00Z'),
    ('f-one','accepted','%193',None,'c'*40,'accepted','reviewer','2025-04-15T11:10:00Z')])
conn.commit()
host_path.write_text(json.dumps({'levels':{'load':'ok'},'last_sample':{
    'at':1744718280,'tmp_used_fraction':0.5,'tmp_free_bytes':1324997412,
    'tmp_inode_used_fraction':0.25,'load1':1.0,'load5':0.8,'cpus':8,
    'cpu_psi_some_avg60':0.01,'process_count':123}}))
oc = sqlite3.connect(opencode_path)
oc.execute('CREATE TABLE session(time_created INTEGER,cost REAL,time_updated INTEGER)')
oc.executemany('INSERT INTO session VALUES(?,?,?)', [(1744710000000,0.1255,1744713600000),(1742000000000,2.0,1742000000000)])
oc.commit(); oc.close()
now=datetime.fromisoformat('2025-04-15T12:00:00+00:00')
print(json.dumps(namespace['summary'](conn, now=now),separators=(',',':')))
"#;

const PY_OPTIONAL_ORACLE: &str = r#"
import ast, json, sqlite3, sys
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo
repo, host_missing, opencode_missing, invalid_host, stale_host, invalid_db, fractional_host = map(Path,sys.argv[1:])
tree=ast.parse((repo/'bridges/ops_metrics.py').read_text())
names={'iso','host_health','opencode_usage'}
nodes=[node for node in tree.body if isinstance(node,(ast.FunctionDef,ast.AsyncFunctionDef)) and node.name in names]
namespace={'datetime':datetime,'timezone':timezone,'Path':Path,'ZoneInfo':ZoneInfo,
           'json':json,'sqlite3':sqlite3}
exec(compile(ast.fix_missing_locations(ast.Module(body=nodes,type_ignores=[])),'<ops_optional_fixture>','exec'),namespace)
now=datetime.fromisoformat('2025-04-15T12:00:00+00:00')
print(json.dumps({
 'missing_host':namespace['host_health'](now,path=host_missing),
 'missing_opencode':namespace['opencode_usage'](now,path=opencode_missing),
 'invalid_host':namespace['host_health'](now,path=invalid_host),
 'fractional_host':namespace['host_health'](now,path=fractional_host),
 'stale_host':namespace['host_health'](now,path=stale_host),
 'invalid_db':namespace['opencode_usage'](now,path=invalid_db)
},separators=(',',':')))
"#;

fn fixture_paths() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let directory = tempdir().expect("private fixture directory");
    let db = directory.path().join("ops.sqlite");
    let host = directory.path().join("host.json");
    let opencode = directory.path().join("opencode.sqlite");
    (directory, db, host, opencode)
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonical_json(value));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        other => other,
    }
}

fn setup_oracle(repo: &Path, db: &Path, host: &Path, opencode: &Path, blob_payload: bool) -> Value {
    let output = Command::new("python3")
        .arg("-c")
        .arg(PY_ORACLE)
        .arg(repo)
        .arg(db)
        .arg(host)
        .arg(opencode)
        .arg(if blob_payload { "blob" } else { "text" })
        .output()
        .expect("run AST-isolated Python fixture");
    assert!(
        output.status.success(),
        "Python fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python summary JSON")
}

#[test]
fn summary_matches_ast_isolated_python_fixture_with_real_sources_and_rounding() {
    let (_dir, db, host, opencode) = fixture_paths();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let expected = setup_oracle(&repo, &db, &host, &opencode, false);
    let conn = Connection::open(&db).expect("fixture operations database");
    let now = DateTime::parse_from_rfc3339("2025-04-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let actual = ops_summary::summary(&conn, now, &host, &opencode).expect("summary");
    assert_eq!(canonical_json(actual.clone()), canonical_json(expected));
    assert_eq!(
        actual["quotas"]["codex"]["burn_percentage_points_per_hour"],
        10.0
    );
    assert_eq!(actual["experiment_lab"]["bodies"][0]["trial_id"], "trial-1");
    assert_eq!(actual["opencode_usage"]["month_estimated_usd"], 0.1255);
    let groups = actual["experiment_lab"]["efficiency_by_type_model"]
        .as_array()
        .unwrap();
    let casefold_group = groups
        .iter()
        .find(|group| group["agent_type"] == "strasse" && group["model"] == "model-x")
        .unwrap();
    assert_eq!(casefold_group["trials"], 2);
}

#[test]
fn optional_readers_preserve_missing_invalid_and_stale_semantics() {
    let now = DateTime::parse_from_rfc3339("2025-04-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let directory = tempdir().unwrap();
    assert_eq!(
        ops_summary::host_health(now, &directory.path().join("absent")),
        None
    );
    assert_eq!(
        ops_summary::opencode_usage(now, &directory.path().join("absent")),
        None
    );
    let invalid = directory.path().join("invalid.json");
    std::fs::write(&invalid, b"{bad").unwrap();
    assert_eq!(ops_summary::host_health(now, &invalid), None);
    let state = directory.path().join("old.json");
    std::fs::write(&state,br#"{"last_sample":{"at":1744718219,"tmp_used_fraction":0.4,"tmp_free_bytes":1324997412,"tmp_inode_used_fraction":0.2,"load1":0.2,"load5":0.3,"cpus":4,"cpu_psi_some_avg60":0.0}}"#).unwrap();
    let health = ops_summary::host_health(now, &state).unwrap();
    assert_eq!(health["age_s"], 181);
    assert_eq!(health["stale"], true);
    assert_eq!(health["tmp_free_gib"], 1.23);
    let fractional = directory.path().join("fractional.json");
    std::fs::write(&fractional, br#"{"last_sample":{"at":1744718339.9995,"tmp_used_fraction":0.4,"tmp_free_bytes":1324997412,"tmp_inode_used_fraction":0.2,"load1":0.2,"load5":0.3,"cpus":4,"cpu_psi_some_avg60":0.0}}"#).unwrap();
    assert_eq!(
        ops_summary::host_health(now, &fractional).unwrap()["observed_at"],
        "2025-04-15T11:58:59Z"
    );
    let invalid_db = directory.path().join("invalid.sqlite");
    std::fs::write(&invalid_db, b"not a sqlite database").unwrap();
    assert_eq!(ops_summary::opencode_usage(now, &invalid_db), None);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = Command::new("python3")
        .arg("-c")
        .arg(PY_OPTIONAL_ORACLE)
        .arg(repo)
        .arg(directory.path().join("absent-host"))
        .arg(directory.path().join("absent-opencode"))
        .arg(&invalid)
        .arg(&state)
        .arg(&invalid_db)
        .arg(&fractional)
        .output()
        .expect("run AST-isolated optional-reader oracle");
    assert!(
        output.status.success(),
        "optional Python fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(expected["missing_host"], Value::Null);
    assert_eq!(expected["missing_opencode"], Value::Null);
    assert_eq!(expected["invalid_host"], Value::Null);
    assert_eq!(expected["invalid_db"], Value::Null);
    assert_eq!(expected["stale_host"]["age_s"], 181);
    assert_eq!(expected["stale_host"]["stale"], true);
    assert_eq!(expected["stale_host"]["tmp_free_gib"], 1.23);
    assert_eq!(
        ops_summary::host_health(now, &fractional).unwrap(),
        expected["fractional_host"]
    );
    assert_eq!(
        ops_summary::host_health(now, &state).unwrap(),
        expected["stale_host"]
    );
}

#[test]
fn python_rounding_and_database_errors_are_explicit() {
    assert_eq!(ops_summary::round_even(1.25, 1), 1.2);
    assert_eq!(ops_summary::round_even(1.35, 1), 1.4);
    assert_eq!(ops_summary::round_even(2.675, 2), 2.67);
    assert_eq!(ops_summary::round_even(1.234_567_5, 6), 1.234_568);
    let conn = Connection::open_in_memory().unwrap();
    let now = DateTime::parse_from_rfc3339("2025-04-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let error = ops_summary::summary(
        &conn,
        now,
        Path::new("/missing/host"),
        Path::new("/missing/opencode"),
    )
    .unwrap_err();
    assert!(matches!(error, ops_summary::SummaryError::Sql(_)));
}

#[test]
fn summary_parses_json_stored_as_sqlite_blob_like_python_bytes() {
    let (_dir, db, host, opencode) = fixture_paths();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let expected = setup_oracle(&repo, &db, &host, &opencode, true);
    let conn = Connection::open(&db).expect("fixture operations database");
    let now = DateTime::parse_from_rfc3339("2025-04-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let actual =
        ops_summary::summary(&conn, now, &host, &opencode).expect("summary with BLOB payload");
    assert_eq!(canonical_json(actual), canonical_json(expected));
}
