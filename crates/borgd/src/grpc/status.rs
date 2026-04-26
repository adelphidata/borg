//! Status service — read-only views of agent health.
//!
//! Backed by the same data the in-process `borg status` and TUI status popup
//! read today: vitals events (HMAC-replayed), evolution chain, posture
//! config. Budget tracking surfaces the configured monthly token limit; the
//! per-day USD spend lands when the cost-tracking pipeline is wired into the
//! daemon.

use borg_core::config::Config;
use borg_core::db::Database;
use borg_proto::status::{status_server::Status, Budget, Empty, Heartbeat, Posture, Vitals};
use tonic::{Request, Response, Status as TStatus};

/// Status service implementation.
pub struct StatusSvc;

impl StatusSvc {
    /// Construct a new Status service.
    pub fn new() -> Self {
        Self
    }
}

impl Default for StatusSvc {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl Status for StatusSvc {
    async fn get_vitals(&self, _req: Request<Empty>) -> Result<Response<Vitals>, TStatus> {
        let snap = tokio::task::spawn_blocking(|| -> anyhow::Result<_> {
            let db = Database::open()?;
            let state = db.get_vitals_state()?;
            let state = borg_core::vitals::apply_decay(&state, chrono::Utc::now());
            let evo = db.get_evolution_state().ok();
            Ok((state, evo))
        })
        .await
        .map_err(|e| TStatus::internal(format!("vitals task panicked: {e}")))?
        .map_err(|e| TStatus::internal(format!("vitals load failed: {e}")))?;
        let (state, evo) = snap;
        let (stage, xp) = match evo {
            Some(e) => {
                let stage = match e.stage {
                    borg_core::evolution::Stage::Base => "Base",
                    borg_core::evolution::Stage::Evolved => "Evolved",
                    borg_core::evolution::Stage::Final => "Final",
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
        }))
    }

    async fn get_posture(&self, _req: Request<Empty>) -> Result<Response<Posture>, TStatus> {
        // Real security-posture wiring lands in a follow-up task. Surface a
        // truthful "neutral" value rather than fabricate a multiplier — when
        // posture lands this method updates alongside the new state.
        Ok(Response::new(Posture {
            posture: "Balanced".into(),
            xp_multiplier: 1.0,
        }))
    }

    async fn get_budget(&self, _req: Request<Empty>) -> Result<Response<Budget>, TStatus> {
        // Today's `BudgetConfig` is a *monthly token* limit, not a daily USD
        // cap; map it onto the wire field as-is and leave spent at 0 until
        // cost tracking lands. Renaming the proto field is a wire-break we'll
        // do alongside the cost-tracking task.
        let cfg = tokio::task::spawn_blocking(Config::load_from_db)
            .await
            .map_err(|e| TStatus::internal(format!("config task panicked: {e}")))?
            .map_err(|e| TStatus::internal(format!("config load failed: {e}")))?;
        Ok(Response::new(Budget {
            daily_usd_cap: cfg.budget.monthly_token_limit as f64,
            daily_usd_spent: 0.0,
        }))
    }

    async fn get_heartbeat(&self, _req: Request<Empty>) -> Result<Response<Heartbeat>, TStatus> {
        // The scheduler's exact next-fire timestamp lives inside the daemon
        // task and isn't routed here yet. Return the configured interval —
        // better than fabricating "next at +30s".
        let cfg = tokio::task::spawn_blocking(Config::load_from_db)
            .await
            .map_err(|e| TStatus::internal(format!("config task panicked: {e}")))?
            .map_err(|e| TStatus::internal(format!("config load failed: {e}")))?;
        let next_in = parse_interval_seconds(&cfg.heartbeat.interval).unwrap_or(0);
        Ok(Response::new(Heartbeat {
            next_in_seconds: next_in,
            last_unix_ts: 0,
        }))
    }
}

/// Parse a heartbeat interval like `"30m"`, `"2h"`, `"45s"` to seconds.
/// Returns `None` for malformed input — the caller falls back to 0.
fn parse_interval_seconds(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let n: u64 = num.trim().parse().ok()?;
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
    use super::parse_interval_seconds;

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
    fn interval_returns_none_on_garbage() {
        assert_eq!(parse_interval_seconds(""), None);
        assert_eq!(parse_interval_seconds("forever"), None);
        assert_eq!(parse_interval_seconds("30x"), None);
    }
}
