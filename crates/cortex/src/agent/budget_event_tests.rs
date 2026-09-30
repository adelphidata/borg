//! The events a budget decision puts on the turn stream. Headless callers map
//! an `Error` event to a failed exit code, so the event kind is a contract.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::tests::minimal_test_agent;
use super::{budget_check_events, AgentEvent, BudgetCheck};
use crate::config::Config;
use crate::hooks::{Hook, HookAction, HookContext, HookPoint};
use crate::types::Message;

/// Short tag per event, so a whole sequence compares in one assert.
fn tags(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::Error(msg) => format!("error:{msg}"),
            AgentEvent::SystemNotice { category, text } => format!("notice:{category}:{text}"),
            AgentEvent::TextDelta(text) => format!("text:{text}"),
            AgentEvent::TurnComplete => "turn_complete".to_string(),
            _ => "other".to_string(),
        })
        .collect()
}

#[test]
fn budget_check_maps_to_the_right_events() {
    let cases = [
        (BudgetCheck::Ok, Vec::<&str>::new(), false),
        (
            BudgetCheck::Warning("90% used".to_string()),
            vec!["notice:budget:90% used"],
            false,
        ),
        (
            BudgetCheck::Exceeded("limit reached".to_string()),
            vec!["error:limit reached", "turn_complete"],
            true,
        ),
    ];
    for (check, expected, expected_stop) in cases {
        let (events, stops_turn) = budget_check_events(check);
        assert_eq!(tags(&events), expected);
        assert_eq!(stops_turn, expected_stop);
    }
}

struct RefuseLlmCall;

#[async_trait]
impl Hook for RefuseLlmCall {
    fn name(&self) -> &str {
        "refuse-llm-call"
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::BeforeAgentStart, HookPoint::BeforeLlmCall]
    }

    async fn execute(&self, _ctx: &HookContext) -> HookAction {
        HookAction::Skip { reason: None }
    }
}

#[tokio::test]
async fn hard_cap_refusal_sends_an_error_before_the_turn_completes() {
    let mut config = Config::default();
    // The monthly gate reads the live database; this test is about the
    // pre-flight refusal only.
    config.budget.monthly_token_limit = 0;
    let mut agent = minimal_test_agent(config);
    agent.hook_registry.register(Box::new(RefuseLlmCall));
    agent.history.push(Message::user("hello"));

    let (tx, mut rx) = mpsc::channel(64);
    agent
        .run_agent_loop(tx, tokio_util::sync::CancellationToken::new())
        .await
        .expect("a refused turn ends cleanly");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    // The refusal is saved to the history but is not streamed as text, so
    // the error event is the only signal a client gets.
    assert_eq!(
        tags(&events),
        vec![
            "error:Budget hard cap reached — agent paused. Raise cap in /settings → Budgets, \
             or wait for daily reset."
                .to_string(),
            "turn_complete".to_string(),
        ]
    );
}
