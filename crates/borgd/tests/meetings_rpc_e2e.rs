//! `Meetings` RPCs end to end against a real (temporary) data directory:
//! settings and rows go through `Config::load_from_db` / `Database::open`
//! exactly as in the daemon, transcription goes to a fake Whisper, and the
//! Google sign-in goes through the real loopback listener to a fake Google.
//!
//! A process-wide mutex serialises the tests: `BORG_HOME` is per process.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::await_holding_lock)]

use std::sync::Mutex;
use std::time::Duration;

use borgd::grpc::meetings::MeetingsSvc;
use borgd::meetings::MeetingsHandles;
use cortex::db::Database;
use cortex::meetings::calendar::google::GoogleEndpoints;
use protocols::meetings::meeting_event::Kind;
use protocols::meetings::meetings_server::Meetings;
use protocols::meetings::{
    ConnectCalendarRequest, Empty, ListMeetingsRequest, MeetingEvent, MeetingId, PushAudioRequest,
    StartMeetingRequest, Track, UpdateNotesRequest,
};
use serde_json::json;
use tonic::{Code, Request};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static SERIAL: Mutex<()> = Mutex::new(());

struct Env {
    _tmp: tempfile::TempDir,
    db: Database,
    meetings_dir: std::path::PathBuf,
}

/// A fresh data dir with meetings on (or off) and OpenAI transcription
/// pointed at `whisper`, configured the way Settings saves it: only
/// `audio.models` and the saved key, with dictation (`audio.enabled`) off.
async fn env(enabled: bool, whisper: Option<&MockServer>) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: tests in this binary are serialised by SERIAL.
    unsafe { std::env::set_var("BORG_HOME", tmp.path()) };
    let db = Database::open().await.unwrap();
    let meetings_dir = tmp.path().join("notes");
    db.set_setting("meetings.enabled", if enabled { "true" } else { "false" })
        .await
        .unwrap();
    db.set_setting("meetings.dir", &meetings_dir.display().to_string())
        .await
        .unwrap();
    if let Some(server) = whisper {
        let models = json!([{ "provider": "openai", "base_url": server.uri() }]);
        db.set_setting("audio.models", &models.to_string())
            .await
            .unwrap();
        db.set_setting(
            "credentials.openai",
            r#"{"type":"literal","value":"sk-test"}"#,
        )
        .await
        .unwrap();
    }
    Env {
        _tmp: tmp,
        db,
        meetings_dir,
    }
}

async fn whisper(text: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": text })))
        .mount(&server)
        .await;
    server
}

/// `ms` of a 440 Hz tone as 16 kHz mono 16-bit WAV.
fn wav(ms: u32) -> Vec<u8> {
    let rate = 16_000u32;
    let n = rate * ms / 1000;
    let mut out = Vec::with_capacity(44 + n as usize * 2);
    let data_len = n * 2;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let t = f64::from(i) / f64::from(rate);
        let s = (t * 440.0 * std::f64::consts::TAU).sin() * 8_000.0;
        out.extend_from_slice(&(s as i16).to_le_bytes());
    }
    out
}

fn push(id: &str, track: Track, seq: u32, offset_ms: i64) -> Request<PushAudioRequest> {
    Request::new(PushAudioRequest {
        meeting_id: id.into(),
        track: track as i32,
        seq,
        offset_ms,
        wav: wav(2_000),
    })
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<MeetingEvent>) -> Vec<Kind> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.extend(e.kind);
    }
    out
}

fn code<T>(r: Result<tonic::Response<T>, tonic::Status>) -> Code {
    match r {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => e.code(),
    }
}

fn teardown() {
    // SAFETY: serialised by SERIAL.
    unsafe { std::env::remove_var("BORG_HOME") };
}

/// A key the provider rejects is a refusal the person must fix, not an
/// outage: the client gets FAILED_PRECONDITION with words that name the
/// provider and where to paste a new key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_transcription_key_reaches_the_client_as_its_fix() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stt = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "code": "invalid_api_key", "message": "Incorrect API key provided" }
        })))
        .mount(&stt)
        .await;
    let _env = env(true, Some(&stt)).await;
    let (handles, _finalize_rx) = MeetingsHandles::new();
    let svc = MeetingsSvc::new(handles);
    let id = svc
        .start_meeting(Request::new(StartMeetingRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .summary
        .unwrap()
        .id;

    let refused = svc
        .push_audio(push(&id, Track::Mic, 0, 0))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(
        refused.message(),
        "OpenAI rejected the API key. Paste a working key in Settings → Meetings."
    );
    teardown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_meeting_records_transcribes_stops_renames_and_deletes() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stt = whisper("we patch weekly").await;
    let env = env(true, Some(&stt)).await;
    let (handles, mut finalize_rx) = MeetingsHandles::new();
    let mut events = handles.bus.subscribe();
    let svc = MeetingsSvc::new(handles);

    let status = svc
        .get_meetings_status(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner();
    assert!(status.enabled);
    assert!(
        status.transcription_ready,
        "audio.models alone makes it ready, dictation off"
    );
    assert_eq!(
        status.recordings_dir,
        env.meetings_dir.display().to_string()
    );

    let started = svc
        .start_meeting(Request::new(StartMeetingRequest {
            title: String::new(),
            source_app: "Zoom".into(),
            calendar_event_id: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    let summary = started.summary.unwrap();
    let id = summary.id.clone();
    assert_eq!(summary.title, "Zoom meeting");
    assert_eq!(summary.state, "recording");

    let first = svc
        .push_audio(push(&id, Track::Mic, 0, 0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.appended.len(), 1);
    assert_eq!(first.appended[0].text, "we patch weekly");
    assert_eq!(
        (first.appended[0].start_ms, first.appended[0].end_ms),
        (0, 2_000)
    );
    // A retry of the same chunk is answered from the store, not transcribed again.
    let retry = svc
        .push_audio(push(&id, Track::Mic, 0, 0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(retry.appended, first.appended);
    assert_eq!(stt.received_requests().await.unwrap().len(), 1);

    svc.update_notes(Request::new(UpdateNotesRequest {
        meeting_id: id.clone(),
        notes: "ask about IL5".into(),
        title: Some("Tiger Sync".into()),
    }))
    .await
    .unwrap();

    let stopped = svc
        .stop_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap()
        .into_inner();
    let s = stopped.summary.as_ref().unwrap();
    assert_eq!(
        (s.title.as_str(), s.state.as_str()),
        ("Tiger Sync", "finalizing")
    );
    assert_eq!(stopped.notes, "ask about IL5");
    assert_eq!(stopped.segments.len(), 1);
    assert_eq!(
        finalize_rx.try_recv().unwrap(),
        id,
        "queued for its summary"
    );

    let kinds = drain(&mut events);
    assert!(matches!(&kinds[0], Kind::Started(m) if m.id == id && !m.auto_started));
    let appended: Vec<_> = kinds
        .iter()
        .filter_map(|k| match k {
            Kind::TranscriptAppended(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(
        appended.len(),
        1,
        "a retry announces nothing new: {kinds:?}"
    );
    assert_eq!(appended[0].segments, first.appended);
    assert!(matches!(kinds.last(), Some(Kind::Stopped(m)) if m.id == id));

    let dir = std::path::PathBuf::from(&stopped.dir);
    assert!(dir.starts_with(&env.meetings_dir));
    let transcript = std::fs::read_to_string(dir.join("transcript.md")).unwrap();
    assert!(transcript.contains("we patch weekly"), "{transcript}");

    let list = svc
        .list_meetings(Request::new(ListMeetingsRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.meetings.len(), 1);

    svc.delete_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap();
    let gone = svc
        .get_meeting(Request::new(MeetingId { id }))
        .await
        .unwrap_err();
    assert_eq!(gone.code(), Code::NotFound);
    assert!(!dir.exists(), "the folder went with it");
    drop(env);
    teardown();
}

/// Resume takes a stopped meeting back to recording once its notes are
/// written: the desktop hears MeetingStarted (which reopens capture), and new
/// audio joins the same meeting. While the notes are still being written it
/// is refused with a reason, not silently ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_meeting_resumes_once_its_notes_are_written() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stt = whisper("one more thing").await;
    let env = env(true, Some(&stt)).await;
    let (handles, _finalize_rx) = MeetingsHandles::new();
    let mut events = handles.bus.subscribe();
    let svc = MeetingsSvc::new(handles);

    let id = svc
        .start_meeting(Request::new(StartMeetingRequest {
            title: "Standup".into(),
            source_app: String::new(),
            calendar_event_id: String::new(),
        }))
        .await
        .unwrap()
        .into_inner()
        .summary
        .unwrap()
        .id;
    svc.push_audio(push(&id, Track::Mic, 0, 0)).await.unwrap();
    svc.stop_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap();

    let early = svc
        .resume_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap_err();
    assert_eq!(early.code(), Code::FailedPrecondition);
    assert!(
        early.message().contains("still writing"),
        "{}",
        early.message()
    );

    env.db
        .set_meeting_summary(&id, "Notes", 2_000)
        .await
        .unwrap();
    drain(&mut events);
    let back = svc
        .resume_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap()
        .into_inner()
        .summary
        .unwrap();
    assert_eq!(back.state, "recording");
    assert_eq!(back.ended_at, 0, "no end while recording");
    let kinds = drain(&mut events);
    assert!(
        matches!(kinds.as_slice(), [Kind::Started(m)] if m.id == id && !m.auto_started),
        "{kinds:?}"
    );

    let later = svc
        .push_audio(push(&id, Track::Mic, 90_000, 90_000))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(later.appended[0].text, "one more thing");
    let meeting = svc
        .get_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        meeting.segments.len(),
        2,
        "both stretches in one transcript"
    );
    teardown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_refusal_has_its_own_status_code() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stt = whisper("hello").await;
    let env = env(true, Some(&stt)).await;
    let svc = MeetingsSvc::new(MeetingsHandles::detached());
    let start = || {
        Request::new(StartMeetingRequest {
            title: "Sync".into(),
            source_app: String::new(),
            calendar_event_id: String::new(),
        })
    };
    let id = svc
        .start_meeting(start())
        .await
        .unwrap()
        .into_inner()
        .summary
        .unwrap()
        .id;

    assert_eq!(code(svc.start_meeting(start()).await), Code::AlreadyExists);
    assert_eq!(
        code(svc.push_audio(push(&id, Track::Unspecified, 0, 0)).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(svc.push_audio(push("mtg_nope", Track::Mic, 0, 0)).await),
        Code::NotFound
    );
    let mut negative = push(&id, Track::Mic, 0, -5);
    negative.get_mut().offset_ms = -5;
    assert_eq!(code(svc.push_audio(negative).await), Code::InvalidArgument);
    svc.stop_meeting(Request::new(MeetingId { id: id.clone() }))
        .await
        .unwrap();
    assert_eq!(
        code(svc.push_audio(push(&id, Track::Mic, 1, 25_000)).await),
        Code::FailedPrecondition
    );

    env.db
        .set_setting("meetings.enabled", "false")
        .await
        .unwrap();
    let off = svc
        .push_audio(push(&id, Track::Mic, 2, 50_000))
        .await
        .unwrap_err();
    assert_eq!(off.code(), Code::FailedPrecondition);
    assert!(
        off.message().contains("Settings"),
        "names the next step: {}",
        off.message()
    );
    assert_eq!(
        code(svc.start_meeting(start()).await),
        Code::FailedPrecondition
    );

    let bad_ics = svc
        .connect_calendar(Request::new(ConnectCalendarRequest {
            kind: "ics".into(),
            ics_url: "http://cal.example.com/a.ics".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(bad_ics.code(), Code::InvalidArgument);
    let status = svc
        .get_meetings_status(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !status.calendar.unwrap().connected,
        "a refused feed stores nothing"
    );
    drop(env);
    teardown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn google_sign_in_completes_through_the_loopback_and_disconnect_forgets_it() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = env(true, None).await;
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "at", "refresh_token": "rt"})),
        )
        .mount(&google)
        .await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/primary"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "me@example.com"})))
        .mount(&google)
        .await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/primary/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&google)
        .await;
    let handles = MeetingsHandles::detached();
    let mut events = handles.bus.subscribe();
    let svc = MeetingsSvc::new(handles).with_google_endpoints(GoogleEndpoints {
        auth: format!("{}/auth", google.uri()),
        token: format!("{}/token", google.uri()),
        api: format!("{}/calendar/v3", google.uri()),
    });

    let missing = svc
        .connect_calendar(Request::new(ConnectCalendarRequest {
            kind: "google".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::InvalidArgument);

    let reply = svc
        .connect_calendar(Request::new(ConnectCalendarRequest {
            kind: "google".into(),
            google_client_id: "cid".into(),
            google_client_secret: "cs".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let auth = reqwest::Url::parse(&reply.auth_url).unwrap();
    let q: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let redirect = q["redirect_uri"].clone();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");

    // A forged callback is turned away and saves nothing.
    let forged = reqwest::get(format!("{redirect}?code=x&state=forged"))
        .await
        .unwrap();
    assert_eq!(forged.status(), 400);

    let ok = reqwest::get(format!("{redirect}?code=good&state={}", q["state"]))
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let changed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(e) = events.recv().await {
                if let Some(Kind::CalendarChanged(c)) = e.kind {
                    return c.connected;
                }
            }
        }
    })
    .await
    .expect("CalendarChanged after the redirect");
    assert!(changed);
    let status = svc
        .get_meetings_status(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner();
    let cal = status.calendar.unwrap();
    assert_eq!(
        (cal.kind.as_str(), cal.account_label.as_str()),
        ("google", "me@example.com")
    );

    svc.disconnect_calendar(Request::new(Empty {}))
        .await
        .unwrap();
    let cal = svc
        .get_meetings_status(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner()
        .calendar
        .unwrap();
    assert!(!cal.connected);
    assert!(drain(&mut events)
        .iter()
        .any(|k| matches!(k, Kind::CalendarChanged(c) if !c.connected)));
    drop(env);
    teardown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_google_sign_in_tells_the_waiting_page_why_and_saves_nothing() {
    let _g = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = env(true, None).await;
    let google = MockServer::start().await;
    // A mistyped client secret: Google answers the code exchange with 401.
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_client",
            "error_description": "The OAuth client was not found."
        })))
        .mount(&google)
        .await;
    let handles = MeetingsHandles::detached();
    let mut events = handles.bus.subscribe();
    let svc = MeetingsSvc::new(handles).with_google_endpoints(GoogleEndpoints {
        auth: format!("{}/auth", google.uri()),
        token: format!("{}/token", google.uri()),
        api: format!("{}/calendar/v3", google.uri()),
    });
    let reply = svc
        .connect_calendar(Request::new(ConnectCalendarRequest {
            kind: "google".into(),
            google_client_id: "cid".into(),
            google_client_secret: "wrong".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let auth = reqwest::Url::parse(&reply.auth_url).unwrap();
    let q: std::collections::HashMap<_, _> = auth.query_pairs().into_owned().collect();
    let landed = reqwest::get(format!(
        "{}?code=good&state={}",
        q["redirect_uri"], q["state"]
    ))
    .await
    .unwrap();
    assert_eq!(landed.status(), 200);

    let changed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(e) = events.recv().await {
                if let Some(Kind::CalendarChanged(c)) = e.kind {
                    return c;
                }
            }
        }
    })
    .await
    .expect("the page must hear that the sign-in failed, not wait forever");
    assert!(!changed.connected);
    assert!(
        changed.error.contains("client ID and secret"),
        "the reason names what to fix: {}",
        changed.error
    );
    let cal = svc
        .get_meetings_status(Request::new(Empty {}))
        .await
        .unwrap()
        .into_inner()
        .calendar
        .unwrap();
    assert!(!cal.connected, "a refused sign-in stores nothing");
    drop(env);
    teardown();
}
