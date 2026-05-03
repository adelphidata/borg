//! `BondHook` — lifecycle hook that records bond events with HMAC chain
//! integrity. Caches the last replayed `BondState` to avoid replaying the
//! full event chain on every `InjectContext` call. Cache is refreshed after
//! recording.

use std::fmt;

use crate::db::Database;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};
use crate::vitals;

use super::format::format_context;
use super::heuristics::{looks_like_acceptance, looks_like_rejection};
use super::metrics::{compute_correction_rate, compute_routine_success_rate};
use super::replay::{replay_events_with_key, BOND_HMAC_DOMAIN};
use super::types::{
    BondState, DELTA_CORRECTION, DELTA_CREATION_EVENT, DELTA_SUGGESTION_ACCEPTED,
    DELTA_SUGGESTION_REJECTED, DELTA_TOOL_FAILURE, DELTA_TOOL_SUCCESS,
};

/// Lifecycle hook that tracks bond events with HMAC chain integrity.
pub struct BondHook {
    db: std::sync::Mutex<Database>,
    cached_state: std::sync::Mutex<Option<BondState>>,
}

impl BondHook {
    /// Create a new bond hook, opening a database connection and pre-computing state.
    pub fn new() -> anyhow::Result<Self> {
        let db = Database::open()?;
        Ok(Self::from_db(db))
    }

    /// Build a hook around an already-open database. Used by `new()` and by
    /// tests, which pass an in-memory `Database::test_db()` to avoid touching
    /// `~/.borg/borg.db`.
    pub(crate) fn from_db(db: Database) -> Self {
        let events = match db.get_all_bond_events() {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("bond: failed to load events, using baseline: {e}");
                Vec::new()
            }
        };
        let hmac_key = db.derive_hmac_key(BOND_HMAC_DOMAIN);
        let state = replay_events_with_key(&hmac_key, &events);
        Self {
            db: std::sync::Mutex::new(db),
            cached_state: std::sync::Mutex::new(Some(state)),
        }
    }

    /// Record a bond event atomically (read prev_hmac + compute + insert in one transaction).
    fn record(&self, event_type: &str, delta: i32, reason: &str) {
        let Ok(db) = self.db.lock() else {
            tracing::warn!("bond: mutex poisoned, skipping event");
            return;
        };

        if let Err(e) = db.record_bond_event_chained(event_type, delta, reason) {
            tracing::warn!("bond: failed to record event: {e}");
            return;
        }

        if let Ok(mut cache) = self.cached_state.lock() {
            *cache = None;
        }
    }

    fn get_or_refresh_state(&self, db: &Database) -> BondState {
        if let Ok(cache) = self.cached_state.lock() {
            if let Some(state) = cache.as_ref() {
                return state.clone();
            }
        }
        let events = match db.get_all_bond_events() {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("bond: failed to refresh events, using baseline: {e}");
                Vec::new()
            }
        };
        let hmac_key = db.derive_hmac_key(BOND_HMAC_DOMAIN);
        let state = replay_events_with_key(&hmac_key, &events);
        if let Ok(mut cache) = self.cached_state.lock() {
            *cache = Some(state.clone());
        }
        state
    }

    fn inject_context(&self) -> HookAction {
        let Ok(db) = self.db.lock() else {
            return HookAction::Continue;
        };
        let state = self.get_or_refresh_state(&db);
        let correction_rate = compute_correction_rate(&db);
        let routine_rate = compute_routine_success_rate(&db);
        HookAction::InjectContext(format_context(&state, correction_rate, routine_rate))
    }
}

impl Hook for BondHook {
    fn name(&self) -> &str {
        "bond"
    }

    fn points(&self) -> &[HookPoint] {
        &[
            HookPoint::SessionStart,
            HookPoint::BeforeAgentStart,
            HookPoint::BeforeLlmCall,
            HookPoint::AfterToolCall,
        ]
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        match &ctx.data {
            HookData::SessionStart { .. } => HookAction::Continue,
            HookData::AgentStart { user_message } => {
                if vitals::looks_like_correction(user_message) {
                    self.record("correction", DELTA_CORRECTION, "user_message");
                } else if looks_like_acceptance(user_message) {
                    self.record(
                        "suggestion_accepted",
                        DELTA_SUGGESTION_ACCEPTED,
                        "user_message",
                    );
                } else if looks_like_rejection(user_message) {
                    self.record(
                        "suggestion_rejected",
                        DELTA_SUGGESTION_REJECTED,
                        "user_message",
                    );
                }
                self.inject_context()
            }
            HookData::LlmCall { .. } => self.inject_context(),
            HookData::ToolResult { name, is_error, .. } => {
                let category = vitals::classify_tool(name, *is_error);
                match category {
                    vitals::EventCategory::Failure => {
                        self.record("tool_failure", DELTA_TOOL_FAILURE, name);
                    }
                    vitals::EventCategory::Creation => {
                        self.record("creation", DELTA_CREATION_EVENT, name);
                    }
                    vitals::EventCategory::Success => {
                        self.record("tool_success", DELTA_TOOL_SUCCESS, name);
                    }
                    _ => {}
                }
                HookAction::Continue
            }
            _ => HookAction::Continue,
        }
    }
}

impl fmt::Debug for BondHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BondHook").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookPoint;

    fn ctx_agent_start(msg: &str) -> HookContext {
        HookContext {
            point: HookPoint::BeforeAgentStart,
            session_id: "s".into(),
            turn_count: 0,
            data: HookData::AgentStart {
                user_message: msg.into(),
            },
        }
    }

    fn ctx_tool_result(name: &str, is_error: bool) -> HookContext {
        HookContext {
            point: HookPoint::AfterToolCall,
            session_id: "s".into(),
            turn_count: 0,
            data: HookData::ToolResult {
                name: name.into(),
                result: String::new(),
                is_error,
            },
        }
    }

    fn ctx_session_start() -> HookContext {
        HookContext {
            point: HookPoint::SessionStart,
            session_id: "s".into(),
            turn_count: 0,
            data: HookData::SessionStart {
                session_id: "s".into(),
            },
        }
    }

    fn fresh_hook() -> BondHook {
        BondHook::from_db(Database::test_db())
    }

    /// Read the persisted bond chain. The hook keeps an internal Database
    /// behind a Mutex; we open a second handle to the same in-memory pool
    /// won't work (each `:memory:` connection is isolated), so we drop
    /// access to the hook's DB by going through the public hook surface
    /// instead — `inject_context` reflects the chain state.
    fn events_from_hook(hook: &BondHook) -> Vec<crate::bond::BondEvent> {
        let db = hook.db.lock().expect("lock");
        db.get_all_bond_events().expect("read events")
    }

    #[test]
    fn agent_start_acceptance_records_suggestion_accepted() {
        let hook = fresh_hook();
        hook.execute(&ctx_agent_start("go ahead"));
        let events = events_from_hook(&hook);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "suggestion_accepted");
        assert_eq!(events[0].score_delta, 1);
        assert_eq!(events[0].reason, "user_message");
    }

    #[test]
    fn agent_start_rejection_records_suggestion_rejected() {
        let hook = fresh_hook();
        hook.execute(&ctx_agent_start("cancel that"));
        let events = events_from_hook(&hook);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "suggestion_rejected");
        assert_eq!(events[0].score_delta, -1);
    }

    #[test]
    fn agent_start_correction_records_correction() {
        let hook = fresh_hook();
        hook.execute(&ctx_agent_start("that's wrong"));
        let events = events_from_hook(&hook);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "correction");
        assert_eq!(events[0].score_delta, -2);
    }

    #[test]
    fn agent_start_neutral_message_records_nothing() {
        let hook = fresh_hook();
        hook.execute(&ctx_agent_start("hello world"));
        assert!(events_from_hook(&hook).is_empty());
    }

    #[test]
    fn tool_result_creation_tool_records_creation() {
        // apply_patch is in vitals::classify_tool's Creation list.
        let hook = fresh_hook();
        hook.execute(&ctx_tool_result("apply_patch", false));
        let events = events_from_hook(&hook);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "creation");
        assert_eq!(events[0].score_delta, 1);
        assert_eq!(events[0].reason, "apply_patch");
    }

    #[test]
    fn tool_result_error_records_tool_failure() {
        let hook = fresh_hook();
        hook.execute(&ctx_tool_result("read_file", true));
        let events = events_from_hook(&hook);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "tool_failure");
        assert_eq!(events[0].score_delta, -1);
    }

    #[test]
    fn tool_result_success_non_creation_records_tool_success() {
        let hook = fresh_hook();
        hook.execute(&ctx_tool_result("read_file", false));
        let events = events_from_hook(&hook);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "tool_success");
        assert_eq!(events[0].score_delta, 1);
    }

    #[test]
    fn session_start_does_not_record_or_inject() {
        let hook = fresh_hook();
        let action = hook.execute(&ctx_session_start());
        assert!(matches!(action, HookAction::Continue));
        assert!(events_from_hook(&hook).is_empty());
    }

    /// After `record(...)`, the cached state is invalidated and the next
    /// `inject_context` reflects the new chain. If the cache stuck, every
    /// system prompt would still report the pre-event score forever.
    #[test]
    fn record_invalidates_cache_so_next_inject_reflects_new_event() {
        let hook = fresh_hook();
        // Pre-event injection should reflect the baseline (25). Two corrections
        // (-2 each) drop the score by 4.
        let pre = match hook.inject_context() {
            HookAction::InjectContext(s) => s,
            _ => panic!("expected InjectContext"),
        };
        assert!(pre.contains("(25/100)"), "baseline missing: {pre}");

        hook.execute(&ctx_agent_start("that's wrong"));
        hook.execute(&ctx_agent_start("that's wrong"));

        let post = match hook.inject_context() {
            HookAction::InjectContext(s) => s,
            _ => panic!("expected InjectContext"),
        };
        assert!(
            post.contains("(21/100)"),
            "expected score 25 - 4 = 21 after two corrections, got: {post}"
        );
    }
}
