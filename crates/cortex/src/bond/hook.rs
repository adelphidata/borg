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
        let events = match db.get_all_bond_events() {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("bond: failed to load events, using baseline: {e}");
                Vec::new()
            }
        };
        let hmac_key = db.derive_hmac_key(BOND_HMAC_DOMAIN);
        let state = replay_events_with_key(&hmac_key, &events);
        Ok(Self {
            db: std::sync::Mutex::new(db),
            cached_state: std::sync::Mutex::new(Some(state)),
        })
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
