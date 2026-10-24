//! Persistence for subscriptions and their event ledger.
//!
//! `subscription_events` is the queue between the producers and the
//! dispatcher. The storm-control budget is enforced here, inside one
//! `BEGIN IMMEDIATE` transaction per insert, so two producers racing on the
//! same subscription cannot both slip under a limit.

use anyhow::{Context, Result};
use libsql::params;

use super::Database;
use crate::subscriptions::{event_status, new_id, status, EVENT_ID_PREFIX};

/// A stored subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionRow {
    /// `sub_…`; also the endpoint path.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `webhook` | `channel` | `email` | `internal`.
    pub source: String,
    /// Preset id (`github`, `sentry`, …).
    pub provider: String,
    /// How endpoint requests authenticate (preset default, stored).
    pub auth_scheme: String,
    /// `SecretRef` JSON of Borg's endpoint key.
    pub secret_ref: Option<String>,
    /// `SecretRef` JSON of the signing secret the provider issued, if any.
    pub signing_secret_ref: Option<String>,
    /// Filter JSON.
    pub filter: String,
    /// What the agent should do.
    pub instruction: String,
    /// `None` = the subscription's session only.
    pub delivery_channel: Option<String>,
    /// Recipient on `delivery_channel`.
    pub delivery_target: Option<String>,
    /// `active` | `paused` | `storm`.
    pub status: String,
    /// Runs allowed per hour.
    pub rate_limit_per_hour: i64,
    /// Seconds a run may take.
    pub timeout_secs: i64,
    /// Events refused at the edge (bursts, storm).
    pub dropped_count: i64,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
    /// Unix seconds of the newest accepted event.
    pub last_event_at: Option<i64>,
    /// Status of the newest finished run.
    pub last_run_status: Option<String>,
    /// Why the newest run failed, or why the subscription paused itself.
    pub last_error: Option<String>,
}

/// A stored event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionEventRow {
    /// `evt_…`.
    pub id: String,
    /// Owning subscription.
    pub subscription_id: String,
    /// Normalised kind.
    pub kind: Option<String>,
    /// Provider id used for dedup.
    pub dedup_key: Option<String>,
    /// One line.
    pub summary: String,
    /// Payload JSON (capped).
    pub payload: String,
    /// Whether the payload was cut.
    pub payload_truncated: bool,
    /// `EventOrigin` JSON for channel events.
    pub origin: Option<String>,
    /// See [`crate::subscriptions::event_status`].
    pub status: String,
    /// Run this event was handled in.
    pub run_id: Option<String>,
    /// Unix seconds.
    pub received_at: i64,
    /// Unix seconds.
    pub started_at: Option<i64>,
    /// Unix seconds.
    pub finished_at: Option<i64>,
    /// Run duration.
    pub duration_ms: Option<i64>,
    /// The run's reply.
    pub result: Option<String>,
    /// Why it failed or was skipped.
    pub error: Option<String>,
}

/// Fields of a new subscription.
#[derive(Debug, Clone)]
pub struct NewSubscription<'a> {
    /// Display name.
    pub name: &'a str,
    /// Source.
    pub source: &'a str,
    /// Preset id.
    pub provider: &'a str,
    /// Auth scheme.
    pub auth_scheme: &'a str,
    /// `SecretRef` JSON of the endpoint key, for endpoint sources.
    pub secret_ref: Option<&'a str>,
    /// Filter JSON.
    pub filter: &'a str,
    /// Instruction.
    pub instruction: &'a str,
    /// Delivery channel.
    pub delivery_channel: Option<&'a str>,
    /// Delivery target.
    pub delivery_target: Option<&'a str>,
    /// Initial status (new ones start paused).
    pub status: &'a str,
    /// Runs per hour.
    pub rate_limit_per_hour: i64,
    /// Run timeout.
    pub timeout_secs: i64,
}

/// A partial update. `None` leaves a field alone; for the nullable delivery
/// fields `Some(None)` clears them.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionUpdate {
    /// New name.
    pub name: Option<String>,
    /// New filter JSON.
    pub filter: Option<String>,
    /// New instruction.
    pub instruction: Option<String>,
    /// New delivery channel.
    pub delivery_channel: Option<Option<String>>,
    /// New delivery target.
    pub delivery_target: Option<Option<String>>,
    /// New hourly run limit.
    pub rate_limit_per_hour: Option<i64>,
    /// New timeout.
    pub timeout_secs: Option<i64>,
    /// New provider signing secret (`SecretRef` JSON); `Some(None)` clears it.
    pub signing_secret_ref: Option<Option<String>>,
}

/// A new event, before storm control decides whether to keep it.
#[derive(Debug, Clone)]
pub struct NewSubscriptionEvent<'a> {
    /// Owning subscription.
    pub subscription_id: &'a str,
    /// Kind.
    pub kind: Option<&'a str>,
    /// Dedup key; `None` disables dedup for this event.
    pub dedup_key: Option<&'a str>,
    /// One line.
    pub summary: &'a str,
    /// Payload JSON, already capped.
    pub payload: &'a str,
    /// Whether it was capped.
    pub payload_truncated: bool,
    /// Origin JSON.
    pub origin: Option<&'a str>,
    /// Unix seconds.
    pub received_at: i64,
}

/// The ingest budget one insert is checked against.
#[derive(Debug, Clone, Copy)]
pub struct IngestLimits {
    /// Events accepted per subscription per minute.
    pub per_minute: u32,
    /// Accepted events in ten minutes that trip the storm breaker.
    pub storm_threshold: u32,
    /// Waiting events per subscription.
    pub max_pending: u32,
}

/// What happened to an insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Stored as `pending`; the id is returned to the caller.
    Inserted(String),
    /// Same dedup key as an earlier event; nothing stored.
    Duplicate,
    /// Over the per-minute budget; nothing stored, `dropped_count` bumped.
    RateLimited,
    /// Too many events waiting; stored as `skipped` so the history shows it.
    QueueFull(String),
    /// This insert tripped the storm breaker: the subscription is now
    /// `storm` and the event was not stored. Returned once; later inserts see
    /// [`InsertOutcome::NotAccepting`].
    StormTripped {
        /// Accepted events in the window that tripped it.
        events_in_window: i64,
    },
    /// The subscription is paused, in a storm, or gone.
    NotAccepting,
}

const STORM_WINDOW_SECS: i64 = 600;

const SUB_COLS: &str = "id, name, source, provider, auth_scheme, secret_ref, signing_secret_ref, \
     filter, instruction, delivery_channel, delivery_target, status, rate_limit_per_hour, \
     timeout_secs, dropped_count, created_at, updated_at, last_event_at, last_run_status, last_error";

const EVENT_COLS: &str =
    "id, subscription_id, kind, dedup_key, summary, payload, payload_truncated, \
     origin, status, run_id, received_at, started_at, finished_at, duration_ms, result, error";

fn map_sub(row: &libsql::Row) -> Result<SubscriptionRow> {
    Ok(SubscriptionRow {
        id: row.get(0)?,
        name: row.get(1)?,
        source: row.get(2)?,
        provider: row.get(3)?,
        auth_scheme: row.get(4)?,
        secret_ref: row.get(5)?,
        signing_secret_ref: row.get(6)?,
        filter: row.get(7)?,
        instruction: row.get(8)?,
        delivery_channel: row.get(9)?,
        delivery_target: row.get(10)?,
        status: row.get(11)?,
        rate_limit_per_hour: row.get(12)?,
        timeout_secs: row.get(13)?,
        dropped_count: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
        last_event_at: row.get(17)?,
        last_run_status: row.get(18)?,
        last_error: row.get(19)?,
    })
}

fn map_event(row: &libsql::Row) -> Result<SubscriptionEventRow> {
    Ok(SubscriptionEventRow {
        id: row.get(0)?,
        subscription_id: row.get(1)?,
        kind: row.get(2)?,
        dedup_key: row.get(3)?,
        summary: row.get(4)?,
        payload: row.get(5)?,
        payload_truncated: row.get::<i64>(6)? != 0,
        origin: row.get(7)?,
        status: row.get(8)?,
        run_id: row.get(9)?,
        received_at: row.get(10)?,
        started_at: row.get(11)?,
        finished_at: row.get(12)?,
        duration_ms: row.get(13)?,
        result: row.get(14)?,
        error: row.get(15)?,
    })
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Database {
    /// Store a new subscription and return it.
    pub async fn create_subscription(&self, new: &NewSubscription<'_>) -> Result<SubscriptionRow> {
        let id = new_id(crate::subscriptions::ID_PREFIX);
        let now = now_unix();
        self.conn
            .execute(
                "INSERT INTO subscriptions (id, name, source, provider, auth_scheme, secret_ref, \
                 filter, instruction, delivery_channel, delivery_target, status, \
                 rate_limit_per_hour, timeout_secs, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                params!(
                    id.clone(),
                    new.name,
                    new.source,
                    new.provider,
                    new.auth_scheme,
                    new.secret_ref,
                    new.filter,
                    new.instruction,
                    new.delivery_channel,
                    new.delivery_target,
                    new.status,
                    new.rate_limit_per_hour,
                    new.timeout_secs,
                    now
                ),
            )
            .await
            .context("insert subscription")?;
        self.get_subscription(&id)
            .await?
            .context("subscription vanished right after insert")
    }

    /// One subscription.
    pub async fn get_subscription(&self, id: &str) -> Result<Option<SubscriptionRow>> {
        let sql = format!("SELECT {SUB_COLS} FROM subscriptions WHERE id = ?1");
        let mut rows = self.conn.query(&sql, params!(id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_sub(&row)?)),
            None => Ok(None),
        }
    }

    /// Every subscription, newest first.
    pub async fn list_subscriptions(&self) -> Result<Vec<SubscriptionRow>> {
        let sql = format!("SELECT {SUB_COLS} FROM subscriptions ORDER BY created_at DESC, id");
        let mut rows = self.conn.query(&sql, ()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_sub(&row)?);
        }
        Ok(out)
    }

    /// Active subscriptions of one source (the fan-out set for channel and
    /// internal events).
    pub async fn active_subscriptions_for_source(
        &self,
        source: &str,
    ) -> Result<Vec<SubscriptionRow>> {
        let sql = format!(
            "SELECT {SUB_COLS} FROM subscriptions WHERE source = ?1 AND status = ?2 ORDER BY id"
        );
        let mut rows = self
            .conn
            .query(&sql, params!(source, status::ACTIVE))
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_sub(&row)?);
        }
        Ok(out)
    }

    /// Apply a partial update. Returns the row after the change, or `None`
    /// when it does not exist.
    pub async fn update_subscription(
        &self,
        id: &str,
        update: &SubscriptionUpdate,
    ) -> Result<Option<SubscriptionRow>> {
        let Some(mut row) = self.get_subscription(id).await? else {
            return Ok(None);
        };
        if let Some(v) = &update.name {
            row.name.clone_from(v);
        }
        if let Some(v) = &update.filter {
            row.filter.clone_from(v);
        }
        if let Some(v) = &update.instruction {
            row.instruction.clone_from(v);
        }
        if let Some(v) = &update.delivery_channel {
            row.delivery_channel.clone_from(v);
        }
        if let Some(v) = &update.delivery_target {
            row.delivery_target.clone_from(v);
        }
        if let Some(v) = update.rate_limit_per_hour {
            row.rate_limit_per_hour = v;
        }
        if let Some(v) = update.timeout_secs {
            row.timeout_secs = v;
        }
        if let Some(v) = &update.signing_secret_ref {
            row.signing_secret_ref.clone_from(v);
        }
        self.conn
            .execute(
                "UPDATE subscriptions SET name = ?2, filter = ?3, instruction = ?4, \
                 delivery_channel = ?5, delivery_target = ?6, rate_limit_per_hour = ?7, \
                 timeout_secs = ?8, signing_secret_ref = ?9, updated_at = ?10 WHERE id = ?1",
                params!(
                    id,
                    row.name,
                    row.filter,
                    row.instruction,
                    row.delivery_channel,
                    row.delivery_target,
                    row.rate_limit_per_hour,
                    row.timeout_secs,
                    row.signing_secret_ref,
                    now_unix()
                ),
            )
            .await?;
        self.get_subscription(id).await
    }

    /// Set the status. Setting `active` clears a storm's error and the
    /// dropped counter, which is what "Resume" means to the user.
    pub async fn set_subscription_status(
        &self,
        id: &str,
        new_status: &str,
        last_error: Option<&str>,
    ) -> Result<Option<SubscriptionRow>> {
        let clears = new_status == status::ACTIVE;
        self.conn
            .execute(
                "UPDATE subscriptions SET status = ?2, updated_at = ?3, \
                 last_error = CASE WHEN ?4 THEN NULL ELSE COALESCE(?5, last_error) END, \
                 dropped_count = CASE WHEN ?4 THEN 0 ELSE dropped_count END \
                 WHERE id = ?1",
                params!(id, new_status, now_unix(), i64::from(clears), last_error),
            )
            .await?;
        self.get_subscription(id).await
    }

    /// Replace the endpoint key reference.
    pub async fn set_subscription_secret(&self, id: &str, secret_ref: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE subscriptions SET secret_ref = ?2, updated_at = ?3 WHERE id = ?1",
                params!(id, secret_ref, now_unix()),
            )
            .await?;
        Ok(n > 0)
    }

    /// Delete a subscription and its events. Returns whether it existed.
    pub async fn delete_subscription(&self, id: &str) -> Result<bool> {
        // Events first: the cascade needs `foreign_keys`, and this must not
        // depend on a pragma.
        self.conn
            .execute(
                "DELETE FROM subscription_events WHERE subscription_id = ?1",
                params!(id),
            )
            .await?;
        let n = self
            .conn
            .execute("DELETE FROM subscriptions WHERE id = ?1", params!(id))
            .await?;
        Ok(n > 0)
    }

    /// Store an event, or refuse it under the ingest budget.
    ///
    /// Order matters and is the storm-control contract: refuse a paused or
    /// stormed subscription, then the per-minute burst cap (nothing stored),
    /// then the storm breaker (trips once), then dedup, then the pending cap
    /// (stored as `skipped`).
    pub async fn insert_subscription_event(
        &self,
        ev: &NewSubscriptionEvent<'_>,
        limits: IngestLimits,
    ) -> Result<InsertOutcome> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = self.insert_event_in_tx(ev, limits).await;
        self.finish_immediate_tx(result, "subscription event insert")
            .await
    }

    async fn insert_event_in_tx(
        &self,
        ev: &NewSubscriptionEvent<'_>,
        limits: IngestLimits,
    ) -> Result<InsertOutcome> {
        let sub_status: Option<String> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT status FROM subscriptions WHERE id = ?1",
                    params!(ev.subscription_id),
                )
                .await?;
            match rows.next().await? {
                Some(r) => Some(r.get(0)?),
                None => None,
            }
        };
        match sub_status.as_deref() {
            Some(s) if s == status::ACTIVE => {}
            Some(s) if s == status::STORM => {
                self.bump_dropped(ev.subscription_id).await?;
                return Ok(InsertOutcome::NotAccepting);
            }
            _ => return Ok(InsertOutcome::NotAccepting),
        }

        // Dedup before the budget: a provider retrying a delivery we already
        // hold must get "duplicate", not a 429 that makes it retry again.
        if let Some(key) = ev.dedup_key {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM subscription_events WHERE subscription_id = ?1 AND dedup_key = ?2",
                    params!(ev.subscription_id, key),
                )
                .await?;
            if rows.next().await?.is_some() {
                return Ok(InsertOutcome::Duplicate);
            }
        }

        let accepted_since = |since: i64| {
            let sub = ev.subscription_id.to_string();
            async move {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT COUNT(*) FROM subscription_events \
                         WHERE subscription_id = ?1 AND received_at > ?2",
                        params!(sub, since),
                    )
                    .await?;
                let n: i64 = match rows.next().await? {
                    Some(r) => r.get(0)?,
                    None => 0,
                };
                anyhow::Ok(n)
            }
        };

        if accepted_since(ev.received_at - 60).await? >= i64::from(limits.per_minute) {
            self.bump_dropped(ev.subscription_id).await?;
            return Ok(InsertOutcome::RateLimited);
        }

        let in_window = accepted_since(ev.received_at - STORM_WINDOW_SECS).await?;
        if in_window >= i64::from(limits.storm_threshold) {
            let dropped = self.dropped_count(ev.subscription_id).await?;
            let reason = format!(
                "Paused itself: {} events in {} minutes. Resume when the source is quiet.",
                in_window + dropped + 1,
                STORM_WINDOW_SECS / 60
            );
            self.conn
                .execute(
                    "UPDATE subscriptions SET status = ?2, last_error = ?3, \
                     dropped_count = dropped_count + 1, updated_at = ?4 WHERE id = ?1",
                    params!(ev.subscription_id, status::STORM, reason, now_unix()),
                )
                .await?;
            return Ok(InsertOutcome::StormTripped {
                events_in_window: in_window + dropped + 1,
            });
        }

        let pending: i64 = {
            let mut rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM subscription_events WHERE subscription_id = ?1 AND status = ?2",
                    params!(ev.subscription_id, event_status::PENDING),
                )
                .await?;
            match rows.next().await? {
                Some(r) => r.get(0)?,
                None => 0,
            }
        };
        let queue_full = pending >= i64::from(limits.max_pending);

        let id = new_id(EVENT_ID_PREFIX);
        let (row_status, error) = if queue_full {
            (
                event_status::SKIPPED,
                Some("Queue full: too many events were already waiting."),
            )
        } else {
            (event_status::PENDING, None)
        };
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO subscription_events (id, subscription_id, kind, dedup_key, \
                 summary, payload, payload_truncated, origin, status, received_at, error, finished_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
                 CASE WHEN ?9 = 'skipped' THEN ?10 ELSE NULL END)",
                params!(
                    id.clone(),
                    ev.subscription_id,
                    ev.kind,
                    ev.dedup_key,
                    ev.summary,
                    ev.payload,
                    i64::from(ev.payload_truncated),
                    ev.origin,
                    row_status,
                    ev.received_at,
                    error
                ),
            )
            .await?;
        if inserted == 0 {
            return Ok(InsertOutcome::Duplicate);
        }
        self.conn
            .execute(
                "UPDATE subscriptions SET last_event_at = ?2 WHERE id = ?1",
                params!(ev.subscription_id, ev.received_at),
            )
            .await?;
        Ok(if queue_full {
            InsertOutcome::QueueFull(id)
        } else {
            InsertOutcome::Inserted(id)
        })
    }

    async fn bump_dropped(&self, id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE subscriptions SET dropped_count = dropped_count + 1 WHERE id = ?1",
                params!(id),
            )
            .await?;
        Ok(())
    }

    async fn dropped_count(&self, id: &str) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT dropped_count FROM subscriptions WHERE id = ?1",
                params!(id),
            )
            .await?;
        Ok(match rows.next().await? {
            Some(r) => r.get(0)?,
            None => 0,
        })
    }

    /// Claim the oldest runnable event as the lead of run `run_id`.
    ///
    /// Runnable means: `pending`, its subscription `active`, no run of that
    /// subscription in flight (one run at a time per subscription), and the
    /// subscription under its hourly run budget.
    pub async fn claim_next_subscription_event(
        &self,
        run_id: &str,
        now: i64,
    ) -> Result<Option<SubscriptionEventRow>> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = async {
            let mut rows = self
                .conn
                .query(
                    "SELECT e.id FROM subscription_events e \
                     JOIN subscriptions s ON s.id = e.subscription_id \
                     WHERE e.status = 'pending' AND s.status = 'active' \
                     AND NOT EXISTS (SELECT 1 FROM subscription_events r \
                         WHERE r.subscription_id = e.subscription_id AND r.status = 'running') \
                     AND (SELECT COUNT(DISTINCT h.run_id) FROM subscription_events h \
                         WHERE h.subscription_id = e.subscription_id AND h.run_id IS NOT NULL \
                         AND h.started_at > ?1) < s.rate_limit_per_hour \
                     ORDER BY e.received_at, e.id LIMIT 1",
                    params!(now - 3600),
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return anyhow::Ok(None);
            };
            let id: String = row.get(0)?;
            drop(rows);
            self.conn
                .execute(
                    "UPDATE subscription_events SET status = 'running', run_id = ?2, started_at = ?3 \
                     WHERE id = ?1",
                    params!(id.clone(), run_id, now),
                )
                .await?;
            self.get_subscription_event(&id).await
        }
        .await;
        self.finish_immediate_tx(result, "subscription event claim")
            .await
    }

    /// Pull up to `limit` more waiting events of `subscription_id` into run
    /// `run_id` (the digest). Oldest first.
    pub async fn claim_subscription_batch(
        &self,
        subscription_id: &str,
        run_id: &str,
        limit: u32,
        now: i64,
    ) -> Result<Vec<SubscriptionEventRow>> {
        self.conn
            .execute(
                "UPDATE subscription_events SET status = 'running', run_id = ?2, started_at = ?3 \
                 WHERE id IN (SELECT id FROM subscription_events \
                     WHERE subscription_id = ?1 AND status = 'pending' \
                     ORDER BY received_at, id LIMIT ?4)",
                params!(subscription_id, run_id, now, i64::from(limit)),
            )
            .await?;
        self.run_events(run_id).await
    }

    /// Every event in a run, oldest first.
    pub async fn run_events(&self, run_id: &str) -> Result<Vec<SubscriptionEventRow>> {
        let sql = format!(
            "SELECT {EVENT_COLS} FROM subscription_events WHERE run_id = ?1 ORDER BY received_at, id"
        );
        let mut rows = self.conn.query(&sql, params!(run_id)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_event(&row)?);
        }
        Ok(out)
    }

    /// One event.
    pub async fn get_subscription_event(&self, id: &str) -> Result<Option<SubscriptionEventRow>> {
        let sql = format!("SELECT {EVENT_COLS} FROM subscription_events WHERE id = ?1");
        let mut rows = self.conn.query(&sql, params!(id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_event(&row)?)),
            None => Ok(None),
        }
    }

    /// Close one event that will not run (did not match the filter).
    pub async fn skip_subscription_event(&self, id: &str, reason: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE subscription_events SET status = 'skipped', error = ?2, finished_at = ?3, \
                 run_id = NULL, started_at = NULL WHERE id = ?1",
                params!(id, reason, now_unix()),
            )
            .await?;
        Ok(())
    }

    /// Close every still-running event of a run, and record the outcome on
    /// the subscription.
    pub async fn finish_subscription_run(
        &self,
        subscription_id: &str,
        run_id: &str,
        final_status: &str,
        result: Option<&str>,
        error: Option<&str>,
        duration_ms: i64,
    ) -> Result<()> {
        let now = now_unix();
        self.conn
            .execute(
                "UPDATE subscription_events SET status = ?2, result = ?3, error = ?4, \
                 finished_at = ?5, duration_ms = ?6 WHERE run_id = ?1 AND status = 'running'",
                params!(run_id, final_status, result, error, now, duration_ms),
            )
            .await?;
        self.conn
            .execute(
                "UPDATE subscriptions SET last_run_status = ?2, \
                 last_error = CASE WHEN ?2 = 'failed' THEN ?3 \
                                   WHEN status = 'storm' THEN last_error ELSE NULL END \
                 WHERE id = ?1",
                params!(subscription_id, final_status, error),
            )
            .await?;
        Ok(())
    }

    /// The statuses of the newest `n` runs of a subscription, newest first.
    pub async fn recent_subscription_run_statuses(
        &self,
        subscription_id: &str,
        n: u32,
    ) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT MIN(status) FROM subscription_events \
                 WHERE subscription_id = ?1 AND run_id IS NOT NULL AND finished_at IS NOT NULL \
                 GROUP BY run_id ORDER BY MAX(finished_at) DESC, run_id DESC LIMIT ?2",
                params!(subscription_id, i64::from(n)),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(r.get(0)?);
        }
        Ok(out)
    }

    /// Runs started since `since` across every subscription (the daily cap).
    pub async fn subscription_runs_since(&self, since: i64) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(DISTINCT run_id) FROM subscription_events \
                 WHERE run_id IS NOT NULL AND started_at > ?1",
                params!(since),
            )
            .await?;
        Ok(match rows.next().await? {
            Some(r) => r.get(0)?,
            None => 0,
        })
    }

    /// Newest events of a subscription, newest first.
    pub async fn list_subscription_events(
        &self,
        subscription_id: &str,
        limit: u32,
    ) -> Result<Vec<SubscriptionEventRow>> {
        let sql = format!(
            "SELECT {EVENT_COLS} FROM subscription_events WHERE subscription_id = ?1 \
             ORDER BY received_at DESC, id DESC LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params!(subscription_id, i64::from(limit)))
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_event(&row)?);
        }
        Ok(out)
    }

    /// Put every `running` event back to `pending`. Run once at boot: a run
    /// that was in flight when the daemon stopped will run again.
    pub async fn reset_running_subscription_events(&self) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "UPDATE subscription_events SET status = 'pending', run_id = NULL, started_at = NULL \
                 WHERE status = 'running'",
                (),
            )
            .await?;
        Ok(n)
    }

    /// Delete finished events beyond the newest `keep` of each subscription.
    /// Waiting and running events are never pruned.
    pub async fn prune_subscription_events(&self, subscription_id: &str, keep: u32) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "DELETE FROM subscription_events WHERE subscription_id = ?1 \
                 AND status NOT IN ('pending', 'running') \
                 AND id NOT IN (SELECT id FROM subscription_events WHERE subscription_id = ?1 \
                     ORDER BY received_at DESC, id DESC LIMIT ?2)",
                params!(subscription_id, i64::from(keep)),
            )
            .await?;
        Ok(n)
    }

    /// Channel names with at least one active channel subscription.
    pub async fn tapped_subscription_channels(&self) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT DISTINCT provider FROM subscriptions WHERE source = 'channel' AND status = 'active'",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(r.get(0)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: IngestLimits = IngestLimits {
        per_minute: 120,
        storm_threshold: 300,
        max_pending: 200,
    };

    async fn sub(db: &Database, status: &str) -> SubscriptionRow {
        db.create_subscription(&NewSubscription {
            name: "Sentry errors",
            source: "webhook",
            provider: "sentry",
            auth_scheme: "sentry",
            secret_ref: Some(r#"{"source":"literal","value":"bsk_x"}"#),
            filter: "{}",
            instruction: "Diagnose.",
            delivery_channel: None,
            delivery_target: None,
            status,
            rate_limit_per_hour: 60,
            timeout_secs: 300,
        })
        .await
        .unwrap()
    }

    fn event<'a>(sub_id: &'a str, dedup: Option<&'a str>, at: i64) -> NewSubscriptionEvent<'a> {
        NewSubscriptionEvent {
            subscription_id: sub_id,
            kind: Some("issue.created"),
            dedup_key: dedup,
            summary: "TypeError",
            payload: "{}",
            payload_truncated: false,
            origin: None,
            received_at: at,
        }
    }

    #[tokio::test]
    async fn crud_round_trip_and_partial_update() {
        let db = Database::test_db().await;
        let s = sub(&db, status::PAUSED).await;
        assert!(s.id.starts_with("sub_"));
        assert_eq!(s.status, "paused", "a new subscription starts paused");

        let updated = db
            .update_subscription(
                &s.id,
                &SubscriptionUpdate {
                    instruction: Some("New.".into()),
                    delivery_channel: Some(Some("slack".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.instruction, "New.");
        assert_eq!(updated.delivery_channel.as_deref(), Some("slack"));
        assert_eq!(updated.name, "Sentry errors", "unset fields are left alone");

        let cleared = db
            .update_subscription(
                &s.id,
                &SubscriptionUpdate {
                    delivery_channel: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleared.delivery_channel, None);

        assert_eq!(db.list_subscriptions().await.unwrap().len(), 1);
        assert!(db.delete_subscription(&s.id).await.unwrap());
        assert!(db.get_subscription(&s.id).await.unwrap().is_none());
        assert!(!db.delete_subscription(&s.id).await.unwrap());
    }

    #[tokio::test]
    async fn paused_subscriptions_refuse_events_and_dedup_ignores_repeats() {
        let db = Database::test_db().await;
        let s = sub(&db, status::PAUSED).await;
        let now = 1_000_000;
        assert_eq!(
            db.insert_subscription_event(&event(&s.id, Some("d1"), now), LIMITS)
                .await
                .unwrap(),
            InsertOutcome::NotAccepting
        );
        db.set_subscription_status(&s.id, status::ACTIVE, None)
            .await
            .unwrap();
        assert!(matches!(
            db.insert_subscription_event(&event(&s.id, Some("d1"), now), LIMITS)
                .await
                .unwrap(),
            InsertOutcome::Inserted(_)
        ));
        assert_eq!(
            db.insert_subscription_event(&event(&s.id, Some("d1"), now + 1), LIMITS)
                .await
                .unwrap(),
            InsertOutcome::Duplicate
        );
        let events = db.list_subscription_events(&s.id, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, "pending");
        assert_eq!(
            db.get_subscription(&s.id)
                .await
                .unwrap()
                .unwrap()
                .last_event_at,
            Some(now)
        );
    }

    /// 1 000 events in one minute: only `per_minute` are stored, the rest are
    /// counted as dropped, and nothing trips the breaker yet.
    #[tokio::test]
    async fn a_burst_is_capped_per_minute_and_counted() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let now = 2_000_000;
        let mut stored = 0;
        let mut limited = 0;
        for i in 0..1000 {
            let key = format!("e{i}");
            match db
                .insert_subscription_event(&event(&s.id, Some(&key), now), LIMITS)
                .await
                .unwrap()
            {
                InsertOutcome::Inserted(_) => stored += 1,
                InsertOutcome::RateLimited => limited += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(stored, 120);
        assert_eq!(limited, 880);
        let row = db.get_subscription(&s.id).await.unwrap().unwrap();
        assert_eq!(row.dropped_count, 880);
        assert_eq!(row.status, "active");
    }

    /// Sustained traffic across minutes trips the storm breaker exactly once;
    /// after that the subscription refuses events until resumed.
    #[tokio::test]
    async fn sustained_traffic_trips_the_storm_breaker_once() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let limits = IngestLimits {
            per_minute: 1000,
            storm_threshold: 300,
            max_pending: 10_000,
        };
        let start = 3_000_000;
        let mut trips = 0;
        let mut refused = 0;
        for i in 0..9000_i64 {
            let key = format!("e{i}");
            match db
                .insert_subscription_event(&event(&s.id, Some(&key), start + i / 40), limits)
                .await
                .unwrap()
            {
                InsertOutcome::Inserted(_) => {}
                InsertOutcome::StormTripped { events_in_window } => {
                    trips += 1;
                    assert!(events_in_window >= 300);
                }
                InsertOutcome::NotAccepting => refused += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(trips, 1, "the breaker trips once");
        let row = db.get_subscription(&s.id).await.unwrap().unwrap();
        assert_eq!(row.status, "storm");
        assert!(row.last_error.unwrap().starts_with("Paused itself"));
        assert_eq!(refused, 9000 - 300 - 1);
        assert_eq!(
            db.list_subscription_events(&s.id, 10_000)
                .await
                .unwrap()
                .len(),
            300
        );

        let resumed = db
            .set_subscription_status(&s.id, status::ACTIVE, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.last_error, None);
        assert_eq!(resumed.dropped_count, 0);
    }

    #[tokio::test]
    async fn a_full_queue_keeps_the_event_as_skipped() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let limits = IngestLimits {
            per_minute: 100,
            storm_threshold: 100,
            max_pending: 2,
        };
        for i in 0..2 {
            let key = format!("k{i}");
            assert!(matches!(
                db.insert_subscription_event(&event(&s.id, Some(&key), 10), limits)
                    .await
                    .unwrap(),
                InsertOutcome::Inserted(_)
            ));
        }
        let full = db
            .insert_subscription_event(&event(&s.id, Some("k2"), 10), limits)
            .await
            .unwrap();
        let InsertOutcome::QueueFull(id) = full else {
            panic!("{full:?}")
        };
        let row = db.get_subscription_event(&id).await.unwrap().unwrap();
        assert_eq!(row.status, "skipped");
        assert!(row.error.unwrap().contains("Queue full"));
    }

    /// One run per subscription at a time, and a digest pulls in the rest.
    #[tokio::test]
    async fn claims_serialise_per_subscription_and_batch_the_backlog() {
        let db = Database::test_db().await;
        let a = sub(&db, status::ACTIVE).await;
        let b = sub(&db, status::ACTIVE).await;
        for i in 0..5 {
            let key = format!("a{i}");
            db.insert_subscription_event(&event(&a.id, Some(&key), 100 + i), LIMITS)
                .await
                .unwrap();
        }
        db.insert_subscription_event(&event(&b.id, Some("b0"), 200), LIMITS)
            .await
            .unwrap();

        let now = 1_000;
        let lead = db
            .claim_next_subscription_event("run_1", now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lead.subscription_id, a.id, "oldest first");
        let next = db
            .claim_next_subscription_event("run_2", now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.subscription_id, b.id, "a is busy, so b goes next");
        assert!(db
            .claim_next_subscription_event("run_3", now)
            .await
            .unwrap()
            .is_none());

        let batch = db
            .claim_subscription_batch(&a.id, "run_1", 50, now)
            .await
            .unwrap();
        assert_eq!(batch.len(), 5, "lead plus the four waiting");
        db.finish_subscription_run(&a.id, "run_1", "succeeded", Some("ok"), None, 12)
            .await
            .unwrap();
        let events = db.list_subscription_events(&a.id, 10).await.unwrap();
        assert!(events
            .iter()
            .all(|e| e.status == "succeeded" && e.run_id.as_deref() == Some("run_1")));
        assert_eq!(
            db.get_subscription(&a.id)
                .await
                .unwrap()
                .unwrap()
                .last_run_status
                .as_deref(),
            Some("succeeded")
        );
        assert_eq!(db.subscription_runs_since(0).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn the_hourly_run_budget_holds_events_back() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        db.update_subscription(
            &s.id,
            &SubscriptionUpdate {
                rate_limit_per_hour: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.insert_subscription_event(&event(&s.id, Some("x"), 10), LIMITS)
            .await
            .unwrap();
        db.insert_subscription_event(&event(&s.id, Some("y"), 11), LIMITS)
            .await
            .unwrap();
        let now = 5_000;
        db.claim_next_subscription_event("run_1", now)
            .await
            .unwrap()
            .unwrap();
        db.finish_subscription_run(&s.id, "run_1", "succeeded", None, None, 1)
            .await
            .unwrap();
        assert!(
            db.claim_next_subscription_event("run_2", now + 10)
                .await
                .unwrap()
                .is_none(),
            "second run in the same hour is held"
        );
        assert!(db
            .claim_next_subscription_event("run_2", now + 3601)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn boot_recovery_and_pruning_never_lose_waiting_events() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        for i in 0..6 {
            let key = format!("p{i}");
            db.insert_subscription_event(&event(&s.id, Some(&key), 10 + i), LIMITS)
                .await
                .unwrap();
        }
        db.claim_next_subscription_event("run_1", 100)
            .await
            .unwrap()
            .unwrap();
        db.claim_subscription_batch(&s.id, "run_1", 2, 100)
            .await
            .unwrap();
        db.finish_subscription_run(&s.id, "run_1", "failed", None, Some("timed out"), 1)
            .await
            .unwrap();
        db.claim_next_subscription_event("run_2", 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(db.reset_running_subscription_events().await.unwrap(), 1);

        let pruned = db.prune_subscription_events(&s.id, 1).await.unwrap();
        assert_eq!(pruned, 3, "the three finished events go");
        let left = db.list_subscription_events(&s.id, 10).await.unwrap();
        assert_eq!(left.len(), 3);
        assert!(left.iter().all(|e| e.status == "pending"));

        assert_eq!(
            db.recent_subscription_run_statuses(&s.id, 5).await.unwrap(),
            Vec::<String>::new()
        );
        let row = db.get_subscription(&s.id).await.unwrap().unwrap();
        assert_eq!(row.last_error.as_deref(), Some("timed out"));
    }

    #[tokio::test]
    async fn run_statuses_are_reported_per_run_newest_first() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        for (i, outcome) in ["succeeded", "failed", "failed"].iter().enumerate() {
            let key = format!("r{i}");
            db.insert_subscription_event(&event(&s.id, Some(&key), i as i64), LIMITS)
                .await
                .unwrap();
            let run = format!("run_{i}");
            db.claim_next_subscription_event(&run, 100 + i as i64)
                .await
                .unwrap()
                .unwrap();
            db.finish_subscription_run(&s.id, &run, outcome, None, None, 1)
                .await
                .unwrap();
            // Finished events need distinct finish times for the ordering.
            db.conn
                .execute(
                    "UPDATE subscription_events SET finished_at = ?2 WHERE run_id = ?1",
                    params!(run, 1000 + i as i64),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            db.recent_subscription_run_statuses(&s.id, 5).await.unwrap(),
            vec!["failed", "failed", "succeeded"]
        );
    }

    #[tokio::test]
    async fn only_active_channel_subscriptions_are_tapped() {
        let db = Database::test_db().await;
        for (provider, st) in [("slack", status::ACTIVE), ("discord", status::PAUSED)] {
            db.create_subscription(&NewSubscription {
                name: provider,
                source: "channel",
                provider,
                auth_scheme: "none",
                secret_ref: None,
                filter: "{}",
                instruction: "x",
                delivery_channel: None,
                delivery_target: None,
                status: st,
                rate_limit_per_hour: 60,
                timeout_secs: 60,
            })
            .await
            .unwrap();
        }
        assert_eq!(
            db.tapped_subscription_channels().await.unwrap(),
            vec!["slack"]
        );
        assert_eq!(
            db.active_subscriptions_for_source("channel")
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
