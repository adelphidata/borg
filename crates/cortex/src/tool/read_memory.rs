//! `read_memory` tool: typed args + handler.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReadMemoryArgs {
    pub filename: String,
    #[serde(default)]
    pub scope: Option<String>,
}

impl ReadMemoryArgs {
    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ReadMemoryArgs is JSON-safe")
    }
}

pub(crate) fn handle(args: &ReadMemoryArgs) -> Result<String> {
    tool_handlers::handle_read_memory(&args.to_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: ReadMemoryArgs =
            serde_json::from_value(serde_json::json!({"filename":"x"})).unwrap();
        assert_eq!(args.filename, "x");
        assert!(args.scope.is_none());
    }

    #[test]
    fn deserialize_with_scope() {
        let args: ReadMemoryArgs =
            serde_json::from_value(serde_json::json!({"filename":"x","scope":"project:abc"}))
                .unwrap();
        assert_eq!(args.scope.as_deref(), Some("project:abc"));
    }

    #[test]
    fn round_trip_to_value_preserves_fields() {
        let args = ReadMemoryArgs {
            filename: "n".into(),
            scope: Some("global".into()),
        };
        let v = args.to_value();
        assert_eq!(v["filename"], "n");
        assert_eq!(v["scope"], "global");
    }
}
