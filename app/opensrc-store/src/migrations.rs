pub const SCHEMA_V1: &str = r"
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS schema_meta (
    version INTEGER NOT NULL
);
INSERT INTO schema_meta(version)
SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_meta);

CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    project_root TEXT NOT NULL,
    title TEXT,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES runs(id),
    sequence INTEGER NOT NULL,
    role TEXT NOT NULL,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(conversation_id, sequence)
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation
    ON messages(conversation_id, sequence);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    mode TEXT NOT NULL,
    status TEXT NOT NULL,
    request TEXT NOT NULL,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_runs_conversation ON runs(conversation_id, created_at);

CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    parent_id TEXT REFERENCES agents(id),
    canonical_path TEXT NOT NULL,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(run_id, canonical_path)
);
CREATE INDEX IF NOT EXISTS idx_agents_run ON agents(run_id, canonical_path);
CREATE INDEX IF NOT EXISTS idx_agents_parent ON agents(parent_id);

CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    assigned_agent TEXT REFERENCES agents(id),
    status TEXT NOT NULL,
    priority INTEGER NOT NULL,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_run_status ON tasks(run_id, status, priority DESC);

CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY(task_id, depends_on),
    CHECK(task_id <> depends_on)
);

CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    run_id TEXT REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    idempotency_key TEXT UNIQUE,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_run_id ON events(run_id, id);

CREATE TABLE IF NOT EXISTS tool_calls (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    tool_name TEXT NOT NULL,
    state TEXT NOT NULL,
    input_json TEXT NOT NULL,
    output_json TEXT,
    idempotency_key TEXT NOT NULL UNIQUE,
    destructive INTEGER NOT NULL DEFAULT 0,
    started_at TEXT,
    finished_at TEXT
);

CREATE TABLE IF NOT EXISTS model_calls (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    request_json TEXT NOT NULL,
    response_json TEXT,
    state TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE TABLE IF NOT EXISTS token_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    provider TEXT,
    model TEXT,
    usage_json TEXT NOT NULL,
    cost_microusd INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS timings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    timing_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS file_changes (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    workspace_path TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    preimage_hash TEXT,
    postimage_hash TEXT,
    patch TEXT,
    state TEXT NOT NULL DEFAULT 'applied',
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    tool_call_id TEXT REFERENCES tool_calls(id),
    decision TEXT NOT NULL,
    reason TEXT,
    data_json TEXT,
    created_at TEXT NOT NULL,
    decided_at TEXT
);

CREATE TABLE IF NOT EXISTS permission_rules (
    id TEXT PRIMARY KEY,
    scope TEXT NOT NULL,
    effect TEXT NOT NULL,
    run_id TEXT REFERENCES runs(id) ON DELETE CASCADE,
    project_root TEXT,
    tool_name TEXT NOT NULL,
    arguments_json TEXT NOT NULL,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_permission_rules_match
    ON permission_rules(tool_name, effect, scope, run_id, project_root);

CREATE TABLE IF NOT EXISTS errors (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    category TEXT NOT NULL,
    message TEXT NOT NULL,
    retryable INTEGER NOT NULL,
    data_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS checkpoints (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    state_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS completion_objects (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    completion_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(agent_id, task_id)
);

CREATE TABLE IF NOT EXISTS providers (
    id TEXT PRIMARY KEY,
    capabilities_json TEXT NOT NULL,
    config_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workspace_leases (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    task_id TEXT REFERENCES tasks(id),
    mode TEXT NOT NULL,
    root TEXT NOT NULL,
    owned_paths_json TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    released_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_active
    ON workspace_leases(state, root, created_at);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_run
    ON workspace_leases(run_id, state);
CREATE INDEX IF NOT EXISTS idx_workspace_leases_agent
    ON workspace_leases(agent_id, state);

CREATE TABLE IF NOT EXISTS routing_benchmarks (
    id TEXT PRIMARY KEY,
    policy_version TEXT NOT NULL,
    role TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    scenario_id TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_routing_benchmarks_route
    ON routing_benchmarks(policy_version, role, provider, model, created_at);
CREATE INDEX IF NOT EXISTS idx_routing_benchmarks_scenario
    ON routing_benchmarks(scenario_id, created_at);

UPDATE schema_meta SET version = 3 WHERE version < 3;
";
