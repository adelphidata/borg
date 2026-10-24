//! One run: every event of a subscription that waited together, one agent
//! turn in the subscription's own session, one report.
//!
//! Digesting is what turns a burst into a handful of turns. While a run is in
//! flight no other run of the same subscription can start (the claim refuses
//! it), so everything that arrives meanwhile waits and is handled by the next
//! run, together.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use cortex::config::Config;
use cortex::db::{Database, SubscriptionEventRow, SubscriptionRow};
use cortex::subscriptions::prompt::{build_run_prompt, PromptContext, PromptEvent};
use cortex::subscriptions::{event_status, presets, session_key, EventOrigin};
use gateway::{InboundTurn, TurnKind};

use super::bus::{kind, SubscriptionActivity, SubscriptionBus};
use super::RunHost;
use crate::heartbeat::{collect_heartbeat_response, CollectOutcome};

/// Events handled by one run, the first one included. The rest wait for the
/// next run; they are never dropped here.
pub(crate) const DIGEST_LIMIT: u32 = 50;

/// How long a run waits for the user to finish a turn in the subscription's
/// session before it hands its events back to the queue.
pub(crate) const BUSY_WAIT: Duration = Duration::from_secs(600);

/// How often a waiting run looks at the session again.
const BUSY_POLL: Duration = Duration::from_secs(1);

/// What a run ended as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOutcome {
    /// One of [`event_status`].
    pub status: &'static str,
    /// The reply, when there was one worth keeping.
    pub result: Option<String>,
    /// Why it failed, or what went wrong after it succeeded.
    pub error: Option<String>,
}

/// Map how the turn ended to a run outcome. Pure, so the mapping is tested
/// without an agent.
pub(crate) fn outcome_of(collected: CollectOutcome, timeout_secs: u64) -> RunOutcome {
    match collected {
        CollectOutcome::Completed(text) => {
            let text = text.trim();
            if text.is_empty() || cortex::agent::silence::is_silent_autonomous(text) {
                RunOutcome {
                    status: event_status::SILENT,
                    result: None,
                    error: None,
                }
            } else {
                RunOutcome {
                    status: event_status::SUCCEEDED,
                    result: Some(text.to_string()),
                    error: None,
                }
            }
        }
        CollectOutcome::Timeout => RunOutcome {
            status: event_status::FAILED,
            result: None,
            error: Some(format!(
                "Took longer than {timeout_secs} seconds and was stopped. Give it more time in Advanced, or narrow the instruction."
            )),
        },
        CollectOutcome::Failed(error) => RunOutcome {
            status: event_status::FAILED,
            result: None,
            error: Some(error),
        },
        CollectOutcome::InteractivePromptRequested(_) => RunOutcome {
            status: event_status::FAILED,
            result: None,
            error: Some(
                "Stopped to ask a question, but nobody answers a subscription run. Say in the instruction what to do instead of asking."
                    .to_string(),
            ),
        },
    }
}

/// Where a finished run's reply goes, besides the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// The session only.
    SessionOnly,
    /// A channel and a stored target.
    Channel {
        /// Channel name.
        channel: String,
        /// Bare recipient id or `{sender, thread_id, session_key}` JSON.
        target: String,
    },
}

/// Resolve the subscription's delivery against the event that led the run.
///
/// `origin` means "reply where it came from": the chat and thread of the
/// channel message that fired it. The reply goes to the chat, not the
/// sender's direct messages.
pub(crate) fn resolve_route(
    sub: &SubscriptionRow,
    lead: &SubscriptionEventRow,
) -> std::result::Result<Route, String> {
    let Some(channel) = sub.delivery_channel.as_deref().filter(|c| !c.is_empty()) else {
        return Ok(Route::SessionOnly);
    };
    if channel == cortex::heartbeat_task::ORIGIN_DELIVERY {
        let origin: EventOrigin = lead
            .origin
            .as_deref()
            .and_then(|o| serde_json::from_str(o).ok())
            .ok_or_else(|| "There is no conversation to reply to for this event.".to_string())?;
        let target = serde_json::json!({
            "sender": origin.reply_address(),
            "thread_id": origin.thread_id,
            "session_key": origin.session_key,
        })
        .to_string();
        return Ok(Route::Channel {
            channel: origin.channel,
            target,
        });
    }
    match sub.delivery_target.as_deref().filter(|t| !t.is_empty()) {
        Some(target) => Ok(Route::Channel {
            channel: channel.to_string(),
            target: target.to_string(),
        }),
        None => Err(format!("No recipient is set for {channel}.")),
    }
}

/// The delivery in plain words, for the prompt.
fn delivery_words(sub: &SubscriptionRow) -> String {
    match sub.delivery_channel.as_deref() {
        None | Some("") => "this session".to_string(),
        Some(cortex::heartbeat_task::ORIGIN_DELIVERY) => {
            "this session and the conversation the event came from".to_string()
        }
        Some(channel) => format!("this session and {channel}"),
    }
}

/// Create the subscription's session with its name as the title, so the
/// conversation list shows "Sentry errors", not the first run prompt.
pub(crate) fn materialize_session(key: &str, name: &str) {
    if cortex::session::Session::load(key).is_ok() {
        return;
    }
    let mut session = cortex::session::Session::new_with_id(key.to_string());
    session.meta.title = name.to_string();
    if let Err(e) = session.save() {
        tracing::warn!(session = %key, "subscription: could not create its session: {e}");
    }
}

/// Run the events of `lead`'s run. Errors are returned to the dispatcher,
/// which closes the run as failed with the message.
pub(crate) async fn execute(
    host: Arc<dyn RunHost>,
    config: Config,
    bus: SubscriptionBus,
    lead: SubscriptionEventRow,
) -> Result<()> {
    let Some(run_id) = lead.run_id.clone() else {
        anyhow::bail!("claimed event {} has no run", lead.id);
    };
    let db = Database::open().await?;
    let Some(sub) = db.get_subscription(&lead.subscription_id).await? else {
        // Deleted while it waited; its events went with it.
        return Ok(());
    };
    let key = session_key(&sub.id);

    // The user may be chatting in the subscription's session. An internal
    // turn never interrupts a live one; it would be dropped instead, and the
    // events with it. Wait for the session, then hand the events back.
    let waited = Instant::now();
    while host.is_busy(&key) {
        if waited.elapsed() >= BUSY_WAIT {
            db.release_subscription_run(&run_id).await?;
            tracing::info!(subscription = %sub.id, "subscription run waited for a busy session; events wait again");
            return Ok(());
        }
        tokio::time::sleep(BUSY_POLL).await;
    }

    let now = chrono::Utc::now().timestamp();
    // One reply per run: when it goes back where the events came from, the
    // digest keeps to the lead's conversation.
    let same_origin = (sub.delivery_channel.as_deref()
        == Some(cortex::heartbeat_task::ORIGIN_DELIVERY))
    .then_some(lead.origin.as_deref())
    .flatten();
    let events = db
        .claim_subscription_batch(&sub.id, &run_id, DIGEST_LIMIT - 1, now, same_origin)
        .await?;
    let count = events.len().max(1) as u32;

    let started = Instant::now();
    materialize_session(&key, &sub.name);
    let label = presets::get(&sub.provider).map_or(sub.provider.as_str(), |p| p.label);
    let delivery = delivery_words(&sub);
    let ctx = PromptContext {
        name: &sub.name,
        source_label: label,
        instruction: &sub.instruction,
        delivery: &delivery,
    };
    let prompt_events: Vec<PromptEvent<'_>> = events
        .iter()
        .map(|e| PromptEvent {
            kind: e.kind.as_deref(),
            summary: &e.summary,
            payload: &e.payload,
            received_at: e.received_at,
        })
        .collect();
    let prompt = build_run_prompt(&ctx, &prompt_events);

    let timeout_secs = u64::try_from(sub.timeout_secs).unwrap_or(1).max(1);
    let turn = InboundTurn {
        session_key: key.clone(),
        text: prompt,
        images: Vec::new(),
        kind: TurnKind::Internal,
    };
    let collected = match host.start_turn(turn).await {
        // A pre-cancelled token is the executor's "deferred": the emergency
        // stop holds, or the user started a turn in the session after the
        // busy check. No turn ran, so the events wait again; closing them
        // would report thousands of alerts as handled.
        Ok((_, cancel)) if cancel.is_cancelled() => {
            db.release_subscription_run(&run_id).await?;
            tracing::info!(subscription = %sub.id, "subscription run deferred; events wait again");
            return Ok(());
        }
        Ok((rx, cancel)) => {
            publish_started(&bus, &sub, &lead, &run_id, count, now);
            // A run dropped mid-turn (shutdown, supervisor restart) stops
            // its turn too. Left running, the turn would keep acting while
            // the boot reset hands the same events to a second run.
            let stop_turn = cancel.clone().drop_guard();
            let collected =
                collect_heartbeat_response(rx, cancel, Duration::from_secs(timeout_secs)).await;
            stop_turn.disarm();
            collected
        }
        Err(e) => {
            publish_started(&bus, &sub, &lead, &run_id, count, now);
            CollectOutcome::Failed(format!("Borg could not start the run: {e}"))
        }
    };
    let mut outcome = outcome_of(collected, timeout_secs);

    if outcome.status == event_status::SUCCEEDED {
        if let Some(reply) = outcome.result.as_deref() {
            match resolve_route(&sub, &lead) {
                Ok(Route::SessionOnly) => {}
                Ok(Route::Channel { channel, target }) => {
                    let text = format!("[{}]\n{reply}", sub.name);
                    if let Err(e) = host.deliver(&config, &channel, &target, &text).await {
                        tracing::warn!(subscription = %sub.id, "subscription delivery to {channel} failed: {e}");
                        outcome.error =
                            Some(format!("The reply could not be sent to {channel}: {e}"));
                    }
                }
                Err(why) => outcome.error = Some(why),
            }
        }
    }

    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    db.finish_subscription_run(
        &sub.id,
        &run_id,
        outcome.status,
        outcome.result.as_deref(),
        outcome.error.as_deref(),
        duration_ms,
    )
    .await?;

    if outcome.status == event_status::FAILED {
        if let Some(reason) = failure_breaker(&db, &sub, &config).await? {
            super::announce_pause(&host, &db, &config, &bus, &sub, &reason).await;
        }
    }
    if let Err(e) = db
        .prune_subscription_events(&sub.id, config.subscriptions.event_retention, now)
        .await
    {
        tracing::warn!(subscription = %sub.id, "subscription: prune failed: {e}");
    }

    bus.publish(SubscriptionActivity {
        subscription_id: sub.id.clone(),
        kind: kind::FINISHED.to_string(),
        run_id: Some(run_id),
        status: Some(outcome.status.to_string()),
        summary: outcome
            .error
            .clone()
            .or(outcome.result.clone())
            .unwrap_or_else(|| "Nothing needed attention.".to_string()),
        event_count: count,
        emitted_at_unix: chrono::Utc::now().timestamp(),
    });
    Ok(())
}

fn publish_started(
    bus: &SubscriptionBus,
    sub: &SubscriptionRow,
    lead: &SubscriptionEventRow,
    run_id: &str,
    count: u32,
    now: i64,
) {
    bus.publish(SubscriptionActivity {
        subscription_id: sub.id.clone(),
        kind: kind::STARTED.to_string(),
        run_id: Some(run_id.to_string()),
        status: None,
        summary: if count == 1 {
            lead.summary.clone()
        } else {
            format!("{count} events")
        },
        event_count: count,
        emitted_at_unix: now,
    });
}

/// Pause the subscription when its last `storm_failures` runs since it was
/// last resumed all failed. Returns the reason when this call paused it.
pub(crate) async fn failure_breaker(
    db: &Database,
    sub: &SubscriptionRow,
    config: &Config,
) -> Result<Option<String>> {
    let n = config.subscriptions.storm_failures;
    if n == 0 {
        return Ok(None);
    }
    let recent = db.recent_subscription_run_statuses(&sub.id, n).await?;
    let all_failed = recent.len() == n as usize && recent.iter().all(|s| s == event_status::FAILED);
    if !all_failed {
        return Ok(None);
    }
    let reason = format!(
        "Paused itself: the last {n} runs failed. Fix the cause in the instruction or the source, then resume it."
    );
    Ok(db
        .trip_subscription_storm(&sub.id, &reason)
        .await?
        .then_some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_endings_map_to_run_statuses() {
        let cases = [
            (
                CollectOutcome::Completed("  Found the cause.  ".into()),
                event_status::SUCCEEDED,
                true,
            ),
            (
                CollectOutcome::Completed("[SILENT]".into()),
                event_status::SILENT,
                false,
            ),
            (
                CollectOutcome::Completed("   ".into()),
                event_status::SILENT,
                false,
            ),
            (CollectOutcome::Timeout, event_status::FAILED, false),
            (
                CollectOutcome::Failed("provider down".into()),
                event_status::FAILED,
                false,
            ),
            (
                CollectOutcome::InteractivePromptRequested("x".into()),
                event_status::FAILED,
                false,
            ),
        ];
        for (collected, want, has_result) in cases {
            let label = format!("{collected:?}");
            let got = outcome_of(collected, 300);
            assert_eq!(got.status, want, "{label}");
            assert_eq!(got.result.is_some(), has_result, "{label}");
            assert_eq!(got.error.is_some(), want == event_status::FAILED, "{label}");
        }
        assert_eq!(
            outcome_of(CollectOutcome::Completed(" ok ".into()), 1)
                .result
                .as_deref(),
            Some("ok")
        );
        assert!(outcome_of(CollectOutcome::Timeout, 42)
            .error
            .unwrap()
            .contains("42 seconds"));
    }

    fn sub_row(channel: Option<&str>, target: Option<&str>) -> SubscriptionRow {
        SubscriptionRow {
            id: "sub_x".into(),
            name: "ops".into(),
            source: "channel".into(),
            provider: "slack".into(),
            auth_scheme: "none".into(),
            secret_ref: None,
            signing_secret_ref: None,
            filter: "{}".into(),
            instruction: "x".into(),
            delivery_channel: channel.map(str::to_string),
            delivery_target: target.map(str::to_string),
            status: "active".into(),
            rate_limit_per_hour: 60,
            timeout_secs: 60,
            dropped_count: 0,
            created_at: 0,
            updated_at: 0,
            last_event_at: None,
            last_run_status: None,
            last_error: None,
        }
    }

    fn event_row(origin: Option<&str>) -> SubscriptionEventRow {
        SubscriptionEventRow {
            id: "evt_1".into(),
            subscription_id: "sub_x".into(),
            kind: Some("message".into()),
            dedup_key: None,
            summary: "s".into(),
            payload: "{}".into(),
            payload_truncated: false,
            origin: origin.map(str::to_string),
            status: "running".into(),
            run_id: Some("run_1".into()),
            received_at: 0,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn origin_replies_go_to_the_chat_thread_not_the_sender() {
        let origin = r#"{"channel":"slack","sender":"U1","chat":"C9","thread_id":"1.0","session_key":"U1:1.0"}"#;
        let route =
            resolve_route(&sub_row(Some("origin"), None), &event_row(Some(origin))).unwrap();
        let Route::Channel { channel, target } = route else {
            panic!("expected a channel route");
        };
        assert_eq!(channel, "slack");
        let t: serde_json::Value = serde_json::from_str(&target).unwrap();
        assert_eq!(t["sender"], "C9");
        assert_eq!(t["thread_id"], "1.0");
        assert_eq!(t["session_key"], "U1:1.0");
        let parsed = crate::task_runner::parse_delivery_target(&target);
        assert_eq!(
            parsed.sender, "C9",
            "the task runner's parser reads the same route"
        );
    }

    #[test]
    fn routes_without_somewhere_to_go_say_why() {
        assert_eq!(
            resolve_route(&sub_row(None, None), &event_row(None)),
            Ok(Route::SessionOnly)
        );
        assert!(resolve_route(&sub_row(Some("origin"), None), &event_row(None)).is_err());
        assert!(resolve_route(&sub_row(Some("telegram"), None), &event_row(None)).is_err());
        assert_eq!(
            resolve_route(&sub_row(Some("telegram"), Some("42")), &event_row(None)),
            Ok(Route::Channel {
                channel: "telegram".into(),
                target: "42".into()
            })
        );
    }
}
