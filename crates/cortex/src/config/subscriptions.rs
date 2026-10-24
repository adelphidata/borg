use serde::{Deserialize, Serialize};

/// Subscriptions: outside events (a webhook, a channel message, Borg's own
/// goals and tasks) that wake the agent with an instruction.
///
/// The defaults are the storm-control budget. Every limit counts against a
/// single subscription unless its name says otherwise, so one noisy source
/// cannot use up the budget of the others.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionsConfig {
    /// Master switch. Off: the endpoint refuses events, the dispatcher runs
    /// nothing, and the `subscribe` tool is not offered to the model.
    pub enabled: bool,
    /// Subscription runs that may be in flight at once, across every
    /// subscription. Separate from `tasks.max_concurrent` so a burst cannot
    /// starve scheduled tasks.
    pub max_concurrent: usize,
    /// Runs (not events) a new subscription may start per hour.
    pub default_rate_limit_per_hour: u32,
    /// Runs every subscription together may start in one day.
    pub max_runs_per_day: u32,
    /// Events one subscription accepts per minute. Past it the endpoint
    /// answers 429 and stores nothing, so a burst never fills the ledger.
    pub ingest_burst: u32,
    /// Accepted events in ten minutes that make a subscription pause itself.
    pub storm_threshold: u32,
    /// Failed runs in a row that make a subscription pause itself.
    pub storm_failures: u32,
    /// Agent-loop steps one run may take; well under
    /// `conversation.max_iterations`, because nobody is watching it.
    pub max_steps: u32,
    /// Events one subscription may have waiting. Past it new events are kept
    /// as `skipped` so the history shows what was refused.
    pub max_pending: u32,
    /// Seconds a new subscription's run may take.
    pub default_timeout_secs: u64,
    /// Events kept per subscription; older ones are pruned.
    pub event_retention: u32,
    /// Bytes of an event payload kept; the rest is cut and marked.
    pub payload_max_bytes: usize,
}

impl Default for SubscriptionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: 2,
            default_rate_limit_per_hour: 60,
            max_runs_per_day: 500,
            ingest_burst: 120,
            storm_threshold: 300,
            storm_failures: 5,
            max_steps: 40,
            max_pending: 200,
            default_timeout_secs: 300,
            event_retention: 50,
            payload_max_bytes: 64 * 1024,
        }
    }
}
