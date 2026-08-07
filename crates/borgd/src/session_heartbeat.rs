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
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        if let Err(e) = poll_once(&registry, &db).await {
            tracing::warn!("session heartbeat poll failed: {e}");
        }
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
    let fireable = select_fireable(&due, |id| registry.get(id).map(|h| !h.is_busy()));

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

        // Spawned, not awaited. `start_turn` internally awaits the prior turn's
        // forwarder, so a session that went busy between the idle check above
        // and this call would otherwise block the poller — and therefore every
        // other session's heartbeat — for the length of that turn.
        let Some(host) = registry.get(&hb.session_id) else {
            continue;
        };
        let text = injected_text(&hb.prompt);
        tokio::spawn(async move {
            host.start_turn(text, Vec::new(), None, None).await;
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
