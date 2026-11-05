CREATE TABLE IF NOT EXISTS collection_runs (
    id INTEGER PRIMARY KEY,
    observed_at TEXT NOT NULL,
    panes_state TEXT NOT NULL,
    github_state TEXT NOT NULL,
    vp_messages_state TEXT NOT NULL,
    dunk_metrics_state TEXT NOT NULL,
    accounting_state TEXT NOT NULL DEFAULT 'unavailable',
    agent_messages_state TEXT NOT NULL DEFAULT 'unavailable',
    errors_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_observed_at ON collection_runs(observed_at);
CREATE TABLE IF NOT EXISTS lane_assignments (
    project TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('pm','coder','reviewer','other')),
    focus TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(project,pane_id)
);
CREATE TABLE IF NOT EXISTS pane_samples (
    run_id INTEGER NOT NULL REFERENCES collection_runs(id) ON DELETE CASCADE,
    pane_id TEXT NOT NULL,
    project TEXT NOT NULL,
    agent_kind TEXT,
    agent_status TEXT,
    agent_name TEXT,
    lane_name TEXT,
    role TEXT NOT NULL,
    PRIMARY KEY(run_id,pane_id)
);
CREATE INDEX IF NOT EXISTS pane_samples_pane_run ON pane_samples(pane_id,run_id);
CREATE TABLE IF NOT EXISTS pr_latest (
    project TEXT NOT NULL,
    number INTEGER NOT NULL,
    state TEXT NOT NULL,
    head_oid TEXT,
    merged_at TEXT,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY(project,number)
);
CREATE TABLE IF NOT EXISTS pr_state_events (
    project TEXT NOT NULL,
    number INTEGER NOT NULL,
    state TEXT NOT NULL,
    head_oid TEXT NOT NULL DEFAULT '',
    observed_at TEXT NOT NULL,
    merged_at TEXT,
    PRIMARY KEY(project,number,state,head_oid)
);
CREATE TABLE IF NOT EXISTS vp_message_counts (
    run_id INTEGER NOT NULL REFERENCES collection_runs(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    role TEXT NOT NULL,
    total INTEGER NOT NULL,
    PRIMARY KEY(run_id,channel,role)
);
CREATE TABLE IF NOT EXISTS quota_observations (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    provider TEXT NOT NULL,
    cohort TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    precision TEXT NOT NULL CHECK(precision IN ('second','minute','reported')),
    weekly_percent_left REAL NOT NULL CHECK(weekly_percent_left BETWEEN 0 AND 100),
    reset_at TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    UNIQUE(project,provider,cohort,observed_at,evidence_ref)
);
CREATE INDEX IF NOT EXISTS quota_recent ON quota_observations(project,provider,cohort,observed_at);
CREATE TABLE IF NOT EXISTS subscription_costs (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    provider TEXT NOT NULL,
    monthly_usd_cents INTEGER NOT NULL CHECK(monthly_usd_cents >= 0),
    observed_at TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    UNIQUE(project,provider,observed_at,evidence_ref)
);
CREATE INDEX IF NOT EXISTS subscription_latest ON subscription_costs(project,provider,id);
CREATE TABLE IF NOT EXISTS goal_observations (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('local','overall')),
    goal_key TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('planned','in_progress','blocked','verified','unknown')),
    value REAL,
    target REAL,
    unit TEXT,
    evidence_ref TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    UNIQUE(project,goal_key,observed_at,evidence_ref)
);
CREATE INDEX IF NOT EXISTS goals_latest ON goal_observations(project,goal_key,id);
CREATE TABLE IF NOT EXISTS dunk_metrics_snapshots (
    id INTEGER PRIMARY KEY,
    observed_at TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    interval_start TEXT NOT NULL,
    interval_end TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    UNIQUE(generated_at,interval_start,interval_end)
);
CREATE TABLE IF NOT EXISTS accounting_snapshots (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    commit_oid TEXT NOT NULL,
    strict_full_c INTEGER NOT NULL,
    verified_game_identities INTEGER NOT NULL,
    ceiling INTEGER NOT NULL,
    bound_kind TEXT NOT NULL CHECK(bound_kind IN ('exact','upper_bound')),
    source_signature TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    UNIQUE(project,observed_at,commit_oid)
);
CREATE INDEX IF NOT EXISTS accounting_recent ON accounting_snapshots(project,observed_at);
CREATE TABLE IF NOT EXISTS agent_message_events (
    source_event_id INTEGER PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('inbox.sent','message.sent')),
    delivery_source TEXT NOT NULL,
    sender_alias TEXT,
    mailbox_alias TEXT,
    message_bytes INTEGER,
    duration_ms INTEGER,
    attempts INTEGER,
    source_ref TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_message_recent ON agent_message_events(occurred_at,pane_id);
CREATE TABLE IF NOT EXISTS agent_trials (
    trial_id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    reasoning_effort TEXT,
    hypothesis TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
    metered_usd REAL CHECK(metered_usd IS NULL OR metered_usd >= 0),
    correction_minutes REAL CHECK(correction_minutes IS NULL OR correction_minutes >= 0),
    host_psi10 REAL CHECK(host_psi10 IS NULL OR host_psi10 >= 0),
    usage_source TEXT,
    evidence_ref TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_trials_recent ON agent_trials(project,started_at);
CREATE TABLE IF NOT EXISTS function_body_events (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    function_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('authored','accepted','withdrawn')),
    pane_id TEXT NOT NULL,
    trial_id TEXT REFERENCES agent_trials(trial_id),
    source_head TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    reviewed_by TEXT,
    occurred_at TEXT NOT NULL,
    UNIQUE(project,function_id,status,source_head)
);
CREATE INDEX IF NOT EXISTS body_events_recent ON function_body_events(project,occurred_at);
CREATE INDEX IF NOT EXISTS body_events_by_function_time ON function_body_events(project,function_id,occurred_at,id);
