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
