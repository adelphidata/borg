//! `browser` tool. Acts on a many-actioned schema; defer field validation to
//! the underlying handler, which already validates per-action via
//! `validate_browser_args`.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::browser::BrowserSession;
use crate::config::Config;
use crate::tool_handlers;
use crate::types::ToolOutput;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct BrowserArgs(pub serde_json::Value);

impl BrowserArgs {
    pub(crate) fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

pub(crate) async fn handle(
    args: &BrowserArgs,
    config: &Config,
    session: &mut Option<BrowserSession>,
) -> Result<ToolOutput> {
    tool_handlers::handle_browser(args.as_value(), config, session).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_passes_through_value() {
        let raw = serde_json::json!({"action":"navigate","url":"https://example.com"});
        let args: BrowserArgs = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(args.as_value(), &raw);
    }
}
