//! `web_search` tool.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WebSearchArgs {
    pub query: String,
    #[serde(default)]
    pub mode: Option<String>,
}

impl WebSearchArgs {
    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("WebSearchArgs is JSON-safe")
    }
}

pub(crate) async fn handle(args: &WebSearchArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_web_search(&args.to_value(), config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: WebSearchArgs =
            serde_json::from_value(serde_json::json!({"query":"rust async"})).unwrap();
        assert_eq!(args.query, "rust async");
    }

    #[test]
    fn deserialize_with_mode() {
        let args: WebSearchArgs =
            serde_json::from_value(serde_json::json!({"query":"x","mode":"live"})).unwrap();
        assert_eq!(args.mode.as_deref(), Some("live"));
    }
}
