//! t1225 addendum: the `blocked` event `block_by` writes (`task block --by`) carries the task's
//! scope and priority, as `set_status` does. Without them a scoped metrics query
//! (`scopes=[global]`) silently drops the block.

use super::metrics::MetricsOptions;
use super::Tasks;

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("block-by-scope-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

#[tokio::test]
async fn a_block_by_event_counts_in_a_scoped_metrics_query() {
    let store = store("count").await;
    let dep = store.add("global", "dependency", "").await.unwrap();
    let waiting = store
        .add("global", "waits on the dependency", "")
        .await
        .unwrap();
    store.block_by(&waiting.id, &dep.id).await.unwrap();

    let metrics = store
        .metrics(MetricsOptions {
            scopes: Some(vec!["global".to_string()]),
            ..MetricsOptions::default()
        })
        .await
        .unwrap();
    assert_eq!(1, metrics["throughput"]["blocked"]);
}

#[tokio::test]
async fn an_auto_unblocked_event_counts_in_a_scoped_metrics_query() {
    let store = store("unblock").await;
    let dep = store.add("global", "dependency", "").await.unwrap();
    let waiting = store
        .add("global", "waits on the dependency", "")
        .await
        .unwrap();
    store.block_by(&waiting.id, &dep.id).await.unwrap();
    // Completing the dependency runs the t1218 event path: the waiting task reopens and an
    // `unblocked` event is written in the same writer call.
    store.complete(&dep.id).await.unwrap();
    assert_eq!("open", store.get(&waiting.id).await.unwrap().status);

    let metrics = store
        .metrics(MetricsOptions {
            scopes: Some(vec!["global".to_string()]),
            ..MetricsOptions::default()
        })
        .await
        .unwrap();
    assert_eq!(1, metrics["throughput"]["unblocked"]);
}
