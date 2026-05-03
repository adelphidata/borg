//! Convert `cortex::agent::AgentEvent` → wire `protocols::session::AgentEvent`.
//!
//! Two variants carry an `oneshot::Sender` for the user's reply
//! (`ShellConfirmation`, `UserInputRequest`). On the wire we replace the
//! sender with a `prompt_id` and stash the sender in the per-session
//! [`PromptRegistry`]; the client later calls
//! `RespondToPrompt(prompt_id, value)` to deliver the reply.

use super::prompts::{PendingPrompt, PromptRegistry};
use cortex::agent::AgentEvent as CoreEvent;
use protocols::session as pb;
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
                    // as the lookup key. Wire schema keeps `id` so adding an
                    // explicit id field later is non-breaking.
                    id: String::new(),
                    text: s.title,
                    status: match s.status {
                        cortex::types::PlanStepStatus::Pending => "pending".into(),
                        cortex::types::PlanStepStatus::InProgress => "in_progress".into(),
                        cortex::types::PlanStepStatus::Completed => "completed".into(),
                    },
                })
                .collect(),
        }),
        CoreEvent::UserInputRequest { questions, respond } => {
            let prompt_id = prompts.register(PendingPrompt::UserInput(respond));
            // Mirror the first question into the legacy `prompt` slot so
            // older clients (if any) still see something readable; new clients
            // read the structured `questions` field exclusively.
            let legacy_prompt = questions
                .first()
                .map(|q| q.question.clone())
                .unwrap_or_default();
            Kind::UserInputRequest(pb::UserInputRequest {
                prompt: legacy_prompt,
                choices: Vec::new(),
                allow_custom: true,
                prompt_id,
                questions: questions
                    .into_iter()
                    .map(|q| pb::UserInputQuestion {
                        id: q.id,
                        header: q.header,
                        question: q.question,
                        multi_select: q.multi_select,
                        options: q
                            .options
                            .into_iter()
                            .map(|o| pb::UserInputQuestionOption {
                                label: o.label,
                                description: o.description,
                            })
                            .collect(),
                    })
                    .collect(),
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
        CoreEvent::SystemNotice { category, text } => {
            Kind::SystemNotice(pb::SystemNotice { category, text })
        }
    };
    pb::AgentEvent {
        event_seq: 0, // assigned by SequencedBuffer::push
        kind: Some(kind),
    }
}

/// Translate a wire `pb::AgentEvent` back into a `core::AgentEvent` for
/// in-process consumers (e.g. the gateway's `SessionExecutor` bridge).
pub fn from_proto(event: pb::AgentEvent) -> Option<CoreEvent> {
    use pb::agent_event::Kind;
    let kind = event.kind?;
    Some(match kind {
        Kind::TextDelta(t) => CoreEvent::TextDelta(t.text),
        Kind::ThinkingDelta(t) => CoreEvent::ThinkingDelta(t.text),
        Kind::ToolExecuting(t) => CoreEvent::ToolExecuting {
            name: t.name,
            args: t.args_json,
        },
        Kind::ToolResult(t) => CoreEvent::ToolResult {
            name: t.name,
            result: t.result,
        },
        Kind::ToolOutputDelta(t) => CoreEvent::ToolOutputDelta {
            name: t.name,
            delta: t.delta,
            is_stderr: t.is_stderr,
        },
        Kind::Usage(u) => CoreEvent::Usage(cortex::llm::UsageData {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
            cached_input_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            provider: String::new(),
            model: String::new(),
            provider_cost_usd: None,
        }),
        Kind::SubAgentUpdate(s) => CoreEvent::SubAgentUpdate {
            agent_id: s.agent_id,
            nickname: s.nickname,
            status: s.status,
        },
        Kind::SteerReceived(s) => CoreEvent::SteerReceived { text: s.text },
        Kind::PlanUpdated(p) => CoreEvent::PlanUpdated {
            steps: p
                .steps
                .into_iter()
                .map(|s| cortex::types::PlanStep {
                    title: s.text,
                    status: match s.status.as_str() {
                        "in_progress" => cortex::types::PlanStepStatus::InProgress,
                        "completed" => cortex::types::PlanStepStatus::Completed,
                        _ => cortex::types::PlanStepStatus::Pending,
                    },
                })
                .collect(),
        },
        Kind::Preparing(_) => CoreEvent::Preparing,
        Kind::TurnComplete(_) => CoreEvent::TurnComplete,
        Kind::HistoryCompacted(h) => CoreEvent::HistoryCompacted {
            dropped: h.dropped as usize,
            before_tokens: h.before_tokens as usize,
            after_tokens: h.after_tokens as usize,
            iterative: h.iterative,
        },
        Kind::Error(e) => CoreEvent::Error(e.message),
        Kind::SystemNotice(s) => CoreEvent::SystemNotice {
            category: s.category,
            text: s.text,
        },
        Kind::ShellConfirmation(sc) => {
            tracing::warn!(prompt_id = %sc.prompt_id, command = %sc.command,
                "from_proto: dropping ShellConfirmation");
            return None;
        }
        Kind::UserInputRequest(ui) => {
            tracing::warn!(prompt_id = %ui.prompt_id, prompt = %ui.prompt,
                "from_proto: dropping UserInputRequest");
            return None;
        }
    })
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
    use cortex::tool_handlers::user_input::{Answer, Question, QuestionOption};
    use std::collections::HashMap;
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
    fn user_input_request_serializes_question_batch_and_round_trips_answers() {
        // Real failure mode: a regression that drops the structured
        // questions[] field would leave the new TUI carousel with nothing
        // to render and the LLM would silently see empty answers back.
        let prompts = Arc::new(PromptRegistry::default());
        let (tx, _rx) = oneshot::channel::<HashMap<String, Answer>>();
        let event = CoreEvent::UserInputRequest {
            questions: vec![Question {
                id: "color".into(),
                header: "Color".into(),
                question: "Pick a color".into(),
                multi_select: false,
                options: vec![
                    QuestionOption {
                        label: "Red".into(),
                        description: "warm".into(),
                    },
                    QuestionOption {
                        label: "Blue".into(),
                        description: "cool".into(),
                    },
                ],
            }],
            respond: tx,
        };
        let proto = to_proto(event, &prompts);
        let pb::agent_event::Kind::UserInputRequest(ui) = proto.kind.expect("kind") else {
            panic!("expected UserInputRequest");
        };
        assert_eq!(ui.questions.len(), 1);
        assert_eq!(ui.questions[0].id, "color");
        assert_eq!(ui.questions[0].options.len(), 2);
        assert_eq!(ui.questions[0].options[0].label, "Red");
        assert!(!ui.prompt_id.is_empty());
        // And the daemon can resolve the matching answer payload through the registry.
        prompts
            .respond(&ui.prompt_id, r#"{"color":{"answers":["Red"]}}"#)
            .expect("respond ok");
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
