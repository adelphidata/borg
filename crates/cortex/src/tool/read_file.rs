//! `read_file` tool.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;
use crate::types::ToolOutput;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReadFileArgs {
    pub path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub max_chars: Option<u64>,
}

impl ReadFileArgs {
    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ReadFileArgs is JSON-safe")
    }
}

pub(crate) fn handle(args: &ReadFileArgs, config: &Config) -> Result<ToolOutput> {
    tool_handlers::handle_read_file(&args.to_value(), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: ReadFileArgs = serde_json::from_value(serde_json::json!({"path":"a.rs"})).unwrap();
        assert_eq!(args.path, "a.rs");
        assert!(args.offset.is_none());
    }

    #[test]
    fn deserialize_with_pagination() {
        let args: ReadFileArgs = serde_json::from_value(
            serde_json::json!({"path":"a.rs","offset":10,"limit":50,"max_chars":100}),
        )
        .unwrap();
        assert_eq!(args.offset, Some(10));
        assert_eq!(args.limit, Some(50));
        assert_eq!(args.max_chars, Some(100));
    }
}
