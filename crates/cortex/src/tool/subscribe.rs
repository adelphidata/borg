//! `subscribe` tool arguments.
//!
//! Permissive like `schedule`: each action takes a different set of fields,
//! so the handler (`crate::tool_handlers::subscribe`) validates per action.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SubscribeArgs(pub serde_json::Value);
