//! Message persistence and redaction helpers.
//!
//! Pulled out of `agent/mod.rs` as part of the structural split. These methods
//! own the path from a freshly-built [`Message`] into both in-memory history
//! and the SQLite store, plus the secret-redaction / truncation pipeline that
//! every persisted assistant or tool message flows through.
//!
//! All methods stay on `impl Agent` so they retain access to the agent's
//! private fields (history, db lock, config) without weakening visibility.

use tracing::warn;

use crate::logging::log_message;
use crate::security::secrets::redact_secrets;
use crate::types::Message;
use crate::util::truncate::truncate_output;

use super::{Agent, TOOL_OUTPUT_MAX_TOKENS};

impl Agent {
    /// Append a system-injected assistant message to the agent's history.
    ///
    /// Used to mirror text the user already saw on this client (e.g. a
    /// heartbeat that was streamed in by the daemon while the TUI was open)
    /// into the active session so the next user reply has it in context.
    /// Without this, the user's "yes please" reply lands in a session whose
    /// history has no record of the heartbeat, and the agent answers the
    /// previous turn instead.
    ///
    /// Refuses to inject if doing so would split an `assistant(tool_calls)`
    /// message from its outstanding `tool` results — both Anthropic and
    /// OpenAI reject orphaned tool_use blocks on the next turn, so a
    /// well-meaning mid-turn mirror would brick the session.
    pub fn append_assistant_message(&mut self, text: &str) {
        if super::tool_validation::has_unresolved_tool_calls(&self.history) {
            warn!(
                "append_assistant_message: refused — last assistant turn has unresolved \
                 tool_calls; injecting now would orphan them. Dropping the mirrored message."
            );
            return;
        }
        self.persist_message(Message::assistant(text));
    }

    /// Log a message and push it to history + SQLite in one step.
    pub(super) fn log_and_persist(&mut self, msg: Message) {
        log_message(&msg);
        self.persist_message(msg);
    }

    /// Record a skipped/cancelled tool call as a tool_result message.
    pub(super) fn skip_tool_call(&mut self, tool_call_id: &str, reason: &str) {
        self.log_and_persist(Message::tool_result(tool_call_id, reason));
    }

    /// Optionally redact secrets from a string based on config.
    pub(super) fn maybe_redact(&self, s: String) -> String {
        if self.config.security.secret_detection {
            redact_secrets(&s)
        } else {
            s
        }
    }

    /// Truncate and optionally redact secrets from raw tool output.
    pub(super) fn truncate_and_redact(&self, raw: &str) -> String {
        self.maybe_redact(truncate_output(raw, TOOL_OUTPUT_MAX_TOKENS))
    }

    /// Push a message to history and persist it to SQLite for crash recovery.
    pub(super) fn persist_message(&mut self, msg: Message) {
        let session_id = self.session.meta.id.clone();
        let role = match msg.role {
            crate::types::Role::System => "system",
            crate::types::Role::User => "user",
            crate::types::Role::Assistant => "assistant",
            crate::types::Role::Tool => "tool",
        };
        let tool_calls_json = msg
            .tool_calls
            .as_ref()
            .and_then(|tc| serde_json::to_string(tc).ok());
        let (content_text, content_parts_json) = match &msg.content {
            Some(crate::types::MessageContent::Parts(parts)) => {
                let full = msg
                    .content
                    .as_ref()
                    .map(crate::types::MessageContent::full_text);
                let parts_json = serde_json::to_string(parts).ok();
                (full, parts_json)
            }
            _ => (msg.text_content().map(str::to_string), None),
        };
        {
            let guard = self.db_guard();
            if let Some(ref db) = *guard {
                if let Err(e) = db.insert_message(
                    &session_id,
                    role,
                    content_text.as_deref(),
                    tool_calls_json.as_deref(),
                    msg.tool_call_id.as_deref(),
                    msg.timestamp.as_deref(),
                    content_parts_json.as_deref(),
                ) {
                    warn!("Failed to persist message to SQLite: {e}");
                }
            }
        }
        self.history.push(msg);
    }
}
