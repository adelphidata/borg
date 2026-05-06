//! `memory_search` tool: typed args + handler.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemorySearchArgs {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<u64>,
    #[serde(default)]
    pub min_score: Option<f64>,
    #[serde(default)]
    pub sources: Option<Vec<String>>,
}

impl MemorySearchArgs {
    fn to_value(&self) -> serde_json::Value {
        // Plain struct of primitives; serialization is infallible.
        // The expect would only fire on a serde upstream bug.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("MemorySearchArgs is JSON-safe")
        }
    }
}

pub(crate) async fn handle(args: &MemorySearchArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_memory_search(&args.to_value(), config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: MemorySearchArgs =
            serde_json::from_value(serde_json::json!({"query":"hello"})).unwrap();
        assert_eq!(args.query, "hello");
        assert!(args.max_results.is_none());
    }

    #[test]
    fn deserialize_with_sources() {
        let args: MemorySearchArgs = serde_json::from_value(
            serde_json::json!({"query":"x","sources":["memory","messages"]}),
        )
        .unwrap();
        assert_eq!(
            args.sources.as_deref(),
            Some(&["memory".to_string(), "messages".to_string()][..])
        );
    }
}
