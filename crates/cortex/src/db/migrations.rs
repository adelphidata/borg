use std::sync::LazyLock;

use anyhow::Result;
use libsql::params;

use super::Database;
use crate::constants::{
    PRE_RUN_DEFAULT_TIMEOUT_MS, SCHEDULED_TASK_DEFAULT_TIMEOUT_MS, SEEDED_WORKFLOW_TIMEOUT_MS,
};

impl Database {
    /// Full schema. Borg has no released DBs, so all prior incremental
    /// migrations were squashed into this single emit-the-final-shape step.
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

    /// Adds the `apns_device_tokens` table that the APNs push pipeline
    /// keys off — one row per (device_id, apns_token) registration.
    /// Provider: Apple Push Notification Service. The App Attest pubkey
    /// is stored DER-encoded so the verifier (when wired) can re-check
    /// future assertion signatures without re-running attestation.
    pub(super) async fn migrate_v3(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS apns_device_tokens (
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
                    ON apns_device_tokens (invalidated_at) WHERE invalidated_at IS NULL;",
            )
            .await?;
        Ok(())
    }

    /// No-op: FTS5 virtual tables are emitted directly in `SCHEMA_SQL`. Kept as
    /// a placeholder so `CURRENT_VERSION` stays at 3 and fresh installs skip it.
    pub(super) async fn migrate_v2(&self) -> Result<()> {
        Ok(())
    }

    /// Hash gateway pairing codes at rest.
    ///
    /// `pairing_requests.code` previously held the plaintext 6-word BIP-39
    /// phrase a sender had to present for approval. Anyone with read access
    /// to the DB file could replay the code before approval expiry. Rename
    /// the column to `code_hash` and expire all pending rows — we cannot
    /// re-hash plaintext we no longer have, but pairings are short-lived so
    /// the operator just creates new ones.
    /// Add /handoff state-machine columns to `sessions`. Idempotent — only
    /// adds the columns that the current schema is missing, so fresh installs
    /// (which already get the full shape from `SCHEMA_SQL`) skip the ALTERs.
    pub(super) async fn migrate_v5(&self) -> Result<()> {
        let mut rows = self.conn.query("PRAGMA table_info(sessions)", ()).await?;
        let mut existing = std::collections::HashSet::new();
        while let Some(row) = rows.next().await? {
            existing.insert(row.get::<String>(1)?);
        }
        for (col, ddl) in [
            (
                "handoff_state",
                "ALTER TABLE sessions ADD COLUMN handoff_state TEXT",
            ),
            (
                "handoff_target",
                "ALTER TABLE sessions ADD COLUMN handoff_target TEXT",
            ),
            (
                "handoff_error",
                "ALTER TABLE sessions ADD COLUMN handoff_error TEXT",
            ),
            (
                "handoff_requested_at",
                "ALTER TABLE sessions ADD COLUMN handoff_requested_at INTEGER",
            ),
        ] {
            if !existing.contains(col) {
                self.conn.execute_batch(ddl).await?;
            }
        }
        Ok(())
    }

    /// Backfill `sub_agent_runs` with the per-agent metrics + model columns
    /// and create `agent_tree_snapshots`. Idempotent — fresh installs get
    /// the final shape from `SCHEMA_SQL` and ALTERs short-circuit when the
    /// column already exists.
    pub(super) async fn migrate_v6(&self) -> Result<()> {
        let mut rows = self
            .conn
            .query("PRAGMA table_info(sub_agent_runs)", ())
            .await?;
        let mut existing = std::collections::HashSet::new();
        while let Some(row) = rows.next().await? {
            existing.insert(row.get::<String>(1)?);
        }
        for (col, ddl) in [
            (
                "model",
                "ALTER TABLE sub_agent_runs ADD COLUMN model TEXT NOT NULL DEFAULT ''",
            ),
            (
                "input_tokens",
                "ALTER TABLE sub_agent_runs ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "output_tokens",
                "ALTER TABLE sub_agent_runs ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "reasoning_tokens",
                "ALTER TABLE sub_agent_runs ADD COLUMN reasoning_tokens INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "cost_usd",
                "ALTER TABLE sub_agent_runs ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0",
            ),
            (
                "tool_calls",
                "ALTER TABLE sub_agent_runs ADD COLUMN tool_calls INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "files_touched",
                "ALTER TABLE sub_agent_runs ADD COLUMN files_touched TEXT NOT NULL DEFAULT '[]'",
            ),
        ] {
            if !existing.contains(col) {
                self.conn.execute(ddl, ()).await?;
            }
        }
        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sub_agent_runs_session ON sub_agent_runs(session_id);
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
                    ON agent_tree_snapshots(session_id, id DESC);",
            )
            .await?;
        Ok(())
    }

    /// Add covering indexes for time-range scans on `sessions` and
    /// `messages`. `sessions(created_at)` and `messages(created_at)`
    /// drove full-table scans on dashboard / audit / consolidation
    /// queries; the partial `messages(created_at) WHERE tool_calls_json
    /// IS NOT NULL` index speeds up `tool_call_counts_since` without
    /// indexing the much larger plain-message majority. Idempotent.
    pub(super) async fn migrate_v7(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_sessions_created_at
                    ON sessions(created_at);
                 CREATE INDEX IF NOT EXISTS idx_messages_created_at
                    ON messages(created_at);
                 CREATE INDEX IF NOT EXISTS idx_messages_toolcalls
                    ON messages(created_at)
                    WHERE tool_calls_json IS NOT NULL;",
            )
            .await?;
        Ok(())
    }

    /// Elevate `messages` into the first-class session event log: add the
    /// `event_kind`/`event_json` payload columns and the `hmac`/`prev_hmac`
    /// chain columns. Idempotent — fresh installs get the final shape from
    /// `SCHEMA_SQL` and these ALTERs short-circuit when the column already
    /// exists. Existing message rows keep working: NULL `event_kind` decodes
    /// as a conversational message, NULL `hmac` is a legacy-unchained row.
    /// Add the per-session lease columns (worker-pool / fallible executors).
    /// Idempotent: guarded by `PRAGMA table_info` so re-running on an
    /// already-migrated DB is a no-op.
    pub(super) async fn migrate_v9(&self) -> Result<()> {
        let mut rows = self.conn.query("PRAGMA table_info(sessions)", ()).await?;
        let mut existing = std::collections::HashSet::new();
        while let Some(row) = rows.next().await? {
            existing.insert(row.get::<String>(1)?);
        }
        for (col, ddl) in [
            (
                "lease_owner",
                "ALTER TABLE sessions ADD COLUMN lease_owner TEXT",
            ),
            (
                "lease_expiry",
                "ALTER TABLE sessions ADD COLUMN lease_expiry INTEGER",
            ),
            (
                "lease_epoch",
                "ALTER TABLE sessions ADD COLUMN lease_epoch INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            if !existing.contains(col) {
                self.conn.execute_batch(ddl).await?;
            }
        }
        Ok(())
    }

    /// Cache of stdio MCP servers' advertised tool catalogs, so a restart can
    /// present tools while the server process is still starting. See
    /// `db::mcp_catalog_cache` for why the key is a keyed MAC.
    pub(super) async fn migrate_v10(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS mcp_catalog_cache (
                    config_hash TEXT PRIMARY KEY,
                    server_name TEXT NOT NULL,
                    tools_json  TEXT NOT NULL,
                    server_info TEXT,
                    cached_at   INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_mcp_catalog_cache_server
                    ON mcp_catalog_cache(server_name);",
            )
            .await?;
        Ok(())
    }

    pub(super) async fn migrate_v8(&self) -> Result<()> {
        let mut rows = self.conn.query("PRAGMA table_info(messages)", ()).await?;
        let mut existing = std::collections::HashSet::new();
        while let Some(row) = rows.next().await? {
            existing.insert(row.get::<String>(1)?);
        }
        for (col, ddl) in [
            (
                "event_kind",
                "ALTER TABLE messages ADD COLUMN event_kind TEXT",
            ),
            (
                "event_json",
                "ALTER TABLE messages ADD COLUMN event_json TEXT",
            ),
            ("hmac", "ALTER TABLE messages ADD COLUMN hmac TEXT"),
            (
                "prev_hmac",
                "ALTER TABLE messages ADD COLUMN prev_hmac TEXT",
            ),
        ] {
            if !existing.contains(col) {
                self.conn.execute_batch(ddl).await?;
            }
        }
        Ok(())
    }

    /// Shape repair for stranded DBs. All worktrees share `~/.borg/borg.db`,
    /// so a binary whose migration numbering differed can stamp
    /// `schema_version` past a column-adding migration without ever running
    /// it — e.g. a DB at version 10 whose `messages` table still lacks the
    /// v8 `hmac`/`prev_hmac`/`event_*` columns, making every message persist
    /// fail with "no such column: hmac". Re-run the idempotent ALTER-based
    /// migrations; each probes `PRAGMA table_info` first, so on a healthy DB
    /// this is a no-op.
    pub(super) async fn migrate_v11(&self) -> Result<()> {
        self.migrate_v5().await?;
        self.migrate_v6().await?;
        self.migrate_v8().await?;
        self.migrate_v9().await?;
        Ok(())
    }

    pub(super) async fn migrate_v4(&self) -> Result<()> {
        // Fresh installs already emit the final shape via `SCHEMA_SQL`
        // (which defines `code_hash`). Only DBs that ran through the v3
        // schema (plaintext `code` column) still need the rename — probe
        // pragma_table_info to make the rename idempotent.
        let mut rows = self
            .conn
            .query("PRAGMA table_info(pairing_requests)", ())
            .await?;
        let mut has_plaintext_column = false;
        while let Some(row) = rows.next().await? {
            let col: String = row.get(1)?;
            if col == "code" {
                has_plaintext_column = true;
                break;
            }
        }
        if has_plaintext_column {
            self.conn
                .execute_batch(
                    "ALTER TABLE pairing_requests RENAME COLUMN code TO code_hash;
                     UPDATE pairing_requests
                        SET status = 'expired'
                        WHERE status = 'pending';",
                )
                .await?;
        }
        Ok(())
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
    lease_epoch INTEGER NOT NULL DEFAULT 0
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
    content_parts_json TEXT,
    -- Session event log (the agent IS this log). `event_kind`/`event_json`
    -- carry fine-grained lifecycle events (tool-call-started, compaction,
    -- fork markers); conversational rows leave them NULL and decode from
    -- role/content. `hmac`/`prev_hmac` form a per-session tamper-evident
    -- chain; NULL on rows written before chaining (legacy-tolerant on replay).
    event_kind TEXT,
    event_json TEXT,
    hmac TEXT,
    prev_hmac TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);

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

    // Worktrees share ~/.borg/borg.db, so a binary with different migration
    // numbering can stamp schema_version past a column-adding migration
    // without running it. Observed in the wild: a version-10 DB whose
    // messages table lacked the v8 hmac/prev_hmac/event_* columns, failing
    // every message persist with "no such column: hmac". migrate_v11 must
    // repair the shape and advance the version.
    #[tokio::test]
    async fn stranded_schema_version_gets_messages_columns_repaired() {
        let db = Database::test_db().await;

        // Simulate the stranded state: rebuild `messages` with the exact
        // pre-v8 column set observed in the wild, version stamped past 8.
        db.conn
            .execute_batch(
                "DROP TABLE messages;
                 CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT,
                    tool_calls_json TEXT,
                    tool_call_id TEXT,
                    timestamp TEXT,
                    created_at INTEGER NOT NULL,
                    content_parts_json TEXT
                 );",
            )
            .await
            .unwrap();
        db.set_meta("schema_version", "10").await.unwrap();

        db.run_migrations().await.unwrap();

        let mut rows = db.conn.query("PRAGMA table_info(messages)", ()).await.unwrap();
        let mut cols: Vec<String> = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            cols.push(row.get::<String>(1).unwrap());
        }
        for required in ["event_kind", "event_json", "hmac", "prev_hmac"] {
            assert!(
                cols.iter().any(|c| c == required),
                "messages must regain {required}; columns: {cols:?}"
            );
        }
        assert_eq!(
            db.schema_version().await.unwrap(),
            Database::CURRENT_VERSION,
            "repair must advance the version so it doesn't re-run every boot"
        );

        // The originally-failing operation now succeeds end-to-end.
        db.upsert_session("s1", 100, 100, 0, "m", "test").await.unwrap();
        db.insert_message("s1", "user", Some("hello"), None, None, None, None)
            .await
            .unwrap();
    }
}
