//! `list` tool (and the bare-alias variants `list_skills` / `list_channels`).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::multi_agent::AgentControl;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ListArgs {
    pub what: String,
}

impl ListArgs {
    fn to_value(&self) -> serde_json::Value {
        // Plain struct of primitives; serialization is infallible.
        // The expect would only fire on a serde upstream bug.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("ListArgs is JSON-safe")
        }
    }
}

pub(crate) fn handle(
    args: &ListArgs,
    config: &Config,
    agent_control: Option<&AgentControl>,
) -> Result<String> {
    tool_handlers::handle_list(&args.to_value(), config, agent_control)
}

pub(crate) fn handle_list_skills(config: &Config) -> Result<String> {
    tool_handlers::handle_list_skills(config)
}

pub(crate) fn handle_list_channels(config: &Config) -> Result<String> {
    tool_handlers::handle_list_channels(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_what_field() {
        let args: ListArgs = serde_json::from_value(serde_json::json!({"what":"skills"})).unwrap();
        assert_eq!(args.what, "skills");
    }

    #[test]
    fn missing_what_field_errors() {
        let err = serde_json::from_value::<ListArgs>(serde_json::json!({})).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("what"));
    }
}
