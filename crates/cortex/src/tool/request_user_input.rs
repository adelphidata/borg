//! `request_user_input` tool.
//!
//! The actual question / option schema validation lives in
//! [`tool_handlers::user_input::parse_questions`]; this struct only carries the
//! payload through so that surrounding parse failures surface uniformly via
//! [`crate::tool::ToolParseError`].

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use crate::tool_handlers;
use crate::types::ToolOutput;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RequestUserInputArgs(pub serde_json::Value);

impl RequestUserInputArgs {
    pub(crate) fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

pub(crate) async fn handle(
    args: &RequestUserInputArgs,
    event_tx: &mpsc::Sender<AgentEvent>,
) -> Result<ToolOutput> {
    tool_handlers::handle_request_user_input(args.as_value(), event_tx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_passes_through_value() {
        let raw = serde_json::json!({
            "questions":[{
                "id":"a","header":"H","question":"?",
                "options":[{"label":"x","description":"y"},{"label":"z","description":"w"}]
            }]
        });
        let args: RequestUserInputArgs = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(args.as_value(), &raw);
    }
}
