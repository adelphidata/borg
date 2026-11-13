//! The `meetings-finalizer` worker: writes each stopped meeting's summary,
//! one at a time.
//!
//! A queue (`StopMeeting` sends the id) makes the common case immediate; a
//! scan every minute makes it reliable. The scan picks up meetings left
//! `finalizing` by a restart, and stops recordings nothing has written to
//! for five minutes — a desktop app that crashed, or a meeting the detector
//! auto-started with no app open to capture it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cortex::config::Config;
use cortex::db::Database;
use cortex::meetings::summary::{AuxSummaryWriter, SummaryWriter};
use cortex::meetings::{now_unix, store, MeetingState};
use protocols::meetings::meeting_event::Kind;
use protocols::meetings::{MeetingStopped, SummaryFailed, SummaryReady};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use super::MeetingEventBus;

/// A recording silent this long is treated as abandoned.
pub const STALE_RECORDING_SECS: i64 = 300;
/// How often the scan runs.
const SCAN_EVERY: Duration = Duration::from_secs(60);

/// The `StopMeeting` queue, shared by every incarnation of the worker so a
/// restarted finalizer still hears about the next stop.
#[derive(Clone)]
pub struct FinalizeQueue(Arc<Mutex<mpsc::Receiver<String>>>);

impl FinalizeQueue {
    /// Wrap the queue's receiving end.
    pub fn new(rx: mpsc::Receiver<String>) -> Self {
        Self(Arc::new(Mutex::new(rx)))
    }

    /// The next meeting to finalize. Once every sender is gone this never
    /// resolves, and the worker carries on with its periodic scan: a closed
    /// queue is not a reason to stop writing summaries.
    pub async fn next(&self) -> String {
        match self.0.lock().await.recv().await {
            Some(id) => id,
            None => std::future::pending().await,
        }
    }
}

/// Run until cancelled.
pub async fn run(
    queue: FinalizeQueue,
    bus: MeetingEventBus,
    liveness: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    loop {
        liveness.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = live_scan(&bus).await {
            tracing::warn!("meetings finalizer scan failed: {e:#}");
        }
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(SCAN_EVERY) => {}
            id = queue.next() => {
                if let Err(e) = live_finalize(&bus, &id).await {
                    tracing::warn!(meeting = %id, "meetings finalizer could not start: {e:#}");
                }
            }
        }
    }
}

// The database and the summary writer are built from the live settings on
// every pass, so a model switched in Settings writes the next summary.

async fn live_scan(bus: &MeetingEventBus) -> anyhow::Result<()> {
    let config = Config::load_from_db().await?;
    let db = Database::open().await?;
    scan(&db, &AuxSummaryWriter { config: &config }, bus, now_unix()).await
}

async fn live_finalize(bus: &MeetingEventBus, id: &str) -> anyhow::Result<()> {
    let config = Config::load_from_db().await?;
    let db = Database::open().await?;
    finalize_one(
        &db,
        &AuxSummaryWriter { config: &config },
        bus,
        id,
        now_unix(),
    )
    .await;
    Ok(())
}

/// Stop abandoned recordings, then summarise everything `finalizing`.
pub async fn scan(
    db: &Database,
    writer: &dyn SummaryWriter,
    bus: &MeetingEventBus,
    now: i64,
) -> anyhow::Result<()> {
    for stale in db.stale_recordings(now - STALE_RECORDING_SECS).await? {
        tracing::info!(meeting = %stale.id, "stopping a recording that went quiet");
        store::stop_meeting(db, &stale.id, now).await?;
        bus.publish(Kind::Stopped(MeetingStopped { id: stale.id }));
    }
    for meeting in db.meetings_in_state(MeetingState::Finalizing).await? {
        finalize_one(db, writer, bus, &meeting.id, now).await;
    }
    Ok(())
}

/// Summarise one meeting and say how it went on the bus. A meeting no
/// longer `finalizing` (already handled by the scan) is skipped quietly.
pub async fn finalize_one(
    db: &Database,
    writer: &dyn SummaryWriter,
    bus: &MeetingEventBus,
    id: &str,
    now: i64,
) {
    match db.get_meeting(id).await {
        Ok(Some(m)) if m.state == MeetingState::Finalizing => {}
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(meeting = id, "could not read meeting: {e:#}");
            return;
        }
    }
    match store::finalize_meeting(db, writer, id, now).await {
        Ok(row) if row.state == MeetingState::Done => {
            bus.publish(Kind::SummaryReady(SummaryReady { id: id.to_string() }));
        }
        Ok(row) => bus.publish(Kind::SummaryFailed(SummaryFailed {
            id: id.to_string(),
            message: row
                .error
                .unwrap_or_else(|| "the summary could not be written".into()),
        })),
        Err(e) => {
            tracing::warn!(meeting = id, "meeting summary failed: {e:#}");
            bus.publish(Kind::SummaryFailed(SummaryFailed {
                id: id.to_string(),
                message: e.to_string(),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::db::NewMeeting;
    use cortex::llm::UsageData;

    #[tokio::test]
    async fn a_restarted_finalizer_still_hears_the_queue_and_a_closed_one_is_not_an_exit() {
        let (tx, rx) = mpsc::channel(4);
        let first = FinalizeQueue::new(rx);
        // The supervisor hands each incarnation the same queue.
        let restarted = first.clone();
        drop(first);
        tx.send("m1".to_string()).await.unwrap();
        assert_eq!(restarted.next().await, "m1");
        drop(tx);
        // No sender left: the worker must keep scanning, not see "done".
        let closed = tokio::time::timeout(Duration::from_millis(50), restarted.next()).await;
        assert!(closed.is_err(), "a closed queue resolved: {closed:?}");
    }

    /// Writes a fixed summary, or fails.
    struct Fake(Result<&'static str, &'static str>);

    #[async_trait::async_trait]
    impl SummaryWriter for Fake {
        async fn write(
            &self,
            _id: &str,
            _prompt: &str,
        ) -> (anyhow::Result<String>, Vec<UsageData>) {
            (
                self.0.map(str::to_string).map_err(|e| anyhow::anyhow!(e)),
                Vec::new(),
            )
        }
    }

    async fn seed(db: &Database, id: &str, dir: &std::path::Path, started_at: i64) {
        db.insert_meeting(&NewMeeting {
            id,
            title: "Sync",
            source_app: "Zoom",
            calendar_event_id: "",
            attendees: &[],
            started_at,
            dir,
        })
        .await
        .unwrap();
        db.set_meeting_notes(id, "decided to ship", started_at)
            .await
            .unwrap();
    }

    fn drain(
        rx: &mut tokio::sync::broadcast::Receiver<protocols::meetings::MeetingEvent>,
    ) -> Vec<Kind> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.extend(e.kind);
        }
        out
    }

    #[tokio::test]
    async fn scan_stops_quiet_recordings_and_summarises_what_a_restart_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        let now = 100_000;
        // Left `finalizing` by a previous daemon.
        seed(&db, "mtg_left", tmp.path(), now - 3_600).await;
        store::stop_meeting(&db, "mtg_left", now - 3_000)
            .await
            .unwrap();
        // Recording, nothing written for ten minutes.
        seed(&db, "mtg_quiet", tmp.path(), now - 600).await;
        // Recording and alive.
        seed(&db, "mtg_live", tmp.path(), now - 30).await;

        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        scan(&db, &Fake(Ok("**Decisions**\n- ship")), &bus, now)
            .await
            .unwrap();

        let state = |id: &'static str| {
            let db = &db;
            async move { db.get_meeting(id).await.unwrap().unwrap().state }
        };
        assert_eq!(state("mtg_left").await, MeetingState::Done);
        assert_eq!(
            state("mtg_quiet").await,
            MeetingState::Done,
            "stopped, then summarised"
        );
        assert_eq!(state("mtg_live").await, MeetingState::Recording);
        let events = drain(&mut rx);
        assert!(events.contains(&Kind::Stopped(MeetingStopped {
            id: "mtg_quiet".into()
        })));
        assert!(events.contains(&Kind::SummaryReady(SummaryReady {
            id: "mtg_left".into()
        })));
        assert!(events.contains(&Kind::SummaryReady(SummaryReady {
            id: "mtg_quiet".into()
        })));
        assert!(!events
            .iter()
            .any(|k| matches!(k, Kind::Stopped(s) if s.id == "mtg_live")));
    }

    #[tokio::test]
    async fn a_failed_summary_is_reported_and_the_next_meeting_still_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        seed(&db, "mtg_a", tmp.path(), 1_000).await;
        store::stop_meeting(&db, "mtg_a", 1_100).await.unwrap();
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();

        finalize_one(&db, &Fake(Err("model is down")), &bus, "mtg_a", 1_200).await;
        let row = db.get_meeting("mtg_a").await.unwrap().unwrap();
        assert_eq!(row.state, MeetingState::Failed);
        match drain(&mut rx).as_slice() {
            [Kind::SummaryFailed(f)] => {
                assert_eq!(f.id, "mtg_a");
                assert!(f.message.contains("model is down"), "{}", f.message);
            }
            other => panic!("expected one SummaryFailed, got {other:?}"),
        }

        // Already handled: a second request for it does nothing.
        finalize_one(&db, &Fake(Ok("x")), &bus, "mtg_a", 1_300).await;
        assert!(drain(&mut rx).is_empty());
        assert_eq!(
            db.get_meeting("mtg_a").await.unwrap().unwrap().state,
            MeetingState::Failed
        );
    }
}
