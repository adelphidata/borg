//! The `meetings-detector` worker (macOS): notices a call starting in Zoom,
//! Teams or a browser and offers to take notes.
//!
//! Every two seconds it asks the probe which apps hold the microphone, adds
//! the calendar event happening now, and lets [`state`] decide. A detection
//! becomes `Detected` on the bus (the desktop shows "Meeting detected"), or,
//! with `meetings.ask_before_recording` off and a calendar event to anchor
//! it, a meeting started on the spot (`MeetingStarted{auto_started}`).
//!
//! While a meeting is recording it offers nothing new but keeps following
//! the call, so the call's end still reaches the panel (which stops by
//! itself) and a call the person stopped recording is not offered again.
//! While meetings are off it idles, so the daemon never offers anything the
//! person has not turned on.

#[cfg(target_os = "macos")]
pub mod macos;
pub mod probe;
pub mod state;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cortex::config::Config;
use cortex::db::Database;
use cortex::meetings::calendar::nearest_joinable_event;
use cortex::meetings::now_unix;
use cortex::meetings::store::{self, StartParams};
use protocols::meetings::meeting_event::Kind;
use protocols::meetings::{Detected, DetectionEnded, MeetingStarted};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::MeetingEventBus;
use probe::MicProbe;
use state::{DetectorConfig, DetectorState, Transition};

/// Time between probes.
pub const TICK: Duration = Duration::from_secs(2);
/// How far from an event's start a call still counts as that event.
pub const CALENDAR_WINDOW_SECS: i64 = 120;

/// Run until cancelled.
pub async fn run(
    probe: Arc<dyn MicProbe>,
    config_rx: watch::Receiver<Config>,
    bus: MeetingEventBus,
    liveness: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let db = Database::open().await?;
    let mut detector = DetectorState::new(DetectorConfig::default());
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        liveness.fetch_add(1, Ordering::Relaxed);
        let config = config_rx.borrow().clone();
        if !config.meetings.enabled {
            detector.reset();
            continue;
        }
        let probe = probe.clone();
        let bundles = match tokio::task::spawn_blocking(move || probe.snapshot()).await? {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!("microphone probe failed: {e:#}");
                continue;
            }
        };
        if let Err(e) = step(
            &mut detector,
            &db,
            &config,
            &bus,
            &bundles,
            Instant::now(),
            now_unix(),
        )
        .await
        {
            tracing::warn!("meeting detector: {e:#}");
        }
    }
}

/// One tick, after the probe. Split out so the decisions around the state
/// machine (stand aside while recording, auto-start or ask) are tested with
/// a real database and no microphone.
pub async fn step(
    detector: &mut DetectorState,
    db: &Database,
    config: &Config,
    bus: &MeetingEventBus,
    mic_bundles: &[String],
    now: Instant,
    now_unix: i64,
) -> anyhow::Result<()> {
    let apps = probe::labels(mic_bundles);
    // While a meeting records, keep following the call rather than forgetting
    // it: the end of the call is what lets the panel stop by itself, and a
    // call still going after the person pressed Stop must not be offered (or
    // auto-started) again. Only a new detection is held back.
    if db.recording_meeting().await?.is_some() {
        if detector.on_tick(now, &apps, None) == Some(Transition::Ended) {
            bus.publish(Kind::DetectionEnded(DetectionEnded {}));
        }
        return Ok(());
    }
    let nearby = db
        .calendar_events_between(
            now_unix - CALENDAR_WINDOW_SECS,
            now_unix + CALENDAR_WINDOW_SECS,
        )
        .await?;
    let event = nearest_joinable_event(&nearby, now_unix, CALENDAR_WINDOW_SECS);
    match detector.on_tick(now, &apps, event) {
        Some(Transition::Detected {
            source_app,
            calendar_event,
        }) => match calendar_event {
            Some(event) if !config.meetings.ask_before_recording => {
                let row = store::start_meeting(
                    db,
                    config,
                    StartParams {
                        title: String::new(),
                        source_app,
                        calendar_event_id: event.id.clone(),
                    },
                    now_unix,
                )
                .await?;
                tracing::info!(meeting = %row.id, "auto-started notes for {}", row.title);
                bus.publish(Kind::Started(MeetingStarted {
                    id: row.id,
                    auto_started: true,
                }));
            }
            calendar_event => bus.publish(Kind::Detected(Detected {
                source_app,
                calendar_event: calendar_event
                    .as_ref()
                    .map(crate::grpc::meetings::to_pb_event),
            })),
        },
        Some(Transition::Ended) => bus.publish(Kind::DetectionEnded(DetectionEnded {})),
        None => {}
    }
    Ok(())
}

/// The probe for this platform.
pub fn platform_probe() -> Arc<dyn MicProbe> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::CoreAudioProbe)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(probe::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::meetings::calendar::CalendarEvent;
    use cortex::meetings::MeetingState;
    use protocols::meetings::MeetingEvent;

    const NOW: i64 = 1_800_000_000;

    fn zoom() -> Vec<String> {
        vec!["us.zoom.xos".to_string()]
    }

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<MeetingEvent>) -> Vec<Kind> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.extend(e.kind);
        }
        out
    }

    async fn with_event(db: &Database) {
        let e = CalendarEvent {
            id: "ics:standup".into(),
            source: "ics".into(),
            title: "Standup".into(),
            start_unix: NOW + 60,
            end_unix: NOW + 960,
            all_day: false,
            attendees: vec![],
            join_url: "https://zoom.us/j/123456789".into(),
            provider: "zoom".into(),
        };
        db.replace_calendar_events("ics", &[e], NOW).await.unwrap();
    }

    fn enabled(tmp: &std::path::Path, ask: bool) -> Config {
        let mut config = Config::default();
        config.meetings.enabled = true;
        config.meetings.ask_before_recording = ask;
        config.meetings.dir = Some(tmp.display().to_string());
        config
    }

    /// Two ticks five seconds apart with Zoom on the mic.
    async fn hold_mic(db: &Database, config: &Config, bus: &MeetingEventBus) {
        let mut d = DetectorState::new(DetectorConfig::default());
        let t0 = Instant::now();
        step(&mut d, db, config, bus, &zoom(), t0, NOW)
            .await
            .unwrap();
        step(
            &mut d,
            db,
            config,
            bus,
            &zoom(),
            t0 + Duration::from_secs(5),
            NOW + 5,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn asking_first_offers_the_meeting_and_records_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        with_event(&db).await;
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        hold_mic(&db, &enabled(tmp.path(), true), &bus).await;
        match drain(&mut rx).as_slice() {
            [Kind::Detected(d)] => {
                assert_eq!(d.source_app, "Zoom");
                assert_eq!(d.calendar_event.as_ref().unwrap().title, "Standup");
            }
            other => panic!("expected one Detected, got {other:?}"),
        }
        assert!(db.recording_meeting().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn not_asking_starts_a_calendar_meeting_on_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        with_event(&db).await;
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        hold_mic(&db, &enabled(tmp.path(), false), &bus).await;
        let row = db
            .recording_meeting()
            .await
            .unwrap()
            .expect("a meeting started");
        assert_eq!(row.title, "Standup");
        assert_eq!(row.calendar_event_id, "ics:standup");
        assert_eq!(row.state, MeetingState::Recording);
        assert_eq!(
            drain(&mut rx),
            [Kind::Started(MeetingStarted {
                id: row.id,
                auto_started: true
            })]
        );
    }

    #[tokio::test]
    async fn without_a_calendar_event_it_always_asks() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        hold_mic(&db, &enabled(tmp.path(), false), &bus).await;
        assert!(
            matches!(drain(&mut rx).as_slice(), [Kind::Detected(d)] if d.calendar_event.is_none())
        );
        assert!(db.recording_meeting().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_detector_stands_aside_while_a_meeting_records() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        let config = enabled(tmp.path(), true);
        store::start_meeting(&db, &config, StartParams::default(), NOW)
            .await
            .unwrap();
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        hold_mic(&db, &config, &bus).await;
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn a_recorded_call_is_followed_to_its_end_and_never_offered_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        with_event(&db).await;
        let config = enabled(tmp.path(), false);
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        let mut d = DetectorState::new(DetectorConfig::default());
        let t0 = Instant::now();
        macro_rules! tick {
            ($secs:expr, $mic:expr) => {
                step(
                    &mut d,
                    &db,
                    &config,
                    &bus,
                    &$mic,
                    t0 + Duration::from_secs($secs),
                    NOW + $secs as i64,
                )
                .await
                .unwrap()
            };
        }

        // Zoom holds the mic: the calendar meeting starts on its own.
        tick!(0, zoom());
        tick!(5, zoom());
        let first = db.recording_meeting().await.unwrap().expect("auto-started");
        assert!(matches!(drain(&mut rx).as_slice(), [Kind::Started(_)]));

        // The person presses Stop while the call goes on: the same call is
        // neither offered nor started again.
        store::stop_meeting(&db, &first.id, NOW + 10).await.unwrap();
        for secs in [12, 14, 20, 40] {
            tick!(secs, zoom());
        }
        assert!(db.recording_meeting().await.unwrap().is_none());
        assert!(drain(&mut rx).is_empty(), "the same call came back as new");

        // Leaving the call ends it; joining later is a new call again.
        tick!(42, Vec::<String>::new());
        tick!(72, Vec::<String>::new());
        assert_eq!(drain(&mut rx), [Kind::DetectionEnded(DetectionEnded {})]);
        tick!(100, zoom());
        tick!(105, zoom());
        assert!(db.recording_meeting().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_end_of_a_call_reaches_the_panel_while_it_records() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::test_db().await;
        let config = enabled(tmp.path(), true);
        let bus = MeetingEventBus::default();
        let mut rx = bus.subscribe();
        let mut d = DetectorState::new(DetectorConfig::default());
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        step(&mut d, &db, &config, &bus, &zoom(), at(0), NOW)
            .await
            .unwrap();
        step(&mut d, &db, &config, &bus, &zoom(), at(5), NOW + 5)
            .await
            .unwrap();
        // "Take notes" was clicked.
        store::start_meeting(&db, &config, StartParams::default(), NOW + 6)
            .await
            .unwrap();
        drain(&mut rx);
        step(&mut d, &db, &config, &bus, &zoom(), at(8), NOW + 8)
            .await
            .unwrap();
        // The call ends; the person forgets to press Stop.
        step(&mut d, &db, &config, &bus, &[], at(10), NOW + 10)
            .await
            .unwrap();
        step(&mut d, &db, &config, &bus, &[], at(40), NOW + 40)
            .await
            .unwrap();
        assert_eq!(
            drain(&mut rx),
            [Kind::DetectionEnded(DetectionEnded {})],
            "the panel's auto-stop waits on this"
        );
    }
}
