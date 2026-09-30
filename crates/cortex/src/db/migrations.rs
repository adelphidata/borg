use std::sync::LazyLock;

use anyhow::Result;
use libsql::params;

use super::Database;
use crate::constants::{
    PRE_RUN_DEFAULT_TIMEOUT_MS, SCHEDULED_TASK_DEFAULT_TIMEOUT_MS, SEEDED_WORKFLOW_TIMEOUT_MS,
};

impl Database {
    /// The whole schema, in one step. Borg has no released DBs, so there is
    /// nothing to upgrade *from* — `SCHEMA_SQL` is the single source of truth
    /// for the shape, and a new table or column belongs there, not in a
    /// follow-on migration.
    ///
    /// The one thing a follow-on migration is still for: reconciling a
    /// database that was already stamped before the column was added to
    /// `SCHEMA_SQL`. Every developer and pre-release install has one, and it
    /// never re-runs this function, so a column added here alone would exist
    /// on fresh databases and be missing on every existing one. See
    /// [`Database::migrate_v2`].
    pub(super) async fn migrate_v1(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA_SQL.as_str()).await?;
        self.seed_default_tasks().await?;
        self.seed_consolidation_tasks().await?;
        self.seed_maintenance_task().await?;
        self.seed_heartbeat_task().await?;
        self.seed_default_budget().await?;
        self.seed_bundled_rate_cards().await?;
        Ok(())
    }

    /// Reconcile `scheduled_tasks` with the columns `SCHEMA_SQL` declares.
    ///
    /// `migrate_v1` only ever runs against a database that has no stamp, so a
    /// column added to `SCHEMA_SQL` afterwards is missing from every database
    /// created before that edit — and every query naming it fails there while
    /// passing on a fresh one. Adding the column here is idempotent: the
    /// `PRAGMA` check means it is a no-op on a database that already has it,
    /// including one freshly built from `SCHEMA_SQL`.
    pub(super) async fn migrate_v2(&self) -> Result<()> {
        if !self.column_exists("scheduled_tasks", "continuity").await? {
            self.conn
                .execute(
                    "ALTER TABLE scheduled_tasks \
                     ADD COLUMN continuity INTEGER NOT NULL DEFAULT 0",
                    (),
                )
                .await?;
        }
        Ok(())
    }

    /// Add the manual-run marker and the failure-delivery route to
    /// `scheduled_tasks` on databases stamped before they joined `SCHEMA_SQL`.
    /// Same contract as [`Database::migrate_v2`]: idempotent, and a no-op on a
    /// database built from the current `SCHEMA_SQL`.
    pub(super) async fn migrate_v3(&self) -> Result<()> {
        for (column, decl) in [
            (
                "run_now_requested",
                "run_now_requested INTEGER NOT NULL DEFAULT 0",
            ),
            ("failure_delivery_channel", "failure_delivery_channel TEXT"),
            ("failure_delivery_target", "failure_delivery_target TEXT"),
        ] {
            if !self.column_exists("scheduled_tasks", column).await? {
                self.conn
                    .execute(
                        &format!("ALTER TABLE scheduled_tasks ADD COLUMN {decl}"),
                        (),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Rewrite legacy `mcp.servers` rows whose `tools.include` is an empty
    /// list.
    ///
    /// Before the include list became optional, the serializer always wrote
    /// `"include": []` and the filter read that as "no allowlist". Today an
    /// explicit empty list admits nothing, so a row written by the old CLI
    /// would silently register zero tools. The old CLI had no way to set the
    /// list, so every stored `[]` from that era means "absent": drop the key.
    /// Idempotent — a row without the key, or with a non-empty list, is left
    /// byte-for-byte as it was.
    ///
    /// Gated on its own `meta` marker rather than on `schema_version`, and
    /// that distinction is load-bearing. `stamp_is_current` is `==`, so an
    /// older binary — another worktree sharing `~/.borg/borg.db`, or a
    /// rollback — restamps the database and makes every `migrate_vN` run
    /// again. Every other migration is safe to repeat; this one is not. Once
    /// the new serializer is in use, `"include": []` is a deliberate
    /// lock-down that must never be rewritten, and a second run could not
    /// tell it apart from the legacy default. The marker survives a stamp
    /// regression, so the rewrite happens exactly once per database.
    pub(super) async fn migrate_v4(&self) -> Result<()> {
        const KEY: &str = "mcp.servers";
        const DONE_MARKER: &str = "mcp_include_optional_rewrite";
        if self.get_meta(DONE_MARKER).await?.is_some() {
            return Ok(());
        }
        // Rewrite first, stamp after: a database with nothing to rewrite is
        // just as finished as one that was rewritten, and leaving it unmarked
        // would arm the rewrite against lock-downs written later. Stamping
        // *before* the write would instead let a failed write (a busy
        // database, a full disk) mark the legacy rows as done, and every
        // stored `[]` would then read as a lock-down forever.
        self.rewrite_legacy_empty_includes(KEY).await?;
        self.set_meta(DONE_MARKER, "1").await?;
        Ok(())
    }

    /// The body of [`Self::migrate_v4`], without the marker bookkeeping.
    async fn rewrite_legacy_empty_includes(&self, key: &str) -> Result<()> {
        let Some(raw) = self.get_setting(key).await? else {
            return Ok(());
        };
        let mut servers: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    key,
                    error = %e,
                    "settings row is not valid JSON; leaving it for the config loader to report"
                );
                return Ok(());
            }
        };
        let rewritten = Self::drop_empty_include_lists(&mut servers);
        if rewritten.is_empty() {
            return Ok(());
        }
        tracing::info!(
            servers = ?rewritten,
            "upgraded legacy empty MCP include lists to \"no allowlist\""
        );
        self.set_setting(key, &serde_json::to_string(&servers)?)
            .await?;
        Ok(())
    }

    /// Remove `tools.include` from every server whose list is empty; returns
    /// the names of the servers that changed.
    fn drop_empty_include_lists(servers: &mut serde_json::Value) -> Vec<String> {
        let mut changed = Vec::new();
        let Some(map) = servers.as_object_mut() else {
            return changed;
        };
        for (name, server) in map.iter_mut() {
            let Some(tools) = server.get_mut("tools").and_then(|t| t.as_object_mut()) else {
                continue;
            };
            if tools
                .get("include")
                .and_then(|v| v.as_array())
                .is_some_and(Vec::is_empty)
            {
                tools.remove("include");
                changed.push(name.clone());
            }
        }
        changed
    }

    /// Whether `table` currently has a column named `column`.
    async fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        // `PRAGMA table_info` does not accept a bound parameter for the table
        // name; every caller passes a literal from this module.
        let mut rows = self
            .conn
            .query(&format!("PRAGMA table_info({table})"), ())
            .await?;
        while let Some(row) = rows.next().await? {
            if row.get::<String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn seed_bundled_rate_cards(&self) -> Result<()> {
        const BUNDLED_EFFECTIVE_AT: i64 = 1_745_020_800; // 2026-04-19T00:00:00Z
        let stmt = self
            .conn
            .prepare(
                "INSERT OR IGNORE INTO provider_rate_cards
                 (provider_id, model, effective_at, prompt_per_mtok_usd,
                  completion_per_mtok_usd, cached_per_mtok_usd, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'bundled')",
            )
            .await?;
        for entry in crate::pricing::bundled_rate_card_entries() {
            // libsql's `Statement` holds binding/execution state between
            // calls — without `reset()`, only the first iteration's
            // INSERT actually fires (the rest are silently no-op'd). See
            // the matching fix in `Database::ensure_all_settings`.
            stmt.reset();
            stmt.execute(params!(
                entry.provider_id,
                entry.model,
                BUNDLED_EFFECTIVE_AT,
                entry.prompt_per_mtok_usd,
                entry.completion_per_mtok_usd,
            ))
            .await?;
        }
        Ok(())
    }

    /// Built-in default tasks (monthly security audit + daily summary).
    pub(super) async fn seed_default_tasks(&self) -> Result<()> {
        use crate::task_ids::{DAILY_SUMMARY_TASK_ID, SECURITY_AUDIT_TASK_ID};
        const SECURITY_AUDIT_CRON: &str = "0 0 9 1 * *";
        let next_run = crate::tasks::calculate_next_run("cron", SECURITY_AUDIT_CRON, "local")?;
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, ?9)",
            params!(
                SECURITY_AUDIT_TASK_ID,
                "Monthly Security Audit",
                "Run a full security audit and report findings. Check firewall, open ports, SSH config, file permissions, disk encryption, OS updates, and running services.",
                "cron",
                SECURITY_AUDIT_CRON,
                "local",
                next_run,
                now,
                SCHEDULED_TASK_DEFAULT_TIMEOUT_MS,
            ),
        ).await?;

        const DAILY_SUMMARY_CRON: &str = "0 0 9 * * 1-5";
        let next_run_daily = crate::tasks::calculate_next_run("cron", DAILY_SUMMARY_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, ?9)",
            params!(
                DAILY_SUMMARY_TASK_ID,
                "Daily Summary",
                "Produce a daily standup summary of recent activity. Review sessions, tasks, and memory for what was done, what's planned, and any blockers.",
                "cron",
                DAILY_SUMMARY_CRON,
                "local",
                next_run_daily,
                now,
                SCHEDULED_TASK_DEFAULT_TIMEOUT_MS,
            ),
        ).await?;
        Ok(())
    }

    async fn seed_consolidation_tasks(&self) -> Result<()> {
        let nightly_task_id = crate::memory::consolidation::NIGHTLY_CONSOLIDATION_TASK_ID;
        let weekly_task_id = crate::memory::consolidation::WEEKLY_CONSOLIDATION_TASK_ID;
        let now = chrono::Utc::now().timestamp();
        let allowed_tools = crate::memory::consolidation::CONSOLIDATION_TOOLS.join(",");

        const NIGHTLY_CRON: &str = "0 0 3 * * *";
        let next_nightly = crate::tasks::calculate_next_run("cron", NIGHTLY_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, ?9, ?10, 'prompt')",
            params!(
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
                SEEDED_WORKFLOW_TIMEOUT_MS,
                allowed_tools.clone(),
            ),
        ).await?;

        const WEEKLY_CRON: &str = "0 0 4 * * 7";
        let next_weekly = crate::tasks::calculate_next_run("cron", WEEKLY_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 3, ?9, ?10, 'prompt')",
            params!(
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
                SEEDED_WORKFLOW_TIMEOUT_MS,
                allowed_tools,
            ),
        ).await?;
        Ok(())
    }

    async fn seed_maintenance_task(&self) -> Result<()> {
        let task_id = crate::maintenance::MAINTENANCE_TASK_ID;
        const MAINTENANCE_CRON: &str = "0 0 2 * * *";
        let now = chrono::Utc::now().timestamp();
        let next_run = crate::tasks::calculate_next_run("cron", MAINTENANCE_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 1, ?9, NULL, 'maintenance')",
            params!(
                task_id,
                "Daily Self-Healing Maintenance",
                "Runs the doctor sweep, prunes old logs and activity rows, heals stalled scheduled tasks, and surfaces persistent warnings. Pure maintenance — does not invoke the LLM.",
                "cron",
                MAINTENANCE_CRON,
                "local",
                next_run,
                now,
                SCHEDULED_TASK_DEFAULT_TIMEOUT_MS,
            ),
        ).await?;
        Ok(())
    }

    async fn seed_heartbeat_task(&self) -> Result<()> {
        let task_id = crate::heartbeat_task::HEARTBEAT_TASK_ID;
        let now = chrono::Utc::now().timestamp();
        let next_run = crate::tasks::calculate_next_run("interval", "30m", "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, delivery_channel, delivery_target, allowed_tools, task_type, pre_run_command, pre_run_cwd, pre_run_timeout_ms, min_interval_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 0, ?9, ?10, NULL, NULL, 'heartbeat', NULL, NULL, ?11, ?12)",
            params!(
                task_id,
                "Heartbeat",
                "",
                "interval",
                "30m",
                "local",
                next_run,
                now,
                SCHEDULED_TASK_DEFAULT_TIMEOUT_MS,
                crate::heartbeat_task::BROADCAST_DELIVERY,
                PRE_RUN_DEFAULT_TIMEOUT_MS,
                crate::constants::MIN_HEARTBEAT_INTERVAL_SECS as i64,
            ),
        ).await?;
        Ok(())
    }

    async fn seed_default_budget(&self) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT OR IGNORE INTO budgets
             (id, scope, reset_period, hard_cap_usd, soft_cap_usd,
              current_spent_usd, current_period_start, created_at, updated_at)
             VALUES ('default', 'default', 'daily', '20', '5', '0', ?1, ?1, ?1)",
                params!(now),
            )
            .await?;
        Ok(())
    }
}

static SCHEMA_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
-- Core sessions / messages -------------------------------------------------
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    model TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    parent_session_id TEXT,
    forked_from_message_id INTEGER,
    -- /handoff state machine: NULL → 'pending' → 'running' → ('completed' | 'failed').
    -- `handoff_target` is the destination channel name (e.g. "telegram"); the
    -- destination gateway claims by `UPDATE … SET handoff_state='running'`
    -- where state='pending'. `handoff_error` carries the failure reason when
    -- a claim/run fails.
    handoff_state TEXT,
    handoff_target TEXT,
    handoff_error TEXT,
    handoff_requested_at INTEGER,
    -- Per-session lease (worker-pool / fallible executors). A worker takes the
    -- lease before advancing a session and releases it after; on crash the
    -- lease simply expires and any worker can reconstruct from the log and
    -- continue. `lease_epoch` fences a revived dead worker from writing under a
    -- lease that has since been reclaimed. NULL owner = free.
    lease_owner TEXT,
    lease_expiry INTEGER,
    lease_epoch INTEGER NOT NULL DEFAULT 0,
    -- `pinned` exempts a session from retention purging and from the stale
    -- auto-archive sweep. `archived_at` soft-hides a session from listings; it
    -- is never a delete, so archiving is always reversible.
    pinned INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER,
    cwd TEXT,
    git_repo_root TEXT
);
-- ^ `cwd`/`git_repo_root`: workspace identity for workspace-scoped resume
-- (`borg --continue`) — the canonicalized directory the session was opened
-- from and the toplevel of its enclosing git checkout. NULL when the session
-- has no meaningful workspace (gateway channels, daemon-internal lanes).
-- Comments must stay OUTSIDE the column list: SQLite's ALTER machinery
-- rewrites the stored CREATE text and trips over inline comments.
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_sessions_created_at ON sessions(created_at);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_calls_json TEXT,
    tool_call_id TEXT,
    timestamp TEXT,
    created_at INTEGER NOT NULL,
    content_parts_json TEXT,
    -- Session event log (the agent IS this log). `event_kind`/`event_json`
    -- carry fine-grained lifecycle events (tool-call-started, compaction,
    -- fork markers); conversational rows leave them NULL and decode from
    -- role/content. `hmac`/`prev_hmac` form a per-session tamper-evident
    -- chain.
    event_kind TEXT,
    event_json TEXT,
    hmac TEXT,
    prev_hmac TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at);
-- Partial index: `tool_call_counts_since` scans only rows carrying tool calls,
-- so indexing the much larger plain-message majority would be dead weight.
CREATE INDEX IF NOT EXISTS idx_messages_toolcalls
    ON messages(created_at)
    WHERE tool_calls_json IS NOT NULL;

-- FTS lives on a standard FTS5 virtual table with `messages` as the
-- external content source. Sync triggers below keep the index aligned with
-- inserts/updates/deletes on the source table. BM25 ranking is computed
-- in Rust (`crate::memory::bm25`) for parity with the hybrid-search
-- pipeline; libsql does expose a SQL-callable `bm25()`, but routing
-- through Rust keeps vector + BM25 fusion in one place.
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;

-- Per-session goals --------------------------------------------------------
CREATE TABLE IF NOT EXISTS goals (
    session_id        TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    goal_id           TEXT NOT NULL,
    objective         TEXT NOT NULL,
    status            TEXT NOT NULL,
    token_budget      INTEGER,
    tokens_used       INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_goals_status ON goals(status);

-- A recurring prompt that re-enters its own session when the session is idle
-- and `next_fire_at` has passed. Distinct from `scheduled_tasks`, which runs a
-- fresh isolated session per tick; this one reuses the live conversation.
CREATE TABLE IF NOT EXISTS session_heartbeats (
    session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    prompt       TEXT NOT NULL,
    interval_secs INTEGER NOT NULL,
    paused       INTEGER NOT NULL DEFAULT 0,
    next_fire_at INTEGER NOT NULL,
    created_at   INTEGER NOT NULL
);

-- Quality gates attached to a session's goal. A gate is a shell command that
-- must exit 0 before the goal may complete; `last_fingerprint` records the
-- workspace state it last ran against so an unchanged workspace replays the
-- recorded verdict instead of re-running the command.
CREATE TABLE IF NOT EXISTS goal_gates (
    session_id       TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal          INTEGER NOT NULL,
    command          TEXT NOT NULL,
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_exit_code   INTEGER,
    last_output      TEXT,
    last_fingerprint TEXT,
    created_at       INTEGER NOT NULL,
    PRIMARY KEY (session_id, ordinal)
);

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
    timeout_ms INTEGER NOT NULL DEFAULT {SCHEDULED_TASK_DEFAULT_TIMEOUT_MS},
    delivery_channel TEXT,
    delivery_target TEXT,
    allowed_tools TEXT,
    task_type TEXT NOT NULL DEFAULT 'prompt',
    pre_run_command TEXT,
    pre_run_cwd TEXT,
    pre_run_timeout_ms INTEGER NOT NULL DEFAULT {PRE_RUN_DEFAULT_TIMEOUT_MS},
    bypass_quiet_hours_once INTEGER NOT NULL DEFAULT 0,
    min_interval_secs INTEGER,
    -- 1 when each run should see what the previous runs produced. Opt-in:
    -- carrying prior output costs context on every fire, and most scheduled
    -- tasks are stateless by design.
    continuity INTEGER NOT NULL DEFAULT 0,
    last_response_hash TEXT,
    model TEXT,
    model_provider TEXT,
    monitor_mode INTEGER NOT NULL DEFAULT 0,
    last_pre_run_hash TEXT,
    run_now_requested INTEGER NOT NULL DEFAULT 0,
    failure_delivery_channel TEXT,
    failure_delivery_target TEXT
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
    -- Hex-encoded SHA-256 of the lowercased pairing code. Never store plaintext.
    code_hash TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    display_name TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    approved_at INTEGER,
    UNIQUE(channel_name, code_hash)
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

CREATE TABLE IF NOT EXISTS wasm_plugins (
    plugin_id         TEXT PRIMARY KEY,
    manifest_json     TEXT NOT NULL,
    manifest_sha256   TEXT NOT NULL,
    wasm_sha256       TEXT NOT NULL,
    publisher_key     TEXT NOT NULL,
    signature         TEXT NOT NULL,
    local_hmac        TEXT NOT NULL,
    manifest_path     TEXT NOT NULL,
    wasm_path         TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'verified',
    quarantine_reason TEXT,
    installed_at      INTEGER NOT NULL,
    verified_at       INTEGER
);
CREATE INDEX IF NOT EXISTS idx_wasm_plugins_status ON wasm_plugins(status);

CREATE TABLE IF NOT EXISTS wasm_plugin_tools (
    plugin_id         TEXT NOT NULL,
    name              TEXT PRIMARY KEY,
    description       TEXT NOT NULL,
    input_schema_json TEXT NOT NULL,
    installed_at      INTEGER NOT NULL,
    FOREIGN KEY(plugin_id) REFERENCES wasm_plugins(plugin_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_wasm_plugin_tools_plugin ON wasm_plugin_tools(plugin_id);

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
    completed_at INTEGER,
    model TEXT NOT NULL DEFAULT '',
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd REAL NOT NULL DEFAULT 0,
    tool_calls INTEGER NOT NULL DEFAULT 0,
    files_touched TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_sub_agent_runs_parent ON sub_agent_runs(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_sub_agent_runs_session ON sub_agent_runs(session_id);

-- Per-turn snapshots of the spawn tree. The agent loop writes one row at
-- EndTurn for any turn that spawned ≥1 sub-agent; a per-session ring of 10
-- is enforced at write time so this never grows unbounded.
CREATE TABLE IF NOT EXISTS agent_tree_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    label TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    snapshot_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_tree_snapshots_session
    ON agent_tree_snapshots(session_id, id DESC);

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

-- See messages_fts above: standard FTS5 virtual table, BM25 in Rust.
CREATE VIRTUAL TABLE IF NOT EXISTS memory_chunks_fts USING fts5(
    content,
    content='memory_chunks',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memory_chunks_ai AFTER INSERT ON memory_chunks BEGIN
    INSERT INTO memory_chunks_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_chunks_ad AFTER DELETE ON memory_chunks BEGIN
    INSERT INTO memory_chunks_fts(memory_chunks_fts, rowid, content) VALUES('delete', old.id, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memory_chunks_au AFTER UPDATE ON memory_chunks BEGIN
    INSERT INTO memory_chunks_fts(memory_chunks_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO memory_chunks_fts(rowid, content) VALUES (new.id, new.content);
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

-- MCP tool catalog cache ---------------------------------------------------
-- Lets a restart advertise a stdio server's tools while its process is still
-- starting. `config_hash` is a keyed MAC of the resolved config (see
-- `db::mcp_catalog_cache`) — resolved env carries secrets, so only the MAC is
-- stored, never the values.
CREATE TABLE IF NOT EXISTS mcp_catalog_cache (
    config_hash TEXT PRIMARY KEY,
    server_name TEXT NOT NULL,
    tools_json  TEXT NOT NULL,
    server_info TEXT,
    cached_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_catalog_cache_server
    ON mcp_catalog_cache(server_name);

-- APNs device registrations ------------------------------------------------
-- One row per (device_id, apns_token) registration the push pipeline keys off.
-- The App Attest pubkey is stored DER-encoded so the verifier (when wired) can
-- re-check future assertion signatures without re-running attestation.
CREATE TABLE IF NOT EXISTS apns_device_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    device_id TEXT NOT NULL UNIQUE,
    apns_token TEXT NOT NULL,
    bundle_id TEXT NOT NULL,
    app_attest_key_id TEXT NOT NULL,
    app_attest_pub_key_der BLOB NOT NULL,
    registered_at INTEGER NOT NULL,
    last_push_at INTEGER,
    invalidated_at INTEGER,
    invalidated_reason TEXT
);
CREATE INDEX IF NOT EXISTS apns_device_tokens_active
    ON apns_device_tokens (invalidated_at) WHERE invalidated_at IS NULL;

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
-- client_id) reserved for the refresh flow.
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
"#
    )
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_sql_pins_default_timeouts() {
        // The compiled SCHEMA_SQL must interpolate the constants into exactly
        // the byte sequence the SQLite parser expects. If the constants change,
        // this test fails and forces a deliberate decision about migrating
        // existing rows — the seeded literals on production DBs won't update
        // just because the constant did.
        assert!(
            SCHEMA_SQL.contains("timeout_ms INTEGER NOT NULL DEFAULT 300000,"),
            "scheduled_tasks.timeout_ms default drifted from 300000"
        );
        assert!(
            SCHEMA_SQL.contains("pre_run_timeout_ms INTEGER NOT NULL DEFAULT 60000,"),
            "scheduled_tasks.pre_run_timeout_ms default drifted from 60000"
        );
    }

    /// The squash removed the per-open shape-repair pass, and with it the
    /// tests that asserted a damaged DB got rebuilt. `SCHEMA_SQL` is now the
    /// only thing that decides the shape, so guard it directly: open a fresh
    /// DB and assert the objects a running agent needs are all there. Drop a
    /// `CREATE` from `SCHEMA_SQL` and this fails, which is exactly what the
    /// deleted repair pass used to catch at runtime.
    #[tokio::test]
    async fn fresh_db_carries_the_full_schema() {
        let db = Database::test_db().await;
        let conn = db.conn();
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type IN ('table','index') \
                 AND name NOT LIKE 'sqlite_%'",
                (),
            )
            .await
            .unwrap();
        let mut present: Vec<String> = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            present.push(row.get::<String>(0).unwrap());
        }

        // Tables and indexes that only ever reached the schema through an
        // ALTER-style migration are the ones the squash could have silently
        // dropped, so they are named explicitly alongside the core tables.
        for object in [
            "sessions",
            "messages",
            "meta",
            "settings",
            "scheduled_tasks",
            "memory_entries",
            "goals",
            "goal_gates",
            "apns_device_tokens",
            "apns_device_tokens_active",
            "idx_sessions_created_at",
            "idx_messages_created_at",
            "idx_messages_toolcalls",
        ] {
            assert!(
                present.iter().any(|n| n == object),
                "fresh DB is missing `{object}`; SCHEMA_SQL lost it. Present: {present:?}"
            );
        }
    }

    /// A fresh DB must land on `CURRENT_VERSION`, and a second open must be a
    /// no-op rather than replaying the baseline.
    #[tokio::test]
    async fn fresh_db_is_stamped_current_and_reopen_is_idempotent() {
        let db = Database::test_db().await;
        assert_eq!(
            db.schema_version().await.unwrap(),
            Database::CURRENT_VERSION
        );
        db.run_migrations().await.unwrap();
        assert_eq!(
            db.schema_version().await.unwrap(),
            Database::CURRENT_VERSION
        );
    }

    /// Developer databases predate the squash and still carry a stamp of 16.
    /// A `>=` guard would treat that as "already migrated" and silently skip
    /// every future migration on exactly the machines that run this code.
    #[tokio::test]
    async fn stamp_above_current_still_migrates_and_is_restamped() {
        let db = Database::test_db().await;
        db.set_meta("schema_version", "16").await.unwrap();

        db.run_migrations().await.unwrap();

        assert_eq!(
            db.schema_version().await.unwrap(),
            Database::CURRENT_VERSION,
            "a stale higher stamp must be corrected, not honoured"
        );
    }

    /// A database stamped at v2 predates the run-now marker and the failure
    /// route. Every other test builds from `SCHEMA_SQL` where the columns
    /// already exist, so the `ALTER` branch of `migrate_v3` only runs here.
    #[tokio::test]
    async fn a_v2_database_gains_the_run_now_and_failure_route_columns() {
        let db = Database::test_db().await;
        // Last column first: sqlite rewrites the stored CREATE text on each
        // drop, and peeling from the tail keeps the remaining text well-formed.
        for column in [
            "failure_delivery_target",
            "failure_delivery_channel",
            "run_now_requested",
        ] {
            db.conn
                .execute(
                    &format!("ALTER TABLE scheduled_tasks DROP COLUMN {column}"),
                    (),
                )
                .await
                .expect("drop the column to simulate an older database");
            assert!(!db.column_exists("scheduled_tasks", column).await.unwrap());
        }
        db.set_meta("schema_version", "2").await.unwrap();

        db.run_migrations().await.unwrap();

        assert_eq!(
            db.schema_version().await.unwrap(),
            Database::CURRENT_VERSION
        );
        for column in [
            "run_now_requested",
            "failure_delivery_channel",
            "failure_delivery_target",
        ] {
            assert!(
                db.column_exists("scheduled_tasks", column).await.unwrap(),
                "migrate_v3 must add `{column}`"
            );
        }
        // The upgraded row shape reads back through the normal path.
        let rows = db.list_tasks().await.expect("list on upgraded schema");
        assert!(
            rows.iter().all(|t| !t.run_now_requested),
            "the marker defaults to unset on upgraded rows"
        );
    }

    /// A row written by the old CLI carries `"include": []`, which the
    /// filter now reads as "admit nothing". The migration must drop that key
    /// and leave every other shape untouched.
    #[tokio::test]
    async fn legacy_empty_include_lists_become_absent_on_upgrade() {
        let db = Database::test_db().await;
        let legacy = serde_json::json!({
            "old": {"transport": "stdio", "command": "x", "tools": {"include": [], "exclude": ["rm"]}},
            "pinned": {"transport": "stdio", "command": "y", "tools": {"include": ["read"]}},
            "bare": {"transport": "stdio", "command": "z"}
        });
        db.set_setting("mcp.servers", &legacy.to_string())
            .await
            .unwrap();
        db.set_meta("schema_version", "3").await.unwrap();
        // `test_db` already migrated, so clear the marker to model a database
        // that has never seen the rewrite.
        db.delete_meta("mcp_include_optional_rewrite")
            .await
            .unwrap();

        db.run_migrations().await.unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&db.get_setting("mcp.servers").await.unwrap().unwrap()).unwrap();
        assert!(
            after["old"]["tools"].get("include").is_none(),
            "legacy empty list must be dropped, got {after}"
        );
        assert_eq!(after["old"]["tools"]["exclude"], serde_json::json!(["rm"]));
        assert_eq!(
            after["pinned"]["tools"]["include"],
            serde_json::json!(["read"])
        );
        assert_eq!(after["bare"], legacy["bare"]);

        // Running again is a no-op: nothing left to rewrite.
        let before_second = db.get_setting("mcp.servers").await.unwrap();
        db.migrate_v4().await.unwrap();
        assert_eq!(db.get_setting("mcp.servers").await.unwrap(), before_second);
    }

    #[tokio::test]
    async fn migrate_v4_leaves_an_unparseable_servers_row_alone() {
        let db = Database::test_db().await;
        db.delete_meta("mcp_include_optional_rewrite")
            .await
            .unwrap();
        db.set_setting("mcp.servers", "{not json").await.unwrap();
        db.migrate_v4().await.unwrap();
        assert_eq!(
            db.get_setting("mcp.servers").await.unwrap().as_deref(),
            Some("{not json")
        );
    }

    /// An older binary sharing `~/.borg/borg.db` restamps the schema version,
    /// which re-runs every migration. A deliberate lock-down written after
    /// the rewrite must survive that, or the second run silently turns
    /// "this server exposes no tools" into "this server exposes all of them".
    #[tokio::test]
    async fn a_deliberate_empty_include_survives_a_stamp_regression() {
        let db = Database::test_db().await;
        // The rewrite has already happened on this database.
        assert!(
            db.get_meta("mcp_include_optional_rewrite")
                .await
                .unwrap()
                .is_some(),
            "opening a database must mark the one-shot rewrite as done"
        );
        let locked_down = serde_json::json!({
            "vetted": {"transport": "stdio", "command": "x", "tools": {"include": []}}
        });
        db.set_setting("mcp.servers", &locked_down.to_string())
            .await
            .unwrap();
        // An older binary reopened the shared database and stamped it back.
        db.set_meta("schema_version", "2").await.unwrap();

        db.run_migrations().await.unwrap();

        let after: serde_json::Value =
            serde_json::from_str(&db.get_setting("mcp.servers").await.unwrap().unwrap()).unwrap();
        assert_eq!(
            after["vetted"]["tools"]["include"],
            serde_json::json!([]),
            "the lock-down must survive, got {after}"
        );
    }
}
