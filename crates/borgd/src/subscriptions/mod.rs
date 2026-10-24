//! Subscriptions inside the daemon: the dispatcher that turns waiting events
//! into agent runs, Borg's own events as a source, and the activity bus the
//! app listens to.
//!
//! Producers (the gateway endpoint, the channel tap, [`internal`]) only write
//! rows to `subscription_events` and poke [`cortex::subscriptions::wake`].
//! Everything that costs model usage starts here, under one set of limits:
//! one run per subscription at a time, `subscriptions.max_concurrent` runs at
//! once, the hourly budget in the claim, and the daily cap.

pub mod bus;
pub(crate) mod dispatcher;
pub(crate) mod internal;
pub(crate) mod run;

use std::sync::Arc;

use async_trait::async_trait;
use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::db::{Database, SubscriptionRow};
use cortex::subscriptions::session_key;
use gateway::{InboundTurn, SessionExecutor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use bus::{SubscriptionActivity, SubscriptionBus};

use crate::session::DaemonSessionExecutor;

/// What a run needs from the daemon. A trait so the dispatcher is tested
/// with a recording stand-in instead of a model and a chat network.
#[async_trait]
pub(crate) trait RunHost: Send + Sync + 'static {
    /// Start an agent turn.
    async fn start_turn(
        &self,
        turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)>;
    /// Whether a turn is in flight in `session_key` (the user chatting in
    /// the subscription's session).
    fn is_busy(&self, session_key: &str) -> bool;
    /// Whether the emergency stop (`borg pause`) refuses new turns.
    async fn emergency_stopped(&self) -> bool;
    /// Send `text` along a stored `(channel, target)` route.
    async fn deliver(
        &self,
        config: &Config,
        channel: &str,
        target: &str,
        text: &str,
    ) -> anyhow::Result<()>;
}

/// The production host: the daemon's session executor and channel clients.
pub(crate) struct DaemonRunHost {
    /// Shared with every other lane.
    pub executor: Arc<DaemonSessionExecutor>,
}

#[async_trait]
impl RunHost for DaemonRunHost {
    async fn start_turn(
        &self,
        turn: InboundTurn,
    ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
        self.executor.start_turn(turn).await
    }

    fn is_busy(&self, session_key: &str) -> bool {
        self.executor.is_session_busy(session_key)
    }

    async fn emergency_stopped(&self) -> bool {
        crate::session::executor::emergency_pause_notice()
            .await
            .is_some()
    }

    async fn deliver(
        &self,
        config: &Config,
        channel: &str,
        target: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        crate::channel_delivery::deliver_with_mirror(
            config,
            channel,
            target,
            text,
            Some(&self.executor),
        )
        .await
    }
}

/// Run the dispatcher over the daemon's session executor until `cancel`.
/// The orchestrator supervises one of these; the e2e tests run one beside a
/// real gateway. Bumps `liveness` every pass.
pub async fn run_dispatcher(
    executor: Arc<DaemonSessionExecutor>,
    config_rx: tokio::sync::watch::Receiver<Config>,
    bus: SubscriptionBus,
    cancel: CancellationToken,
    liveness: Arc<std::sync::atomic::AtomicU64>,
) -> anyhow::Result<()> {
    dispatcher::Dispatcher {
        host: Arc::new(DaemonRunHost { executor }),
        config_rx,
        bus,
        defer_backoff: dispatcher::DEFER_BACKOFF,
    }
    .run(cancel, liveness)
    .await
}

/// Tell the user, once, that a subscription paused itself: the app (alert
/// dot and banner, through the bus), a line in the subscription's session,
/// and the delivery channel when it has a fixed one. Best-effort on each
/// path; a failure is logged and the others still run. The channel send
/// runs on its own task: a chat network that does not answer must not stall
/// the dispatcher loop that called this.
pub(crate) async fn announce_pause(
    host: &Arc<dyn RunHost>,
    db: &Database,
    config: &Config,
    bus: &SubscriptionBus,
    sub: &SubscriptionRow,
    reason: &str,
) {
    let now = chrono::Utc::now().timestamp();
    bus.publish(SubscriptionActivity {
        subscription_id: sub.id.clone(),
        kind: bus::kind::STORM.to_string(),
        run_id: None,
        status: Some(cortex::subscriptions::status::STORM.to_string()),
        summary: reason.to_string(),
        event_count: 0,
        emitted_at_unix: now,
    });
    let key = session_key(&sub.id);
    run::materialize_session(&key, &sub.name);
    if let Err(e) = db
        .insert_message(&key, "assistant", Some(reason), None, None, None, None)
        .await
    {
        tracing::warn!(subscription = %sub.id, "subscription: could not note the pause in its session: {e}");
    }
    // "Reply where it came from" has no single place to go without an
    // event; the app and the session carry the notice then.
    let fixed = sub
        .delivery_channel
        .as_deref()
        .filter(|c| !c.is_empty() && *c != cortex::heartbeat_task::ORIGIN_DELIVERY);
    if let (Some(channel), Some(target)) = (fixed, sub.delivery_target.clone()) {
        let text = format!("[{}]\n{reason}", sub.name);
        let (host, config, channel, id) = (
            host.clone(),
            config.clone(),
            channel.to_string(),
            sub.id.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = host.deliver(&config, &channel, &target, &text).await {
                tracing::warn!(subscription = %id, "subscription: pause notice to {channel} failed: {e}");
            }
        });
    }
}
