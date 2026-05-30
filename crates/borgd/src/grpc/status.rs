//! Status service — read-only views of agent health.
//!
//! Backed by the same data the in-process `borg status` and TUI status popup
//! read today: vitals events (HMAC-replayed), evolution chain, posture
//! config. Budget tracking surfaces the configured monthly token limit; the
//! per-day USD spend lands when the cost-tracking pipeline is wired into the
//! daemon.

use cortex::config::Config;
use cortex::db::Database;
use protocols::status::{
    status_server::Status, BondEvent as PbBondEvent, BondEventList, BondStateProto, Budget, Empty,
    EvolutionEvent as PbEvolutionEvent, EvolutionEventList, EvolutionStateProto, Heartbeat,
    Int64Cursor, Posture, StatusText, StatusTextRequest, UsageRequest, Vitals,
    VitalsEvent as PbVitalsEvent, VitalsEventList, XpEvent, XpSnapshot,
};
use tonic::{Request, Response, Status as TStatus};

/// Status service implementation.
pub struct StatusSvc;

impl StatusSvc {
    /// Stateless Status RPC implementation.
    pub fn new() -> Self {
        Self
    }
}

impl Default for StatusSvc {
    fn default() -> Self {
        Self::new()
    }
}

fn drift_notice_for_vitals(
    raw: &cortex::vitals::VitalsState,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let decayed = cortex::vitals::apply_decay(raw, now);
    let drift = cortex::vitals::detect_drift(&decayed, now);
    cortex::vitals::format_drift_notice(&drift).unwrap_or_default()
}

#[tonic::async_trait]
impl Status for StatusSvc {
    async fn get_vitals(&self, _req: Request<Empty>) -> Result<Response<Vitals>, TStatus> {
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("vitals load failed: {e}")))?;
        let raw = db
            .get_vitals_state()
            .await
            .map_err(|e| TStatus::internal(format!("vitals load failed: {e}")))?;
        let now = chrono::Utc::now();
        let state = cortex::vitals::apply_decay(&raw, now);
        let drift_notice = drift_notice_for_vitals(&raw, now);
        let evo = match db.get_evolution_state().await {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "evolution state load failed");
                None
            }
        };
        let (stage, xp) = match evo {
            Some(e) => {
                let stage = match e.stage {
                    cortex::evolution::Stage::Base => "Base",
                    cortex::evolution::Stage::Evolved => "Evolved",
                    cortex::evolution::Stage::Final => "Final",
                };
                (stage, e.total_xp as u64)
            }
            None => ("Base", 0),
        };
        Ok(Response::new(Vitals {
            stability: state.stability as u32,
            focus: state.focus as u32,
            sync: state.sync as u32,
            growth: state.growth as u32,
            happiness: state.happiness as u32,
            stage: stage.into(),
            xp,
            drift_notice,
        }))
    }

    async fn get_posture(&self, _req: Request<Empty>) -> Result<Response<Posture>, TStatus> {
        // Read the head of `posture_snapshots` (denormalized index over the
        // `security_posture_changed` audit chain). On a fresh DB with no
        // posture event yet, fall back to `Default × 1.00` rather than
        // fabricating a state — `xp_gain` writes use the same fallback path
        // (FallbackReason::PostureUndetermined).
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("posture load failed: {e}")))?;
        let head = cortex::evolution::security::current_posture(&db)
            .await
            .map_err(|e| TStatus::internal(format!("posture load failed: {e}")))?;
        let state = head.unwrap_or(cortex::evolution::security::PostureState::Default);
        Ok(Response::new(Posture {
            posture: state.as_str().into(),
            xp_multiplier: state.xp_multiplier() as f64,
        }))
    }

    async fn get_budget(&self, _req: Request<Empty>) -> Result<Response<Budget>, TStatus> {
        // Today's `BudgetConfig` is a *monthly token* limit, not a daily USD
        // cap; map it onto the wire field as-is and leave spent at 0 until
        // cost tracking is wired. Renaming the proto field is a wire-break
        // saved for the same change.
        let cfg = Config::load_from_db()
            .await
            .map_err(|e| TStatus::internal(format!("config load failed: {e}")))?;
        Ok(Response::new(Budget {
            daily_usd_cap: cfg.budget.monthly_token_limit as f64,
            daily_usd_spent: 0.0,
        }))
    }

    async fn get_evolution(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<EvolutionStateProto>, TStatus> {
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("evolution load failed: {e}")))?;
        let state = db
            .get_evolution_state()
            .await
            .map_err(|e| TStatus::internal(format!("evolution load failed: {e}")))?;
        let archetype_scores = state
            .archetype_scores
            .iter()
            .map(|(a, v)| (a.to_string(), *v as f64))
            .collect();
        Ok(Response::new(EvolutionStateProto {
            stage: state.stage.to_string(),
            total_xp: state.total_xp as u64,
            name: state.evolution_name.unwrap_or_default(),
            level: state.level as u32,
            archetype: state
                .dominant_archetype
                .map(|a| a.to_string())
                .unwrap_or_default(),
            archetype_scores,
        }))
    }

    async fn get_bond_state(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<BondStateProto>, TStatus> {
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("bond load failed: {e}")))?;
        let events = db
            .get_all_bond_events()
            .await
            .map_err(|e| TStatus::internal(format!("bond load failed: {e}")))?;
        let key = db.derive_hmac_key(cortex::bond::BOND_HMAC_DOMAIN);
        let state = cortex::bond::replay_events_with_key(&key, &events);
        let correction = cortex::bond::compute_correction_rate(&db).await;
        let routine = cortex::bond::compute_routine_success_rate(&db).await;
        let pref = cortex::bond::compute_preference_learning_count(&db).await;
        Ok(Response::new(BondStateProto {
            score: state.score as f64,
            level: state.level.to_string().to_lowercase(),
            correction_rate: correction as f64,
            routine_rate: routine as f64,
            pref_count: pref,
        }))
    }

    async fn get_bond_events_since(
        &self,
        req: Request<Int64Cursor>,
    ) -> Result<Response<BondEventList>, TStatus> {
        let since = req.into_inner().since_unix;
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("bond_events failed: {e}")))?;
        let rows = db
            .bond_events_since(since)
            .await
            .map_err(|e| TStatus::internal(format!("bond_events failed: {e}")))?;
        Ok(Response::new(BondEventList {
            events: rows
                .into_iter()
                .map(|e| PbBondEvent {
                    timestamp: e.created_at,
                    kind: e.event_type,
                    delta: e.score_delta as f64,
                    note: e.reason,
                })
                .collect(),
        }))
    }

    async fn get_evolution_events_since(
        &self,
        req: Request<Int64Cursor>,
    ) -> Result<Response<EvolutionEventList>, TStatus> {
        let since = req.into_inner().since_unix;
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("evolution_events failed: {e}")))?;
        let rows = db
            .evolution_events_since(since)
            .await
            .map_err(|e| TStatus::internal(format!("evolution_events failed: {e}")))?;
        Ok(Response::new(EvolutionEventList {
            events: rows
                .into_iter()
                .map(|e| PbEvolutionEvent {
                    timestamp: e.created_at,
                    kind: e.event_type,
                    delta: e.xp_delta as f64,
                    note: e.source,
                })
                .collect(),
        }))
    }

    async fn get_vitals_events_since(
        &self,
        req: Request<Int64Cursor>,
    ) -> Result<Response<VitalsEventList>, TStatus> {
        let since = req.into_inner().since_unix;
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("vitals_events failed: {e}")))?;
        let rows = db
            .vitals_events_since(since)
            .await
            .map_err(|e| TStatus::internal(format!("vitals_events failed: {e}")))?;
        Ok(Response::new(VitalsEventList {
            events: rows
                .into_iter()
                .map(|e| PbVitalsEvent {
                    timestamp: e.created_at,
                    category: e.category,
                    source: e.source,
                    stability_delta: e.stability_delta,
                    focus_delta: e.focus_delta,
                    sync_delta: e.sync_delta,
                    growth_delta: e.growth_delta,
                    happiness_delta: e.happiness_delta,
                })
                .collect(),
        }))
    }

    async fn get_xp(&self, _req: Request<Empty>) -> Result<Response<XpSnapshot>, TStatus> {
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("xp load failed: {e}")))?;
        let state = db
            .get_evolution_state()
            .await
            .map_err(|e| TStatus::internal(format!("xp load failed: {e}")))?;
        // Surface the same recent feed `/xp` shows today: level_up +
        // milestone_unlocked events. Cap matches the popup's window so
        // the wire response can't unboundedly grow.
        const RECENT_LIMIT: usize = 20;
        let mut recent: Vec<XpEvent> = state
            .level_up_events_recent
            .iter()
            .take(RECENT_LIMIT)
            .map(|e| XpEvent {
                timestamp: e.created_at,
                source: e.source.clone(),
                amount: e.xp_delta.max(0) as u32,
            })
            .collect();
        // Newest first.
        recent.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        // Per `level_from_xp` semantics, `xp_to_next_level` is `0` only at
        // a saturated stage cap; pass it through verbatim so the client
        // can decide how to render.
        let snap = XpSnapshot {
            total_xp: state.total_xp as u64,
            level: state.level as u32,
            next_level_xp: state.xp_to_next_level as u64,
            recent_events: recent,
        };
        Ok(Response::new(snap))
    }

    async fn get_status_overview_text(
        &self,
        req: Request<StatusTextRequest>,
    ) -> Result<Response<StatusText>, TStatus> {
        let card_width = req.into_inner().card_width;
        let cfg = Config::load_from_db().await.unwrap_or_default();
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("overview text failed: {e}")))?;
        let now = chrono::Utc::now();

        let mut sections: Vec<String> = Vec::new();
        let mut compact_line = String::new();

        let evo_state = if cfg.evolution.enabled {
            db.get_evolution_state().await.ok()
        } else {
            None
        };

        if let Some(ref evo) = evo_state {
            compact_line = cortex::evolution::format_compact(evo);
            let width = if card_width == 0 {
                48
            } else {
                card_width as usize
            };
            sections.push(cortex::evolution::format_status_section_with_width(
                evo, width,
            ));
        }

        // Vitals
        if let Ok(state) = db.get_vitals_state().await {
            let state = cortex::vitals::apply_decay(&state, now);
            let mut drift = cortex::vitals::detect_drift(&state, now);
            let since = (now - chrono::Duration::days(7)).timestamp();
            let events = db.vitals_events_since(since).await.unwrap_or_default();
            if cortex::vitals::detect_failure_drift(&events) {
                drift.push(cortex::vitals::DriftFlag::RepeatedFailures);
            }
            sections.push(cortex::vitals::format_status(&state, &events, &drift));
        }

        // Bond
        if let Ok(bond_events) = db.get_all_bond_events().await {
            let bond_key = db.derive_hmac_key(cortex::bond::BOND_HMAC_DOMAIN);
            let bond_state = cortex::bond::replay_events_with_key(&bond_key, &bond_events);
            let correction_rate = cortex::bond::compute_correction_rate(&db).await;
            let routine_rate = cortex::bond::compute_routine_success_rate(&db).await;
            let pref_count = cortex::bond::compute_preference_learning_count(&db).await;
            let since = (now - chrono::Duration::days(7)).timestamp();
            let recent = db.bond_events_since(since).await.unwrap_or_default();
            sections.push(cortex::bond::format_status(
                &bond_state,
                correction_rate,
                routine_rate,
                pref_count,
                &recent,
            ));
        }

        if let Some(ref evo) = evo_state {
            sections.push(cortex::evolution::format_archetype_scores(evo));
        }

        if cfg.evolution.enabled {
            if let Ok(mut events) = db.evolution_events_since(0).await {
                events.reverse();
                sections.push(cortex::evolution::format_history(&events));
            }
        }

        // Join with blank-line separator, mirroring `push_section`.
        let text = sections.join("\n");
        Ok(Response::new(StatusText {
            text,
            compact_class_line: compact_line,
        }))
    }

    async fn get_evolution_text(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<StatusText>, TStatus> {
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("evolution text failed: {e}")))?;
        let out = cortex::evolution::commands::dispatch(
            cortex::evolution::commands::EvolutionCommand::Evolution,
            &db,
        )
        .await
        .map_err(|e| TStatus::internal(format!("evolution text failed: {e}")))?;
        Ok(Response::new(StatusText {
            text: out.text,
            compact_class_line: String::new(),
        }))
    }

    async fn get_xp_text(&self, _req: Request<Empty>) -> Result<Response<StatusText>, TStatus> {
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("xp text failed: {e}")))?;
        let out = cortex::evolution::commands::dispatch(
            cortex::evolution::commands::EvolutionCommand::Xp,
            &db,
        )
        .await
        .map_err(|e| TStatus::internal(format!("xp text failed: {e}")))?;
        Ok(Response::new(StatusText {
            text: out.text,
            compact_class_line: String::new(),
        }))
    }

    async fn get_archetype_scores_text(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<StatusText>, TStatus> {
        let cfg = Config::load_from_db().await.unwrap_or_default();
        let text = if !cfg.evolution.enabled {
            "Evolution system is disabled.".to_string()
        } else {
            let db = Database::open()
                .await
                .map_err(|e| TStatus::internal(format!("archetypes text failed: {e}")))?;
            let state = db
                .get_evolution_state()
                .await
                .map_err(|e| TStatus::internal(format!("archetypes text failed: {e}")))?;
            cortex::evolution::format_archetype_scores(&state)
        };
        Ok(Response::new(StatusText {
            text,
            compact_class_line: String::new(),
        }))
    }

    async fn get_evolution_history_text(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<StatusText>, TStatus> {
        let cfg = Config::load_from_db().await.unwrap_or_default();
        let text = if !cfg.evolution.enabled {
            "Evolution system is disabled.".to_string()
        } else {
            let db = Database::open()
                .await
                .map_err(|e| TStatus::internal(format!("history text failed: {e}")))?;
            let mut events = db
                .evolution_events_since(0)
                .await
                .map_err(|e| TStatus::internal(format!("history text failed: {e}")))?;
            events.reverse();
            cortex::evolution::format_history(&events)
        };
        Ok(Response::new(StatusText {
            text,
            compact_class_line: String::new(),
        }))
    }

    async fn get_usage_text(
        &self,
        req: Request<UsageRequest>,
    ) -> Result<Response<StatusText>, TStatus> {
        let days = req.into_inner().days;
        let db = Database::open()
            .await
            .map_err(|e| TStatus::internal(format!("usage text failed: {e}")))?;
        let text = cortex::usage::format_usage_text(&db, days)
            .await
            .map_err(|e| TStatus::internal(format!("usage text failed: {e}")))?;
        Ok(Response::new(StatusText {
            text,
            compact_class_line: String::new(),
        }))
    }

    async fn get_heartbeat(&self, _req: Request<Empty>) -> Result<Response<Heartbeat>, TStatus> {
        // The scheduler's exact next-fire timestamp lives inside the daemon
        // task and isn't routed here yet. Return the configured interval —
        // better than fabricating "next at +30s".
        let cfg = Config::load_from_db()
            .await
            .map_err(|e| TStatus::internal(format!("config load failed: {e}")))?;
        let next_in = parse_interval_seconds(&cfg.heartbeat.interval).unwrap_or(0);
        Ok(Response::new(Heartbeat {
            next_in_seconds: next_in,
            last_unix_ts: 0,
        }))
    }
}

/// Parse a heartbeat interval like `"30m"`, `"2h"`, `"45s"` to seconds.
/// Returns `None` for malformed input (including unsupported sub-second units
/// like `"30ms"`) so the caller can fall back to 0 explicitly rather than
/// silently report "next heartbeat in 0 seconds."
fn parse_interval_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.trim().parse().ok()?;
    // Match the EXACT unit string — `"ms"` falls through to `None` so callers
    // see an unconfigured heartbeat instead of being told it'll fire instantly.
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        "d" => Some(n * 86_400),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{drift_notice_for_vitals, parse_interval_seconds};
    use chrono::{Duration, Utc};

    #[test]
    fn drift_notice_uses_decayed_state_thresholds() {
        let mut state = cortex::vitals::baseline();
        state.last_interaction_at = Utc::now() - Duration::hours(49);

        let notice = drift_notice_for_vitals(&state, Utc::now());

        assert!(
            notice.contains("inactive"),
            "expected inactivity drift from the decayed state, got {notice:?}"
        );
    }

    #[test]
    fn interval_parses_common_suffixes() {
        // Real failure mode: a regression to using `Duration::from_secs(s.parse()?)`
        // would fail on "30m" silently — the heartbeat field would always
        // report 0 and the popup would look dead.
        assert_eq!(parse_interval_seconds("30s"), Some(30));
        assert_eq!(parse_interval_seconds("30m"), Some(1800));
        assert_eq!(parse_interval_seconds("2h"), Some(7200));
        assert_eq!(parse_interval_seconds("1d"), Some(86_400));
    }

    #[test]
    fn interval_returns_none_on_garbage_and_subsecond_units() {
        // Real failure mode: a regression that took only the first alpha char
        // would split "30ms" into ("30", "m") and report 30 minutes, then the
        // popup would say "next heartbeat in 1800s" when the user asked for
        // ms-precision. Asserting None forces the caller to surface "unknown".
        assert_eq!(parse_interval_seconds(""), None);
        assert_eq!(parse_interval_seconds("forever"), None);
        assert_eq!(parse_interval_seconds("30x"), None);
        assert_eq!(parse_interval_seconds("30ms"), None);
    }
}
