use anyhow::Result;
use rusqlite::params;

use super::Database;

impl Database {
    /// Full schema. Borg has no released DBs, so all prior incremental
    /// migrations were squashed into this single emit-the-final-shape step.
    pub(super) fn migrate_v1(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA_SQL)?;
        self.seed_default_tasks()?;
        self.seed_consolidation_tasks()?;
        self.seed_maintenance_task()?;
        self.seed_heartbeat_task()?;
        self.seed_default_budget()?;
        self.seed_bundled_rate_cards()?;
        Ok(())
    }

    fn seed_bundled_rate_cards(&self) -> Result<()> {
        const BUNDLED_EFFECTIVE_AT: i64 = 1_745_020_800; // 2026-04-19T00:00:00Z
        let mut stmt = self.conn.prepare(
            "INSERT OR IGNORE INTO provider_rate_cards
             (provider_id, model, effective_at, prompt_per_mtok_usd,
              completion_per_mtok_usd, cached_per_mtok_usd, source)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'bundled')",
        )?;
        for entry in crate::pricing::bundled_rate_card_entries() {
            stmt.execute(params![
                entry.provider_id,
                entry.model,
                BUNDLED_EFFECTIVE_AT,
                entry.prompt_per_mtok_usd,
                entry.completion_per_mtok_usd,
            ])?;
        }
        Ok(())
    }

    /// Built-in default tasks (monthly security audit + daily summary).
    pub(super) fn seed_default_tasks(&self) -> Result<()> {
        const SECURITY_AUDIT_TASK_ID: &str = "00000000-0000-4000-8000-5ec041700001";
        const SECURITY_AUDIT_CRON: &str = "0 0 9 1 * *";
        let next_run = crate::tasks::calculate_next_run("cron", SECURITY_AUDIT_CRON, "local")?;
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, 300000)",
            params![
                SECURITY_AUDIT_TASK_ID,
                "Monthly Security Audit",
                "Run a full security audit and report findings. Check firewall, open ports, SSH config, file permissions, disk encryption, OS updates, and running services.",
                "cron",
                SECURITY_AUDIT_CRON,
                "local",
                next_run,
                now,
            ],
        )?;

        const DAILY_SUMMARY_CRON: &str = "0 0 9 * * 1-5";
        let next_run_daily = crate::tasks::calculate_next_run("cron", DAILY_SUMMARY_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, 300000)",
            params![
                crate::daily_summary::DAILY_SUMMARY_TASK_ID,
                "Daily Summary",
                "Produce a daily standup summary of recent activity. Review sessions, tasks, and memory for what was done, what's planned, and any blockers.",
                "cron",
                DAILY_SUMMARY_CRON,
                "local",
                next_run_daily,
                now,
            ],
        )?;
        Ok(())
    }

    fn seed_consolidation_tasks(&self) -> Result<()> {
        let nightly_task_id = crate::consolidation::NIGHTLY_CONSOLIDATION_TASK_ID;
        let weekly_task_id = crate::consolidation::WEEKLY_CONSOLIDATION_TASK_ID;
        let now = chrono::Utc::now().timestamp();

        const NIGHTLY_CRON: &str = "0 0 3 * * *";
        let next_nightly = crate::tasks::calculate_next_run("cron", NIGHTLY_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, 600000, ?9, 'prompt')",
            params![
                nightly_task_id,
                "Nightly Memory Consolidation",
                "Review today's sessions and extract durable information into long-term memory. \
                 For each piece of information: APPEND to an existing topic if relevant, CREATE a new topic, or SKIP if already captured. \
                 Never duplicate information already in existing memory entries. When in doubt, keep the information.",
                "cron",
                NIGHTLY_CRON,
                "local",
                next_nightly,
                now,
                "write_memory,read_memory,memory_search",
            ],
        )?;

        const WEEKLY_CRON: &str = "0 0 4 * * 7";
        let next_weekly = crate::tasks::calculate_next_run("cron", WEEKLY_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, 600000, ?9, 'prompt')",
            params![
                weekly_task_id,
                "Weekly Memory Maintenance",
                "Review all long-term memory for: (a) duplicate information across entries, \
                 (b) outdated information superseded by newer entries, (c) entries that should be merged, \
                 (d) overly verbose entries that can be tightened. \
                 Never delete information unless it is provably superseded. When in doubt, keep it.",
                "cron",
                WEEKLY_CRON,
                "local",
                next_weekly,
                now,
                "write_memory,read_memory,memory_search",
            ],
        )?;
        Ok(())
    }

    fn seed_maintenance_task(&self) -> Result<()> {
        let task_id = crate::maintenance::MAINTENANCE_TASK_ID;
        const MAINTENANCE_CRON: &str = "0 0 2 * * *";
        let now = chrono::Utc::now().timestamp();
        let next_run = crate::tasks::calculate_next_run("cron", MAINTENANCE_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 1, 300000, NULL, 'maintenance')",
            params![
                task_id,
                "Daily Self-Healing Maintenance",
                "Runs the doctor sweep, prunes old logs and activity rows, heals stalled scheduled tasks, and surfaces persistent warnings. Pure maintenance — does not invoke the LLM.",
                "cron",
                MAINTENANCE_CRON,
                "local",
                next_run,
                now,
            ],
        )?;
        Ok(())
    }

    fn seed_heartbeat_task(&self) -> Result<()> {
        let task_id = crate::heartbeat_task::HEARTBEAT_TASK_ID;
        let now = chrono::Utc::now().timestamp();
        let next_run = crate::tasks::calculate_next_run("interval", "30m", "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type, pre_run_command, pre_run_cwd, pre_run_timeout_ms, min_interval_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 0, 300000, ?9, NULL, NULL, 'heartbeat', NULL, NULL, 60000, ?10)",
            rusqlite::params![
                task_id,
                "Heartbeat",
                "",
                "interval",
                "30m",
                "local",
                next_run,
                now,
                crate::heartbeat_task::BROADCAST_DELIVERY,
                crate::constants::MIN_HEARTBEAT_INTERVAL_SECS as i64,
            ],
        )?;
        Ok(())
    }

    fn seed_default_budget(&self) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR IGNORE INTO budgets
             (id, scope, reset_period, hard_cap_usd, soft_cap_usd,
              current_spent_usd, current_period_start, created_at, updated_at)
             VALUES ('default', 'default', 'daily', '20', '5', '0', ?1, ?1, ?1)",
            params![now],
        )?;
        Ok(())
    }
}

const SCHEMA_SQL: &str = r#"
-- Core sessions / messages -------------------------------------------------
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    model TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    parent_session_id TEXT,
    forked_from_message_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_calls_json TEXT,
    tool_call_id TEXT,
    timestamp TEXT,
    created_at INTEGER NOT NULL,
    content_parts_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    session_id UNINDEXED,
    role UNINDEXED,
    content,
    content='messages',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, session_id, role, content)
    VALUES (new.id, new.session_id, new.role, COALESCE(new.content, ''));
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, session_id, role, content)
    VALUES ('delete', old.id, old.session_id, old.role, COALESCE(old.content, ''));
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, session_id, role, content)
    VALUES ('delete', old.id, old.session_id, old.role, COALESCE(old.content, ''));
    INSERT INTO messages_fts(rowid, session_id, role, content)
    VALUES (new.id, new.session_id, new.role, COALESCE(new.content, ''));
END;

CREATE TABLE IF NOT EXISTS token_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    prompt_tokens INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL,
    provider TEXT,
    model TEXT,
    cost_usd REAL,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_token_usage_ts ON token_usage(timestamp);
CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model);

-- Scheduler / tasks --------------------------------------------------------
CREATE TABLE IF NOT EXISTS scheduled_tasks (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    prompt TEXT NOT NULL,
    schedule_type TEXT NOT NULL,
    schedule_expr TEXT NOT NULL,
    timezone TEXT NOT NULL DEFAULT 'local',
    status TEXT NOT NULL DEFAULT 'active',
    next_run INTEGER,
    created_at INTEGER NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 3,
    retry_after INTEGER,
    last_error TEXT,
    timeout_ms INTEGER NOT NULL DEFAULT 300000,
    delivery_channel TEXT,
    delivery_target TEXT,
    allowed_tools TEXT,
    task_type TEXT NOT NULL DEFAULT 'prompt',
    pre_run_command TEXT,
    pre_run_cwd TEXT,
    pre_run_timeout_ms INTEGER NOT NULL DEFAULT 60000,
    bypass_quiet_hours_once INTEGER NOT NULL DEFAULT 0,
    min_interval_secs INTEGER,
    last_response_hash TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_due ON scheduled_tasks(status, next_run);
CREATE INDEX IF NOT EXISTS idx_tasks_retry ON scheduled_tasks(status, retry_after);

CREATE TABLE IF NOT EXISTS task_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL REFERENCES scheduled_tasks(id),
    started_at INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    result TEXT,
    error TEXT,
    status TEXT NOT NULL DEFAULT 'success'
);
CREATE INDEX IF NOT EXISTS idx_task_runs_task ON task_runs(task_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_task_runs_status ON task_runs(status);

CREATE TABLE IF NOT EXISTS daemon_lock (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    pid          INTEGER NOT NULL,
    started_at   INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL
);

-- Channels / pairing -------------------------------------------------------
CREATE TABLE IF NOT EXISTS channel_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_name TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_active INTEGER NOT NULL,
    UNIQUE(channel_name, sender_id)
);

CREATE TABLE IF NOT EXISTS channel_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_name TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    direction TEXT NOT NULL,
    content TEXT,
    metadata_json TEXT,
    session_id TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_channel_messages_session
    ON channel_messages(channel_name, sender_id, created_at);

CREATE TABLE IF NOT EXISTS pairing_requests (
    id TEXT PRIMARY KEY,
    channel_name TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    code TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    display_name TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    approved_at INTEGER,
    UNIQUE(channel_name, code)
);
CREATE INDEX IF NOT EXISTS idx_pairing_channel_sender
    ON pairing_requests(channel_name, sender_id, status);

CREATE TABLE IF NOT EXISTS approved_senders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_name TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    display_name TEXT,
    approved_at INTEGER NOT NULL,
    pairing_request_id TEXT,
    UNIQUE(channel_name, sender_id)
);

-- Plugins / customizations -------------------------------------------------
CREATE TABLE IF NOT EXISTS customizations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    category TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'installed',
    version TEXT NOT NULL DEFAULT '1.0.0',
    installed_at INTEGER NOT NULL,
    verified_at INTEGER,
    config_json TEXT
);

CREATE TABLE IF NOT EXISTS installed_tools (
    name TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    runtime TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'user',
    customization_id TEXT,
    installed_at INTEGER NOT NULL,
    FOREIGN KEY(customization_id) REFERENCES customizations(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS installed_channels (
    name TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    runtime TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'user',
    customization_id TEXT,
    webhook_path TEXT NOT NULL,
    installed_at INTEGER NOT NULL,
    FOREIGN KEY(customization_id) REFERENCES customizations(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS customization_credentials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    customization_id TEXT NOT NULL,
    credential_key TEXT NOT NULL,
    storage_type TEXT NOT NULL,
    keychain_service TEXT,
    env_var TEXT,
    FOREIGN KEY(customization_id) REFERENCES customizations(id) ON DELETE CASCADE,
    UNIQUE(customization_id, credential_key)
);

CREATE TABLE IF NOT EXISTS file_hashes (
    customization_id TEXT NOT NULL,
    file_path TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(customization_id) REFERENCES customizations(id) ON DELETE CASCADE,
    UNIQUE(customization_id, file_path)
);

-- Delivery queue -----------------------------------------------------------
CREATE TABLE IF NOT EXISTS delivery_queue (
    id TEXT PRIMARY KEY,
    channel_name TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    channel_id TEXT,
    session_id TEXT,
    payload_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 5,
    next_retry_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    error TEXT
);
CREATE INDEX IF NOT EXISTS idx_delivery_queue_status
    ON delivery_queue(status, next_retry_at);

-- Multi-agent --------------------------------------------------------------
CREATE TABLE IF NOT EXISTS agent_roles (
    name TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    model TEXT,
    provider TEXT,
    temperature REAL,
    system_instructions TEXT,
    tools_allowed TEXT,
    max_iterations INTEGER,
    is_builtin INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sub_agent_runs (
    id TEXT PRIMARY KEY,
    nickname TEXT NOT NULL,
    role TEXT NOT NULL,
    parent_session_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    depth INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending_init',
    result_text TEXT,
    error_text TEXT,
    created_at INTEGER NOT NULL,
    completed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sub_agent_runs_parent ON sub_agent_runs(parent_session_id);

-- Memory -------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS memory_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL DEFAULT 'global',
    name TEXT NOT NULL,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    estimated_tokens INTEGER,
    UNIQUE(scope, name)
);
CREATE INDEX IF NOT EXISTS idx_memory_entries_scope ON memory_entries(scope);
CREATE INDEX IF NOT EXISTS idx_memory_entries_updated ON memory_entries(updated_at);

CREATE TABLE IF NOT EXISTS memory_embeddings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    scope       TEXT NOT NULL DEFAULT 'global',
    filename    TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    embedding   BLOB NOT NULL,
    dimension   INTEGER NOT NULL,
    model       TEXT NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(scope, filename)
);
CREATE INDEX IF NOT EXISTS idx_memory_embeddings_scope ON memory_embeddings(scope);

CREATE TABLE IF NOT EXISTS memory_chunks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    scope        TEXT NOT NULL DEFAULT 'global',
    filename     TEXT NOT NULL,
    chunk_index  INTEGER NOT NULL,
    start_line   INTEGER,
    end_line     INTEGER,
    content      TEXT NOT NULL,
    content_hash TEXT NOT NULL DEFAULT '',
    embedding    BLOB,
    dimension    INTEGER,
    model        TEXT,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(scope, filename, chunk_index)
);
CREATE INDEX IF NOT EXISTS idx_memory_chunks_scope_file ON memory_chunks(scope, filename);

CREATE VIRTUAL TABLE IF NOT EXISTS memory_chunks_fts USING fts5(
    scope UNINDEXED,
    filename UNINDEXED,
    chunk_index UNINDEXED,
    content,
    content='memory_chunks',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memory_chunks_ai AFTER INSERT ON memory_chunks BEGIN
    INSERT INTO memory_chunks_fts(rowid, scope, filename, chunk_index, content)
    VALUES (new.id, new.scope, new.filename, new.chunk_index, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_chunks_ad AFTER DELETE ON memory_chunks BEGIN
    INSERT INTO memory_chunks_fts(memory_chunks_fts, rowid, scope, filename, chunk_index, content)
    VALUES ('delete', old.id, old.scope, old.filename, old.chunk_index, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_chunks_au AFTER UPDATE ON memory_chunks BEGIN
    INSERT INTO memory_chunks_fts(memory_chunks_fts, rowid, scope, filename, chunk_index, content)
    VALUES ('delete', old.id, old.scope, old.filename, old.chunk_index, old.content);
    INSERT INTO memory_chunks_fts(rowid, scope, filename, chunk_index, content)
    VALUES (new.id, new.scope, new.filename, new.chunk_index, new.content);
END;

CREATE TABLE IF NOT EXISTS embedding_cache (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    provider     TEXT NOT NULL,
    model        TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    embedding    BLOB NOT NULL,
    dimension    INTEGER NOT NULL,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_accessed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(provider, model, content_hash)
);
CREATE INDEX IF NOT EXISTS idx_embedding_cache_lookup
    ON embedding_cache(provider, model, content_hash);
CREATE INDEX IF NOT EXISTS idx_embedding_cache_last_accessed
    ON embedding_cache(last_accessed_at);

CREATE TABLE IF NOT EXISTS session_index_status (
    session_id    TEXT PRIMARY KEY,
    indexed_at    INTEGER NOT NULL,
    message_count INTEGER NOT NULL DEFAULT 0
);

-- Scripts ------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS scripts (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    description     TEXT NOT NULL DEFAULT '',
    runtime         TEXT NOT NULL DEFAULT 'python',
    entrypoint      TEXT NOT NULL,
    sandbox_profile TEXT NOT NULL DEFAULT 'default',
    network_access  INTEGER NOT NULL DEFAULT 0,
    fs_read         TEXT NOT NULL DEFAULT '[]',
    fs_write        TEXT NOT NULL DEFAULT '[]',
    ephemeral       INTEGER NOT NULL DEFAULT 0,
    hmac            TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    last_run_at     INTEGER,
    run_count       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_scripts_name ON scripts(name);
CREATE INDEX IF NOT EXISTS idx_scripts_ephemeral ON scripts(ephemeral, created_at);

-- Vitals / bond / evolution chains ----------------------------------------
CREATE TABLE IF NOT EXISTS vitals_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    category TEXT NOT NULL,
    source TEXT NOT NULL,
    stability_delta INTEGER NOT NULL DEFAULT 0,
    focus_delta INTEGER NOT NULL DEFAULT 0,
    sync_delta INTEGER NOT NULL DEFAULT 0,
    growth_delta INTEGER NOT NULL DEFAULT 0,
    happiness_delta INTEGER NOT NULL DEFAULT 0,
    metadata_json TEXT,
    created_at INTEGER NOT NULL,
    hmac TEXT NOT NULL,
    prev_hmac TEXT NOT NULL DEFAULT '0'
);
CREATE INDEX IF NOT EXISTS idx_vitals_events_created ON vitals_events(created_at);
CREATE TRIGGER IF NOT EXISTS vitals_events_no_update
    BEFORE UPDATE ON vitals_events
    BEGIN SELECT RAISE(ABORT, 'vitals_events is append-only'); END;
CREATE TRIGGER IF NOT EXISTS vitals_events_no_delete
    BEFORE DELETE ON vitals_events
    BEGIN SELECT RAISE(ABORT, 'vitals_events is append-only'); END;

CREATE TABLE IF NOT EXISTS bond_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    score_delta INTEGER NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    hmac TEXT NOT NULL,
    prev_hmac TEXT NOT NULL DEFAULT '0',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_bond_events_created ON bond_events(created_at);
CREATE INDEX IF NOT EXISTS idx_bond_events_type ON bond_events(event_type);
CREATE TRIGGER IF NOT EXISTS bond_events_no_update
    BEFORE UPDATE ON bond_events
    BEGIN SELECT RAISE(ABORT, 'bond_events is append-only'); END;
CREATE TRIGGER IF NOT EXISTS bond_events_no_delete
    BEFORE DELETE ON bond_events
    BEGIN SELECT RAISE(ABORT, 'bond_events is append-only'); END;

CREATE TABLE IF NOT EXISTS device_keys (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    public_key  BLOB NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evolution_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    xp_delta INTEGER NOT NULL DEFAULT 0,
    archetype TEXT,
    source TEXT NOT NULL,
    metadata_json TEXT,
    created_at INTEGER NOT NULL,
    hmac TEXT NOT NULL,
    prev_hmac TEXT NOT NULL DEFAULT '0',
    session_id TEXT,
    pubkey_id INTEGER REFERENCES device_keys(id)
);
CREATE INDEX IF NOT EXISTS idx_evolution_events_created ON evolution_events(created_at);
CREATE INDEX IF NOT EXISTS idx_evolution_events_archetype ON evolution_events(archetype);
CREATE INDEX IF NOT EXISTS idx_evolution_events_session ON evolution_events(session_id);
CREATE INDEX IF NOT EXISTS idx_evolution_events_pubkey ON evolution_events(pubkey_id);
CREATE TRIGGER IF NOT EXISTS evolution_events_no_update
    BEFORE UPDATE ON evolution_events
    BEGIN SELECT RAISE(ABORT, 'evolution_events is append-only'); END;
CREATE TRIGGER IF NOT EXISTS evolution_events_no_delete
    BEFORE DELETE ON evolution_events
    BEGIN SELECT RAISE(ABORT, 'evolution_events is append-only'); END;

CREATE TABLE IF NOT EXISTS hmac_checkpoints (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    domain TEXT NOT NULL,
    event_id INTEGER NOT NULL,
    prev_hmac TEXT NOT NULL,
    state_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_hmac_checkpoints_domain
    ON hmac_checkpoints(domain, event_id);

-- Outboxes / activity / lifecycle -----------------------------------------
CREATE TABLE IF NOT EXISTS pending_celebrations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    celebration_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    delivered_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_pending_celebrations_undelivered
    ON pending_celebrations (delivered_at) WHERE delivered_at IS NULL;

CREATE TABLE IF NOT EXISTS activity_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    level TEXT NOT NULL,
    category TEXT NOT NULL,
    message TEXT NOT NULL,
    detail TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_activity_log_created ON activity_log(created_at);
CREATE INDEX IF NOT EXISTS idx_activity_log_category ON activity_log(category, created_at);
CREATE INDEX IF NOT EXISTS idx_activity_log_level ON activity_log(level, created_at);

CREATE TABLE IF NOT EXISTS client_tokens (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash    BLOB    NOT NULL UNIQUE,
    label         TEXT    NOT NULL UNIQUE,
    scopes        TEXT    NOT NULL DEFAULT '',
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER,
    revoked_at    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_client_tokens_hash ON client_tokens(token_hash);

CREATE TABLE IF NOT EXISTS webhook_definitions (
    name         TEXT PRIMARY KEY,
    secret_hash  BLOB NOT NULL,
    sender_path  TEXT NOT NULL,
    thread_path  TEXT,
    body_path    TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    revoked_at   INTEGER
);

CREATE TABLE IF NOT EXISTS providers (
    id              TEXT PRIMARY KEY,
    category        TEXT NOT NULL,
    display_name    TEXT NOT NULL,
    endpoint_url    TEXT NOT NULL,
    api_key_env     TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS provider_rate_cards (
    provider_id            TEXT NOT NULL,
    model                  TEXT NOT NULL,
    effective_at           INTEGER NOT NULL,
    prompt_per_mtok_usd    TEXT NOT NULL,
    completion_per_mtok_usd TEXT NOT NULL,
    cached_per_mtok_usd    TEXT,
    source                 TEXT NOT NULL DEFAULT 'bundled',
    PRIMARY KEY (provider_id, model, effective_at)
);
CREATE INDEX IF NOT EXISTS idx_rate_cards_lookup
    ON provider_rate_cards(provider_id, model, effective_at DESC);

CREATE TABLE IF NOT EXISTS budgets (
    id                     TEXT PRIMARY KEY,
    scope                  TEXT NOT NULL,
    reset_period           TEXT NOT NULL,
    hard_cap_usd           TEXT NOT NULL,
    soft_cap_usd           TEXT,
    current_spent_usd      TEXT NOT NULL DEFAULT '0',
    current_period_start   INTEGER NOT NULL,
    last_soft_toast_period INTEGER,
    created_at             INTEGER NOT NULL,
    updated_at             INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_budgets_scope ON budgets(scope);

CREATE TABLE IF NOT EXISTS provider_call_events (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type             TEXT NOT NULL,
    provider_id            TEXT,
    model                  TEXT,
    rate_card_effective_at INTEGER,
    prompt_tokens          INTEGER,
    completion_tokens      INTEGER,
    cached_tokens          INTEGER,
    usd_cost               TEXT,
    budget_scope           TEXT,
    metadata_json          TEXT,
    session_id             TEXT,
    created_at             INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pce_created ON provider_call_events(created_at);
CREATE INDEX IF NOT EXISTS idx_pce_type ON provider_call_events(event_type);

-- PKI / audit chain --------------------------------------------------------
CREATE TABLE IF NOT EXISTS internal_ca_certs (
    serial      INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    pem         TEXT NOT NULL,
    fingerprint TEXT NOT NULL UNIQUE,
    subject     TEXT NOT NULL,
    not_before  INTEGER NOT NULL,
    not_after   INTEGER NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active',
    issued_at   INTEGER NOT NULL,
    revoked_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_internal_ca_certs_kind
    ON internal_ca_certs(kind, status);

CREATE TABLE IF NOT EXISTS paired_devices (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    fingerprint   TEXT NOT NULL UNIQUE,
    device_label  TEXT NOT NULL,
    scope_json    TEXT NOT NULL,
    paired_at     INTEGER NOT NULL,
    last_seen_at  INTEGER,
    status        TEXT NOT NULL DEFAULT 'active',
    principal_id  TEXT NOT NULL,
    cert_serial   INTEGER NOT NULL REFERENCES internal_ca_certs(serial)
);

CREATE TABLE IF NOT EXISTS revoked_fingerprints (
    fingerprint TEXT PRIMARY KEY,
    revoked_at  INTEGER NOT NULL,
    reason      TEXT
);

CREATE TABLE IF NOT EXISTS pairing_codes (
    code_hash   TEXT PRIMARY KEY,
    expires_at  INTEGER NOT NULL,
    intent      TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    consumed_at INTEGER,
    attempts    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_pairing_codes_unconsumed
    ON pairing_codes(consumed_at, expires_at);

CREATE TABLE IF NOT EXISTS audit_log_chain (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    prev_hash    BLOB NOT NULL,
    hash         BLOB NOT NULL,
    event_type   TEXT NOT NULL,
    event_json   TEXT NOT NULL,
    ts           INTEGER NOT NULL,
    principal_id TEXT NOT NULL,
    event_purged INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_audit_log_event_type
    ON audit_log_chain(event_type, ts);

CREATE TABLE IF NOT EXISTS posture_snapshots (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    evolution_event_id  INTEGER NOT NULL UNIQUE,
    taken_at            INTEGER NOT NULL,
    posture             TEXT NOT NULL,
    source              TEXT NOT NULL,
    schema_version      INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_posture_snapshots_taken_at
    ON posture_snapshots(taken_at DESC);

-- Web search cache ---------------------------------------------------------
CREATE TABLE IF NOT EXISTS web_search_cache (
    provider    TEXT NOT NULL,
    query_hash  TEXT NOT NULL,
    query       TEXT NOT NULL,
    body        TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, query_hash)
);
CREATE INDEX IF NOT EXISTS idx_web_search_cache_fetched_at
    ON web_search_cache(fetched_at);

-- Output redaction audit ---------------------------------------------------
-- Records *that* a secret was redacted from agent output (channels / TUI /
-- logs), not the secret itself. The audit log is safe to share because it
-- carries only the pattern label and per-pattern hit count. Adding the secret
-- value or any preview here would defeat the feature.
CREATE TABLE IF NOT EXISTS redaction_audit (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    sink        TEXT NOT NULL,
    label       TEXT NOT NULL,
    hit_count   INTEGER NOT NULL,
    context_key TEXT
);
CREATE INDEX IF NOT EXISTS idx_redaction_audit_ts ON redaction_audit(ts DESC);

-- Skill audit / doctor / maintenance --------------------------------------
CREATE TABLE IF NOT EXISTS skill_audit (
    name           TEXT PRIMARY KEY,
    sha256         TEXT NOT NULL,
    first_seen_at  INTEGER NOT NULL,
    last_seen_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS doctor_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ran_at      INTEGER NOT NULL,
    pass_count  INTEGER NOT NULL DEFAULT 0,
    warn_count  INTEGER NOT NULL DEFAULT 0,
    fail_count  INTEGER NOT NULL DEFAULT 0,
    report_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_doctor_runs_ran_at ON doctor_runs(ran_at DESC);

CREATE TABLE IF NOT EXISTS doctor_squelches (
    category    TEXT NOT NULL,
    name        TEXT NOT NULL,
    squelched_at INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    reason      TEXT,
    PRIMARY KEY (category, name)
);
CREATE INDEX IF NOT EXISTS idx_doctor_squelches_expires_at
    ON doctor_squelches(expires_at);

CREATE TABLE IF NOT EXISTS maintenance_issue_state (
    issue_key            TEXT PRIMARY KEY,
    first_seen_at        INTEGER NOT NULL,
    last_seen_at         INTEGER NOT NULL,
    consecutive_count    INTEGER NOT NULL DEFAULT 0,
    last_status          TEXT NOT NULL,
    auto_fix_attempts    INTEGER NOT NULL DEFAULT 0,
    last_auto_fix_at     INTEGER,
    last_auto_fix_result TEXT,
    resolved_at          INTEGER
);
CREATE INDEX IF NOT EXISTS idx_maintenance_issue_state_open
    ON maintenance_issue_state(last_status, consecutive_count, issue_key);
CREATE INDEX IF NOT EXISTS idx_maintenance_issue_state_resolved
    ON maintenance_issue_state(resolved_at);

-- Credential pool ----------------------------------------------------------
-- Per-provider pool of API keys with cooldown/exhaustion state. The plaintext
-- key is never stored; rows are keyed by a 16-hex BLAKE3 fingerprint that
-- re-attaches state to live keys at load time.
--
-- `auth_type` distinguishes API keys (default) from OAuth credentials, the
-- latter of which carry `expires_at_ms` and (in memory only) a refresh token.
-- `base_url` overrides the provider's default endpoint for OpenAI-compatible
-- custom endpoints. `extra_json` is a per-provider blob (PKCE verifier, OAuth
-- client_id) reserved for the refresh flow. Pre-release: schema is edited in
-- place rather than chained — existing dev DBs blow away.
CREATE TABLE IF NOT EXISTS credential_pool (
    provider               TEXT NOT NULL,
    fingerprint            TEXT NOT NULL,
    id                     TEXT NOT NULL,
    label                  TEXT NOT NULL,
    priority               INTEGER NOT NULL,
    source_json            TEXT NOT NULL,
    use_count              INTEGER NOT NULL DEFAULT 0,
    last_status            TEXT NOT NULL DEFAULT 'ok',
    last_status_at_ms      INTEGER,
    last_error_code        INTEGER,
    last_error_reason      TEXT,
    last_error_message     TEXT,
    last_error_reset_at_ms INTEGER,
    removed_at_ms          INTEGER,
    auth_type              TEXT NOT NULL DEFAULT 'api_key',
    expires_at_ms          INTEGER,
    base_url               TEXT,
    extra_json             TEXT,
    PRIMARY KEY (provider, fingerprint)
);
CREATE INDEX IF NOT EXISTS idx_credential_pool_provider
    ON credential_pool(provider);

-- Suppressed credential sources. Operators removing a key via `borg auth
-- remove` write a row here so re-seeding from env/config doesn't undo it.
CREATE TABLE IF NOT EXISTS credential_suppressions (
    provider         TEXT NOT NULL,
    source_kind      TEXT NOT NULL,
    source_value     TEXT NOT NULL,
    suppressed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (provider, source_kind, source_value)
);
"#;
