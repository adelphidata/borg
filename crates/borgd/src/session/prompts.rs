//! Tracks oneshot::Sender handles for in-flight ShellConfirmation /
//! UserInputRequest events so the gRPC `RespondToPrompt` RPC can route a
//! client reply back to the waiting agent.
//!
//! `core::AgentEvent::{ShellConfirmation, UserInputRequest}` carry an
//! `oneshot::Sender` directly (cheap when the agent runs in-process). On the
//! wire we strip that channel and substitute a `prompt_id`; the daemon parks
//! the sender here keyed by `(session_id, prompt_id)` and resolves it when
//! the client responds.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

use cortex::tool_handlers::user_input::Answer;

/// Awaiting reply for a single prompt.
pub enum PendingPrompt {
    /// ShellConfirmation expects "true"/"false".
    Shell(oneshot::Sender<bool>),
    /// UserInputRequest expects a JSON-encoded `{ "<qid>": { "answers": [...] } }`
    /// map matching the question batch the UI rendered.
    UserInput(oneshot::Sender<HashMap<String, Answer>>),
}

/// Per-session map of `prompt_id` → pending oneshot sender.
#[derive(Default)]
pub struct PromptRegistry {
    inner: Mutex<HashMap<String, PendingPrompt>>,
}

impl PromptRegistry {
    /// Allocate a fresh `prompt_id` and park the sender. The id is the
    /// stringified `Uuid::new_v4` so collisions across sessions never matter.
    pub fn register(&self, prompt: PendingPrompt) -> String {
        let id = Uuid::new_v4().to_string();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(id.clone(), prompt);
        id
    }

    /// Resolve a parked prompt with `value`. Returns `Ok(())` if delivered,
    /// `Err(message)` if the id is unknown / the value couldn't be parsed for
    /// the expected reply type / the agent already dropped the receiver.
    pub fn respond(&self, prompt_id: &str, value: &str) -> Result<(), String> {
        let pending = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .remove(prompt_id)
                .ok_or_else(|| format!("unknown prompt_id `{prompt_id}`"))?
        };
        match pending {
            PendingPrompt::Shell(tx) => {
                let approved = match value.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "y" | "1" => true,
                    "false" | "no" | "n" | "0" => false,
                    other => return Err(format!("expected boolean, got `{other}`")),
                };
                tx.send(approved)
                    .map_err(|_| "agent dropped shell-confirmation receiver".to_string())
            }
            PendingPrompt::UserInput(tx) => {
                let map: HashMap<String, Answer> =
                    serde_json::from_str(value).map_err(|e| {
                        format!(
                            "expected JSON answer map (e.g. {{\"qid\": {{\"answers\": [\"…\"]}}}}), got error: {e}"
                        )
                    })?;
                tx.send(map)
                    .map_err(|_| "agent dropped user-input receiver".to_string())
            }
        }
    }

    /// Drop every pending prompt — used when the session is closed mid-turn so
    /// agents waiting on `respond.await` unblock with an error rather than
    /// hanging forever.
    pub fn clear(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_confirmation_round_trips_through_registry() {
        // Real failure mode: a regression to `value.parse::<bool>()` would
        // accept "true"/"false" but silently reject the natural "yes"/"no" the
        // gateway and CLI both send today.
        let reg = PromptRegistry::default();
        let (tx, rx) = oneshot::channel();
        let id = reg.register(PendingPrompt::Shell(tx));
        reg.respond(&id, "yes").expect("respond ok");
        assert!(rx.await.expect("receive"));
    }

    #[tokio::test]
    async fn user_input_response_decodes_answer_map() {
        // Real failure mode: a regression to plain-string passthrough would
        // break the LLM round-trip — the agent expects a structured answer
        // map keyed by question id.
        let reg = PromptRegistry::default();
        let (tx, rx) = oneshot::channel::<HashMap<String, Answer>>();
        let id = reg.register(PendingPrompt::UserInput(tx));
        reg.respond(
            &id,
            r#"{"color":{"answers":["Red","user_note: prefer crimson"]}}"#,
        )
        .expect("respond ok");
        let got = rx.await.expect("receive");
        let entry = got.get("color").expect("color present");
        assert_eq!(
            entry.answers,
            vec!["Red".to_string(), "user_note: prefer crimson".to_string()]
        );
    }

    #[test]
    fn user_input_response_rejects_malformed_json() {
        let reg = PromptRegistry::default();
        let (tx, _rx) = oneshot::channel::<HashMap<String, Answer>>();
        let id = reg.register(PendingPrompt::UserInput(tx));
        let err = reg.respond(&id, "not json at all").expect_err("must fail");
        assert!(err.contains("JSON answer map"));
    }

    #[test]
    fn unknown_prompt_id_is_an_error_not_a_silent_drop() {
        // Real failure mode: a misrouted PromptResponse should be visible to
        // callers, never silently swallowed (CLAUDE.md "no silent error
        // swallowing").
        let reg = PromptRegistry::default();
        let err = reg.respond("nope", "true").expect_err("must fail");
        assert!(err.contains("unknown prompt_id"));
    }

    #[test]
    fn shell_confirmation_rejects_non_boolean_strings() {
        let reg = PromptRegistry::default();
        let (tx, _rx) = oneshot::channel();
        let id = reg.register(PendingPrompt::Shell(tx));
        let err = reg.respond(&id, "maybe").expect_err("must fail");
        assert!(err.contains("expected boolean"));
    }

    #[tokio::test]
    async fn clear_drops_pending_prompts_so_agents_unblock() {
        // Real failure mode: closing a session mid-prompt would leak the
        // oneshot sender, leaving the agent hanging on `respond.await` and
        // its turn never completing.
        let reg = PromptRegistry::default();
        let (tx, rx) = oneshot::channel::<bool>();
        let _id = reg.register(PendingPrompt::Shell(tx));
        reg.clear();
        assert!(rx.await.is_err(), "sender should be dropped, recv errors");
    }
}
