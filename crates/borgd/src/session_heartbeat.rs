//! Idle poller for session heartbeats.
//!
//! A session heartbeat is a recurring prompt that re-enters *its own*
//! conversation — same history, same context, same prompt-cache prefix — when
//! that session is idle and the interval has elapsed. It is the in-conversation
//! counterpart to the scheduler in the `heartbeat` crate, which dispatches
//! autonomous turns into a fresh session per tick. If the recurring prompt
//! needs what was said earlier in the chat, it belongs here; if it is a
//! self-contained job that must survive a daemon restart, it belongs there.
//!
//! Firing rules, all load-bearing:
//!
//! * **Idle only.** A due tick never interrupts a running turn; it waits for
//!   the next poll after the turn ends.
//! * **Missed ticks coalesce.** The timer re-anchors on *fire*, so a session
//!   busy through six intervals owes one check-in, not six.
//! * **Live sessions only.** A heartbeat for a session that isn't currently
//!   hosted simply waits — this is in-process by design.
//! * **Plain user message.** No system-prompt mutation and no toolset change,
//!   so the provider's cached prefix survives the injection.

use std::sync::Arc;
use std::time::Duration;

use cortex::db::Database;
use tokio_util::sync::CancellationToken;

use crate::session::SessionRegistry;

/// How often the poller looks for due heartbeats.
///
/// Well under the 60s minimum interval so a heartbeat fires promptly once its
/// session goes idle, but coarse enough that an idle daemon is not doing
/// meaningful work. The poll only touches the DB when a live session exists.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Appended to every injected prompt.
///
/// Without it an idle check-in reliably manufactures work: the model reads a
/// bare instruction, finds nothing to report, and invents something to justify
/// the turn. Saying "nothing changed" has to be an explicitly acceptable answer.
pub const NO_BUSYWORK_GUARD: &str = "\n\n(This is a recurring check-in that fired on a timer, \
     not a new request from the user. If nothing meaningful has changed, say so briefly and stop \
     — do not invent work, and do not repeat what you already reported.)";

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Text injected for `prompt`.
pub fn injected_text(prompt: &str) -> String {
    format!("{prompt}{NO_BUSYWORK_GUARD}")
}

/// Run the poll loop until `cancel` fires.
///
/// Returns `Ok(())` on cancellation. Errors from a single poll are logged and
/// the loop continues — a transient DB error must not take the subsystem down
/// and burn a supervisor restart.
pub async fn run(
    registry: SessionRegistry,
    db: Arc<Database>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Emergency-stop edge tracker: log once per engagement, not per poll.
    let mut pause_gate = cortex::emergency_pause::PauseGate::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        // Emergency stop (`borg pause`): skip prompt injection while engaged.
        // Due rows are never re-anchored here, so missed ticks coalesce and
        // one check-in fires after `borg resume` — deferred, not lost. The
        // registry sweep below still runs: eviction is maintenance, not new
        // agent work. Fail-safe: a corrupt/unreadable flag reads as paused.
        if cortex::emergency_pause::is_engaged(&db).await {
            if let Some(cortex::emergency_pause::PauseTransition::Engaged) =
                pause_gate.observe(true)
            {
                tracing::warn!(
                    "emergency pause engaged — session-heartbeat injection suspended \
                     until `borg resume`"
                );
            }
        } else {
            if let Some(cortex::emergency_pause::PauseTransition::Released) =
                pause_gate.observe(false)
            {
                tracing::info!("emergency pause released — session-heartbeat injection resumed");
            }
            if let Err(e) = poll_once(&registry, &db).await {
                tracing::warn!("session heartbeat poll failed: {e}");
            }
        }
        // Bounded registry: evict idle-expired hosts and enforce the LRU cap.
        // Piggybacks on this poller rather than owning another supervised
        // loop; eviction is lossless (history is persisted, a later turn
        // rebuilds the host) and never touches a session with a turn in
        // flight, a live subscriber, or an active heartbeat. Runs even while
        // the emergency pause is engaged — eviction is maintenance, not new
        // agent work — and the heartbeat exemption is what keeps a pause
        // longer than the idle TTL from stranding every heartbeat session.
        sweep_registry(&registry, &db).await;
    }
}

/// One registry maintenance pass: evict idle/over-cap hosts, exempting every
/// session that owns an active (unpaused) heartbeat.
///
/// The exemption is what keeps intervals ≥ the idle TTL alive: a heartbeat
/// fires only into a live host ([`select_fireable`] leaves un-hosted rows
/// due), and nothing on the heartbeat path rebuilds a host — so evicting a
/// heartbeat session would strand its row due forever. If the exemption set
/// cannot be loaded, the sweep is skipped for this tick rather than run
/// unprotected: deferring eviction one poll is harmless, evicting a heartbeat
/// session is not.
async fn sweep_registry(registry: &SessionRegistry, db: &Arc<Database>) {
    let exempt: std::collections::HashSet<String> =
        match db.active_session_heartbeat_session_ids().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::warn!(
                    "session registry sweep skipped this tick: \
                     failed to load heartbeat exemptions: {e}"
                );
                return;
            }
        };
    let evicted = registry.sweep(
        crate::session::SESSION_IDLE_TTL,
        crate::session::SESSION_REGISTRY_MAX,
        &exempt,
    );
    if evicted > 0 {
        tracing::debug!(evicted, "session registry sweep evicted idle hosts");
    }
}

/// Which due heartbeats this poll should fire.
///
/// Split out from [`poll_once`] so the decision — the part with real rules —
/// is testable without a live `SessionHost` or a database. `busy` reports
/// whether a session is mid-turn; a session absent from the live map is not
/// hosted and cannot fire.
fn select_fireable(
    due: &[cortex::db::SessionHeartbeat],
    is_live_and_idle: impl Fn(&str) -> Option<bool>,
) -> Vec<&cortex::db::SessionHeartbeat> {
    due.iter()
        .filter(|hb| match is_live_and_idle(&hb.session_id) {
            // Hosted and idle → fire.
            Some(true) => true,
            // Hosted but mid-turn → leave the row due; the next poll after the
            // turn ends picks it up. A heartbeat must never interrupt a turn.
            Some(false) => {
                tracing::debug!(
                    session_id = %hb.session_id,
                    "session heartbeat due but the session is busy; deferring"
                );
                false
            }
            // Not hosted → leave `next_fire_at` in the past so it fires when
            // the session is reopened, rather than silently skipping.
            None => false,
        })
        .collect()
}

/// One poll pass: fire every heartbeat whose session is live, idle, and due.
async fn poll_once(registry: &SessionRegistry, db: &Arc<Database>) -> anyhow::Result<()> {
    // No hosted sessions means nothing can fire; skip the query entirely so an
    // idle daemon does no work.
    if registry.live_ids().is_empty() {
        return Ok(());
    }

    let now = now_unix();
    let due = db.due_session_heartbeats(now).await?;
    // `peek`, not `get`: this is a busy-probe, not activity — a `get` here
    // would refresh the session's last-activity stamp every poll and keep a
    // due-but-unfired heartbeat session alive past its idle TTL.
    let fireable = select_fireable(&due, |id| registry.peek(id).map(|h| !h.is_busy()));

    for hb in fireable {
        // Re-anchor BEFORE starting the turn. `start_turn` returns only once
        // the turn is installed, so anchoring afterwards would leave the row
        // due for that whole window — and a daemon that died mid-turn would
        // fire the heartbeat again immediately on restart.
        if let Err(e) = db.reanchor_session_heartbeat(&hb.session_id, now).await {
            tracing::warn!(
                session_id = %hb.session_id,
                "session heartbeat: re-anchor failed, skipping this fire to avoid a loop: {e}"
            );
            continue;
        }

        tracing::info!(
            session_id = %hb.session_id,
            interval_secs = hb.interval_secs,
            "firing session heartbeat"
        );

        // Spawned, not awaited: the guarded start takes the turn lock, which a
        // turn that is still draining can hold, and the poller must not stall
        // every other session's heartbeat on one session. The guard re-checks
        // idle *under* that lock — a session that went busy, or was closed,
        // between the idle check above and here is skipped rather than queued
        // behind the live turn (where it would fire after the user cancelled
        // it, or on a closed backend). The row is already re-anchored, so a
        // skipped fire simply coalesces into the next interval.
        let Some(host) = registry.get(&hb.session_id) else {
            continue;
        };
        let text = injected_text(&hb.prompt);
        let session_id = hb.session_id.clone();
        tokio::spawn(async move {
            if host.start_turn_if_idle(text).await.is_none() {
                tracing::info!(
                    session_id = %session_id,
                    "session heartbeat skipped: session went busy or closed before it could start"
                );
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_prompt_keeps_the_user_text_and_adds_the_guard() {
        // The user's instruction must arrive verbatim — a heartbeat that
        // paraphrased it would drift from what they asked for — and the
        // don't-invent-work guard must ride along.
        let out = injected_text("Check whether CI finished");
        assert!(out.starts_with("Check whether CI finished"));
        assert!(
            out.contains("do not invent work"),
            "idle check-ins manufacture busywork without this: {out}"
        );
        assert!(
            out.contains("not a new request from the user"),
            "the model must know this fired on a timer: {out}"
        );
    }

    fn hb(session_id: &str) -> cortex::db::SessionHeartbeat {
        cortex::db::SessionHeartbeat {
            session_id: session_id.to_string(),
            prompt: "check".into(),
            interval_secs: 60,
            paused: false,
            next_fire_at: 0,
            created_at: 0,
        }
    }

    #[test]
    fn a_busy_session_defers_instead_of_being_interrupted() {
        // The invariant users feel: a heartbeat must never cut into a turn
        // they are actively watching. The row stays due, so it fires on the
        // next poll after the turn ends rather than being dropped.
        let due = vec![hb("busy")];
        let fired = select_fireable(&due, |_| Some(false));
        assert!(fired.is_empty());
    }

    #[test]
    fn an_unhosted_session_does_not_fire_and_is_not_skipped() {
        // No live host → nothing to inject into. `select_fireable` returning
        // empty leaves `next_fire_at` untouched, so reopening the session
        // fires it rather than silently swallowing the interval.
        let due = vec![hb("closed")];
        assert!(select_fireable(&due, |_| None).is_empty());
    }

    #[test]
    fn one_busy_session_does_not_block_an_idle_one() {
        // Regression guard for head-of-line blocking: a busy session must not
        // suppress every other session's heartbeat in the same pass.
        let due = vec![hb("busy"), hb("idle"), hb("closed")];
        let fired = select_fireable(&due, |id| match id {
            "busy" => Some(false),
            "idle" => Some(true),
            _ => None,
        });
        let ids: Vec<&str> = fired.iter().map(|h| h.session_id.as_str()).collect();
        assert_eq!(ids, vec!["idle"]);
    }

    /// Minimal idle backend so registry-sweep wiring tests can host a session
    /// without an LLM. Never runs a turn; everything else uses trait defaults.
    struct IdleStub(String);

    #[async_trait::async_trait]
    impl crate::session::backend::SessionBackend for IdleStub {
        async fn run_turn(
            &self,
            _text: String,
            _images: Vec<crate::session::backend::InlineImage>,
            _event_tx: tokio::sync::mpsc::Sender<cortex::agent::AgentEvent>,
            _cancel: CancellationToken,
            _bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
        ) {
        }

        fn session_id(&self) -> &str {
            &self.0
        }
    }

    #[tokio::test]
    async fn poller_sweep_exempts_sessions_with_an_active_heartbeat() {
        // End-to-end wiring for the eviction regression: the poller's sweep
        // must fetch active heartbeat session ids from the DB and pass them
        // through as the exempt set — a `/heartbeat every 2h` session idle
        // past the 1h TTL must stay hosted (an evicted host means the row
        // stays due forever), while a plain idle session is still evicted.
        let db = Arc::new(cortex::db::Database::test_db().await);
        db.upsert_session("s-hb", 0, 0, 0, "m", "t").await.unwrap();
        db.set_session_heartbeat("s-hb", "check in", 7200)
            .await
            .unwrap();

        let registry = SessionRegistry::new();
        registry.insert(crate::session::SessionHost::new(Arc::new(IdleStub(
            "s-hb".into(),
        ))));
        registry.insert(crate::session::SessionHost::new(Arc::new(IdleStub(
            "s-plain".into(),
        ))));
        // Idle for double the production TTL — without the exemption both
        // would be evicted by the TTL pass.
        let stale = crate::session::SESSION_IDLE_TTL * 2;
        registry.backdate("s-hb", stale);
        registry.backdate("s-plain", stale);

        super::sweep_registry(&registry, &db).await;

        assert!(
            registry.peek("s-hb").is_some(),
            "a session with an active heartbeat must survive the poller sweep"
        );
        assert!(
            registry.peek("s-plain").is_none(),
            "a plain idle-expired session must still be evicted"
        );
    }

    #[tokio::test]
    async fn run_returns_promptly_when_cancelled() {
        // The supervisor cancels on shutdown; a loop that only noticed on the
        // next 15s tick would stall daemon teardown.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let db = Arc::new(cortex::db::Database::test_db().await);
        let out = tokio::time::timeout(
            Duration::from_secs(1),
            run(SessionRegistry::new(), db, cancel),
        )
        .await
        .expect("cancellation must not wait for the next tick");
        assert!(out.is_ok());
    }
}
