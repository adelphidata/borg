//! `web_fetch` tool.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WebFetchArgs {
    pub url: String,
    #[serde(default)]
    pub max_chars: Option<u64>,
}

impl WebFetchArgs {
    fn to_value(&self) -> serde_json::Value {
        // Plain struct of primitives; serialization is infallible.
        // The expect would only fire on a serde upstream bug.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("WebFetchArgs is JSON-safe")
        }
    }
}

pub(crate) async fn handle(args: &WebFetchArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_web_fetch(&args.to_value(), config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: WebFetchArgs =
            serde_json::from_value(serde_json::json!({"url":"https://example.com"})).unwrap();
        assert_eq!(args.url, "https://example.com");
    }

    #[test]
    fn deserialize_with_max_chars() {
        let args: WebFetchArgs =
            serde_json::from_value(serde_json::json!({"url":"https://x","max_chars":1000}))
                .unwrap();
        assert_eq!(args.max_chars, Some(1000));
    }
}
