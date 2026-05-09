//! Hook context construction and `AfterToolCall` dispatch.
//!
//! Pulled out of `agent/mod.rs` as part of the structural split. The
//! `Agent::hook_ctx` helper builds a [`HookContext`] from the agent's session
//! state; `Agent::fire_after_tool_hook` dispatches the post-tool hook with the
//! tool name and (already redacted) result. Both stay on `impl Agent` so they
//! retain access to `self`'s private fields without weakening visibility.

use crate::hooks::{HookContext, HookData, HookPoint, HookRegistry};

use super::Agent;

impl Agent {
    /// Build a HookContext for the current agent state.
    pub(super) fn hook_ctx(&self, point: HookPoint, data: HookData) -> HookContext {
        HookContext {
            point,
            session_id: self.session.meta.id.clone(),
            turn_count: self.turn_count,
            data,
        }
    }

    pub(super) fn fire_after_tool_hook(
        hook_registry: &mut HookRegistry,
        session_id: &str,
        turn_count: u32,
        name: &str,
        result: &str,
    ) {
        let hook_ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: session_id.to_string(),
            turn_count,
            data: HookData::ToolResult {
                name: name.to_string(),
                result: result.to_string(),
                is_error: result.starts_with("Error:"),
            },
        };
        hook_registry.dispatch(&hook_ctx);
    }
}
