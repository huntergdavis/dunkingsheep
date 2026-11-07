use super::*;
use std::{path::Path, process::Command};

/// Execute the actual source oracle on a read-only scratch database, never a daemon.
pub(crate) fn oracle(path: &Path, method: &str, args: Value) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let script = r#"
import json, pathlib, sqlite3, sys
from dunk_tasks import Tasks
from dunk_core import Flock
conn = sqlite3.connect(pathlib.Path(sys.argv[1]).resolve().as_uri() + '?mode=ro', uri=True)
conn.execute('PRAGMA query_only=ON')
conn.row_factory = sqlite3.Row
class ReadTasks(Tasks):
    def _connect(self): return conn
tasks = ReadTasks.__new__(ReadTasks)
flock = Flock.__new__(Flock)
flock.tasks = tasks
method, args = sys.argv[2], json.loads(sys.argv[3])
if method == 'task_add_plan':
    # Run the actual Python role/default path, capturing its store-call arguments
    # instead of writing. The fixture role is not a production API parameter.
    role = args.pop('_fixture_role')
    flock.caller_role = lambda pane: role
    class PlanTasks:
        def add(self, scope, title, **kwargs):
            return dict(scope=scope, title=title, **kwargs)
    flock.tasks = PlanTasks()
    method = 'task_add'
try:
    target = flock if method.startswith('task_') else tasks
    print(json.dumps({'ok': getattr(target, method)(**args)}))
except Exception as error:
    print(json.dumps({'error': str(error)}))
finally:
    conn.close()
"#;
    let output = Command::new("python3")
        .current_dir(root)
        .args(["-c", script])
        .arg(path)
        .arg(method)
        .arg(args.to_string())
        .output()
        .expect("Python oracle must execute; no skip/fake success");
    assert!(
        output.status.success(),
        "Python oracle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python oracle JSON")
}

pub(crate) async fn fixture() -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir("task-wire");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    tasks.allocation_init().await.unwrap();
    tasks.writer.call(|conn| {
        conn.execute_batch(r#"
INSERT INTO tasks (id,scope,title,status,priority,created_at,updated_at) VALUES
 ('t1','global','parent','open','urgent',10,11),
 ('t2','global','full review','in_progress','high',12,13),
 ('t3','%99','private child','open','low',30,31),
 ('t4','global','first child','open','low',20,21),
 ('t5','%4','my private queue','blocked','low',10,11),
 ('t6','%99','other queue','open','medium',10,11),
 ('t7','global','cancelled dependency','cancelled','low',10,11),
 ('t8','global','unlanded dependency','done','low',10,11),
 ('t9','global','needs owner','blocked','low',10,11),
 ('t10','global','allocated','open','low',10,11);
UPDATE tasks SET body='metadata Δ',parent_id='t1', multi_assignee=1,attempts=7,
 assignees='["%4", "%8"]',depends_on='["t7", "t8", "t999"]',
 created_by='%1',completed_by='%8',completed_at=15,cancelled_at=16,
 kind='review',author='%1',repo='fixture/repo',pr=41,head='fixture-head',
 verdict='needs-fix',accounting_pr=42,landed=0 WHERE id='t2';
UPDATE tasks SET parent_id='t2' WHERE id IN ('t3','t4');
UPDATE tasks SET block_reason='needs-owner: decision' WHERE id IN ('t5','t9');
UPDATE tasks SET kind='report' WHERE id='t9';
UPDATE tasks SET landed=0 WHERE id='t8';
UPDATE tasks SET assignees='["%4"]' WHERE id='t10';
INSERT INTO allocation_offers (task_id,pool,role,requires,excluded,owners,offered_by,state)
 VALUES ('t10','fixture','worker','[]','[]','["%4"]','%1','ready');
INSERT INTO task_leases
 (task_id,pane_id,claimed_at,heartbeat_at,expires_at,caller_pid,caller_start,caller_name,assignee_added)
 VALUES ('t2','%4',10,11,1000000000000,123,'start','fixture codex Δ',1),
 ('t2','%99',10,11,1,NULL,NULL,'expired',0);
INSERT INTO task_events (task_id,event,at,actor,meta) VALUES
 ('t2','note',1,'%1','{"reason":"oldest excluded"}'),
 ('t2','note',2,'%1','{"reason":"second excluded"}'),
 ('t2','reviewed',3,'%8','{"reason":"  first kept  "}'),
 ('t2','ignored',4,'%8','bad JSON'),
 ('t2','ignored',5,NULL,'{"reason":0}'),
 ('t2','ignored',6,NULL,'{"reason":"  "}'),
 ('t2','note',7,NULL,'{"reason":true}'),
 ('t2','note',8,'%1','{"reason":["x",null,{"z":false,"a":3}]}');
"#).map_err(map_sqlite)
    }).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

async fn dump(tasks: &Tasks) -> Value {
    tasks
        .writer
        .call(|conn| {
            let mut out = serde_json::Map::new();
            for table in [
                "tasks",
                "task_events",
                "task_leases",
                "counters",
                "allocation_offers",
            ] {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                    .map_err(map_sqlite)?;
                let rows = stmt
                    .query_map([], row_wire)
                    .map_err(map_sqlite)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_sqlite)?;
                out.insert(table.into(), json!(rows));
            }
            Ok(Value::Object(out))
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn task_wire_full_rows_children_parent_leases_and_notes_match_actual_python() {
    let tasks = fixture().await;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    tasks.on_event(Box::new(move |_, _, _, _| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let before = dump(&tasks).await;
    for id in ["t1", "t2", "t3", "t5", "t10"] {
        // No-pane reads may inspect any queue, just like Python's Flock privacy rule.
        for pane in [None, Some("%4")] {
            if id == "t3" && pane.is_some() {
                continue;
            }
            let full = tasks.get_full_wire(id, pane).await.unwrap();
            let mut wire = full.wire.clone();
            let expected_wire =
                oracle(tasks.path(), "get", json!({"task_id":id,"pane_id":pane}))["ok"].clone();
            crate::tasks::claim::claim_assigned_offer_tests::accept_t3617_claims(
                &mut wire,
                &expected_wire,
                pane,
            );
            assert_eq!(wire, expected_wire);
            assert_eq!(full.typed, tasks.get(id).await.unwrap());
            let read = tasks.read_visible_wire_with_notes(id, pane).await.unwrap();
            let mut read_wire = read.wire.clone();
            let expected_read = oracle(
                tasks.path(),
                "task_read",
                json!({"task_id":id,"self_pane_id":pane}),
            )["ok"]
                .clone();
            crate::tasks::claim::claim_assigned_offer_tests::accept_t3617_claims(
                &mut read_wire,
                &expected_read,
                pane,
            );
            assert_eq!(read_wire, expected_read);
            assert_eq!(read.typed, full.typed);
        }
    }
    let full = tasks.get_full_wire("t2", Some("%4")).await.unwrap();
    assert_eq!(full.wire["children"][0]["id"], "t4");
    assert_eq!(full.wire["children"][1]["id"], "t3");
    assert_eq!(
        full.wire["parent"],
        json!({"id":"t1","title":"parent","status":"open"})
    );
    assert_eq!(full.wire["leases"][0]["caller_name"], "fixture codex Δ");
    assert_eq!(full.wire["leases"][0]["assignee_added"], 1);
    assert_eq!(full.wire["attempts"], 7);
    assert_eq!(full.wire["accounting_pr"], 42);
    assert!(!tasks
        .get_full_wire("t1", None)
        .await
        .unwrap()
        .wire
        .as_object()
        .unwrap()
        .contains_key("parent"));
    assert_eq!(
        dump(&tasks).await,
        before,
        "reads and Python oracle must not mutate rows/events"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn task_wire_list_filters_sort_slice_and_full_decorators_match_actual_python() {
    let tasks = fixture().await;
    let mut count = 0;
    for scope in [None, Some("global"), Some("%4"), Some("self")] {
        for limit in [
            None,
            Some(0),
            Some(1),
            Some(-1),
            Some(i64::MIN),
            Some(i64::MAX),
        ] {
            let rows = tasks
                .list_visible_wire(scope, None, None, true, limit, Some("%4"), None, false)
                .await
                .unwrap();
            let expected = oracle(
                tasks.path(),
                "list",
                json!({"scope":scope,"self_pane_id":"%4","limit":limit.unwrap_or(200)}),
            );
            let mut list_wire = json!(rows.iter().map(|r| &r.wire).collect::<Vec<_>>());
            let expected_list = expected["ok"].clone();
            crate::tasks::claim::claim_assigned_offer_tests::accept_t3617_claims(
                &mut list_wire,
                &expected_list,
                Some("%4"),
            );
            assert_eq!(list_wire, expected_list, "scope={scope:?} limit={limit:?}");
            for row in rows {
                assert_eq!(
                    row.typed,
                    tasks
                        .decorate(&row.typed.task.id, Some("%4"))
                        .await
                        .unwrap()
                );
                assert!(!row.wire.as_object().unwrap().contains_key("children"));
                assert!(!row.wire.as_object().unwrap().contains_key("parent"));
            }
            count += 1;
        }
    }
    for (status, assignee, done, kind, needs_owner) in [
        (None, Some("%4"), false, None, false),
        (None, Some("  "), true, None, false),
        (Some("done"), None, false, None, false),
        (None, None, true, Some("review"), false),
        (None, None, true, None, true),
        (Some("open"), None, true, None, true),
        (None, None, true, Some("report"), true),
    ] {
        let rows = tasks
            .list_visible_wire(
                None,
                status,
                assignee,
                done,
                None,
                Some("%4"),
                kind,
                needs_owner,
            )
            .await
            .unwrap();
        let expected = oracle(
            tasks.path(),
            "list",
            json!({"scope":null,"status":status,"assignee":assignee,"include_done":done,"self_pane_id":"%4","kind":kind,"needs_owner":needs_owner}),
        );
        let mut filtered_wire = json!(rows.iter().map(|r| &r.wire).collect::<Vec<_>>());
        let expected_filtered = expected["ok"].clone();
        crate::tasks::claim::claim_assigned_offer_tests::accept_t3617_claims(
            &mut filtered_wire,
            &expected_filtered,
            Some("%4"),
        );
        assert_eq!(filtered_wire, expected_filtered);
        count += 1;
    }
    assert_eq!(count, 31, "nonzero oracle cases");
    // Defaults and negative slicing beyond the 200-row default.
    tasks.writer.call(|conn| conn.execute_batch(
        "WITH RECURSIVE n(x) AS (SELECT 100 UNION ALL SELECT x+1 FROM n WHERE x<304) INSERT INTO tasks (id,scope,title,created_at,updated_at) SELECT 't'||x,'global','bulk',50,50 FROM n"
    ).map_err(map_sqlite)).await.unwrap();
    for limit in [None, Some(0), Some(-1), Some(201)] {
        let rows = tasks
            .list_visible_wire(None, None, None, true, limit, Some("%4"), None, false)
            .await
            .unwrap();
        assert_eq!(
            json!(rows.iter().map(|r| &r.wire).collect::<Vec<_>>()),
            oracle(
                tasks.path(),
                "list",
                json!({"self_pane_id":"%4","limit":limit.unwrap_or(200)})
            )["ok"]
        );
        if limit.is_none() {
            assert_eq!(rows.len(), 200);
        }
    }
}

#[tokio::test]
async fn task_wire_visibility_missing_and_validation_errors_preserve_order() {
    let tasks = fixture().await;
    let expected = oracle(
        tasks.path(),
        "task_read",
        json!({"task_id":"t6","self_pane_id":"%4"}),
    )["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        tasks
            .get_full_wire("t6", Some("%4"))
            .await
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!(
        tasks
            .read_visible_wire_with_notes("t6", Some("%4"))
            .await
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!(
        tasks
            .get_full_wire("missing", None)
            .await
            .unwrap_err()
            .to_string(),
        tasks.get("missing").await.unwrap_err().to_string()
    );
    for (scope, status, kind) in [
        (Some("bad"), Some("bad"), Some("bad")),
        (Some("self"), Some("bad"), Some("bad")),
        (None, Some("bad"), Some("bad")),
        (None, None, Some("bad")),
        (None, None, Some(" BAD ")),
        (None, None, Some("can't")),
        (None, None, Some("bad\"'\nkind\u{1}")),
    ] {
        let actual = tasks
            .list_visible_wire(scope, status, None, true, Some(0), None, kind, false)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            actual,
            oracle(
                tasks.path(),
                "list",
                json!({"scope":scope,"status":status,"kind":kind,"limit":0})
            )["error"]
        );
    }
    tasks
        .writer
        .call(|conn| {
            conn.execute_batch("UPDATE tasks SET parent_id='missing' WHERE id='t10'")
                .map_err(map_sqlite)
        })
        .await
        .unwrap();
    assert_eq!(
        tasks.get_full_wire("t10", None).await.unwrap().wire["parent"],
        Value::Null
    );
}

#[tokio::test]
async fn task_wire_note_failure_is_empty_and_reads_keep_snapshot_read_only() {
    let tasks = fixture().await;
    let read = tasks
        .read_visible_wire_with_notes("t2", None)
        .await
        .unwrap();
    assert_eq!(
        read.notes
            .iter()
            .map(|n| n.reason.as_str())
            .collect::<Vec<_>>(),
        ["first kept", "True", "['x', None, {'z': False, 'a': 3}]"]
    );
    // Python scans only the last 12 events, including malformed/reasonless ones.
    tasks.writer.call(|conn| conn.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<12) INSERT INTO task_events(task_id,event,at,meta) SELECT 't2','noise',10,'{}' FROM n").map_err(map_sqlite)).await.unwrap();
    assert!(tasks
        .read_visible_wire_with_notes("t2", None)
        .await
        .unwrap()
        .notes
        .is_empty());
    // A truthy non-dictionary meta makes recent_notes fail: discard all notes.
    tasks.writer.call(|conn| conn.execute_batch("INSERT INTO task_events(task_id,event,at,meta) VALUES ('t2','note',11,'[1]'), ('t2','note',12,'{\"reason\":\"discard me too\"}')").map_err(map_sqlite)).await.unwrap();
    assert_eq!(
        tasks
            .read_visible_wire_with_notes("t2", None)
            .await
            .unwrap()
            .wire,
        oracle(tasks.path(), "task_read", json!({"task_id":"t2"}))["ok"]
    );
    tasks
        .writer
        .call(|conn| {
            conn.execute_batch("DROP TABLE task_events")
                .map_err(map_sqlite)
        })
        .await
        .unwrap();
    let read = tasks
        .read_visible_wire_with_notes("t2", None)
        .await
        .unwrap();
    assert!(read.notes.is_empty());
    assert_eq!(read.wire["notes"], json!([]));
    assert_eq!(
        read.wire,
        oracle(tasks.path(), "task_read", json!({"task_id":"t2"}))["ok"]
    );
    assert_eq!(
        tasks
            .read_visible_wire_with_notes("t6", Some("%4"))
            .await
            .unwrap_err()
            .to_string(),
        "task t6 belongs to another pane's private queue (%99)"
    );
    // A failed full-row query must propagate (only notes errors are swallowed).
    tasks
        .writer
        .call(|conn| {
            conn.execute_batch("DROP TABLE task_leases")
                .map_err(map_sqlite)
        })
        .await
        .unwrap();
    assert!(tasks
        .read_visible_wire_with_notes("t2", None)
        .await
        .is_err());
}

#[tokio::test]
async fn task_wire_snapshot_keeps_scope_metadata_children_and_notes_on_one_version() {
    let tasks = fixture().await;
    let path = tasks.path().to_path_buf();
    tasks.writer.call(move |conn| {
        let tx = conn.transaction().map_err(map_sqlite)?;
        // Pin SQLite's read version, then commit changes from an external connection.
        let _: String = tx.query_row("SELECT scope FROM tasks WHERE id='t2'", [], |r| r.get(0)).map_err(map_sqlite)?;
        let other = Connection::open(path).map_err(map_sqlite)?;
        other.execute_batch("UPDATE tasks SET scope='%99', head='new head' WHERE id='t2'; UPDATE tasks SET parent_id=NULL WHERE id='t4'; DELETE FROM task_events WHERE task_id='t2';").map_err(map_sqlite)?;
        let full = full_conn(&tx,"t2",Some("%4"),crate::clock::now_s())?;
        assert_eq!(full.typed.scope,"global");
        assert_eq!(full.wire["scope"],"global");
        assert_eq!(full.wire["head"],"fixture-head");
        assert_eq!(full.wire["children"][0]["id"],"t4");
        assert_eq!(notes_conn(&tx,"t2")?.len(),3);
        Ok(())
    }).await.unwrap();
    assert!(
        tasks.get_full_wire("t2", Some("%4")).await.is_err(),
        "later reads must enforce the new private scope"
    );
    assert_eq!(
        tasks.get_full_wire("t2", None).await.unwrap().wire["head"],
        "new head"
    );
    // Refuse a nested transaction without committing or rolling back somebody else's.
    tasks
        .writer
        .call(|conn| {
            conn.execute_batch("BEGIN; UPDATE tasks SET title='uncommitted' WHERE id='t1'")
                .map_err(map_sqlite)
        })
        .await
        .unwrap();
    assert!(tasks.get_full_wire("t1", None).await.is_err());
    tasks
        .writer
        .call(|conn| {
            assert!(!conn.is_autocommit());
            let title: String = conn
                .query_row("SELECT title FROM tasks WHERE id='t1'", [], |r| r.get(0))
                .map_err(map_sqlite)?;
            assert_eq!(title, "uncommitted");
            conn.execute_batch("ROLLBACK").map_err(map_sqlite)
        })
        .await
        .unwrap();
    assert_eq!(
        tasks.get_full_wire("t1", None).await.unwrap().typed.title,
        "parent"
    );
}
