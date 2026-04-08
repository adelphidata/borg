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
        let next_run = crate::tasks::calculate_next_run("cron", SECURITY_AUDIT_CRON)?;
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
        let next_run_daily = crate::tasks::calculate_next_run("cron", DAILY_SUMMARY_CRON)?;
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
}
