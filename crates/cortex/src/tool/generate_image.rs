//! `generate_image` tool.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GenerateImageArgs {
    pub prompt: String,
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub size: Option<String>,
}

impl GenerateImageArgs {
    fn to_value(&self) -> serde_json::Value {
        // Plain struct of primitives; serialization is infallible.
        // The expect would only fire on a serde upstream bug.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("GenerateImageArgs is JSON-safe")
        }
    }
}

pub(crate) async fn handle(args: &GenerateImageArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_generate_image(&args.to_value(), config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: GenerateImageArgs =
            serde_json::from_value(serde_json::json!({"prompt":"a cat"})).unwrap();
        assert_eq!(args.prompt, "a cat");
        assert!(args.count.is_none());
    }

    #[test]
    fn deserialize_with_count_and_size() {
        let args: GenerateImageArgs =
            serde_json::from_value(serde_json::json!({"prompt":"x","count":2,"size":"1024x1024"}))
                .unwrap();
        assert_eq!(args.count, Some(2));
        assert_eq!(args.size.as_deref(), Some("1024x1024"));
    }
}
