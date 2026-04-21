use anyhow::Result;
use rusqlite::params;
use rusqlite::OptionalExtension;

use super::Database;

impl Database {
    /// V1: Original schema — sessions, scheduled_tasks, task_runs, meta, token_usage
    pub(super) fn migrate_v1(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                model TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS scheduled_tasks (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                prompt TEXT NOT NULL,
                schedule_type TEXT NOT NULL,
                schedule_expr TEXT NOT NULL,
                timezone TEXT NOT NULL DEFAULT 'local',
                status TEXT NOT NULL DEFAULT 'active',
                next_run INTEGER,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS task_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES scheduled_tasks(id),
                started_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                result TEXT,
                error TEXT
            );

            CREATE TABLE IF NOT EXISTS token_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_token_usage_ts ON token_usage(timestamp);
            ",
        )?;
        Ok(())
    }

    /// V2: Add messages table for message persistence + retry_count for tasks
    pub(super) fn migrate_v2(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_calls_json TEXT,
                tool_call_id TEXT,
                timestamp TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
            ",
        )?;
        if !Self::has_column(&self.conn, "scheduled_tasks", "retry_count") {
            self.conn.execute_batch(
                "ALTER TABLE scheduled_tasks ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        Ok(())
    }

    /// V3: Add channel_sessions and channel_messages tables for gateway
    pub(super) fn migrate_v3(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    /// V4: Add customizations, installed_tools, installed_channels, customization_credentials
    pub(super) fn migrate_v4(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    /// V5: Add file_hashes table for integrity verification
    pub(super) fn migrate_v5(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS file_hashes (
                customization_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(customization_id) REFERENCES customizations(id) ON DELETE CASCADE,
                UNIQUE(customization_id, file_path)
            );
            ",
        )?;
        Ok(())
    }

    /// V6: Add delivery_queue table for persistent outbound delivery
    pub(super) fn migrate_v6(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    /// V7: Add agent_roles and sub_agent_runs tables for multi-agent system
    pub(super) fn migrate_v7(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            CREATE INDEX IF NOT EXISTS idx_sub_agent_runs_parent
                ON sub_agent_runs(parent_session_id);
            ",
        )?;
        // Seed built-in roles as frozen SQL (not calling live code, so migration is immutable).
        let now = chrono::Utc::now().timestamp();
        let builtin_roles: &[(&str, &str, f64, &str)] = &[
            (
                "researcher",
                "Information gathering and analysis. Use this role for tasks that require searching, reading, and synthesizing information.",
                0.3,
                r#"["run_shell","web_fetch","web_search","read_memory","write_memory"]"#,
            ),
            (
                "coder",
                "Code writing and modification. Use this role for tasks that require creating or modifying code files.",
                0.2,
                r#"["run_shell","apply_patch","read_memory"]"#,
            ),
            (
                "writer",
                "Documentation and content writing. Use this role for tasks that require writing documentation, notes, or creative content.",
                0.7,
                r#"["run_shell","apply_patch","read_memory","write_memory","web_search"]"#,
            ),
        ];
        for (name, desc, temp, tools_json) in builtin_roles {
            self.conn.execute(
                "INSERT OR IGNORE INTO agent_roles (name, description, temperature, tools_allowed, is_builtin, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                params![name, desc, temp, tools_json, now],
            )?;
        }
        Ok(())
    }

    /// V8: Add content_parts_json column to messages for multimodal content
    pub(super) fn migrate_v8(&self) -> Result<()> {
        if !Self::has_column(&self.conn, "messages", "content_parts_json") {
            self.conn
                .execute_batch("ALTER TABLE messages ADD COLUMN content_parts_json TEXT;")?;
        }
        Ok(())
    }

    /// V9: Add settings table for runtime configuration overrides
    pub(super) fn migrate_v9(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            ",
        )?;
        Ok(())
    }

    /// V10: Add memory_embeddings table for semantic memory search
    pub(super) fn migrate_v10(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    pub(super) fn migrate_v11(&self) -> Result<()> {
        // Add provider, model, cost_usd columns to token_usage.
        let new_columns = [
            ("token_usage", "provider", "TEXT"),
            ("token_usage", "model", "TEXT"),
            ("token_usage", "cost_usd", "REAL"),
        ];
        for (table, col, col_type) in &new_columns {
            if !Self::has_column(&self.conn, table, col) {
                self.conn.execute_batch(&format!(
                    "ALTER TABLE \"{table}\" ADD COLUMN \"{col}\" {col_type}"
                ))?;
            }
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_token_usage_model ON token_usage(model);",
        )?;
        Ok(())
    }

    /// V12: Add memory_chunks table and FTS index for chunked semantic search.
    pub(super) fn migrate_v12(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    pub(super) fn migrate_v13(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    /// V14: Add retry, timeout, and delivery columns to scheduled_tasks
    pub(super) fn migrate_v14(&self) -> Result<()> {
        // retry_count already exists from V2, add remaining columns
        let new_columns = [
            (
                "scheduled_tasks",
                "max_retries",
                "INTEGER NOT NULL DEFAULT 3",
            ),
            ("scheduled_tasks", "retry_after", "INTEGER"),
            ("scheduled_tasks", "last_error", "TEXT"),
            (
                "scheduled_tasks",
                "timeout_ms",
                "INTEGER NOT NULL DEFAULT 300000",
            ),
            ("scheduled_tasks", "delivery_channel", "TEXT"),
            ("scheduled_tasks", "delivery_target", "TEXT"),
        ];
        for (table, col, col_type) in &new_columns {
            if !Self::has_column(&self.conn, table, col) {
                self.conn.execute_batch(&format!(
                    "ALTER TABLE \"{table}\" ADD COLUMN \"{col}\" {col_type}"
                ))?;
            }
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_tasks_retry ON scheduled_tasks(status, retry_after);",
        )?;
        Ok(())
    }

    /// V15: Seed default scheduled tasks (monthly security audit)
    pub(super) fn migrate_v15(&self) -> Result<()> {
        self.seed_default_tasks()?;
        Ok(())
    }

    /// Create built-in default tasks. Uses INSERT OR IGNORE with a fixed ID to be idempotent.
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

        const DAILY_SUMMARY_CRON: &str = "0 0 9 * * 1-5"; // 9 AM Mon-Fri (6-field: sec min hr dom mon dow)
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

    /// V16: Add embedding_cache table for caching API embedding results.
    pub(super) fn migrate_v16(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS embedding_cache (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                provider     TEXT NOT NULL,
                model        TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding    BLOB NOT NULL,
                dimension    INTEGER NOT NULL,
                created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(provider, model, content_hash)
            );
            CREATE INDEX IF NOT EXISTS idx_embedding_cache_lookup
                ON embedding_cache(provider, model, content_hash);
            ",
        )?;
        Ok(())
    }

    /// V17: Add session_index_status table for tracking which sessions have been indexed.
    pub(super) fn migrate_v17(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_index_status (
                session_id    TEXT PRIMARY KEY,
                indexed_at    INTEGER NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0
            );
            ",
        )?;
        Ok(())
    }

    /// V18: Add status column to task_runs and daemon_lock table.
    pub(super) fn migrate_v18(&self) -> Result<()> {
        // Check if status column already exists (idempotent migration)
        let has_status: bool = self
            .conn
            .prepare("SELECT status FROM task_runs LIMIT 0")
            .is_ok();
        if !has_status {
            self.conn.execute_batch(
                "ALTER TABLE task_runs ADD COLUMN status TEXT NOT NULL DEFAULT 'success';",
            )?;
            self.conn
                .execute_batch("UPDATE task_runs SET status = 'failed' WHERE error IS NOT NULL;")?;
        }
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS daemon_lock (
                id           INTEGER PRIMARY KEY CHECK (id = 1),
                pid          INTEGER NOT NULL,
                started_at   INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL
            );
            ",
        )?;
        Ok(())
    }

    /// V19: Add scripts table for agent-created scripts with HMAC integrity verification.
    pub(super) fn migrate_v19(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    /// V20: Add allowed_tools column to scheduled_tasks for tool allowlists.
    pub(super) fn migrate_v20(&self) -> Result<()> {
        // Check if column already exists (idempotent migration)
        let has_column = {
            let mut stmt = self.conn.prepare("PRAGMA table_info(scheduled_tasks)")?;
            let rows: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .collect();
            rows.iter().any(|name| name == "allowed_tools")
        };
        if !has_column {
            self.conn
                .execute_batch("ALTER TABLE scheduled_tasks ADD COLUMN allowed_tools TEXT;")?;
        }
        Ok(())
    }

    /// V21: Add missing indexes for task_runs, scheduled_tasks, and scripts query patterns.
    pub(super) fn migrate_v21(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_task_runs_task
                ON task_runs(task_id, started_at DESC);
            CREATE INDEX IF NOT EXISTS idx_tasks_due
                ON scheduled_tasks(status, next_run);
            CREATE INDEX IF NOT EXISTS idx_task_runs_status
                ON task_runs(status);
            CREATE INDEX IF NOT EXISTS idx_scripts_ephemeral
                ON scripts(ephemeral, created_at);
            ",
        )?;
        Ok(())
    }

    /// V22: Vitals system — event-sourced agent health tracking.
    /// State is derived by replaying verified events from baseline.
    /// No mutable state table — the event ledger is the single source of truth.
    pub(super) fn migrate_v22(&self) -> Result<()> {
        // Repair: if table exists from a prior dev build without hmac column, drop it
        if !Self::has_column(&self.conn, "vitals_events", "hmac") {
            self.conn
                .execute_batch("DROP TABLE IF EXISTS vitals_events;")?;
        }
        self.conn.execute_batch(
            "
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
            CREATE INDEX IF NOT EXISTS idx_vitals_events_created
                ON vitals_events(created_at);
            ",
        )?;
        Ok(())
    }

    /// V23: Bond system — event-sourced trust tracking with HMAC chain.
    pub(super) fn migrate_v23(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS bond_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                score_delta INTEGER NOT NULL,
                reason TEXT NOT NULL DEFAULT '',
                hmac TEXT NOT NULL,
                prev_hmac TEXT NOT NULL DEFAULT '0',
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_bond_events_created
                ON bond_events(created_at);
            CREATE INDEX IF NOT EXISTS idx_bond_events_type
                ON bond_events(event_type);
            ",
        )?;
        Ok(())
    }

    /// V24: Evolution system — event-sourced specialization tracking with HMAC chain.
    pub(super) fn migrate_v24(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS evolution_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                xp_delta INTEGER NOT NULL DEFAULT 0,
                archetype TEXT,
                source TEXT NOT NULL,
                metadata_json TEXT,
                created_at INTEGER NOT NULL,
                hmac TEXT NOT NULL,
                prev_hmac TEXT NOT NULL DEFAULT '0'
            );
            CREATE INDEX IF NOT EXISTS idx_evolution_events_created
                ON evolution_events(created_at);
            CREATE INDEX IF NOT EXISTS idx_evolution_events_archetype
                ON evolution_events(archetype);
            ",
        )?;
        Ok(())
    }

    pub(super) fn migrate_v25(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            -- Append-only triggers: prevent UPDATE/DELETE on event ledgers
            CREATE TRIGGER IF NOT EXISTS vitals_events_no_update
                BEFORE UPDATE ON vitals_events
                BEGIN SELECT RAISE(ABORT, 'vitals_events is append-only'); END;
            CREATE TRIGGER IF NOT EXISTS vitals_events_no_delete
                BEFORE DELETE ON vitals_events
                BEGIN SELECT RAISE(ABORT, 'vitals_events is append-only'); END;

            CREATE TRIGGER IF NOT EXISTS bond_events_no_update
                BEFORE UPDATE ON bond_events
                BEGIN SELECT RAISE(ABORT, 'bond_events is append-only'); END;
            CREATE TRIGGER IF NOT EXISTS bond_events_no_delete
                BEFORE DELETE ON bond_events
                BEGIN SELECT RAISE(ABORT, 'bond_events is append-only'); END;

            CREATE TRIGGER IF NOT EXISTS evolution_events_no_update
                BEFORE UPDATE ON evolution_events
                BEGIN SELECT RAISE(ABORT, 'evolution_events is append-only'); END;
            CREATE TRIGGER IF NOT EXISTS evolution_events_no_delete
                BEFORE DELETE ON evolution_events
                BEGIN SELECT RAISE(ABORT, 'evolution_events is append-only'); END;
            ",
        )?;
        Ok(())
    }

    pub(super) fn migrate_v26(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;
        Ok(())
    }

    /// V27: Seed daily summary default task
    pub(super) fn migrate_v27(&self) -> Result<()> {
        self.seed_default_tasks()?;
        Ok(())
    }

    /// V28: Add task_type column for cron job support
    pub(super) fn migrate_v28(&self) -> Result<()> {
        if !Self::has_column(&self.conn, "scheduled_tasks", "task_type") {
            self.conn.execute_batch(
                "ALTER TABLE scheduled_tasks ADD COLUMN task_type TEXT NOT NULL DEFAULT 'prompt';",
            )?;
        }
        Ok(())
    }

    /// V29: Structured activity log table
    pub(super) fn migrate_v29(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS activity_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                level TEXT NOT NULL,
                category TEXT NOT NULL,
                message TEXT NOT NULL,
                detail TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_activity_log_level ON activity_log(level, created_at);
            CREATE INDEX IF NOT EXISTS idx_activity_log_category ON activity_log(category, created_at);
            CREATE INDEX IF NOT EXISTS idx_activity_log_created ON activity_log(created_at);",
        )?;
        Ok(())
    }

    /// V30: Track prompt cache effectiveness by persisting cached-read and
    /// cache-creation token counts alongside the primary usage row.
    pub(super) fn migrate_v30(&self) -> Result<()> {
        for (table, col) in [
            ("token_usage", "cached_input_tokens"),
            ("token_usage", "cache_creation_tokens"),
        ] {
            if !Self::has_column(&self.conn, table, col) {
                self.conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0;"
                ))?;
            }
        }
        Ok(())
    }

    /// V31: Workflow engine — durable multi-step task orchestration + projects
    pub(super) fn migrate_v31(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'active',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_projects_status ON projects(status);

            CREATE TABLE IF NOT EXISTS workflows (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                goal TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                current_step INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                completed_at INTEGER,
                error TEXT,
                session_id TEXT REFERENCES sessions(id),
                project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
                delivery_channel TEXT,
                delivery_target TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_workflows_status ON workflows(status);
            CREATE INDEX IF NOT EXISTS idx_workflows_session ON workflows(session_id);
            CREATE INDEX IF NOT EXISTS idx_workflows_project ON workflows(project_id);

            CREATE TABLE IF NOT EXISTS workflow_steps (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
                step_index INTEGER NOT NULL,
                title TEXT NOT NULL,
                instructions TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                output TEXT,
                error TEXT,
                started_at INTEGER,
                completed_at INTEGER,
                max_retries INTEGER NOT NULL DEFAULT 3,
                retry_count INTEGER NOT NULL DEFAULT 0,
                timeout_ms INTEGER NOT NULL DEFAULT 300000,
                UNIQUE(workflow_id, step_index)
            );
            CREATE INDEX IF NOT EXISTS idx_workflow_steps_workflow
                ON workflow_steps(workflow_id, step_index);
            ",
        )?;
        Ok(())
    }

    /// V32: Import config.toml into settings table, then rename to .bak.
    /// This is the migration that moves config from file to DB-only.
    pub(super) fn migrate_v32(&self) -> Result<()> {
        use crate::config::Config;
        use crate::settings::SETTING_REGISTRY;

        let config_path = match Config::data_dir() {
            Ok(dir) => dir.join("config.toml"),
            Err(_) => return Ok(()),
        };

        if !config_path.exists() {
            return Ok(());
        }

        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("V32 migration: cannot read config.toml: {e}");
                return Ok(());
            }
        };

        // Deduplicate TOML table headers before parsing
        let content = Config::dedup_toml_tables(&content);
        let file_config: Config = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("V32 migration: cannot parse config.toml: {e}");
                return Ok(());
            }
        };

        let default_config = Config::default();
        let now = chrono::Utc::now().timestamp();

        for &(key, extractor) in SETTING_REGISTRY {
            let file_val = extractor(&file_config);
            let default_val = extractor(&default_config);
            if file_val != default_val && !file_val.is_empty() {
                // Only import if not already set in DB
                let existing: Option<String> = self
                    .conn
                    .prepare("SELECT value FROM settings WHERE key = ?1")
                    .and_then(|mut stmt| stmt.query_row(params![key], |row| row.get(0)).optional())
                    .unwrap_or(None);
                if existing.is_none() {
                    if let Err(e) = self.conn.execute(
                        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
                        params![key, file_val, now],
                    ) {
                        tracing::warn!("V32 migration: failed to import {key}: {e}");
                    }
                }
            }
        }

        // Rename config.toml to config.toml.bak
        let bak_path = config_path.with_extension("toml.bak");
        if let Err(e) = std::fs::rename(&config_path, &bak_path) {
            tracing::warn!("V32 migration: could not rename config.toml to .bak: {e}");
        }

        Ok(())
    }

    /// V33: Pending celebrations outbox for async delivery of evolution messages.
    pub(super) fn migrate_v33(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_celebrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                celebration_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                delivered_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_pending_celebrations_undelivered
                ON pending_celebrations (delivered_at) WHERE delivered_at IS NULL;",
        )?;
        Ok(())
    }

    /// V34: DB-only memory — `memory_entries` table replaces filesystem markdown files.
    /// Also adds `last_accessed_at` to embedding_cache for TTL pruning and seeds
    /// nightly/weekly memory consolidation scheduled tasks.
    pub(super) fn migrate_v34(&self) -> Result<()> {
        // 1. Create memory_entries table
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope TEXT NOT NULL DEFAULT 'global',
                name TEXT NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(scope, name)
            );
            CREATE INDEX IF NOT EXISTS idx_memory_entries_scope
                ON memory_entries(scope);
            CREATE INDEX IF NOT EXISTS idx_memory_entries_updated
                ON memory_entries(updated_at);",
        )?;

        // 2. Add last_accessed_at to embedding_cache for TTL pruning
        if !Self::has_column(&self.conn, "embedding_cache", "last_accessed_at") {
            self.conn.execute_batch(
                "ALTER TABLE embedding_cache ADD COLUMN last_accessed_at INTEGER NOT NULL DEFAULT (unixepoch());",
            )?;
        }

        // 3. Migrate existing markdown memory files into DB
        self.migrate_memory_files_to_db();

        // 4. Seed consolidation scheduled tasks
        self.seed_consolidation_tasks()?;

        Ok(())
    }

    /// Best-effort migration of existing `~/.borg/MEMORY.md` and `~/.borg/memory/*.md`
    /// files into the `memory_entries` table. Queues `.bak` renames to be executed
    /// after the migration transaction commits — if the DB rolls back, the source
    /// files are left untouched for a clean retry on the next startup.
    fn migrate_memory_files_to_db(&self) {
        use sha2::{Digest, Sha256};

        let data_dir = match crate::config::Config::data_dir() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("V34 migration: cannot resolve data dir: {e}");
                return;
            }
        };
        let now = chrono::Utc::now().timestamp();

        // Insert content into memory_entries AND queue the source file for a
        // post-commit rename to `.bak`. Scans content for injection patterns
        // first so migrated files are held to the same policy as new writes.
        let import = |scope: &str, name: &str, content: &str, path: &std::path::Path| {
            if let Err(e) = crate::memory::scan_for_injection(content) {
                tracing::warn!(
                    "V34 migration: skipping {} ({name}) — injection scan rejected: {e}",
                    path.display()
                );
                return;
            }
            let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
            match self.conn.execute(
                "INSERT OR IGNORE INTO memory_entries (scope, name, content, content_hash, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![scope, name, content, hash, now],
            ) {
                Ok(_) => {
                    let bak = path.with_extension("md.bak");
                    self.queue_post_migration_rename(path.to_path_buf(), bak);
                }
                Err(e) => tracing::warn!("V34 migration: failed to import {name}: {e}"),
            }
        };

        // Import MEMORY.md → name="INDEX"
        let memory_md = data_dir.join("MEMORY.md");
        if memory_md.exists() {
            match std::fs::read_to_string(&memory_md) {
                Ok(content) if !content.trim().is_empty() => {
                    import("global", "INDEX", &content, &memory_md);
                }
                Ok(_) => tracing::debug!("V34 migration: MEMORY.md empty, skipping"),
                Err(e) => {
                    tracing::warn!("V34 migration: failed to read {}: {e}", memory_md.display())
                }
            }
        }

        // Import memory/*.md → name=stem (read_dir is non-recursive, skips subdirectories)
        let mem_dir = data_dir.join("memory");
        if mem_dir.is_dir() {
            match std::fs::read_dir(&mem_dir) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = match entry {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(
                                    "V34 migration: read_dir entry error in {}: {e}",
                                    mem_dir.display()
                                );
                                continue;
                            }
                        };
                        let path = entry.path();
                        if path.is_file() && path.extension().map(|e| e == "md").unwrap_or(false) {
                            let name = path
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            match std::fs::read_to_string(&path) {
                                Ok(content) if !content.trim().is_empty() => {
                                    import("global", &name, &content, &path);
                                }
                                Ok(_) => tracing::debug!(
                                    "V34 migration: {} empty, skipping",
                                    path.display()
                                ),
                                Err(e) => tracing::warn!(
                                    "V34 migration: failed to read {}: {e}",
                                    path.display()
                                ),
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    "V34 migration: failed to read_dir {}: {e}",
                    mem_dir.display()
                ),
            }

            // Import memory/daily/*.md → name="daily/YYYY-MM-DD"
            let daily_dir = mem_dir.join("daily");
            if daily_dir.is_dir() {
                match std::fs::read_dir(&daily_dir) {
                    Ok(entries) => {
                        for entry in entries {
                            let entry = match entry {
                                Ok(e) => e,
                                Err(e) => {
                                    tracing::warn!(
                                        "V34 migration: read_dir entry error in {}: {e}",
                                        daily_dir.display()
                                    );
                                    continue;
                                }
                            };
                            let path = entry.path();
                            if path.is_file()
                                && path.extension().map(|e| e == "md").unwrap_or(false)
                            {
                                let stem = path
                                    .file_stem()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string();
                                let name = format!("daily/{stem}");
                                match std::fs::read_to_string(&path) {
                                    Ok(content) if !content.trim().is_empty() => {
                                        import("global", &name, &content, &path);
                                    }
                                    Ok(_) => tracing::debug!(
                                        "V34 migration: {} empty, skipping",
                                        path.display()
                                    ),
                                    Err(e) => tracing::warn!(
                                        "V34 migration: failed to read {}: {e}",
                                        path.display()
                                    ),
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        "V34 migration: failed to read_dir {}: {e}",
                        daily_dir.display()
                    ),
                }
            }

            // Import memory/scopes/<scope>/*.md → scope=<scope>, name=stem
            let scopes_dir = mem_dir.join("scopes");
            if scopes_dir.is_dir() {
                match std::fs::read_dir(&scopes_dir) {
                    Ok(scope_entries) => {
                        for scope_entry in scope_entries.flatten() {
                            let scope_path = scope_entry.path();
                            if !scope_path.is_dir() {
                                continue;
                            }
                            let scope_name = scope_path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            if scope_name.is_empty() {
                                continue;
                            }
                            match std::fs::read_dir(&scope_path) {
                                Ok(files) => {
                                    for file in files.flatten() {
                                        let path = file.path();
                                        if path.is_file()
                                            && path.extension().map(|e| e == "md").unwrap_or(false)
                                        {
                                            let name = path
                                                .file_stem()
                                                .unwrap_or_default()
                                                .to_string_lossy()
                                                .to_string();
                                            match std::fs::read_to_string(&path) {
                                                Ok(content) if !content.trim().is_empty() => {
                                                    import(&scope_name, &name, &content, &path);
                                                }
                                                Ok(_) => tracing::debug!(
                                                    "V34 migration: {} empty, skipping",
                                                    path.display()
                                                ),
                                                Err(e) => tracing::warn!(
                                                    "V34 migration: failed to read {}: {e}",
                                                    path.display()
                                                ),
                                            }
                                        }
                                    }
                                }
                                Err(e) => tracing::warn!(
                                    "V34 migration: failed to read_dir {}: {e}",
                                    scope_path.display()
                                ),
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        "V34 migration: failed to read_dir {}: {e}",
                        scopes_dir.display()
                    ),
                }
            }
        }
    }

    /// Seed nightly and weekly memory consolidation scheduled tasks.
    fn seed_consolidation_tasks(&self) -> Result<()> {
        // Single source of truth for these UUIDs lives in crate::consolidation.
        let nightly_task_id = crate::consolidation::NIGHTLY_CONSOLIDATION_TASK_ID;
        let weekly_task_id = crate::consolidation::WEEKLY_CONSOLIDATION_TASK_ID;

        let now = chrono::Utc::now().timestamp();

        // Nightly consolidation: 3 AM daily
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

        // Weekly consolidation: 4 AM Sunday
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

    /// V35: FTS5 over `messages` so the agent can search raw session transcripts
    /// with `memory_search sources=["sessions"]`.
    ///
    /// Mirrors the V12 pattern for `memory_chunks_fts`: content-sourced virtual
    /// table + INSERT/UPDATE/DELETE triggers. Backfills existing rows so pre-V35
    /// sessions become searchable immediately.
    pub(super) fn migrate_v35(&self) -> Result<()> {
        self.conn.execute_batch(
            "
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
            ",
        )?;

        // Backfill any rows that existed before V35 — triggers only fire on
        // new DML. `rebuild` re-derives the full FTS index from the content
        // table (external-content FTS5), which is idempotent and cheap on a
        // fresh index. NULL/empty content rows harmlessly index as empty.
        self.conn.execute(
            "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
            [],
        )?;

        Ok(())
    }

    /// V36: Cache the BPE token estimate per memory entry so each turn's
    /// memory-load doesn't re-encode every entry from scratch. NULL until
    /// first-use lazy populate by the memory loader.
    pub(super) fn migrate_v36(&self) -> Result<()> {
        if !Self::has_column(&self.conn, "memory_entries", "estimated_tokens") {
            self.conn
                .execute_batch("ALTER TABLE memory_entries ADD COLUMN estimated_tokens INTEGER;")?;
        }
        Ok(())
    }

    /// V37: Self-healing maintenance.
    ///
    /// - Creates `doctor_runs` to audit the daily maintenance sweep.
    /// - Seeds a `task_type = 'maintenance'` scheduled task firing at 02:00
    ///   daily (one hour before the nightly memory consolidation at 03:00).
    pub(super) fn migrate_v37(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS doctor_runs (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ran_at      INTEGER NOT NULL,
                pass_count  INTEGER NOT NULL DEFAULT 0,
                warn_count  INTEGER NOT NULL DEFAULT 0,
                fail_count  INTEGER NOT NULL DEFAULT 0,
                report_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_doctor_runs_ran_at
                ON doctor_runs(ran_at DESC);",
        )?;
        self.seed_maintenance_task()?;
        Ok(())
    }

    /// V38: Purge stale entries from the `settings` table that are no longer
    /// recognized by the compiled-in `SETTING_REGISTRY`. Before this, renamed
    /// or removed settings would sit in the DB and, on every config-watcher
    /// poll (every 3 s), trigger an "Ignoring invalid setting …" warn that
    /// ballooned `~/.borg/logs/*.log` by tens of MB over weeks.
    ///
    /// Dynamic keys (`skills.entries.*.enabled`) are preserved because they
    /// are pattern-matched at apply time, not registered.
    pub(super) fn migrate_v38(&self) -> Result<()> {
        let known: std::collections::HashSet<&'static str> = crate::settings::SETTING_REGISTRY
            .iter()
            .map(|(k, _)| *k)
            .collect();

        let mut stmt = self.conn.prepare("SELECT key FROM settings")?;
        let all_keys: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut removed = 0usize;
        for key in all_keys {
            if known.contains(key.as_str()) {
                continue;
            }
            // Preserve dynamic skill entry keys
            if key.starts_with("skills.entries.") && key.ends_with(".enabled") {
                continue;
            }
            self.conn.execute(
                "DELETE FROM settings WHERE key = ?1",
                rusqlite::params![key],
            )?;
            removed += 1;
            tracing::info!("V38 migration: removed stale setting '{key}'");
        }
        if removed > 0 {
            tracing::info!("V38 migration: purged {removed} stale setting row(s)");
        }
        Ok(())
    }

    /// Seed the daily self-healing maintenance task. Idempotent via
    /// `INSERT OR IGNORE` so re-running on an existing install leaves the
    /// row untouched.
    fn seed_maintenance_task(&self) -> Result<()> {
        let task_id = crate::maintenance::MAINTENANCE_TASK_ID;
        const MAINTENANCE_CRON: &str = "0 0 2 * * *";
        let now = chrono::Utc::now().timestamp();
        let next_run = crate::tasks::calculate_next_run("cron", MAINTENANCE_CRON, "local")?;
        self.conn.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
             (id, name, prompt, schedule_type, schedule_expr, timezone, status, next_run, created_at, max_retries, timeout_ms, allowed_tools, task_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, 1, 300000, NULL, 'maintenance')",
            rusqlite::params![
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
}
