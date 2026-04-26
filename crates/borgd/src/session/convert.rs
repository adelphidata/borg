//! Convert `borg_core::agent::AgentEvent` → wire `borg_proto::session::AgentEvent`.
//!
//! Two variants carry an `oneshot::Sender` for the user's reply
//! (`ShellConfirmation`, `UserInputRequest`). On the wire we replace the
//! sender with a `prompt_id` and stash the sender in the per-session
//! [`PromptRegistry`](super::prompts::PromptRegistry); the client later calls
//! `RespondToPrompt(prompt_id, value)` to deliver the reply.

use super::prompts::{PendingPrompt, PromptRegistry};
use borg_core::agent::AgentEvent as CoreEvent;
use borg_proto::session as pb;
use std::sync::Arc;

/// Translate one core event. Returns `None` only when the event has no
/// useful wire representation (currently: no such case — every variant maps).
pub fn to_proto(event: CoreEvent, prompts: &Arc<PromptRegistry>) -> pb::AgentEvent {
    use pb::agent_event::Kind;
    let kind = match event {
        CoreEvent::TextDelta(text) => Kind::TextDelta(pb::TextDelta { text }),
        CoreEvent::ThinkingDelta(text) => Kind::ThinkingDelta(pb::ThinkingDelta { text }),
        CoreEvent::ToolExecuting { name, args } => Kind::ToolExecuting(pb::ToolExecuting {
            name,
            args_json: args,
        }),
        CoreEvent::ToolResult { name, result } => Kind::ToolResult(pb::ToolResult { name, result }),
        CoreEvent::ShellConfirmation { command, respond } => {
            let prompt_id = prompts.register(PendingPrompt::Shell(respond));
            Kind::ShellConfirmation(pb::ShellConfirmation { command, prompt_id })
        }
        CoreEvent::ToolOutputDelta {
            name,
            delta,
            is_stderr,
        } => Kind::ToolOutputDelta(pb::ToolOutputDelta {
            name,
            delta,
            is_stderr,
        }),
        CoreEvent::Usage(usage) => Kind::Usage(pb::Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cache_read_tokens: usage.cached_input_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        }),
        CoreEvent::SubAgentUpdate {
            agent_id,
            nickname,
            status,
        } => Kind::SubAgentUpdate(pb::SubAgentUpdate {
            agent_id,
            nickname,
            status,
        }),
        CoreEvent::SteerReceived { text } => Kind::SteerReceived(pb::SteerReceived { text }),
        CoreEvent::PlanUpdated { steps } => Kind::PlanUpdated(pb::PlanUpdated {
            steps: steps
                .into_iter()
                .map(|s| pb::PlanStep {
                    // PlanStep currently has no stable id — use the title slot
                    // as the lookup key. Wire schema keeps `id` for forward
                    // compat when an explicit id field lands in core.
                    id: String::new(),
                    text: s.title,
                    status: match s.status {
                        borg_core::types::PlanStepStatus::Pending => "pending".into(),
                        borg_core::types::PlanStepStatus::InProgress => "in_progress".into(),
                        borg_core::types::PlanStepStatus::Completed => "completed".into(),
                    },
                })
                .collect(),
        }),
        CoreEvent::UserInputRequest {
            prompt,
            choices,
            allow_custom,
            respond,
        } => {
            let prompt_id = prompts.register(PendingPrompt::UserInput(respond));
            Kind::UserInputRequest(pb::UserInputRequest {
                prompt,
                choices: choices
                    .into_iter()
                    .map(|c| pb::UserInputChoice {
                        label: c.label,
                        description: c.description.unwrap_or_default(),
                    })
                    .collect(),
                allow_custom,
                prompt_id,
            })
        }
        CoreEvent::Preparing => Kind::Preparing(pb::Preparing {}),
        CoreEvent::TurnComplete => Kind::TurnComplete(pb::TurnComplete {}),
        CoreEvent::HistoryCompacted {
            dropped,
            before_tokens,
            after_tokens,
            iterative,
        } => Kind::HistoryCompacted(pb::HistoryCompacted {
            dropped: dropped as u64,
            before_tokens: before_tokens as u64,
            after_tokens: after_tokens as u64,
            iterative,
        }),
        CoreEvent::Error(message) => Kind::Error(pb::ErrorEvent { message }),
    };
    pb::AgentEvent {
        event_seq: 0, // assigned by SequencedBuffer::push
        kind: Some(kind),
    }
}

/// Returns true if the event terminates a turn (TurnComplete or Error). Used
/// by `Session.Send` to know when to close the per-turn server-stream.
pub fn is_terminal(event: &pb::AgentEvent) -> bool {
    use pb::agent_event::Kind;
    matches!(
        event.kind,
        Some(Kind::TurnComplete(_)) | Some(Kind::Error(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use borg_core::tool_handlers::user_input::UserInputChoice;
    use tokio::sync::oneshot;

    #[test]
    fn shell_confirmation_extracts_sender_and_assigns_prompt_id() {
        // Real failure mode: forgetting to register the sender would mean the
        // client's RespondToPrompt could never resolve, and the agent would
        // hang forever on `respond.await`.
        let prompts = Arc::new(PromptRegistry::default());
        let (tx, _rx) = oneshot::channel::<bool>();
        let event = CoreEvent::ShellConfirmation {
            command: "ls".into(),
            respond: tx,
        };
        let proto = to_proto(event, &prompts);
        let pb::agent_event::Kind::ShellConfirmation(sc) = proto.kind.expect("kind") else {
            panic!("expected ShellConfirmation");
        };
        assert!(!sc.prompt_id.is_empty(), "prompt_id must be allocated");
        // And the registry can route a reply back through it.
        prompts.respond(&sc.prompt_id, "true").expect("respond ok");
    }

    #[test]
    fn user_input_request_carries_choices_and_prompt_id() {
        let prompts = Arc::new(PromptRegistry::default());
        let (tx, _rx) = oneshot::channel::<String>();
        let event = CoreEvent::UserInputRequest {
            prompt: "pick one".into(),
            choices: vec![UserInputChoice {
                label: "red".into(),
                description: Some("warm".into()),
            }],
            allow_custom: true,
            respond: tx,
        };
        let proto = to_proto(event, &prompts);
        let pb::agent_event::Kind::UserInputRequest(ui) = proto.kind.expect("kind") else {
            panic!("expected UserInputRequest");
        };
        assert_eq!(ui.choices.len(), 1);
        assert_eq!(ui.choices[0].label, "red");
        assert_eq!(ui.choices[0].description, "warm");
        assert!(ui.allow_custom);
        assert!(!ui.prompt_id.is_empty());
    }

    #[test]
    fn turn_complete_and_error_are_terminal_other_events_are_not() {
        // Drives the "close the per-turn stream" decision in Session.Send.
        let prompts = Arc::new(PromptRegistry::default());
        assert!(is_terminal(&to_proto(CoreEvent::TurnComplete, &prompts)));
        assert!(is_terminal(&to_proto(
            CoreEvent::Error("boom".into()),
            &prompts
        )));
        assert!(!is_terminal(&to_proto(
            CoreEvent::TextDelta("hi".into()),
            &prompts
        )));
        assert!(!is_terminal(&to_proto(CoreEvent::Preparing, &prompts)));
    }
}
