-- t3721 populated-metrics fixture: ops DB seeds (Python VP_OPS_DB / Python VP_DATA_DIR/operations.sqlite3,
-- Rust DeskConfig.operations_db). Run AFTER the schema exists (ops_metrics.connect() /
-- ops_schema.sql) and BEFORE the POST /api/ops-metrics/refresh under test.
--
-- Fixed clock baseline T0 = 2026-11-08T12:00:00Z. Every literal below is absolute, so the same file seeds
-- both languages. Idempotent by explicit ids / INSERT OR IGNORE, so a re-run cannot duplicate rows.
--
-- NOT covered here on purpose: pr_latest, pr_state_events, dunk_metrics_snapshots, pane_samples for the
-- refresh run, agent_message_events - those are written by the refresh itself and are the "after"
-- observations in SPEC.md section 8. agent_message_events stays empty so the collector's
-- id > MAX(source_event_id) copy is observable.

-- A prior ok run, 10 minutes before the refresh. Gives source_freshness last_ok_at for all six sources
-- and makes the refresh's own row the second run.
INSERT OR IGNORE INTO collection_runs
  (id, observed_at, panes_state, github_state, vp_messages_state, dunk_metrics_state,
   accounting_state, agent_messages_state, errors_json)
VALUES
  (1, '2026-11-08T11:50:00Z', 'ok', 'ok', 'ok', 'ok', 'ok', 'ok', '{}');

INSERT OR IGNORE INTO pane_samples
  (run_id, pane_id, project, agent_kind, agent_status, agent_name, lane_name, role)
VALUES
  (1, '%201', 'poolrad', 'claude', 'working', 'w1', 'lane-1', 'coder');

-- Roles the refresh run assigns to the panes the CLI/daemon stub reports.
INSERT OR IGNORE INTO lane_assignments (project, pane_id, role, focus, updated_at) VALUES
  ('poolrad', '%201', 'coder',    'strict C',  '2026-11-07T00:00:00Z'),
  ('poolrad', '%202', 'reviewer', 'reviews',   '2026-11-07T00:00:00Z'),
  ('poolrad', '%203', 'pm',       'planning',  '2026-11-07T00:00:00Z');

-- Two direct readings per provider, one hour apart, same reset. Expected burn (pp/hour):
--   codex  (60.0 - 58.0) / 1h = 2.0
--   claude (40.0 - 40.0) / 1h = 0.0
INSERT OR IGNORE INTO quota_observations
  (project, provider, cohort, pane_id, observed_at, precision, weekly_percent_left, reset_at, evidence_ref)
VALUES
  ('poolrad', 'codex',  'weekly', '%201', '2026-11-08T10:00:00Z', 'second', 60.0, '2026-11-15T00:00:00Z', 'fixture:quota:codex:1'),
  ('poolrad', 'codex',  'weekly', '%201', '2026-11-08T11:00:00Z', 'second', 58.0, '2026-11-15T00:00:00Z', 'fixture:quota:codex:2'),
  ('poolrad', 'claude', 'weekly', '%202', '2026-11-08T10:00:00Z', 'minute', 40.0, '2026-11-15T00:00:00Z', 'fixture:quota:claude:1'),
  ('poolrad', 'claude', 'weekly', '%202', '2026-11-08T11:00:00Z', 'minute', 40.0, '2026-11-15T00:00:00Z', 'fixture:quota:claude:2');

-- Owner-reported fixed subscriptions; expected monthly_fixed_usd = 400.0.
-- UNIQUE(project,provider,observed_at,evidence_ref) makes OR IGNORE safe on a re-seed.
INSERT OR IGNORE INTO subscription_costs (project, provider, monthly_usd_cents, observed_at, evidence_ref) VALUES
  ('poolrad', 'codex',  20000, '2026-11-01T00:00:00Z', 'fixture:subscription:codex'),
  ('poolrad', 'claude', 20000, '2026-11-01T00:00:00Z', 'fixture:subscription:claude');

-- Goals: scope is CHECK-constrained to 'local' or 'overall' (ops_metrics.py:125-136 and
-- rust/crates/ds-web/src/ops_schema.sql:83-96) - whole-project goals therefore use 'overall'.
-- status is CHECK-constrained to planned/in_progress/blocked/verified/unknown; the summary takes MAX(id)
-- per goal_key with status='verified'. Explicit ids + OR IGNORE keep a re-seed idempotent.
INSERT OR IGNORE INTO goal_observations
  (id, project, scope, goal_key, observed_at, status, value, target, unit, evidence_ref, note)
VALUES
  (1, 'poolrad', 'overall', 'strict_fullc',             '2026-11-08T11:00:00Z', 'verified', 10.0, 815.0, 'functions',  'fixture:goal:strict', 'fixture seed'),
  (2, 'poolrad', 'overall', 'verified_game_identities', '2026-11-08T11:00:00Z', 'verified', 12.0, 815.0, 'identities', 'fixture:goal:ident',  'fixture seed');

-- Prior accounting snapshot 2 hours before the refresh, same ceiling / bound_kind / source_signature as
-- the fake gh payload, strict 8 (the refresh brings strict 10). That is what makes completion_rate
-- expressible: delta 2 strict functions over 2 h => strict_functions_per_hour 1.0,
-- function_coverage_points_per_hour 5.0 (ceiling 20).
-- source_signature = sha256('{"a": "x", "b": 2}')  (json.dumps(inputs, sort_keys=True) in both languages)
INSERT OR IGNORE INTO accounting_snapshots
  (project, observed_at, commit_oid, strict_full_c, verified_game_identities, ceiling,
   bound_kind, source_signature, evidence_ref)
VALUES
  ('poolrad', '2026-11-08T10:00:00Z', 'dddddddddddddddddddddddddddddddddddddddd', 8, 12, 20,
   'upper_bound', '16688fc9adafaed795638c7e391262e36a34263ad1e2be62fdd980c7157dec61',
   'docs/re/evidence.md');
