//! Persist posture transitions.
//!
//! Two writes per real transition:
//! 1. A row into `posture_snapshots` (denormalized index, fast head lookup
//!    for `GetPosture` / `/evolution`).
//! 2. A `security_posture_changed` row into `audit_log_chain` carrying
//!    the full `PostureSnapshot` JSON (forensic tamper-evident record).
//!
//! Rate limit: **2 transitions per hour**. Excess writes are dropped — the
//! prior posture continues for any `xp_gain` written in the dropped
//! window. This kills the "flip-flop Boosted ↔ Reduced timed with
//! high-XP actions" attack at one effective transition every 30 minutes.
//!
//! Posture events land in the audit chain only; `posture_snapshots.evolution_event_id`
//! stores the `audit_log_chain.seq` so the cross-reference is unambiguous
//! (every `posture_snapshots` row points at the audit event that created it).
//! Wiring posture into `evolution_events` (HMAC-chained alongside `xp_gain`)
//! for full replay determinism is a follow-up.
//!
//! TODO: wire `xp_gain` events to read the head of `posture_snapshots`,
//! apply `compute_xp_breakdown(...)` from `super::compute_xp_breakdown`,
//! and persist the resulting `XpBreakdown` into
//! `evolution_events.metadata_json` with `posture_event_seq` set to the
//! current `posture_snapshots.evolution_event_id`. Until that lands,
//! `Status::GetPosture` shows live posture but XP accrual is unmultiplied.

use anyhow::{anyhow, Context, Result};
use libsql::params;

#[cfg(all(test, feature = "phase9-tests-disabled"))]
use super::PostureSnapshot;
use super::{compute_live_snapshot, PostureSource, PostureState};
use crate::config::Config;
use crate::db::Database;

/// Ceiling for posture transitions per rolling hour. Per the locked
/// decision in `security-posture.md`. Tests can rely on this value
/// staying at 2 — relaxing the limit re-opens the flip-flop attack.
pub const POSTURE_RATE_LIMIT_PER_HOUR: i64 = 2;

use crate::constants::SECS_PER_HOUR as RATE_WINDOW_SECS;

/// Outcome of a [`maybe_emit_posture_change`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitOutcome {
    /// Live posture matches the current head — nothing written.
    Unchanged,
    /// 2/hr rate limit kicked in; the would-be transition was dropped.
    RateLimited {
        /// `PostureState` we tried to transition to.
        intended: PostureState,
    },
    /// New row written. `posture_snapshots.id` of the inserted row.
    Emitted {
        snapshot_id: i64,
        state: PostureState,
        previous: Option<PostureState>,
    },
}

/// Read the most recent posture from `posture_snapshots`. Returns `None`
/// on a fresh DB.
pub async fn current_posture(db: &Database) -> Result<Option<PostureState>> {
    let mut rows = db
        .conn()
        .query(
            "SELECT posture FROM posture_snapshots
             ORDER BY taken_at DESC, id DESC LIMIT 1",
            (),
        )
        .await?;
    let s: Option<String> = match rows.next().await? {
        Some(r) => Some(r.get::<String>(0)?),
        None => None,
    };
    Ok(s.and_then(|s| PostureState::parse(&s)))
}

/// Compute live posture; if it differs from the head of `posture_snapshots`,
/// append a `security_posture_changed` audit event and a `posture_snapshots`
/// index row. Subject to the 2/hr rate limit.
///
/// On daemon startup, call with `PostureSource::DaemonStartReconcile`. On a
/// settings or listener change, call with the matching source — the source
/// is recorded for forensics but does not affect the resolution outcome.
pub async fn maybe_emit_posture_change(
    db: &Database,
    config: &Config,
    source: PostureSource,
) -> Result<EmitOutcome> {
    let snap = compute_live_snapshot(config, db, source).await?;
    let prior = current_posture(db).await?;
    if Some(snap.state) == prior {
        return Ok(EmitOutcome::Unchanged);
    }
    if recent_transitions_count(db, snap.computed_at).await? >= POSTURE_RATE_LIMIT_PER_HOUR {
        tracing::warn!(
            "posture: rate-limited (>= {POSTURE_RATE_LIMIT_PER_HOUR}/hr) — \
             dropping transition {prior:?} → {state:?}",
            state = snap.state,
        );
        return Ok(EmitOutcome::RateLimited {
            intended: snap.state,
        });
    }

    let payload = serde_json::to_string(&snap).context("serialize PostureSnapshot")?;
    let audit_seq = db
        .append_audit_event(
            "security_posture_changed",
            &payload,
            "system",
            snap.computed_at,
        )
        .await
        .context("append posture audit event")?;

    let mut rows = db
        .conn()
        .query(
            "INSERT INTO posture_snapshots
             (evolution_event_id, taken_at, posture, source, schema_version)
             VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id",
            params!(
                audit_seq,
                snap.computed_at,
                snap.state.as_str(),
                posture_source_str(source),
                snap.schema_version,
            ),
        )
        .await
        .context("insert posture_snapshots row")?;
    let snapshot_id: i64 = rows
        .next()
        .await?
        .ok_or_else(|| anyhow!("INSERT ... RETURNING returned no row"))?
        .get::<i64>(0)?;

    tracing::info!(
        "posture: {prior:?} → {state:?} (source={src:?}, audit_seq={audit_seq})",
        state = snap.state,
        src = source,
    );
    Ok(EmitOutcome::Emitted {
        snapshot_id,
        state: snap.state,
        previous: prior,
    })
}

async fn recent_transitions_count(db: &Database, now: i64) -> Result<i64> {
    let cutoff = now - RATE_WINDOW_SECS;
    let mut rows = db
        .conn()
        .query(
            "SELECT COUNT(*) FROM posture_snapshots WHERE taken_at >= ?1",
            params!(cutoff),
        )
        .await?;
    let n: i64 = match rows.next().await? {
        Some(r) => r.get::<i64>(0)?,
        None => 0,
    };
    Ok(n)
}

fn posture_source_str(s: PostureSource) -> &'static str {
    match s {
        PostureSource::ConfigChange => "ConfigChange",
        PostureSource::ListenerBind => "ListenerBind",
        PostureSource::DaemonStartReconcile => "DaemonStartReconcile",
    }
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;
    use crate::db::AuditChainOutcome;

    /// First call on a fresh DB writes a single bootstrap event; second
    /// call (no config change) is a no-op. Regression target: emitting on
    /// every startup without a same-state guard would explode the audit
    /// chain in restart loops.
    #[test]
    fn first_call_writes_bootstrap_second_is_noop() {
        let db = Database::test_db();
        let cfg = Config::default();

        let first = maybe_emit_posture_change(&db, &cfg, PostureSource::DaemonStartReconcile)
            .expect("first emit");
        match first {
            EmitOutcome::Emitted {
                state, previous, ..
            } => {
                assert_eq!(state, PostureState::Default);
                assert!(previous.is_none());
            }
            other => panic!("expected Emitted, got {other:?}"),
        }

        let second = maybe_emit_posture_change(&db, &cfg, PostureSource::DaemonStartReconcile)
            .expect("second emit");
        assert_eq!(second, EmitOutcome::Unchanged);

        // Only one row in `posture_snapshots` and one event in the audit
        // chain. Audit chain still verifies clean.
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM posture_snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        let audit_n: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM audit_log_chain WHERE event_type = 'security_posture_changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audit_n, 1);
        assert_eq!(db.verify_audit_chain().unwrap(), AuditChainOutcome::Ok);
    }

    /// Real transitions write events; same posture is dedup'd; the audit
    /// chain is hash-clean across the whole sequence.
    #[test]
    fn detects_default_to_boosted_transition() {
        let db = Database::test_db();
        let cfg = Config::default();
        // Bootstrap.
        maybe_emit_posture_change(&db, &cfg, PostureSource::DaemonStartReconcile).unwrap();

        // Pair a device → live posture should resolve to Boosted.
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-1", "alice", 0, 1_000_000)
            .unwrap();
        db.insert_paired_device("fp-1", "alice-laptop", "{}", "principal-1", serial)
            .unwrap();

        let r = maybe_emit_posture_change(&db, &cfg, PostureSource::ListenerBind).unwrap();
        match r {
            EmitOutcome::Emitted {
                state, previous, ..
            } => {
                assert_eq!(state, PostureState::Boosted);
                assert_eq!(previous, Some(PostureState::Default));
            }
            other => panic!("expected Emitted(Boosted), got {other:?}"),
        }
        assert_eq!(current_posture(&db).unwrap(), Some(PostureState::Boosted));
        assert_eq!(db.verify_audit_chain().unwrap(), AuditChainOutcome::Ok);
    }

    /// Three transitions within an hour → third is rate-limited and
    /// posture stays at the second one. Anti-flip-flop guard.
    #[test]
    fn rate_limit_drops_third_transition_within_hour() {
        let db = Database::test_db();
        let cfg = Config::default();
        // Synthesize 2 transitions by inserting fixture rows directly:
        // "Default" at t-100, "Reduced" at t-50.
        let now = chrono::Utc::now().timestamp();
        db.append_audit_event(
            "security_posture_changed",
            r#"{"state":"Default"}"#,
            "system",
            now - 100,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO posture_snapshots
                 (evolution_event_id, taken_at, posture, source, schema_version)
                 VALUES (1, ?1, 'Default', 'DaemonStartReconcile', 1)",
                params![now - 100],
            )
            .unwrap();
        db.append_audit_event(
            "security_posture_changed",
            r#"{"state":"Reduced"}"#,
            "system",
            now - 50,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO posture_snapshots
                 (evolution_event_id, taken_at, posture, source, schema_version)
                 VALUES (2, ?1, 'Reduced', 'ConfigChange', 1)",
                params![now - 50],
            )
            .unwrap();

        // Now trigger a third transition by switching live config to
        // "Default" (sandbox.enabled is true by default, so the live
        // config resolves to Default — different from current 'Reduced'
        // head → would emit, but rate-limited).
        let r = maybe_emit_posture_change(&db, &cfg, PostureSource::ConfigChange).unwrap();
        assert_eq!(
            r,
            EmitOutcome::RateLimited {
                intended: PostureState::Default
            }
        );
        // Head stays at the rate-limited prior.
        assert_eq!(current_posture(&db).unwrap(), Some(PostureState::Reduced));
    }

    /// `current_posture` returns None on a fresh DB — important because
    /// `xp_gain` writes use this to decide whether to fall back to
    /// `PostureUndetermined`.
    #[test]
    fn current_posture_is_none_on_fresh_db() {
        let db = Database::test_db();
        assert!(current_posture(&db).unwrap().is_none());
    }

    /// Audit event payload is the full `PostureSnapshot` JSON — round-trip
    /// parses back into the same enum value. Deserialization regression
    /// guard: any rename of the enum or field breaks this.
    #[test]
    fn audit_event_payload_round_trips() {
        let db = Database::test_db();
        let cfg = Config::default();
        maybe_emit_posture_change(&db, &cfg, PostureSource::DaemonStartReconcile).unwrap();

        let row: (String, String) = db
            .conn()
            .query_row(
                "SELECT event_type, event_json FROM audit_log_chain
                 WHERE event_type = 'security_posture_changed'
                 ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "security_posture_changed");
        let snap: PostureSnapshot = serde_json::from_str(&row.1).expect("parse snapshot");
        assert_eq!(snap.state, PostureState::Default);
        assert_eq!(snap.computed_from, PostureSource::DaemonStartReconcile);
    }
}
