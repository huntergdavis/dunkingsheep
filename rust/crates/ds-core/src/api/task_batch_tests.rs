//! t3583: the guarded batch-claim route, driven through the real dispatcher so the role gate and
//! the handler both run. A reviewer, a web caller and an anonymous caller are refused before any
//! write; a worker takes a whole batch in the order given; the single claim is unchanged.

use super::*;
use crate::tasks::AddOptions;

/// A flock with a worker (w1:p1), a reviewer (w1:p2) and an owner (w1:p3), and a task store
/// attached at its tasks.db. The runtime is returned too: the store's background work lives on it.
fn batch_flock(
    tag: &str,
) -> (
    tempfile::TempDir,
    Flock,
    Arc<Tasks>,
    tokio::runtime::Runtime,
) {
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    for (pane, role) in [
        ("w1:p1", "worker"),
        ("w1:p2", "reviewer"),
        ("w1:p3", "owner"),
    ] {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .unwrap();
    }
    flock.attach_roles(roles, None);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(Tasks::open(dir.path().join("tasks.db")))
            .unwrap(),
    );
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (dir, flock, tasks, runtime)
}

fn task_dispatcher() -> Dispatcher {
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_task_mutation_leases();
    dispatcher
}

fn batch_env(dir: &tempfile::TempDir) -> PmApiEnv {
    let home = dir.path().to_string_lossy().into_owned();
    PmApiEnv::new(Env::from_pairs(&[("HOME", &home)]), false)
}

fn ids_args(list: &[&str]) -> Option<Args> {
    let mut args = Args::new();
    args.insert("ids".into(), json!(list));
    Some(args)
}

fn claimed_ids(result: &Value) -> Vec<String> {
    result["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn task_batch_claim_is_refused_for_a_reviewer_before_any_write() {
    let (dir, flock, tasks, runtime) = batch_flock("batch-reviewer");
    let a = runtime.block_on(tasks.add("global", "a", "")).unwrap();
    let b = runtime.block_on(tasks.add("global", "b", "")).unwrap();
    let env = batch_env(&dir);
    dispatch_as_flock_role(
        &task_dispatcher(),
        &flock,
        &env,
        Some("w1:p2"),
        "task_batch_claim",
        ids_args(&[&a.id, &b.id]),
    )
    .unwrap_err();
    for id in [&a.id, &b.id] {
        assert_eq!(runtime.block_on(tasks.get(id)).unwrap().status, "open");
        assert!(runtime
            .block_on(tasks.leases_for(id, None))
            .unwrap()
            .is_empty());
    }
}

#[test]
fn task_batch_claim_is_refused_for_a_web_caller_and_an_anonymous_one() {
    let (dir, flock, tasks, runtime) = batch_flock("batch-web");
    let a = runtime.block_on(tasks.add("global", "a", "")).unwrap();
    let env = batch_env(&dir);
    let dispatcher = task_dispatcher();
    assert!(
        !crate::roles::allowed("web", "task_batch_claim"),
        "the web role is not a worker"
    );
    dispatch_as_flock_role(
        &dispatcher,
        &flock,
        &env,
        None,
        "task_batch_claim",
        ids_args(&[&a.id]),
    )
    .unwrap_err();
    assert_eq!(runtime.block_on(tasks.get(&a.id)).unwrap().status, "open");
}

#[test]
fn the_role_gate_admits_the_owner_and_workers_and_no_other_role() {
    for (role, admitted) in [
        ("owner", true),
        ("worker", true),
        ("reviewer", false),
        ("web", false),
        ("desk", false),
        ("planner", false),
        ("system", false),
    ] {
        assert_eq!(
            crate::roles::allowed(role, "task_batch_claim"),
            admitted,
            "task_batch_claim for {role}"
        );
    }
}

#[test]
fn a_worker_claims_a_whole_batch_in_the_order_given() {
    let (dir, flock, tasks, runtime) = batch_flock("batch-worker");
    let a = runtime.block_on(tasks.add("global", "a", "")).unwrap();
    let b = runtime.block_on(tasks.add("global", "b", "")).unwrap();
    let env = batch_env(&dir);
    let result = dispatch_as_flock_role(
        &task_dispatcher(),
        &flock,
        &env,
        Some("w1:p1"),
        "task_batch_claim",
        ids_args(&[&b.id, &a.id]),
    )
    .unwrap();
    assert_eq!(claimed_ids(&result), vec![b.id.clone(), a.id.clone()]);
    assert!(result["batch_id"].is_number());
    assert!(result["refused"].is_null());
    for id in [&a.id, &b.id] {
        assert_eq!(
            runtime.block_on(tasks.get(id)).unwrap().status,
            "in_progress"
        );
    }
}

#[test]
fn a_refused_batch_claims_nothing_and_names_the_member_at_fault() {
    let (dir, flock, tasks, runtime) = batch_flock("batch-refused");
    let a = runtime.block_on(tasks.add("global", "a", "")).unwrap();
    let b = runtime
        .block_on(tasks.add_with(
            "global",
            "b",
            "",
            AddOptions {
                assignees: vec!["w9:p9".into()],
                ..Default::default()
            },
        ))
        .unwrap();
    let env = batch_env(&dir);
    let result = dispatch_as_flock_role(
        &task_dispatcher(),
        &flock,
        &env,
        Some("w1:p1"),
        "task_batch_claim",
        ids_args(&[&a.id, &b.id]),
    )
    .unwrap();
    assert!(result["batch_id"].is_null());
    assert_eq!(result["refused"]["reason"], "owned_elsewhere");
    assert_eq!(result["refused"]["task_ids"], json!([b.id.clone()]));
    assert_eq!(runtime.block_on(tasks.get(&a.id)).unwrap().status, "open");
}

#[test]
fn the_worker_kind_filter_refuses_a_review_member() {
    let (dir, flock, tasks, runtime) = batch_flock("batch-kind");
    let work = runtime.block_on(tasks.add("global", "work", "")).unwrap();
    let review = runtime
        .block_on(tasks.add_with(
            "global",
            "review",
            "",
            AddOptions {
                kind: Some("review".into()),
                repo: Some("o/r".into()),
                pr: Some("1".into()),
                head: Some("abc".into()),
                ..Default::default()
            },
        ))
        .unwrap();
    let env = batch_env(&dir);
    let result = dispatch_as_flock_role(
        &task_dispatcher(),
        &flock,
        &env,
        Some("w1:p1"),
        "task_batch_claim",
        ids_args(&[&work.id, &review.id]),
    )
    .unwrap();
    assert_eq!(result["refused"]["reason"], "not_claimable");
    assert_eq!(result["refused"]["task_ids"], json!([review.id.clone()]));
    assert_eq!(
        runtime.block_on(tasks.get(&work.id)).unwrap().status,
        "open"
    );
}

#[test]
fn a_malformed_ids_argument_is_refused_before_the_store_is_touched() {
    let (dir, flock, _tasks, _runtime) = batch_flock("batch-malformed");
    let env = batch_env(&dir);
    let mut args = Args::new();
    args.insert("ids".into(), json!("t1"));
    dispatch_as_flock_role(
        &task_dispatcher(),
        &flock,
        &env,
        Some("w1:p1"),
        "task_batch_claim",
        Some(args),
    )
    .unwrap_err();
}

#[test]
fn task_next_single_claim_is_unchanged_by_the_batch_route() {
    let (dir, flock, tasks, runtime) = batch_flock("batch-single");
    let solo = runtime.block_on(tasks.add("global", "solo", "")).unwrap();
    let env = batch_env(&dir);
    let result = dispatch_as_flock_role(
        &task_dispatcher(),
        &flock,
        &env,
        Some("w1:p1"),
        "task_next",
        None,
    )
    .unwrap();
    assert_eq!(result["task"]["id"], json!(solo.id));
    assert!(
        result.get("tasks").is_none(),
        "task_next still returns one task, not a batch"
    );
}
