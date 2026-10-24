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
/// Finished events younger than this are kept whatever the retention says:
/// the daily run budget counts them.
const PRUNE_MIN_AGE_SECS: i64 = 24 * 3600;

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

/// Refuse a filter the endpoint could not read. Stored unreadable, it would
/// reject every event with nobody told.
fn check_filter(filter: &str) -> Result<()> {
    if crate::subscriptions::filter::Filter::parse(filter).is_none() {
        anyhow::bail!(
            "The filter is not valid. Use only the fields the source offers (kinds, severity_min, phrase, …)."
        );
    }
    Ok(())
}

impl Database {
    /// Store a new subscription and return it.
    pub async fn create_subscription(&self, new: &NewSubscription<'_>) -> Result<SubscriptionRow> {
        check_filter(new.filter)?;
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
            check_filter(v)?;
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

    /// Record that the sender reached the subscription, without storing an
    /// event. Used while it is paused, so the app can show that the sender
    /// is set up before the user turns it on.
    pub async fn mark_subscription_seen(&self, id: &str, now: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE subscriptions SET last_event_at = ?2 WHERE id = ?1",
                params!(id, now),
            )
            .await?;
        Ok(())
    }

    /// Set the status. Setting `active` clears a storm's error and the
    /// dropped counter and restarts the burst windows, which is what "Resume"
    /// means to the user: without the restart, the events that tripped the
    /// breaker would trip it again on the first new one.
    pub async fn set_subscription_status(
        &self,
        id: &str,
        new_status: &str,
        last_error: Option<&str>,
    ) -> Result<Option<SubscriptionRow>> {
        let clears = new_status == status::ACTIVE;
        self.conn
            .execute(
                // `status` on the right is the old value: only a real resume
                // (paused or storm to active) restarts the windows, so a
                // repeated "enable" during a burst cannot reopen them.
                "UPDATE subscriptions SET status = ?2, updated_at = ?3, \
                 last_error = CASE WHEN ?4 THEN NULL ELSE COALESCE(?5, last_error) END, \
                 dropped_count = CASE WHEN ?4 AND status != 'active' THEN 0 ELSE dropped_count END, \
                 accepting_since = CASE WHEN ?4 AND status != 'active' THEN ?3 ELSE accepting_since END \
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
    /// stormed subscription; answer a repeat as a duplicate (so a provider's
    /// retry is never told to retry again); then the per-minute burst cap
    /// (nothing stored); then the storm breaker (trips once); then the
    /// pending cap (stored as `skipped`).
    ///
    /// The burst windows only count events since the subscription last
    /// became active, so a resume starts from a clean slate.
    pub async fn insert_subscription_event(
        &self,
        ev: &NewSubscriptionEvent<'_>,
        limits: IngestLimits,
    ) -> Result<InsertOutcome> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = self.insert_event_in_tx(ev, limits).await;
        self.end_subscription_tx(result, "subscription event insert")
            .await
    }

    /// COMMIT on success, ROLLBACK on failure, and ROLLBACK when the COMMIT
    /// itself fails, so a failed write never leaves the connection inside a
    /// transaction.
    ///
    /// Callers own their connection for the duration (the gateway opens one
    /// per request, the dispatcher holds its own), so no other task's
    /// statements land inside this transaction.
    async fn end_subscription_tx<T>(&self, result: Result<T>, label: &str) -> Result<T> {
        let outcome = match result {
            Ok(v) => match self.conn.execute_batch("COMMIT").await {
                Ok(_) => return Ok(v),
                Err(e) => Err(anyhow::Error::from(e).context(format!("{label}: commit"))),
            },
            Err(e) => Err(e),
        };
        if let Err(rb) = self.conn.execute_batch("ROLLBACK").await {
            tracing::warn!("{label}: rollback failed: {rb}");
        }
        outcome
    }

    async fn insert_event_in_tx(
        &self,
        ev: &NewSubscriptionEvent<'_>,
        limits: IngestLimits,
    ) -> Result<InsertOutcome> {
        let found: Option<(String, i64)> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT status, accepting_since FROM subscriptions WHERE id = ?1",
                    params!(ev.subscription_id),
                )
                .await?;
            match rows.next().await? {
                Some(r) => Some((r.get(0)?, r.get(1)?)),
                None => None,
            }
        };
        let accepting_since = found.as_ref().map_or(0, |(_, since)| *since);
        match found.as_ref().map(|(s, _)| s.as_str()) {
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
            let since = since.max(accepting_since);
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
            let seen = in_window + 1;
            let reason = format!(
                "Paused itself: more than {in_window} events in {} minutes. Resume when the source is quiet.",
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
                events_in_window: seen,
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
        // A refused event keeps no dedup key, so the provider's retry after
        // the 429 is accepted once there is room, not treated as a repeat.
        let dedup_key = if queue_full { None } else { ev.dedup_key };
        // A refused event keeps its summary for the history, not its payload:
        // a burst of refusals must not fill the disk.
        let (payload, truncated) = if queue_full {
            ("{}", true)
        } else {
            (ev.payload, ev.payload_truncated)
        };
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
                    dedup_key,
                    ev.summary,
                    payload,
                    i64::from(truncated),
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

    /// Queue a test event: the source's sample payload, kind
    /// [`crate::subscriptions::TEST_KIND`]. It skips the ingest budget and
    /// runs even while the subscription is paused, so Test works before the
    /// user turns the subscription on.
    pub async fn insert_subscription_test_event(
        &self,
        subscription_id: &str,
        summary: &str,
        payload: &str,
    ) -> Result<SubscriptionEventRow> {
        let id = new_id(EVENT_ID_PREFIX);
        let now = now_unix();
        self.conn
            .execute(
                "INSERT INTO subscription_events (id, subscription_id, kind, summary, payload, \
                 payload_truncated, status, received_at) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
                params!(
                    id.clone(),
                    subscription_id,
                    crate::subscriptions::TEST_KIND,
                    summary,
                    payload,
                    event_status::PENDING,
                    now
                ),
            )
            .await?;
        self.get_subscription_event(&id)
            .await?
            .context("test event vanished right after insert")
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
        self.claim_next_subscription_event_except(run_id, now, &[])
            .await
    }

    /// [`Self::claim_next_subscription_event`], passing over the
    /// subscriptions in `skip` (their session is busy), so one subscription
    /// the user is chatting in does not hold up the others.
    pub async fn claim_next_subscription_event_except(
        &self,
        run_id: &str,
        now: i64,
        skip: &[String],
    ) -> Result<Option<SubscriptionEventRow>> {
        let skip = serde_json::to_string(skip).context("encode skip list")?;
        // Most passes find nothing (a run is in flight, the budget is spent).
        // Look first without the write lock, which every producer's insert
        // also needs, and take it only when there is something to claim.
        if self.runnable_lead(now, &skip).await?.is_none() {
            return Ok(None);
        }
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = async {
            // Again under the lock: another claimer may have taken it.
            let Some(id) = self.runnable_lead(now, &skip).await? else {
                return anyhow::Ok(None);
            };
            self.conn
                .execute(
                    "UPDATE subscription_events SET status = 'running', run_id = ?2, started_at = ?3, \
                     attempts = attempts + 1 WHERE id = ?1",
                    params!(id.clone(), run_id, now),
                )
                .await?;
            self.get_subscription_event(&id).await
        }
        .await;
        self.end_subscription_tx(result, "subscription event claim")
            .await
    }

    /// The id of the oldest runnable event, if any.
    async fn runnable_lead(&self, now: i64, skip_json: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT e.id FROM subscription_events e \
                     JOIN subscriptions s ON s.id = e.subscription_id \
                     WHERE e.status = 'pending' AND (s.status = 'active' OR e.kind = 'test') \
                     AND NOT EXISTS (SELECT 1 FROM subscription_events r \
                         WHERE r.subscription_id = e.subscription_id AND r.status = 'running') \
                     AND (SELECT COUNT(DISTINCT h.run_id) FROM subscription_events h \
                         WHERE h.subscription_id = e.subscription_id AND h.run_id IS NOT NULL \
                         AND h.started_at > ?1) < s.rate_limit_per_hour \
                     AND e.subscription_id NOT IN (SELECT value FROM json_each(?2)) \
                     ORDER BY e.received_at, e.id LIMIT 1",
                params!(now - 3600, skip_json),
            )
            .await?;
        Ok(match rows.next().await? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// Pull up to `limit` more waiting events of `subscription_id` into run
    /// `run_id` (the digest). Oldest first.
    ///
    /// `same_origin` (the lead's stored origin) keeps a digest to one
    /// conversation: a run that replies where its events came from has one
    /// reply, so events from another chat or thread wait for their own run.
    pub async fn claim_subscription_batch(
        &self,
        subscription_id: &str,
        run_id: &str,
        limit: u32,
        now: i64,
        same_origin: Option<&str>,
    ) -> Result<Vec<SubscriptionEventRow>> {
        self.conn
            .execute(
                "UPDATE subscription_events SET status = 'running', run_id = ?2, started_at = ?3, \
                 attempts = attempts + 1 \
                 WHERE id IN (SELECT id FROM subscription_events \
                     WHERE subscription_id = ?1 AND status = 'pending' \
                     AND (kind = 'test' OR (SELECT status FROM subscriptions WHERE id = ?1) = 'active') \
                     AND (?5 IS NULL OR ( \
                         json_extract(origin, '$.channel') IS json_extract(?5, '$.channel') \
                         AND COALESCE(json_extract(origin, '$.chat'), json_extract(origin, '$.sender')) \
                             IS COALESCE(json_extract(?5, '$.chat'), json_extract(?5, '$.sender')) \
                         AND json_extract(origin, '$.thread_id') IS json_extract(?5, '$.thread_id'))) \
                     ORDER BY received_at, id LIMIT ?4)",
                params!(subscription_id, run_id, now, i64::from(limit), same_origin),
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

    /// Hand every event of a run that has not started back to the queue, as
    /// if it had never been claimed. Used when the subscription's session is
    /// busy (the user is chatting in it): the run waits instead of being lost,
    /// and does not count against the hourly budget.
    pub async fn release_subscription_run(&self, run_id: &str) -> Result<u64> {
        let n = self
            .conn
            .execute(
                "UPDATE subscription_events SET status = 'pending', run_id = NULL, started_at = NULL, \
                 attempts = MAX(attempts - 1, 0) WHERE run_id = ?1 AND status = 'running'",
                params!(run_id),
            )
            .await?;
        Ok(n)
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
    /// the subscription. A second finish of the same run (a watchdog and the
    /// real completion racing) changes nothing. Returns whether this call
    /// closed the run.
    pub async fn finish_subscription_run(
        &self,
        subscription_id: &str,
        run_id: &str,
        final_status: &str,
        result: Option<&str>,
        error: Option<&str>,
        duration_ms: i64,
    ) -> Result<bool> {
        let now = now_unix();
        let closed = self
            .conn
            .execute(
                "UPDATE subscription_events SET status = ?2, result = ?3, error = ?4, \
                 finished_at = ?5, duration_ms = ?6 WHERE run_id = ?1 AND status = 'running'",
                params!(run_id, final_status, result, error, now, duration_ms),
            )
            .await?;
        if closed == 0 {
            return Ok(false);
        }
        // A storm's reason outlives the run that was in flight when it hit.
        self.conn
            .execute(
                "UPDATE subscriptions SET last_run_status = ?2, \
                 last_error = CASE WHEN status = 'storm' THEN last_error \
                                   WHEN ?2 = 'failed' THEN ?3 ELSE NULL END \
                 WHERE id = ?1",
                params!(subscription_id, final_status, error),
            )
            .await?;
        Ok(true)
    }

    /// Pause an active subscription that keeps failing. Returns whether this
    /// call paused it: one already paused (by the user, or by the ingest
    /// breaker while the run was in flight) keeps its reason, and the caller
    /// announces nothing twice.
    pub async fn trip_subscription_storm(&self, id: &str, reason: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE subscriptions SET status = ?2, last_error = ?3, updated_at = ?4 \
                 WHERE id = ?1 AND status = ?5",
                params!(id, status::STORM, reason, now_unix(), status::ACTIVE),
            )
            .await?;
        Ok(n > 0)
    }

    /// The statuses of the newest `n` runs of a subscription since it last
    /// became active, newest first. Runs before a resume do not count: the
    /// user fixed something and starts from a clean slate.
    pub async fn recent_subscription_run_statuses(
        &self,
        subscription_id: &str,
        n: u32,
    ) -> Result<Vec<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT CASE WHEN SUM(status = 'failed') > 0 THEN 'failed' ELSE MAX(status) END \
                 FROM subscription_events \
                 WHERE subscription_id = ?1 AND run_id IS NOT NULL AND finished_at IS NOT NULL \
                 AND started_at >= (SELECT accepting_since FROM subscriptions WHERE id = ?1) \
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

    /// Runs of one subscription started since `since` ("runs today").
    pub async fn subscription_runs_of_since(
        &self,
        subscription_id: &str,
        since: i64,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(DISTINCT run_id) FROM subscription_events \
                 WHERE subscription_id = ?1 AND run_id IS NOT NULL AND started_at > ?2",
                params!(subscription_id, since),
            )
            .await?;
        Ok(match rows.next().await? {
            Some(r) => r.get(0)?,
            None => 0,
        })
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

    /// Run once at boot for events whose run the daemon's stop cut short.
    /// They run again, unless they were already claimed `max_attempts`
    /// times: then they close as failed, because an event that stops the
    /// daemon each time it runs would otherwise restart it in a loop. The
    /// failed run keeps its id and start time, so the run budgets and the
    /// failure breaker count it. Returns `(requeued, abandoned)`.
    pub async fn reset_running_subscription_events(&self, max_attempts: i64) -> Result<(u64, u64)> {
        let abandoned = self
            .conn
            .execute(
                "UPDATE subscription_events SET status = 'failed', finished_at = ?2, error = ?3 \
                 WHERE status = 'running' AND attempts >= ?1",
                params!(
                    max_attempts,
                    now_unix(),
                    "Borg stopped each time this event ran, so it will not run again."
                ),
            )
            .await?;
        let requeued = self
            .conn
            .execute(
                "UPDATE subscription_events SET status = 'pending', run_id = NULL, started_at = NULL \
                 WHERE status = 'running'",
                (),
            )
            .await?;
        Ok((requeued, abandoned))
    }

    /// Bound the history: the newest `keep` finished events stay whole;
    /// older ones lose their payload, then go.
    ///
    /// Older finished events that ran stay a day, as small rows, because the
    /// hourly and daily run budgets and the failure breaker count them; age
    /// is read from when they ran, not when they arrived, so a backlog
    /// drained after a pause still counts. Events that never ran (refused
    /// as queue full) only matter to the ten-minute storm window. Waiting and
    /// running events are never touched.
    pub async fn prune_subscription_events(
        &self,
        subscription_id: &str,
        keep: u32,
        now: i64,
    ) -> Result<u64> {
        const NEWEST: &str = "SELECT id FROM subscription_events WHERE subscription_id = ?1 \
             AND status NOT IN ('pending', 'running') ORDER BY received_at DESC, id DESC LIMIT ?2";
        let deleted = self
            .conn
            .execute(
                &format!(
                    "DELETE FROM subscription_events WHERE subscription_id = ?1 \
                     AND status NOT IN ('pending', 'running') AND id NOT IN ({NEWEST}) \
                     AND ((run_id IS NULL AND received_at < ?3) \
                          OR COALESCE(finished_at, started_at, received_at) < ?4)"
                ),
                params!(
                    subscription_id,
                    i64::from(keep),
                    now - STORM_WINDOW_SECS,
                    now - PRUNE_MIN_AGE_SECS
                ),
            )
            .await?;
        self.conn
            .execute(
                &format!(
                    "UPDATE subscription_events SET payload = '{{}}', payload_truncated = 1 \
                     WHERE subscription_id = ?1 AND status NOT IN ('pending', 'running') \
                     AND payload != '{{}}' AND id NOT IN ({NEWEST})"
                ),
                params!(subscription_id, i64::from(keep)),
            )
            .await?;
        Ok(deleted)
    }

    /// Add `n` events refused before the ledger (the endpoint's in-memory
    /// gate, a storm) to the dropped counter.
    pub async fn add_subscription_dropped(&self, id: &str, n: i64) -> Result<()> {
        if n <= 0 {
            return Ok(());
        }
        self.conn
            .execute(
                "UPDATE subscriptions SET dropped_count = dropped_count + ?2 WHERE id = ?1",
                params!(id, n),
            )
            .await?;
        Ok(())
    }

    /// Record a problem the user must fix (an unreadable filter) where the
    /// app shows it.
    pub async fn set_subscription_error(&self, id: &str, error: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE subscriptions SET last_error = ?2 WHERE id = ?1",
                params!(id, error),
            )
            .await?;
        Ok(())
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

    async fn sub(db: &Database, status: &'static str) -> SubscriptionRow {
        db.create_subscription(&new_sub(status)).await.unwrap()
    }

    fn new_sub(status: &'static str) -> NewSubscription<'static> {
        NewSubscription {
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
        }
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
    async fn an_event_that_stops_the_daemon_each_time_runs_at_most_three_times() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        db.insert_subscription_event(&event(&s.id, Some("crash"), 10), LIMITS)
            .await
            .unwrap();
        for boot in 1..=3 {
            let run = format!("run_{boot}");
            let claimed = db
                .claim_next_subscription_event(&run, 100 + boot)
                .await
                .unwrap();
            assert!(claimed.is_some(), "boot {boot} runs it again");
            // The daemon dies mid-run; the next boot resets.
            let (requeued, abandoned) = db.reset_running_subscription_events(3).await.unwrap();
            if boot < 3 {
                assert_eq!((requeued, abandoned), (1, 0), "boot {boot}");
            } else {
                assert_eq!((requeued, abandoned), (0, 1), "the third crash closes it");
            }
        }
        assert!(db
            .claim_next_subscription_event("run_4", 200)
            .await
            .unwrap()
            .is_none());
        let rows = db.list_subscription_events(&s.id, 10).await.unwrap();
        assert_eq!(rows[0].status, "failed");
        assert!(
            rows[0].run_id.is_some(),
            "the breaker and budgets still count it"
        );
        assert_eq!(
            db.recent_subscription_run_statuses(&s.id, 5).await.unwrap(),
            vec!["failed"]
        );
    }

    #[tokio::test]
    async fn a_skipped_subscription_does_not_hold_up_the_others() {
        let db = Database::test_db().await;
        let busy = sub(&db, status::ACTIVE).await;
        let free = sub(&db, status::ACTIVE).await;
        db.insert_subscription_event(&event(&busy.id, Some("old"), 10), LIMITS)
            .await
            .unwrap();
        db.insert_subscription_event(&event(&free.id, Some("new"), 20), LIMITS)
            .await
            .unwrap();
        let lead = db
            .claim_next_subscription_event_except("run_1", 100, std::slice::from_ref(&busy.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            lead.subscription_id, free.id,
            "the older busy one is passed over"
        );
    }

    #[tokio::test]
    async fn a_busy_session_hands_events_back_without_using_an_attempt() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        db.insert_subscription_event(&event(&s.id, Some("e"), 10), LIMITS)
            .await
            .unwrap();
        for i in 0..5 {
            let run = format!("busy_{i}");
            db.claim_next_subscription_event(&run, 100 + i)
                .await
                .unwrap()
                .unwrap();
            db.release_subscription_run(&run).await.unwrap();
        }
        db.claim_next_subscription_event("real", 200)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            db.reset_running_subscription_events(3).await.unwrap(),
            (1, 0),
            "releases are not crashes"
        );
    }

    #[tokio::test]
    async fn an_origin_digest_keeps_to_one_conversation() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let thread_a = r#"{"channel":"slack","sender":"U1","chat":"C1","thread_id":"1.0"}"#;
        let thread_a_other_sender =
            r#"{"channel":"slack","sender":"U2","chat":"C1","thread_id":"1.0"}"#;
        let thread_b = r#"{"channel":"slack","sender":"U1","chat":"C1","thread_id":"2.0"}"#;
        let other_chat = r#"{"channel":"slack","sender":"U1","chat":"C2","thread_id":"1.0"}"#;
        for (i, origin) in [thread_a, thread_b, thread_a_other_sender, other_chat]
            .into_iter()
            .enumerate()
        {
            let key = format!("o{i}");
            let ev = NewSubscriptionEvent {
                origin: Some(origin),
                ..event(&s.id, Some(&key), 10 + i as i64)
            };
            db.insert_subscription_event(&ev, LIMITS).await.unwrap();
        }
        let lead = db
            .claim_next_subscription_event("run_1", 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lead.origin.as_deref(), Some(thread_a));
        let run = db
            .claim_subscription_batch(&s.id, "run_1", 50, 100, lead.origin.as_deref())
            .await
            .unwrap();
        let origins: Vec<_> = run.iter().filter_map(|e| e.origin.as_deref()).collect();
        assert_eq!(origins, vec![thread_a, thread_a_other_sender]);

        let all = db
            .claim_subscription_batch(&s.id, "run_2", 50, 100, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "without an origin the rest digest together");
    }

    #[tokio::test]
    async fn the_failure_breaker_forgets_failures_before_a_resume_and_trips_once() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let now = chrono::Utc::now().timestamp();
        let fail = |i: i64| {
            let db = &db;
            let id = s.id.clone();
            async move {
                let key = format!("f{i}");
                db.insert_subscription_event(&event(&id, Some(&key), now + i), LIMITS)
                    .await
                    .unwrap();
                let run = format!("run_{i}");
                db.claim_next_subscription_event(&run, now + i)
                    .await
                    .unwrap()
                    .unwrap();
                db.finish_subscription_run(&id, &run, "failed", None, Some("x"), 1)
                    .await
                    .unwrap();
            }
        };
        fail(0).await;
        fail(1).await;
        db.set_subscription_status(&s.id, status::PAUSED, None)
            .await
            .unwrap();
        // Resume a moment later, so the earlier runs started before it.
        db.conn
            .execute(
                "UPDATE subscriptions SET status = 'paused' WHERE id = ?1",
                params!(s.id.clone()),
            )
            .await
            .unwrap();
        db.set_subscription_status(&s.id, status::ACTIVE, None)
            .await
            .unwrap();
        db.conn
            .execute(
                "UPDATE subscriptions SET accepting_since = ?2 WHERE id = ?1",
                params!(s.id.clone(), now + 2),
            )
            .await
            .unwrap();
        fail(3).await;
        assert_eq!(
            db.recent_subscription_run_statuses(&s.id, 5).await.unwrap(),
            vec!["failed"],
            "only the run after the resume counts"
        );

        assert!(db.trip_subscription_storm(&s.id, "first").await.unwrap());
        assert!(!db.trip_subscription_storm(&s.id, "second").await.unwrap());
        let row = db.get_subscription(&s.id).await.unwrap().unwrap();
        assert_eq!(row.status, "storm");
        assert_eq!(row.last_error.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn crud_round_trip_and_partial_update() {
        let db = Database::test_db().await;
        let s = sub(&db, status::PAUSED).await;
        assert!(s.id.starts_with("sub_"));
        assert_eq!(s.status, "paused");
        assert_eq!(s.dropped_count, 0);
        assert_eq!(s.last_event_at, None);

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
            .claim_subscription_batch(&a.id, "run_1", 50, now, None)
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
    async fn boot_recovery_puts_running_events_back() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        for i in 0..3 {
            let key = format!("p{i}");
            db.insert_subscription_event(&event(&s.id, Some(&key), 10 + i), LIMITS)
                .await
                .unwrap();
        }
        db.claim_next_subscription_event("run_1", 100)
            .await
            .unwrap()
            .unwrap();
        db.claim_subscription_batch(&s.id, "run_1", 50, 100, None)
            .await
            .unwrap();
        assert_eq!(
            db.reset_running_subscription_events(3).await.unwrap(),
            (3, 0)
        );
        let again = db
            .claim_next_subscription_event("run_2", 200)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            again.run_id.as_deref(),
            Some("run_2"),
            "a reset event runs again"
        );
    }

    #[tokio::test]
    async fn a_released_run_waits_and_does_not_use_the_hourly_budget() {
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
        db.insert_subscription_event(&event(&s.id, Some("a"), 10), LIMITS)
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        db.claim_next_subscription_event("run_1", now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(db.release_subscription_run("run_1").await.unwrap(), 1);
        let again = db
            .claim_next_subscription_event("run_2", now)
            .await
            .unwrap();
        assert_eq!(
            again.and_then(|e| e.run_id).as_deref(),
            Some("run_2"),
            "a released run is not a run: the budget of one still allows this claim"
        );
    }

    /// Pruning bounds the history without breaking the budgets: rows that
    /// ran stay a day (aged by when they ran), refused rows only the storm
    /// window, waiting rows always; past the newest `keep`, payloads go.
    #[tokio::test]
    async fn pruning_bounds_the_history_and_keeps_what_the_limits_count() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let day = 24 * 3600;
        let now = chrono::Utc::now().timestamp();
        let big = r#"{"blob":"xxxxxxxx"}"#;
        let finish = |key: &'static str, at: i64| {
            let db = &db;
            let id = s.id.clone();
            async move {
                db.insert_subscription_event(
                    &NewSubscriptionEvent {
                        payload: big,
                        ..event(&id, Some(key), at)
                    },
                    LIMITS,
                )
                .await
                .unwrap();
                let run = format!("run_{key}");
                db.claim_next_subscription_event(&run, at)
                    .await
                    .unwrap()
                    .unwrap();
                db.finish_subscription_run(&id, &run, "succeeded", None, None, 1)
                    .await
                    .unwrap();
                run
            }
        };
        // Ran three days ago.
        for key in ["old1", "old2", "old3"] {
            let run = finish(key, now - 3 * day).await;
            db.conn
                .execute(
                    "UPDATE subscription_events SET finished_at = ?2 WHERE run_id = ?1",
                    params!(run, now - 3 * day),
                )
                .await
                .unwrap();
        }
        // Arrived three days ago, ran just now (a backlog drained after a
        // pause): still counted by the budgets, so it stays.
        finish("backlog", now - 3 * day).await;
        finish("recent", now - 60).await;
        finish("newest", now - 30).await;
        // Refused as queue full twenty minutes ago.
        let full = IngestLimits {
            max_pending: 0,
            ..LIMITS
        };
        for key in ["full1", "full2"] {
            db.insert_subscription_event(&event(&s.id, Some(key), now - 1200), full)
                .await
                .unwrap();
        }
        for key in ["wait1", "wait2"] {
            db.insert_subscription_event(&event(&s.id, Some(key), now - 4 * day), LIMITS)
                .await
                .unwrap();
        }

        let pruned = db.prune_subscription_events(&s.id, 1, now).await.unwrap();
        assert_eq!(pruned, 5, "three old runs and two refusals go");
        let left = db.list_subscription_events(&s.id, 50).await.unwrap();
        let summary: Vec<_> = left
            .iter()
            .map(|e| (e.dedup_key.clone().unwrap_or_default(), e.payload.clone()))
            .collect();
        assert_eq!(
            left.iter().filter(|e| e.status == "pending").count(),
            2,
            "{summary:?}"
        );
        assert_eq!(
            left.iter().filter(|e| e.status == "succeeded").count(),
            3,
            "{summary:?}"
        );
        for e in left.iter().filter(|e| e.status == "succeeded") {
            let whole = e.dedup_key.as_deref() == Some("newest");
            assert_eq!(
                e.payload == big,
                whole,
                "only the newest keeps its payload: {summary:?}"
            );
        }
    }

    #[tokio::test]
    async fn resume_after_a_storm_starts_the_windows_again() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let limits = IngestLimits {
            per_minute: 100,
            storm_threshold: 5,
            max_pending: 100,
        };
        let t = chrono::Utc::now().timestamp() - 60;
        for i in 0..6 {
            let key = format!("s{i}");
            db.insert_subscription_event(&event(&s.id, Some(&key), t), limits)
                .await
                .unwrap();
        }
        assert_eq!(
            db.get_subscription(&s.id).await.unwrap().unwrap().status,
            "storm"
        );

        // Resume a minute later, while the five events are still in the
        // ten-minute window.
        db.set_subscription_status(&s.id, status::ACTIVE, None)
            .await
            .unwrap();
        let resumed_at = db
            .get_subscription(&s.id)
            .await
            .unwrap()
            .unwrap()
            .updated_at;
        let next = db
            .insert_subscription_event(&event(&s.id, Some("after"), resumed_at + 1), limits)
            .await
            .unwrap();
        assert!(matches!(next, InsertOutcome::Inserted(_)), "got {next:?}");
    }

    #[tokio::test]
    async fn enabling_an_active_subscription_again_keeps_the_burst_windows() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let limits = IngestLimits {
            per_minute: 3,
            storm_threshold: 100,
            max_pending: 100,
        };
        let t = chrono::Utc::now().timestamp();
        for i in 0..3 {
            let key = format!("k{i}");
            db.insert_subscription_event(&event(&s.id, Some(&key), t), limits)
                .await
                .unwrap();
        }
        // An agent or a double click enables it again mid-burst.
        db.set_subscription_status(&s.id, status::ACTIVE, None)
            .await
            .unwrap();
        let next = db
            .insert_subscription_event(&event(&s.id, Some("k3"), t + 1), limits)
            .await
            .unwrap();
        assert!(matches!(next, InsertOutcome::RateLimited), "got {next:?}");
    }

    #[tokio::test]
    async fn a_filter_the_endpoint_cannot_read_is_refused_on_save() {
        let db = Database::test_db().await;
        let bad = NewSubscription {
            filter: r#"{"severity":"error"}"#,
            ..new_sub(status::PAUSED)
        };
        assert!(db.create_subscription(&bad).await.is_err());
        let s = sub(&db, status::PAUSED).await;
        let update = SubscriptionUpdate {
            filter: Some(r#"{"drafts":true}"#.into()),
            ..Default::default()
        };
        assert!(db.update_subscription(&s.id, &update).await.is_err());
        assert_eq!(
            db.get_subscription(&s.id).await.unwrap().unwrap().filter,
            "{}"
        );
    }

    #[tokio::test]
    async fn a_retry_after_queue_full_is_accepted_once_there_is_room() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        let limits = IngestLimits {
            per_minute: 100,
            storm_threshold: 100,
            max_pending: 1,
        };
        db.insert_subscription_event(&event(&s.id, Some("a"), 10), limits)
            .await
            .unwrap();
        let full = db
            .insert_subscription_event(&event(&s.id, Some("b"), 10), limits)
            .await
            .unwrap();
        assert!(matches!(full, InsertOutcome::QueueFull(_)));

        db.claim_next_subscription_event("run_1", 20)
            .await
            .unwrap()
            .unwrap();
        db.finish_subscription_run(&s.id, "run_1", "succeeded", None, None, 1)
            .await
            .unwrap();
        let retry = db
            .insert_subscription_event(&event(&s.id, Some("b"), 30), limits)
            .await
            .unwrap();
        assert!(
            matches!(retry, InsertOutcome::Inserted(_)),
            "the retry is not a duplicate: {retry:?}"
        );
    }

    #[tokio::test]
    async fn a_second_finish_of_the_same_run_changes_nothing() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        db.insert_subscription_event(&event(&s.id, Some("a"), 10), LIMITS)
            .await
            .unwrap();
        db.claim_next_subscription_event("run_1", 20)
            .await
            .unwrap()
            .unwrap();
        assert!(db
            .finish_subscription_run(&s.id, "run_1", "failed", None, Some("timed out"), 1)
            .await
            .unwrap());
        assert!(!db
            .finish_subscription_run(&s.id, "run_1", "succeeded", Some("late"), None, 1)
            .await
            .unwrap());
        let row = db.get_subscription(&s.id).await.unwrap().unwrap();
        assert_eq!(row.last_run_status.as_deref(), Some("failed"));
        assert_eq!(row.last_error.as_deref(), Some("timed out"));
    }

    #[tokio::test]
    async fn a_storm_reason_survives_the_run_that_was_in_flight() {
        let db = Database::test_db().await;
        let s = sub(&db, status::ACTIVE).await;
        db.insert_subscription_event(&event(&s.id, Some("a"), 10), LIMITS)
            .await
            .unwrap();
        db.claim_next_subscription_event("run_1", 20)
            .await
            .unwrap()
            .unwrap();
        db.set_subscription_status(&s.id, status::STORM, Some("Paused itself: burst"))
            .await
            .unwrap();
        db.finish_subscription_run(&s.id, "run_1", "failed", None, Some("timed out"), 1)
            .await
            .unwrap();
        let row = db.get_subscription(&s.id).await.unwrap().unwrap();
        assert_eq!(row.last_error.as_deref(), Some("Paused itself: burst"));
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
