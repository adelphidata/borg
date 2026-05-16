use anyhow::Result;
use turso::params;

use super::models::ChainHealth;
use super::Database;

/// Per-call batch size for pending-celebration drains. The daemon scheduler
/// re-invokes on each tick, so a backlog clears across multiple ticks rather
/// than pulling thousands of rows into a single response.
const PENDING_CELEBRATIONS_BATCH: i64 = 256;

impl Database {
    /// Save an HMAC chain checkpoint for recovery.
    pub async fn save_hmac_checkpoint(
        &self,
        domain: &str,
        event_id: i64,
        prev_hmac: &str,
        state_hash: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO hmac_checkpoints (domain, event_id, prev_hmac, state_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params!(domain, event_id, prev_hmac, state_hash, now),
            )
            .await?;
        Ok(())
    }

    /// Load the most recent checkpoint for a domain.
    pub async fn load_latest_hmac_checkpoint(
        &self,
        domain: &str,
    ) -> Result<Option<crate::hmac_chain::ChainCheckpoint>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, domain, event_id, prev_hmac, state_hash, created_at
             FROM hmac_checkpoints WHERE domain = ?1 ORDER BY event_id DESC LIMIT 1",
                params!(domain),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(crate::hmac_chain::ChainCheckpoint {
                id: row.get::<i64>(0)?,
                domain: row.get::<String>(1)?,
                event_id: row.get::<i64>(2)?,
                prev_hmac: row.get::<String>(3)?,
                state_hash: row.get::<String>(4)?,
                created_at: row.get::<i64>(5)?,
            })),
            None => Ok(None),
        }
    }

    // ── Chain Integrity Verification ──

    /// Verify HMAC chain integrity across all event-sourced systems.
    /// Returns health status without blocking. Call on startup to detect tampering.
    pub async fn verify_event_chains(&self) -> ChainHealth {
        let vitals_key = self.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN);
        let vitals_events = self.load_all_vitals_events().await.unwrap_or_default();
        let vitals_state = crate::vitals::replay_events_with_key(&vitals_key, &vitals_events);
        let vitals_valid = vitals_state.chain_valid;
        let vitals_count = vitals_events.len() as u32;

        let bond_key = self.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
        let bond_events = self.get_all_bond_events().await.unwrap_or_default();
        let bond_state = crate::bond::replay_events_with_key(&bond_key, &bond_events);
        let bond_valid = bond_state.chain_valid;
        let bond_count = bond_events.len() as u32;

        let evo_key = self.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN);
        let evolution_events = self.load_all_evolution_events().await.unwrap_or_default();
        let evolution_pubkeys = self.load_device_pubkeys().await.unwrap_or_default();
        let evolution_state = crate::evolution::replay_events_with_keys_at(
            &evo_key,
            &evolution_pubkeys,
            &evolution_events,
            chrono::Utc::now().timestamp(),
        );
        let evolution_valid = evolution_state.chain_valid;
        let evolution_count = evolution_events.len() as u32;

        if !vitals_valid {
            tracing::warn!("integrity: vitals HMAC chain is broken ({vitals_count} events)");
        }
        if !bond_valid {
            tracing::warn!("integrity: bond HMAC chain is broken ({bond_count} events)");
        }
        if !evolution_valid {
            tracing::warn!("integrity: evolution HMAC chain is broken ({evolution_count} events)");
        }

        ChainHealth {
            vitals_valid,
            vitals_count,
            bond_valid,
            bond_count,
            evolution_valid,
            evolution_count,
        }
    }

    // ── Vitals CRUD (event-sourced) ──

    /// Compute vitals state by replaying all verified events from baseline.
    /// Events with broken HMAC chains are skipped. Rate limiting caps impact
    /// per category per hour to prevent gaming.
    pub async fn get_vitals_state(&self) -> Result<crate::vitals::VitalsState> {
        let events = self.load_all_vitals_events().await?;
        let key = self.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN);
        Ok(crate::vitals::replay_events_with_key(&key, &events))
    }

    /// Load all vitals events ordered chronologically (for replay).
    async fn load_all_vitals_events(&self) -> Result<Vec<crate::vitals::VitalsEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, category, source, stability_delta, focus_delta, sync_delta,
                    growth_delta, happiness_delta, metadata_json, created_at, hmac, prev_hmac
             FROM vitals_events ORDER BY id ASC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::vitals::VitalsEvent {
                id: row.get::<i64>(0)?,
                category: row.get::<String>(1)?,
                source: row.get::<String>(2)?,
                stability_delta: row.get::<i32>(3)?,
                focus_delta: row.get::<i32>(4)?,
                sync_delta: row.get::<i32>(5)?,
                growth_delta: row.get::<i32>(6)?,
                happiness_delta: row.get::<i32>(7)?,
                metadata_json: row.get::<Option<String>>(8)?,
                created_at: row.get::<i64>(9)?,
                hmac: row.get::<String>(10)?,
                prev_hmac: row.get::<String>(11)?,
            });
        }
        Ok(out)
    }

    /// Atomically read prev_hmac, compute HMAC, and insert a vitals event.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from reading the same prev_hmac.
    pub async fn record_vitals_event(
        &self,
        category: &str,
        source: &str,
        deltas: &crate::vitals::StatDeltas,
        metadata: Option<&str>,
    ) -> Result<()> {
        // Validate category and deltas to prevent gaming via inflated stat changes
        crate::vitals::validate_deltas(category, *deltas)
            .map_err(|e| anyhow::anyhow!("vitals delta validation failed: {e}"))?;

        self.conn.execute_batch("BEGIN IMMEDIATE").await?;

        let result: Result<()> = async {
            let now = chrono::Utc::now().timestamp();
            let hour_start = now - (now % 3600);

            // Record-time rate limiting: reject if this category already hit its cap this hour
            let count: i64 = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM vitals_events WHERE category = ?1 AND created_at >= ?2",
                        params!(category, hour_start),
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                }
            };
            if count >= crate::vitals::rate_limit_for(category) as i64 {
                return Ok(()); // silently drop — at capacity
            }

            // Get the HMAC of the last event for chaining
            let prev_hmac: String = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT hmac FROM vitals_events ORDER BY id DESC LIMIT 1",
                        (),
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) => row.get::<String>(0).unwrap_or_else(|_| "0".to_string()),
                    None => "0".to_string(),
                }
            };

            let hmac = crate::vitals::compute_event_hmac(
                &self.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN),
                &prev_hmac,
                category,
                source,
                *deltas,
                now,
            );

            self.conn.execute(
                "INSERT INTO vitals_events (category, source, stability_delta, focus_delta,
                    sync_delta, growth_delta, happiness_delta, metadata_json, created_at,
                    hmac, prev_hmac)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params!(
                    category,
                    source,
                    deltas.stability as i32,
                    deltas.focus as i32,
                    deltas.sync as i32,
                    deltas.growth as i32,
                    deltas.happiness as i32,
                    metadata,
                    now,
                    hmac,
                    prev_hmac,
                ),
            ).await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(())
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!(
                        "vitals: rollback after record_vitals_event failure failed: {re}"
                    );
                }
                Err(e)
            }
        }
    }

    /// Get vitals events since a given timestamp (for display, not replay).
    pub async fn vitals_events_since(&self, since: i64) -> Result<Vec<crate::vitals::VitalsEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, category, source, stability_delta, focus_delta, sync_delta,
                    growth_delta, happiness_delta, metadata_json, created_at, hmac, prev_hmac
             FROM vitals_events WHERE created_at >= ?1 ORDER BY created_at DESC",
                params!(since),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::vitals::VitalsEvent {
                id: row.get::<i64>(0)?,
                category: row.get::<String>(1)?,
                source: row.get::<String>(2)?,
                stability_delta: row.get::<i32>(3)?,
                focus_delta: row.get::<i32>(4)?,
                sync_delta: row.get::<i32>(5)?,
                growth_delta: row.get::<i32>(6)?,
                happiness_delta: row.get::<i32>(7)?,
                metadata_json: row.get::<Option<String>>(8)?,
                created_at: row.get::<i64>(9)?,
                hmac: row.get::<String>(10)?,
                prev_hmac: row.get::<String>(11)?,
            });
        }
        Ok(out)
    }

    // ── Bond CRUD (event-sourced) ──

    /// Load all bond events ordered chronologically (for replay).
    pub async fn get_all_bond_events(&self) -> Result<Vec<crate::bond::BondEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, event_type, score_delta, reason, hmac, prev_hmac, created_at
             FROM bond_events ORDER BY id ASC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::bond::BondEvent {
                id: row.get::<i64>(0)?,
                event_type: row.get::<String>(1)?,
                score_delta: row.get::<i32>(2)?,
                reason: row.get::<String>(3)?,
                hmac: row.get::<String>(4)?,
                prev_hmac: row.get::<String>(5)?,
                created_at: row.get::<i64>(6)?,
            });
        }
        Ok(out)
    }

    /// Get the HMAC of the most recent bond event (for chaining).
    pub async fn get_last_bond_event_hmac(&self) -> Result<String> {
        let mut rows = self
            .conn
            .query("SELECT hmac FROM bond_events ORDER BY id DESC LIMIT 1", ())
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<String>(0).unwrap_or_else(|_| "0".to_string())),
            None => Ok("0".to_string()),
        }
    }

    /// Append a bond event with pre-computed HMAC (for testing only).
    #[cfg(test)]
    pub async fn record_bond_event(
        &self,
        event_type: &str,
        delta: i32,
        reason: &str,
        hmac: &str,
        prev_hmac: &str,
        created_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO bond_events (event_type, score_delta, reason, hmac, prev_hmac, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params!(event_type, delta, reason, hmac, prev_hmac, created_at),
        ).await?;
        Ok(())
    }

    /// Atomically read prev_hmac, compute HMAC, and insert a bond event.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from reading the same prev_hmac.
    pub async fn record_bond_event_chained(
        &self,
        event_type: &str,
        delta: i32,
        reason: &str,
    ) -> Result<()> {
        // Validate event_type and delta to prevent gaming via custom types or inflated deltas
        let expected_delta = match event_type {
            "tool_success" => 1,
            "tool_failure" => -1,
            "creation" => 1,
            "correction" => -2,
            "suggestion_accepted" => 1,
            "suggestion_rejected" => -1,
            _ => return Err(anyhow::anyhow!("invalid bond event_type: {event_type}")),
        };
        if delta != expected_delta {
            return Err(anyhow::anyhow!(
                "invalid delta {delta} for bond event_type {event_type}, expected {expected_delta}"
            ));
        }

        self.conn.execute_batch("BEGIN IMMEDIATE").await?;

        let result: Result<()> = async {
            let now = chrono::Utc::now().timestamp();
            let hour_start = now - (now % 3600);

            // Total events per hour cap (15)
            let total_this_hour: i64 = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM bond_events WHERE created_at >= ?1",
                        params!(hour_start),
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                }
            };
            if total_this_hour >= 15 {
                return Ok(()); // silently drop — at capacity
            }

            // Positive-delta events per hour cap (8)
            if delta > 0 {
                let pos_this_hour: i64 = {
                    let mut rows = self.conn.query(
                        "SELECT COUNT(*) FROM bond_events WHERE score_delta > 0 AND created_at >= ?1",
                        params!(hour_start),
                    ).await?;
                    match rows.next().await? {
                        Some(row) => row.get::<i64>(0)?,
                        None => 0,
                    }
                };
                if pos_this_hour >= 8 {
                    return Ok(()); // silently drop — at capacity
                }
            }

            // Per-type rate limiting
            let count: i64 = {
                let mut rows = self.conn.query(
                    "SELECT COUNT(*) FROM bond_events WHERE event_type = ?1 AND created_at >= ?2",
                    params!(event_type, hour_start),
                ).await?;
                match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                }
            };
            if count >= crate::bond::rate_limit_for(event_type) as i64 {
                return Ok(()); // silently drop — at capacity
            }

            let prev_hmac: String = {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT hmac FROM bond_events ORDER BY id DESC LIMIT 1",
                        (),
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) => row.get::<String>(0).unwrap_or_else(|_| "0".to_string()),
                    None => "0".to_string(),
                }
            };

            let hmac = crate::bond::compute_event_hmac(
                &self.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN),
                &prev_hmac,
                event_type,
                delta,
                reason,
                now,
            );

            self.conn.execute(
                "INSERT INTO bond_events (event_type, score_delta, reason, hmac, prev_hmac, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!(event_type, delta, reason, hmac, prev_hmac, now),
            ).await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(())
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("vitals: rollback after record_bond_event failure failed: {re}");
                }
                Err(e)
            }
        }
    }

    /// Get bond events since a given timestamp (for display, DESC order).
    pub async fn bond_events_since(&self, since: i64) -> Result<Vec<crate::bond::BondEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, event_type, score_delta, reason, hmac, prev_hmac, created_at
             FROM bond_events WHERE created_at >= ?1 ORDER BY created_at DESC",
                params!(since),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::bond::BondEvent {
                id: row.get::<i64>(0)?,
                event_type: row.get::<String>(1)?,
                score_delta: row.get::<i32>(2)?,
                reason: row.get::<String>(3)?,
                hmac: row.get::<String>(4)?,
                prev_hmac: row.get::<String>(5)?,
                created_at: row.get::<i64>(6)?,
            });
        }
        Ok(out)
    }

    /// Get last N bond events (for history display, DESC order).
    pub async fn bond_events_recent(&self, limit: usize) -> Result<Vec<crate::bond::BondEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, event_type, score_delta, reason, hmac, prev_hmac, created_at
             FROM bond_events ORDER BY created_at DESC LIMIT ?1",
                params!(limit as i64),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::bond::BondEvent {
                id: row.get::<i64>(0)?,
                event_type: row.get::<String>(1)?,
                score_delta: row.get::<i32>(2)?,
                reason: row.get::<String>(3)?,
                hmac: row.get::<String>(4)?,
                prev_hmac: row.get::<String>(5)?,
                created_at: row.get::<i64>(6)?,
            });
        }
        Ok(out)
    }

    /// Count bond events of a given type since a timestamp (for rate limiting).
    /// Pass empty string for event_type to count all events.
    pub async fn count_bond_events_since(&self, since: i64, event_type: &str) -> Result<u32> {
        let (sql, do_filter) = if event_type.is_empty() {
            (
                "SELECT COUNT(*) FROM bond_events WHERE created_at >= ?1",
                false,
            )
        } else {
            (
                "SELECT COUNT(*) FROM bond_events WHERE created_at >= ?1 AND event_type = ?2",
                true,
            )
        };
        let mut rows = if do_filter {
            self.conn.query(sql, params!(since, event_type)).await?
        } else {
            self.conn.query(sql, params!(since)).await?
        };
        let count: i64 = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(count.max(0) as u32)
    }

    /// Count task_runs since a timestamp, optionally filtering by status.
    /// Returns (matching_count, total_count).
    pub async fn count_task_runs_since(
        &self,
        since: i64,
        status: Option<&str>,
    ) -> Result<(u32, u32)> {
        let total: i64 = {
            let mut rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM task_runs WHERE started_at >= ?1",
                    params!(since),
                )
                .await?;
            match rows.next().await? {
                Some(row) => row.get::<i64>(0)?,
                None => 0,
            }
        };

        let matching: i64 = if let Some(s) = status {
            let mut rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM task_runs WHERE started_at >= ?1 AND status = ?2",
                    params!(since, s),
                )
                .await?;
            match rows.next().await? {
                Some(row) => row.get::<i64>(0)?,
                None => 0,
            }
        } else {
            total
        };

        Ok((matching.max(0) as u32, total.max(0) as u32))
    }

    /// Count vitals events by category since a timestamp.
    /// Returns (category_count, total_count).
    pub async fn count_vitals_events_by_category_since(
        &self,
        since: i64,
        category: &str,
    ) -> Result<(u32, u32)> {
        let total: i64 = {
            let mut rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM vitals_events WHERE created_at >= ?1",
                    params!(since),
                )
                .await?;
            match rows.next().await? {
                Some(row) => row.get::<i64>(0)?,
                None => 0,
            }
        };

        let matching: i64 = {
            let mut rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM vitals_events WHERE created_at >= ?1 AND category = ?2",
                    params!(since, category),
                )
                .await?;
            match rows.next().await? {
                Some(row) => row.get::<i64>(0)?,
                None => 0,
            }
        };

        Ok((matching.max(0) as u32, total.max(0) as u32))
    }

    // ── Evolution CRUD (event-sourced) ──

    /// Compute evolution state by replaying all verified events from baseline.
    pub async fn get_evolution_state(&self) -> Result<crate::evolution::EvolutionState> {
        let events = self.load_all_evolution_events().await?;
        let key = self.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN);
        let pubkeys = self.load_device_pubkeys().await?;
        Ok(crate::evolution::replay_events_with_keys_at(
            &key,
            &pubkeys,
            &events,
            chrono::Utc::now().timestamp(),
        ))
    }

    /// Load all `device_keys` rows into a `pubkey_id → VerifyingKey` map.
    /// Rows with corrupt key bytes are logged and skipped — replay falls back
    /// to "missing device key" for events that referenced them.
    pub(crate) async fn load_device_pubkeys(
        &self,
    ) -> Result<std::collections::HashMap<i64, ed25519_dalek::VerifyingKey>> {
        let mut rows = self
            .conn
            .query("SELECT id, public_key FROM device_keys", ())
            .await?;
        let mut out = std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get::<i64>(0)?;
            let blob: Vec<u8> = row.get::<Vec<u8>>(1)?;
            match crate::evolution::signature::verifying_key_from_bytes(&blob) {
                Ok(pk) => {
                    out.insert(id, pk);
                }
                Err(e) => {
                    tracing::warn!("evolution: device_keys row {id} has invalid public_key: {e}");
                }
            }
        }
        Ok(out)
    }

    /// Load all evolution events ordered chronologically for replay.
    pub(crate) async fn load_all_evolution_events(
        &self,
    ) -> Result<Vec<crate::evolution::EvolutionEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, event_type, xp_delta, archetype, source, metadata_json,
                    created_at, hmac, prev_hmac, session_id, pubkey_id
             FROM evolution_events ORDER BY id ASC",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::evolution::EvolutionEvent {
                id: row.get::<i64>(0)?,
                event_type: row.get::<String>(1)?,
                xp_delta: row.get::<i32>(2)?,
                archetype: row.get::<Option<String>>(3)?,
                source: row.get::<String>(4)?,
                metadata_json: row.get::<Option<String>>(5)?,
                created_at: row.get::<i64>(6)?,
                hmac: row.get::<String>(7)?,
                prev_hmac: row.get::<String>(8)?,
                session_id: row.get::<Option<String>>(9)?,
                pubkey_id: row.get::<Option<i64>>(10)?,
            });
        }
        Ok(out)
    }

    /// Atomically read prev_hmac, compute HMAC, and insert an evolution event.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from reading the same prev_hmac.
    pub async fn record_evolution_event(
        &self,
        event_type: &str,
        xp_delta: i32,
        archetype: Option<&str>,
        source: &str,
        metadata: Option<&str>,
    ) -> Result<()> {
        self.record_evolution_event_with_session(
            event_type, xp_delta, archetype, source, metadata, None,
        )
        .await
    }

    /// Same as `record_evolution_event` but attaches an explicit `session_id`.
    /// Used by signing-aware writers (plan_emission, agent hooks) so the
    /// session linkage survives in a queryable column rather than only in
    /// `metadata_json`.
    pub async fn record_evolution_event_with_session(
        &self,
        event_type: &str,
        xp_delta: i32,
        archetype: Option<&str>,
        source: &str,
        metadata: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()> {
        // Validate event_type and xp_delta to prevent gaming via inflated XP
        if !crate::evolution::VALID_EVOLUTION_EVENT_TYPES.contains(&event_type) {
            return Err(anyhow::anyhow!(
                "invalid evolution event_type: {event_type}"
            ));
        }
        let max_delta = match event_type {
            "xp_gain" => crate::evolution::MAX_XP_DELTA,
            // milestone_bonus has its own (higher) per-event cap; see
            // MAX_MILESTONE_BONUS_XP doc on why this exists.
            "milestone_bonus" => crate::evolution::MAX_MILESTONE_BONUS_XP,
            _ => 0,
        };
        if xp_delta < 0 || xp_delta > max_delta {
            return Err(anyhow::anyhow!(
                "invalid xp_delta {xp_delta} for evolution event_type {event_type}, expected 0..={max_delta}"
            ));
        }

        // Semantic validation: `evolution` rows drive stage transitions. Replay
        // rejects rows without `gates_verified=true` (see replay.rs), so fail
        // loudly at the write path instead of silently dropping later.
        if event_type == "evolution" {
            let meta_str = metadata.ok_or_else(|| {
                anyhow::anyhow!("evolution event requires metadata.gates_verified = true")
            })?;
            let parsed: serde_json::Value = serde_json::from_str(meta_str)
                .map_err(|e| anyhow::anyhow!("evolution event metadata is not valid JSON: {e}"))?;
            if parsed
                .get("gates_verified")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                return Err(anyhow::anyhow!(
                    "evolution event requires metadata.gates_verified = true"
                ));
            }
        }

        self.conn.execute_batch("BEGIN IMMEDIATE").await?;

        let result: Result<()> = async {
            let now = chrono::Utc::now().timestamp();
            let hour_start = now - (now % 3600);

            // Record-time rate limiting: reject if this event_type already hit its cap this hour
            let count: i64 = {
                let mut rows = self.conn.query(
                    "SELECT COUNT(*) FROM evolution_events WHERE event_type = ?1 AND created_at >= ?2",
                    params!(event_type, hour_start),
                ).await?;
                match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                }
            };
            if count >= crate::evolution::rate_limit_for(event_type) as i64 {
                return Ok(()); // silently drop — at capacity
            }

            // Total events per hour cap
            let total_this_hour: i64 = {
                let mut rows = self.conn.query(
                    "SELECT COUNT(*) FROM evolution_events WHERE created_at >= ?1",
                    params!(hour_start),
                ).await?;
                match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                }
            };
            if total_this_hour >= crate::evolution::TOTAL_EVENTS_PER_HOUR {
                return Ok(()); // silently drop — at capacity
            }

            // Per-source rate limiting at write time
            let source_this_hour: i64 = {
                let mut rows = self.conn.query(
                    "SELECT COUNT(*) FROM evolution_events WHERE source = ?1 AND created_at >= ?2",
                    params!(source, hour_start),
                ).await?;
                match rows.next().await? {
                    Some(row) => row.get::<i64>(0)?,
                    None => 0,
                }
            };
            if source_this_hour >= crate::evolution::WRITE_SOURCE_RATE_LIMIT {
                return Ok(()); // silently drop — at capacity
            }

            let prev_hmac: String = {
                let mut rows = self.conn.query(
                    "SELECT hmac FROM evolution_events ORDER BY id DESC LIMIT 1",
                    (),
                ).await?;
                match rows.next().await? {
                    Some(row) => row.get::<String>(0).unwrap_or_else(|_| "0".to_string()),
                    None => "0".to_string(),
                }
            };

            // Sign new events with Ed25519. Legacy rows (pubkey_id NULL)
            // continue to verify against HMAC; the chain bridges naturally
            // because prev_hmac is just bytes the next signer hashes over.
            let (signing_key, pubkey_id) = self.evolution_signing_key().await?;
            let signature = crate::evolution::signature::sign_event(
                &signing_key,
                &prev_hmac,
                event_type,
                xp_delta,
                archetype.unwrap_or(""),
                source,
                metadata.unwrap_or(""),
                now,
            );

            self.conn.execute(
                "INSERT INTO evolution_events (event_type, xp_delta, archetype, source,
                    metadata_json, created_at, hmac, prev_hmac, session_id, pubkey_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params!(
                    event_type, xp_delta, archetype, source, metadata, now, signature, prev_hmac,
                    session_id, pubkey_id,
                ),
            ).await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(())
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!(
                        "vitals: rollback after record_evolution_event failure failed: {re}"
                    );
                }
                Err(e)
            }
        }
    }

    /// Get evolution events since a given timestamp (for display).
    pub async fn evolution_events_since(
        &self,
        since: i64,
    ) -> Result<Vec<crate::evolution::EvolutionEvent>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, event_type, xp_delta, archetype, source, metadata_json,
                    created_at, hmac, prev_hmac, session_id, pubkey_id
             FROM evolution_events WHERE created_at >= ?1 ORDER BY created_at DESC",
                params!(since),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(crate::evolution::EvolutionEvent {
                id: row.get::<i64>(0)?,
                event_type: row.get::<String>(1)?,
                xp_delta: row.get::<i32>(2)?,
                archetype: row.get::<Option<String>>(3)?,
                source: row.get::<String>(4)?,
                metadata_json: row.get::<Option<String>>(5)?,
                created_at: row.get::<i64>(6)?,
                hmac: row.get::<String>(7)?,
                prev_hmac: row.get::<String>(8)?,
                session_id: row.get::<Option<String>>(9)?,
                pubkey_id: row.get::<Option<i64>>(10)?,
            });
        }
        Ok(out)
    }

    /// Get the timestamp of the first evolution event (for usage duration).
    pub async fn first_evolution_event_timestamp(&self) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query("SELECT MIN(created_at) FROM evolution_events", ())
            .await?;
        match rows.next().await? {
            Some(row) => Ok(row.get::<Option<i64>>(0)?),
            None => Ok(None),
        }
    }

    // ── Pending Celebrations ──

    /// Insert a pending celebration for async delivery.
    pub async fn insert_pending_celebration(
        &self,
        celebration_type: &str,
        payload_json: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO pending_celebrations (celebration_type, payload_json, created_at) VALUES (?1, ?2, ?3)",
            params!(celebration_type, payload_json, now),
        ).await?;
        Ok(())
    }

    /// Get all undelivered celebrations, capped at [`PENDING_CELEBRATIONS_BATCH`]
    /// rows per call so a backed-up queue doesn't pull an unbounded set into
    /// memory. The daemon drain loop calls again on the next tick to pick up
    /// any remainder.
    pub async fn get_pending_celebrations(&self) -> Result<Vec<PendingCelebration>> {
        let mut rows = self.conn.query(
            "SELECT id, celebration_type, payload_json, created_at FROM pending_celebrations WHERE delivered_at IS NULL ORDER BY created_at ASC LIMIT ?1",
            params!(PENDING_CELEBRATIONS_BATCH),
        ).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(PendingCelebration {
                id: row.get::<i64>(0)?,
                celebration_type: row.get::<String>(1)?,
                payload_json: row.get::<String>(2)?,
                created_at: row.get::<i64>(3)?,
            });
        }
        Ok(out)
    }

    /// Atomically claim undelivered celebrations by setting `delivered_at` to a
    /// sentinel value (-1) and returning the claimed rows. This prevents
    /// concurrent daemon ticks from delivering the same celebration twice.
    /// Call `mark_celebration_delivered` on success or `unclaim_celebration` on
    /// failure. Same per-call cap as [`Self::get_pending_celebrations`].
    pub async fn claim_pending_celebrations(&self) -> Result<Vec<PendingCelebration>> {
        let sentinel: i64 = -1;
        let mut rows_iter = self.conn.query(
            "SELECT id, celebration_type, payload_json, created_at FROM pending_celebrations WHERE delivered_at IS NULL ORDER BY created_at ASC LIMIT ?1",
            params!(PENDING_CELEBRATIONS_BATCH),
        ).await?;
        let mut rows = Vec::new();
        while let Some(row) = rows_iter.next().await? {
            rows.push(PendingCelebration {
                id: row.get::<i64>(0)?,
                celebration_type: row.get::<String>(1)?,
                payload_json: row.get::<String>(2)?,
                created_at: row.get::<i64>(3)?,
            });
        }
        drop(rows_iter);

        // Mark all claimed rows with sentinel so next tick skips them
        for c in &rows {
            self.conn.execute(
                "UPDATE pending_celebrations SET delivered_at = ?1 WHERE id = ?2 AND delivered_at IS NULL",
                params!(sentinel, c.id),
            ).await?;
        }
        Ok(rows)
    }

    /// Mark a celebration as successfully delivered.
    pub async fn mark_celebration_delivered(&self, id: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE pending_celebrations SET delivered_at = ?1 WHERE id = ?2",
                params!(now, id),
            )
            .await?;
        Ok(())
    }

    /// Release a claimed celebration back to pending state (on delivery failure).
    pub async fn unclaim_celebration(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_celebrations SET delivered_at = NULL WHERE id = ?1 AND delivered_at = -1",
            params!(id),
        ).await?;
        Ok(())
    }
}

/// A pending celebration awaiting async delivery to channels.
#[derive(Debug, Clone)]
pub struct PendingCelebration {
    /// Row ID.
    pub id: i64,
    /// Type of celebration (e.g. "evolution").
    pub celebration_type: String,
    /// Serialized celebration payload.
    pub payload_json: String,
    /// Unix timestamp when recorded.
    pub created_at: i64,
}
