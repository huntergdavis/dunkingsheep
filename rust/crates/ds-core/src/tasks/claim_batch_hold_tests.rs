//! t3582, the owner's ruling on #807: a dependant waits for its in-batch prerequisites. A blocked
//! or open prerequisite holds the dependant; a done or cancelled one satisfies it. The retirement of
//! a blocked member is for the member itself only (the dependant is held, not removed).

use crate::error::DunkError;
use crate::tasks::{ClaimOptions, FieldEdit, Tasks};

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-batch-hold-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

/// The refusal names the prerequisite that holds the dependant back.
fn assert_held_by(error: DunkError, prerequisite: &str) {
    match error {
        DunkError::Refused(message) => assert!(
            message.contains(prerequisite),
            "the refusal names the prerequisite {prerequisite}: {message}"
        ),
        other => panic!("expected a refusal naming {prerequisite}, got {other:?}"),
    }
}

#[tokio::test]
async fn a_dependant_waits_for_its_blocked_prerequisite_until_it_is_cancelled_or_done() {
    let tasks = store("blocked").await;
    let a = tasks.add("global", "prerequisite", "").await.unwrap();
    let b = tasks.add("global", "dependant", "").await.unwrap();
    // b needs a (a is earlier in the batch, so the order is valid).
    tasks
        .update(
            &b.id,
            FieldEdit {
                depends_on: Some(vec![a.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    tasks
        .claim_batch(&[a.id.clone(), b.id.clone()], "%1", ClaimOptions::default())
        .await
        .unwrap();

    // A blocked prerequisite holds the dependant, as the live dependency rule does.
    tasks.block(&a.id, "waiting on the owner").await.unwrap();
    assert_held_by(tasks.complete(&b.id).await.unwrap_err(), &a.id);

    // Unblocking it does not satisfy it either: it is open, not done.
    tasks.unblock(&a.id).await.unwrap();
    assert_held_by(tasks.complete(&b.id).await.unwrap_err(), &a.id);

    // A cancelled prerequisite is satisfied (t530): the dependant completes.
    tasks
        .set_status(&a.id, "cancelled", None, None)
        .await
        .unwrap();
    tasks.complete(&b.id).await.unwrap();
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "done");
}
