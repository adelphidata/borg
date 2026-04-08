use anyhow::Result;
use rusqlite::params;

use super::models::ChainHealth;
use super::Database;

impl Database {
    /// Save an HMAC chain checkpoint for recovery.
    pub fn save_hmac_checkpoint(
        &self,
        domain: &str,
        event_id: i64,
        prev_hmac: &str,
        state_hash: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO hmac_checkpoints (domain, event_id, prev_hmac, state_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![domain, event_id, prev_hmac, state_hash, now],
        )?;
        Ok(())
    }

    /// Load the most recent checkpoint for a domain.
    pub fn load_latest_hmac_checkpoint(
        &self,
        domain: &str,
    ) -> Result<Option<crate::hmac_chain::ChainCheckpoint>> {
        let result = self.conn.query_row(
            "SELECT id, domain, event_id, prev_hmac, state_hash, created_at
             FROM hmac_checkpoints WHERE domain = ?1 ORDER BY event_id DESC LIMIT 1",
            rusqlite::params![domain],
            |row| {
                Ok(crate::hmac_chain::ChainCheckpoint {
                    id: row.get(0)?,
                    domain: row.get(1)?,
                    event_id: row.get(2)?,
                    prev_hmac: row.get(3)?,
                    state_hash: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        );
        match result {
            Ok(cp) => Ok(Some(cp)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ── Chain Integrity Verification ──

    /// Verify HMAC chain integrity across all event-sourced systems.
    /// Returns health status without blocking. Call on startup to detect tampering.
    pub fn verify_event_chains(&self) -> ChainHealth {
        let vitals_key = self.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN);
        let vitals_events = self.load_all_vitals_events().unwrap_or_default();
        let vitals_state = crate::vitals::replay_events_with_key(&vitals_key, &vitals_events);
        let vitals_valid = vitals_state.chain_valid;
        let vitals_count = vitals_events.len() as u32;

        let bond_key = self.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
        let bond_events = self.get_all_bond_events().unwrap_or_default();
        let bond_state = crate::bond::replay_events_with_key(&bond_key, &bond_events);
        let bond_valid = bond_state.chain_valid;
        let bond_count = bond_events.len() as u32;

        let evo_key = self.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN);
        let evolution_events = self.load_all_evolution_events().unwrap_or_default();
        let evolution_state = crate::evolution::replay_events_with_key(&evo_key, &evolution_events);
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
    pub fn get_vitals_state(&self) -> Result<crate::vitals::VitalsState> {
        let events = self.load_all_vitals_events()?;
        let key = self.derive_hmac_key(crate::vitals::VITALS_HMAC_DOMAIN);
        Ok(crate::vitals::replay_events_with_key(&key, &events))
    }

    /// Load all vitals events ordered chronologically (for replay).
    fn load_all_vitals_events(&self) -> Result<Vec<crate::vitals::VitalsEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, category, source, stability_delta, focus_delta, sync_delta,
                    growth_delta, happiness_delta, metadata_json, created_at, hmac, prev_hmac
             FROM vitals_events ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([], |row| {
                Ok(crate::vitals::VitalsEvent {
                    id: row.get(0)?,
                    category: row.get(1)?,
                    source: row.get(2)?,
                    stability_delta: row.get(3)?,
                    focus_delta: row.get(4)?,
                    sync_delta: row.get(5)?,
                    growth_delta: row.get(6)?,
                    happiness_delta: row.get(7)?,
                    metadata_json: row.get(8)?,
                    created_at: row.get(9)?,
                    hmac: row.get(10)?,
                    prev_hmac: row.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
    }

    /// Atomically read prev_hmac, compute HMAC, and insert a vitals event.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from reading the same prev_hmac.
    pub fn record_vitals_event(
        &self,
        category: &str,
        source: &str,
        deltas: &crate::vitals::StatDeltas,
        metadata: Option<&str>,
    ) -> Result<()> {
        // Validate category and deltas to prevent gaming via inflated stat changes
        crate::vitals::validate_deltas(category, *deltas)
            .map_err(|e| anyhow::anyhow!("vitals delta validation failed: {e}"))?;

        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = (|| -> Result<()> {
            let now = chrono::Utc::now().timestamp();
            let hour_start = now - (now % 3600);

            // Record-time rate limiting: reject if this category already hit its cap this hour
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM vitals_events WHERE category = ?1 AND created_at >= ?2",
                params![category, hour_start],
                |row| row.get(0),
            )?;
            if count >= crate::vitals::rate_limit_for(category) as i64 {
                return Ok(()); // silently drop — at capacity
            }

            // Get the HMAC of the last event for chaining
            let prev_hmac: String = self
                .conn
                .query_row(
                    "SELECT hmac FROM vitals_events ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| "0".to_string());

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
                params![
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
                ],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Get vitals events since a given timestamp (for display, not replay).
    pub fn vitals_events_since(&self, since: i64) -> Result<Vec<crate::vitals::VitalsEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, category, source, stability_delta, focus_delta, sync_delta,
                    growth_delta, happiness_delta, metadata_json, created_at, hmac, prev_hmac
             FROM vitals_events WHERE created_at >= ?1 ORDER BY created_at DESC",
        )?;

        let events = stmt
            .query_map(params![since], |row| {
                Ok(crate::vitals::VitalsEvent {
                    id: row.get(0)?,
                    category: row.get(1)?,
                    source: row.get(2)?,
                    stability_delta: row.get(3)?,
                    focus_delta: row.get(4)?,
                    sync_delta: row.get(5)?,
                    growth_delta: row.get(6)?,
                    happiness_delta: row.get(7)?,
                    metadata_json: row.get(8)?,
                    created_at: row.get(9)?,
                    hmac: row.get(10)?,
                    prev_hmac: row.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
    }

    // ── Bond CRUD (event-sourced) ──

    /// Load all bond events ordered chronologically (for replay).
    pub fn get_all_bond_events(&self) -> Result<Vec<crate::bond::BondEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, score_delta, reason, hmac, prev_hmac, created_at
             FROM bond_events ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([], |row| {
                Ok(crate::bond::BondEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    score_delta: row.get(2)?,
                    reason: row.get(3)?,
                    hmac: row.get(4)?,
                    prev_hmac: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
    }

    /// Get the HMAC of the most recent bond event (for chaining).
    pub fn get_last_bond_event_hmac(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT hmac FROM bond_events ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "0".to_string()))
    }

    /// Append a bond event with pre-computed HMAC (for testing only).
    #[cfg(test)]
    pub fn record_bond_event(
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
            params![event_type, delta, reason, hmac, prev_hmac, created_at],
        )?;
        Ok(())
    }

    /// Atomically read prev_hmac, compute HMAC, and insert a bond event.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from reading the same prev_hmac.
    pub fn record_bond_event_chained(
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

        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = (|| -> Result<()> {
            let now = chrono::Utc::now().timestamp();
            let hour_start = now - (now % 3600);

            // Total events per hour cap (15)
            let total_this_hour: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM bond_events WHERE created_at >= ?1",
                params![hour_start],
                |row| row.get(0),
            )?;
            if total_this_hour >= 15 {
                return Ok(()); // silently drop — at capacity
            }

            // Positive-delta events per hour cap (8)
            if delta > 0 {
                let pos_this_hour: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM bond_events WHERE score_delta > 0 AND created_at >= ?1",
                    params![hour_start],
                    |row| row.get(0),
                )?;
                if pos_this_hour >= 8 {
                    return Ok(()); // silently drop — at capacity
                }
            }

            // Per-type rate limiting
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM bond_events WHERE event_type = ?1 AND created_at >= ?2",
                params![event_type, hour_start],
                |row| row.get(0),
            )?;
            if count >= crate::bond::rate_limit_for(event_type) as i64 {
                return Ok(()); // silently drop — at capacity
            }

            let prev_hmac: String = self
                .conn
                .query_row(
                    "SELECT hmac FROM bond_events ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| "0".to_string());

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
                params![event_type, delta, reason, hmac, prev_hmac, now],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Get bond events since a given timestamp (for display, DESC order).
    pub fn bond_events_since(&self, since: i64) -> Result<Vec<crate::bond::BondEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, score_delta, reason, hmac, prev_hmac, created_at
             FROM bond_events WHERE created_at >= ?1 ORDER BY created_at DESC",
        )?;

        let events = stmt
            .query_map(params![since], |row| {
                Ok(crate::bond::BondEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    score_delta: row.get(2)?,
                    reason: row.get(3)?,
                    hmac: row.get(4)?,
                    prev_hmac: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
    }

    /// Get last N bond events (for history display, DESC order).
    pub fn bond_events_recent(&self, limit: usize) -> Result<Vec<crate::bond::BondEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, score_delta, reason, hmac, prev_hmac, created_at
             FROM bond_events ORDER BY created_at DESC LIMIT ?1",
        )?;

        let events = stmt
            .query_map(params![limit as i64], |row| {
                Ok(crate::bond::BondEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    score_delta: row.get(2)?,
                    reason: row.get(3)?,
                    hmac: row.get(4)?,
                    prev_hmac: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(events)
    }

    /// Count bond events of a given type since a timestamp (for rate limiting).
    /// Pass empty string for event_type to count all events.
    pub fn count_bond_events_since(&self, since: i64, event_type: &str) -> Result<u32> {
        if event_type.is_empty() {
            let count: u32 = self.conn.query_row(
                "SELECT COUNT(*) FROM bond_events WHERE created_at >= ?1",
                params![since],
                |row| row.get(0),
            )?;
            Ok(count)
        } else {
            let count: u32 = self.conn.query_row(
                "SELECT COUNT(*) FROM bond_events WHERE created_at >= ?1 AND event_type = ?2",
                params![since, event_type],
                |row| row.get(0),
            )?;
            Ok(count)
        }
    }

    /// Count task_runs since a timestamp, optionally filtering by status.
    /// Returns (matching_count, total_count).
    pub fn count_task_runs_since(&self, since: i64, status: Option<&str>) -> Result<(u32, u32)> {
        let total: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM task_runs WHERE started_at >= ?1",
            params![since],
            |row| row.get(0),
        )?;

        let matching = if let Some(s) = status {
            self.conn.query_row(
                "SELECT COUNT(*) FROM task_runs WHERE started_at >= ?1 AND status = ?2",
                params![since, s],
                |row| row.get(0),
            )?
        } else {
            total
        };

        Ok((matching, total))
    }

    /// Count vitals events by category since a timestamp.
    /// Returns (category_count, total_count).
    pub fn count_vitals_events_by_category_since(
        &self,
        since: i64,
        category: &str,
    ) -> Result<(u32, u32)> {
        let total: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM vitals_events WHERE created_at >= ?1",
            params![since],
            |row| row.get(0),
        )?;

        let matching: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM vitals_events WHERE created_at >= ?1 AND category = ?2",
            params![since, category],
            |row| row.get(0),
        )?;

        Ok((matching, total))
    }

    // ── Evolution CRUD (event-sourced) ──

    /// Compute evolution state by replaying all verified events from baseline.
    pub fn get_evolution_state(&self) -> Result<crate::evolution::EvolutionState> {
        let events = self.load_all_evolution_events()?;
        let key = self.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN);
        Ok(crate::evolution::replay_events_with_key(&key, &events))
    }

    /// Load all evolution events ordered chronologically for replay.
    pub(crate) fn load_all_evolution_events(
        &self,
    ) -> Result<Vec<crate::evolution::EvolutionEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, xp_delta, archetype, source, metadata_json,
                    created_at, hmac, prev_hmac
             FROM evolution_events ORDER BY id ASC",
        )?;
        let events = stmt
            .query_map([], |row| {
                Ok(crate::evolution::EvolutionEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    xp_delta: row.get(2)?,
                    archetype: row.get(3)?,
                    source: row.get(4)?,
                    metadata_json: row.get(5)?,
                    created_at: row.get(6)?,
                    hmac: row.get(7)?,
                    prev_hmac: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    /// Atomically read prev_hmac, compute HMAC, and insert an evolution event.
    /// Uses BEGIN IMMEDIATE to prevent concurrent writers from reading the same prev_hmac.
    pub fn record_evolution_event(
        &self,
        event_type: &str,
        xp_delta: i32,
        archetype: Option<&str>,
        source: &str,
        metadata: Option<&str>,
    ) -> Result<()> {
        // Validate event_type and xp_delta to prevent gaming via inflated XP
        if !crate::evolution::VALID_EVOLUTION_EVENT_TYPES.contains(&event_type) {
            return Err(anyhow::anyhow!(
                "invalid evolution event_type: {event_type}"
            ));
        }
        let max_delta = if event_type == "xp_gain" {
            crate::evolution::MAX_XP_DELTA
        } else {
            0
        };
        if xp_delta < 0 || xp_delta > max_delta {
            return Err(anyhow::anyhow!(
                "invalid xp_delta {xp_delta} for evolution event_type {event_type}, expected 0..={max_delta}"
            ));
        }

        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = (|| -> Result<()> {
            let now = chrono::Utc::now().timestamp();
            let hour_start = now - (now % 3600);

            // Record-time rate limiting: reject if this event_type already hit its cap this hour
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM evolution_events WHERE event_type = ?1 AND created_at >= ?2",
                params![event_type, hour_start],
                |row| row.get(0),
            )?;
            if count >= crate::evolution::rate_limit_for(event_type) as i64 {
                return Ok(()); // silently drop — at capacity
            }

            // Total events per hour cap
            let total_this_hour: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM evolution_events WHERE created_at >= ?1",
                params![hour_start],
                |row| row.get(0),
            )?;
            if total_this_hour >= crate::evolution::TOTAL_EVENTS_PER_HOUR {
                return Ok(()); // silently drop — at capacity
            }

            // Per-source rate limiting at write time
            let source_this_hour: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM evolution_events WHERE source = ?1 AND created_at >= ?2",
                params![source, hour_start],
                |row| row.get(0),
            )?;
            if source_this_hour >= crate::evolution::WRITE_SOURCE_RATE_LIMIT {
                return Ok(()); // silently drop — at capacity
            }

            let prev_hmac: String = self
                .conn
                .query_row(
                    "SELECT hmac FROM evolution_events ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| "0".to_string());

            let hmac = crate::evolution::compute_event_hmac(
                &self.derive_hmac_key(crate::evolution::EVOLUTION_HMAC_DOMAIN),
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
                    metadata_json, created_at, hmac, prev_hmac)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![event_type, xp_delta, archetype, source, metadata, now, hmac, prev_hmac],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Get evolution events since a given timestamp (for display).
    pub fn evolution_events_since(
        &self,
        since: i64,
    ) -> Result<Vec<crate::evolution::EvolutionEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, xp_delta, archetype, source, metadata_json,
                    created_at, hmac, prev_hmac
             FROM evolution_events WHERE created_at >= ?1 ORDER BY created_at DESC",
        )?;
        let events = stmt
            .query_map(params![since], |row| {
                Ok(crate::evolution::EvolutionEvent {
                    id: row.get(0)?,
                    event_type: row.get(1)?,
                    xp_delta: row.get(2)?,
                    archetype: row.get(3)?,
                    source: row.get(4)?,
                    metadata_json: row.get(5)?,
                    created_at: row.get(6)?,
                    hmac: row.get(7)?,
                    prev_hmac: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    /// Get the timestamp of the first evolution event (for usage duration).
    pub fn first_evolution_event_timestamp(&self) -> Result<Option<i64>> {
        let result: Option<i64> = self
            .conn
            .query_row("SELECT MIN(created_at) FROM evolution_events", [], |row| {
                row.get(0)
            })
            .unwrap_or(None);
        Ok(result)
    }

    // ── Pending Celebrations ──

    /// Insert a pending celebration for async delivery.
    pub fn insert_pending_celebration(
        &self,
        celebration_type: &str,
        payload_json: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO pending_celebrations (celebration_type, payload_json, created_at) VALUES (?1, ?2, ?3)",
            params![celebration_type, payload_json, now],
        )?;
        Ok(())
    }

    /// Get all undelivered celebrations.
    pub fn get_pending_celebrations(&self) -> Result<Vec<PendingCelebration>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, celebration_type, payload_json, created_at FROM pending_celebrations WHERE delivered_at IS NULL ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PendingCelebration {
                    id: row.get(0)?,
                    celebration_type: row.get(1)?,
                    payload_json: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically claim undelivered celebrations by setting `delivered_at` to a
    /// sentinel value (-1) and returning the claimed rows. This prevents
    /// concurrent daemon ticks from delivering the same celebration twice.
    /// Call `mark_celebration_delivered` on success or `unclaim_celebration` on
    /// failure.
    pub fn claim_pending_celebrations(&self) -> Result<Vec<PendingCelebration>> {
        let sentinel: i64 = -1;
        let mut stmt = self.conn.prepare(
            "SELECT id, celebration_type, payload_json, created_at FROM pending_celebrations WHERE delivered_at IS NULL ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PendingCelebration {
                    id: row.get(0)?,
                    celebration_type: row.get(1)?,
                    payload_json: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Mark all claimed rows with sentinel so next tick skips them
        for c in &rows {
            self.conn.execute(
                "UPDATE pending_celebrations SET delivered_at = ?1 WHERE id = ?2 AND delivered_at IS NULL",
                params![sentinel, c.id],
            )?;
        }
        Ok(rows)
    }

    /// Mark a celebration as successfully delivered.
    pub fn mark_celebration_delivered(&self, id: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE pending_celebrations SET delivered_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    /// Release a claimed celebration back to pending state (on delivery failure).
    pub fn unclaim_celebration(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_celebrations SET delivered_at = NULL WHERE id = ?1 AND delivered_at = -1",
            params![id],
        )?;
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
