# Mechanism census: what each one is for, in one line

A rule tested only in its happy path may be impossible in its only case. So for
every mechanism that exists for **one purpose**, this table states that purpose in
one line, names the test that exercises it, and says whether that test exists.

> **If the case cannot be stated in one line, we do not know what the mechanism is
> for — and no test can be right.**

`tests/test_mechanism_census.py` enforces this file. It is **derived from the
tree**, not from a list somebody remembers to update:

- a public function or method whose name carries a mechanism word
  (`supersede`, `retire`, `restore`, `recover`, `reap`, `rollback`, `quarantine`,
  `archive`, `backfill`, `resume`, `purge`, `migrate`, `flush`, `clear`,
  `deprecate`) is a **candidate**;
- every candidate must have a **row here**, or an **exemption** in the test with a
  reason *and* a covering test that must exist;
- private helpers (`_`) and registry handlers (`h_*`) are excluded by rule, and so
  are read-only predicates (`is_`, `all_`, `has_`, `mailbox_`, `rule_`);
- every row's case must fit in **90 characters**, and every cited test must exist.

So a `rollback` written tomorrow fails the suite until its only case is stated in
one line. That was proven by adding one.

| mechanism | its only case, one line | test | state |
|---|---|---|---|
| `supersede` | superseding an entry on the same day with the same subject | `test_a_supersede_within_the_same_day_gets_its_own_slug` | tested |
| `retire` | retiring a pane that is already gone (superseded) | `test_retiring_a_pane_that_no_longer_exists_still_records_it` | tested |
| `unretire` | bringing back a pane that was retired under a decorated mailbox | `test_unretire_brings_a_pane_back` | tested |
| `context_retire` | retiring a rule is the owner's act; a lane that tries is refused | `test_promote_and_retire_are_the_owners` | tested |
| `context_supersede` | a lane may replace its own fact but never a rule (owner authority) | `test_a_worker_may_supersede_its_fact_but_not_a_rule` | tested |
| `recovers` | a state-machine decision that means "act now" (the stuck watchdog's single action) | `test_at_the_threshold_exactly_one_attempt_is_decided` | tested |
| `clear_box` | a lane blocked on a full context with a draft in the box | `test_clear_box_works_on_the_case_it_exists_for_a_full_context` | tested |
| `flush_box` | our own text stranded in the box after a failed submit | `test_our_stranded_text_is_flushed_before_the_next_send` | tested |
| `memory_backfill` | a second run over a corpus that was partly done | `test_a_second_start_does_not_re_enqueue_and_limit_is_respected` | tested |
| `tick` | a ready lane with no mail, and a ready lane that already has some | `test_a_ready_lane_with_unread_mail_has_an_assignment_already` | tested |
| `derive_board_items` | an open task nobody has claimed, next to one that is held or dependent | `test_everything_that_is_not_dispatchable_is_skipped` | tested |
| `archive` | bulk clearing mail older than a stamp without deleting it | `test_archive_before_only_touches_older_mail` | tested |
| `reap` | a lease whose caller process is dead, not merely late | `test_reap_requeues_a_dead_caller_even_with_a_live_pane` | tested |
| `recover` | after an upgrade, a bare shell is offered its session and a live agent is left alone | `test_apply_types_only_into_empty_shells` | tested |
| `superuser_recover` | an empty allowlist is bootstrapped once, then gated | `test_recover_bootstraps_an_empty_allowlist_then_requires_superuser` | tested |
| `recover_leases` | an ingest lease left by a stopped process is requeued, not lost | `test_recover_leases_requeues_an_expired_ingest` | tested |
| `resume` | resuming an ingest that was paused mid-corpus | `test_pause_and_resume_change_the_ingest_state` | tested |
| `purge` | retention drops old read mail and keeps unread | `test_purge_keeps_unread_mail_and_drops_old_read_mail` | tested |
| `archive_retired` | mail to a retired mailbox older than a day, which must be kept but not left waiting | `test_mail_to_a_retired_mailbox_is_archived_and_kept` | tested |
| `auto_archive_undeliverable` | mail to a pane that no longer exists, older than the archive age | `test_undeliverable_mail_older_than_24h_is_auto_archived` | tested |
| `kill_decision` | a real TERM of a process we started, and a protected pid refused | `test_a_real_kill_of_a_process_we_started` | tested |
| `write_heartbeat` | the watchdog itself being dead, detected without it | `test_the_deadman_has_exactly_one_recipient` | tested |
| `rule_resource_would_kill` | a lane tripping a policy while doing asked-for work | `test_a_would_kill_names_the_lane_and_the_task_it_was_doing` | tested |
| `plan_recovery` | our own typed prompt stranded in an ambiguous Codex frame, absent from the native session | `test_a_positive_plan_names_its_attempt_and_generation` | tested |

## Why each of these is the only case

- **supersede** exists so a ground truth can change; every real change is same-day
  and same-subject, which is exactly what collided.
- **retire** exists for panes we replaced; a superseded pane is usually unreachable
  already, so a retirement that needs a live pane fails on its own case.
- **clear_box** exists for the nudge that would not submit; the draft sits in a box
  that is *blocked*, not in one whose session is dead.
- **memory_backfill** exists to finish a corpus, so the case is the *second* run.
- **reap** exists to recover work from a dead caller, so the case is a dead `/proc`
  entry, not a late heartbeat alone.
- **kill_decision** exists to stop a runaway; a test that never sends a signal has
  not tested it, and one that only sends has not tested the refusal.

## Adding a mechanism

1. Write the one-line case **first**. If it needs two lines, the mechanism is doing
   two jobs.
2. Add the test for that case, not for a neighbouring one.
3. Add the row here. Nothing else: the tree scan finds the function by name, and the
   suite fails until the row exists. (If it is genuinely not a one-purpose
   mechanism, add an `EXEMPT` entry with a reason and a covering test instead.)
