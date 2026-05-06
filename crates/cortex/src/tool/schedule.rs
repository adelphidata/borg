//! `schedule` (and aliases `manage_tasks` / `manage_cron`) tool.
//!
//! `ScheduleArgs` is intentionally permissive (`serde_json::Value`) — the
//! underlying handler dispatches on `action` + `type` over a wide schema with
//! many optional fields per action. Validation lives in the handler, not at
//! parse time, because each action has different required fields.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ScheduleArgs(pub serde_json::Value);

impl ScheduleArgs {
    pub(crate) fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

pub(crate) fn handle_schedule(args: &ScheduleArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_schedule(args.as_value(), config)
}

pub(crate) fn handle_manage_tasks(args: &ScheduleArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_manage_tasks(args.as_value(), config)
}

pub(crate) fn handle_manage_cron(args: &ScheduleArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_manage_cron(args.as_value(), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_passes_through_value() {
        let raw = serde_json::json!({"action":"list","type":"prompt"});
        let args: ScheduleArgs = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(args.as_value(), &raw);
    }

    #[test]
    fn deserialize_empty_value_is_ok() {
        // Schedule's many actions don't share a single required-field set;
        // the handler validates per-action.
        let args: ScheduleArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(args.as_value().is_object());
    }
}
