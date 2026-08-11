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

/// `SystemNotice.category` emitted by [`from_proto`] when the wire event was a
/// `UserInputRequest`. The two prompt-bearing variants (`UserInputRequest`,
/// `ShellConfirmation`) carry an `oneshot::Sender` on the core side that the
/// wire path has already lowered to a `prompt_id` in the per-session
/// [`PromptRegistry`]. Reverse-mapping back into the original `CoreEvent`
/// variant would require fabricating a sender, so for in-process consumers
/// (e.g. the heartbeat dispatcher's executor bridge) we surface the request
/// as a sender-free `SystemNotice` instead. Heartbeat code matches on this
/// constant to short-circuit non-interactive turns rather than waiting for
/// the 5-minute oneshot timeout in `request_user_input`.
pub const PROMPT_REQUEST_USER_INPUT: &str = "prompt_request:user_input";

/// Companion to [`PROMPT_REQUEST_USER_INPUT`] for `ShellConfirmation`.
pub const PROMPT_REQUEST_SHELL: &str = "prompt_request:shell";

/// `SystemNotice.category` for a dropped provider connection the client is
/// waiting to retry. Remote surfaces can style it as a transient status
/// instead of an error — the turn has not failed.
pub const NOTICE_RECONNECTING: &str = "reconnecting";

/// Translate one core event. Returns `None` when the event has no useful wire
/// representation — currently only `LlmAttemptStarted`, which is an in-process
/// liveness marker for the sub-agent stall watchdog and carries nothing a
/// remote client could render.
pub fn to_proto(event: CoreEvent, prompts: &Arc<PromptRegistry>) -> Option<pb::AgentEvent> {
    use pb::agent_event::Kind;
    let kind = match event {
        CoreEvent::LlmAttemptStarted { .. } => return None,
        // Unlike the liveness marker above, this one is for the user: the
        // client is about to sit still for seconds to a minute, and silence
        // reads as a hang. Carried as a notice rather than a new wire kind —
        // it needs no structure a remote surface would act on.
        CoreEvent::LlmReconnecting { delay_secs, .. } => Kind::SystemNotice(pb::SystemNotice {
            category: NOTICE_RECONNECTING.to_string(),
            text: format!("Reconnecting — waiting for network. Retrying in {delay_secs}s."),
        }),
        CoreEvent::TextDelta(text) => Kind::TextDelta(pb::TextDelta { text }),
        CoreEvent::ThinkingDelta(text) => Kind::ThinkingDelta(pb::ThinkingDelta { text }),
        CoreEvent::ToolExecuting {
            tool_call_id,
            name,
            args,
        } => Kind::ToolExecuting(pb::ToolExecuting {
            tool_call_id,
            name,
            args_json: args,
        }),
        CoreEvent::ToolResult {
            tool_call_id,
            name,
            result,
        } => Kind::ToolResult(pb::ToolResult {
            tool_call_id,
            name,
            result,
        }),
        CoreEvent::ShellConfirmation { command, respond } => {
            let prompt_id = prompts.register(PendingPrompt::Shell(respond));
            Kind::ShellConfirmation(pb::ShellConfirmation { command, prompt_id })
        }
        CoreEvent::ToolOutputDelta {
            tool_call_id,
            name,
            delta,
            is_stderr,
        } => Kind::ToolOutputDelta(pb::ToolOutputDelta {
            tool_call_id,
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
                    status: s.status.as_wire_str().into(),
                })
                .collect(),
        }),
        CoreEvent::UserInputRequest { questions, respond } => {
            let prompt_id = prompts.register(PendingPrompt::UserInput(respond));
            Kind::UserInputRequest(pb::UserInputRequest {
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
    Some(pb::AgentEvent {
        event_seq: 0, // assigned by SequencedBuffer::push
        kind: Some(kind),
    })
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
            tool_call_id: t.tool_call_id,
            name: t.name,
            args: t.args_json,
        },
        Kind::ToolResult(t) => CoreEvent::ToolResult {
            tool_call_id: t.tool_call_id,
            name: t.name,
            result: t.result,
        },
        Kind::ToolOutputDelta(t) => CoreEvent::ToolOutputDelta {
            tool_call_id: t.tool_call_id,
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
                    status: cortex::types::PlanStepStatus::from_wire_str(&s.status),
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
        Kind::ShellConfirmation(sc) => CoreEvent::SystemNotice {
            category: PROMPT_REQUEST_SHELL.into(),
            text: sc.command,
        },
        Kind::UserInputRequest(ui) => CoreEvent::SystemNotice {
            category: PROMPT_REQUEST_USER_INPUT.into(),
            text: format!("{} pending", ui.questions.len()),
        },
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
        let proto = to_proto(event, &prompts).expect("ShellConfirmation has a wire form");
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
        let proto = to_proto(event, &prompts).expect("UserInputRequest has a wire form");
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
    fn from_proto_user_input_request_translates_to_system_notice() {
        // Real failure mode: returning None here (the pre-fix behavior) made
        // the executor bridge swallow the event; an in-process heartbeat
        // consumer then waited on a never-closing channel until the
        // request_user_input tool's 5-minute oneshot fired. Surfacing as a
        // SystemNotice is what lets the heartbeat short-circuit and emit a
        // terminal bus event inside the smorgasborg eval's 90s budget.
        let proto = pb::AgentEvent {
            event_seq: 7,
            kind: Some(pb::agent_event::Kind::UserInputRequest(
                pb::UserInputRequest {
                    prompt_id: "p-1".into(),
                    questions: vec![pb::UserInputQuestion {
                        id: "q1".into(),
                        header: "h".into(),
                        question: "ask".into(),
                        multi_select: false,
                        options: vec![],
                    }],
                },
            )),
        };
        let core = from_proto(proto).expect("must not be dropped");
        match core {
            CoreEvent::SystemNotice { category, text } => {
                assert_eq!(category, PROMPT_REQUEST_USER_INPUT);
                assert!(
                    text.contains('1'),
                    "text should reflect the question count, got {text:?}"
                );
            }
            _ => panic!("expected SystemNotice"),
        }
    }

    #[test]
    fn from_proto_shell_confirmation_translates_to_system_notice() {
        // Symmetric guard for ShellConfirmation — the same silent-drop bug
        // class. Heartbeat / hook in-process consumers must observe the
        // event in some form so they can decide to cancel the turn.
        let proto = pb::AgentEvent {
            event_seq: 11,
            kind: Some(pb::agent_event::Kind::ShellConfirmation(
                pb::ShellConfirmation {
                    prompt_id: "p-2".into(),
                    command: "rm -rf /tmp/foo".into(),
                },
            )),
        };
        let core = from_proto(proto).expect("must not be dropped");
        match core {
            CoreEvent::SystemNotice { category, text } => {
                assert_eq!(category, PROMPT_REQUEST_SHELL);
                assert_eq!(text, "rm -rf /tmp/foo");
            }
            _ => panic!("expected SystemNotice"),
        }
    }

    #[test]
    fn from_proto_handles_every_oneof_variant_without_dropping() {
        // Exhaustive regression guard: every `pb::agent_event::Kind` variant
        // must produce `Some(_)`. The original bug was a silent
        // `return None` for two variants — this test would have caught it,
        // and would catch any future variant author who copy-pastes the
        // same shape. Pairs with the type-level `match` in `from_proto` so
        // adding a new Kind variant forces both a mapping and a row here.
        use pb::agent_event::Kind;
        let cases: Vec<(&'static str, Kind)> = vec![
            (
                "TextDelta",
                Kind::TextDelta(pb::TextDelta { text: "hi".into() }),
            ),
            (
                "ThinkingDelta",
                Kind::ThinkingDelta(pb::ThinkingDelta { text: "t".into() }),
            ),
            (
                "ToolExecuting",
                Kind::ToolExecuting(pb::ToolExecuting {
                    tool_call_id: "id".into(),
                    name: "n".into(),
                    args_json: "{}".into(),
                }),
            ),
            (
                "ToolResult",
                Kind::ToolResult(pb::ToolResult {
                    tool_call_id: "id".into(),
                    name: "n".into(),
                    result: "r".into(),
                }),
            ),
            (
                "ShellConfirmation",
                Kind::ShellConfirmation(pb::ShellConfirmation {
                    prompt_id: "p".into(),
                    command: "ls".into(),
                }),
            ),
            (
                "ToolOutputDelta",
                Kind::ToolOutputDelta(pb::ToolOutputDelta {
                    tool_call_id: "id".into(),
                    name: "n".into(),
                    delta: "x".into(),
                    is_stderr: false,
                }),
            ),
            (
                "Usage",
                Kind::Usage(pb::Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
            ),
            (
                "SubAgentUpdate",
                Kind::SubAgentUpdate(pb::SubAgentUpdate {
                    agent_id: "a".into(),
                    nickname: "n".into(),
                    status: "s".into(),
                }),
            ),
            (
                "SteerReceived",
                Kind::SteerReceived(pb::SteerReceived { text: "s".into() }),
            ),
            (
                "PlanUpdated",
                Kind::PlanUpdated(pb::PlanUpdated { steps: vec![] }),
            ),
            (
                "UserInputRequest",
                Kind::UserInputRequest(pb::UserInputRequest {
                    prompt_id: "p".into(),
                    questions: vec![],
                }),
            ),
            ("Preparing", Kind::Preparing(pb::Preparing {})),
            ("TurnComplete", Kind::TurnComplete(pb::TurnComplete {})),
            (
                "HistoryCompacted",
                Kind::HistoryCompacted(pb::HistoryCompacted {
                    dropped: 0,
                    before_tokens: 0,
                    after_tokens: 0,
                    iterative: false,
                }),
            ),
            (
                "Error",
                Kind::Error(pb::ErrorEvent {
                    message: "x".into(),
                }),
            ),
            (
                "SystemNotice",
                Kind::SystemNotice(pb::SystemNotice {
                    category: "info".into(),
                    text: "hello".into(),
                }),
            ),
        ];
        for (name, kind) in cases {
            let event = pb::AgentEvent {
                event_seq: 1,
                kind: Some(kind),
            };
            assert!(
                from_proto(event).is_some(),
                "from_proto must not silently drop wire variant {name}"
            );
        }
    }

    #[test]
    fn turn_complete_and_error_are_terminal_other_events_are_not() {
        // Drives the "close the per-turn stream" decision in Session.Send.
        let prompts = Arc::new(PromptRegistry::default());
        let wire = |e: CoreEvent| to_proto(e, &prompts).expect("event has a wire form");
        assert!(is_terminal(&wire(CoreEvent::TurnComplete)));
        assert!(is_terminal(&wire(CoreEvent::Error("boom".into()))));
        assert!(!is_terminal(&wire(CoreEvent::TextDelta("hi".into()))));
        assert!(!is_terminal(&wire(CoreEvent::Preparing)));
    }

    #[test]
    fn liveness_only_events_are_dropped_before_the_wire() {
        // `LlmAttemptStarted` exists so the in-process sub-agent stall watchdog
        // can tell a slow provider from a wedged child. It carries nothing a
        // remote client could render, and forwarding it would burn a sequence
        // number per retry on every session stream.
        let prompts = Arc::new(PromptRegistry::default());
        assert!(
            to_proto(CoreEvent::LlmAttemptStarted { attempt: 0 }, &prompts).is_none(),
            "liveness markers must not reach the wire"
        );
    }
}
