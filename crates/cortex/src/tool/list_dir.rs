//! `list_dir` tool.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::tool_handlers;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ListDirArgs {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<u64>,
    #[serde(default)]
    pub include_hidden: Option<bool>,
    #[serde(default)]
    pub fuzzy_match: Option<String>,
    #[serde(default)]
    pub max_results: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

impl ListDirArgs {
    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ListDirArgs is JSON-safe")
    }
}

pub(crate) fn handle(args: &ListDirArgs, config: &Config) -> Result<String> {
    tool_handlers::handle_list_dir(&args.to_value(), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_empty_args_uses_defaults() {
        let args: ListDirArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(args.path.is_none());
        assert!(args.fuzzy_match.is_none());
    }

    #[test]
    fn deserialize_full_args() {
        let args: ListDirArgs = serde_json::from_value(serde_json::json!({
            "path":"./src","depth":2,"include_hidden":true,"fuzzy_match":"main",
            "max_results":10,"offset":0,"limit":5
        }))
        .unwrap();
        assert_eq!(args.path.as_deref(), Some("./src"));
        assert_eq!(args.depth, Some(2));
        assert_eq!(args.include_hidden, Some(true));
        assert_eq!(args.fuzzy_match.as_deref(), Some("main"));
    }
}
