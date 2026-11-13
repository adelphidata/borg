//! A meeting's lifecycle: start, take audio chunks, take notes, stop, write
//! the summary, delete. The gRPC service and the call detector both drive
//! meetings through here, so the rules live in one place.
//!
//! The database row is the source of truth; the folder's Markdown files are
//! rewritten from it after every change.

use chrono::{Local, TimeZone};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::db::{Database, MeetingRow, NewMeeting, SegmentRow};
use crate::media::understanding::AudioTranscriber;
use crate::meetings::summary::{self, SummaryWriter};
use crate::meetings::{audio, files, new_meeting_id, transcript, Attendee, MeetingState, Track};

/// Chunks shorter than this hold no words worth a request.
const MIN_CHUNK_MS: i64 = 300;

/// Why a meeting operation was refused. Each maps to one gRPC status, so
/// callers never match on message text.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No such meeting.
    #[error("there is no meeting {0}")]
    NotFound(String),
    /// Audio for a meeting that has stopped.
    #[error("that meeting is no longer recording")]
    NotRecording,
    /// A second recording while one is running.
    #[error("another meeting is already recording")]
    AlreadyRecording,
    /// `meetings.enabled` is off.
    #[error("meeting notes are turned off; turn them on in Settings → Meetings")]
    Disabled,
    /// No `audio.models` provider can run.
    #[error("no transcription provider is set up; choose one in Settings → Meetings")]
    TranscriptionUnavailable,
    /// Every provider in the chain failed for this chunk; retrying may work.
    #[error("transcription failed: {0}")]
    TranscriptionFailed(String),
    /// The request itself is wrong.
    #[error("{0}")]
    Invalid(String),
    /// Anything else.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Result of a store operation.
pub type StoreResult<T> = Result<T, StoreError>;

/// Serialises rewrites of a meeting's files: two tracks' chunks finishing
/// together would otherwise race on `transcript.md`.
static FILE_LOCK: Mutex<()> = Mutex::const_new(());

/// Held from "is anything recording?" to the insert, so the detector's
/// auto-start and a click at the same moment cannot both start a meeting.
static START_LOCK: Mutex<()> = Mutex::const_new(());

/// What [`start_meeting`] takes.
#[derive(Debug, Clone, Default)]
pub struct StartParams {
    /// Empty picks the calendar event's title, then the app's.
    pub title: String,
    /// "Zoom", "Chrome"… or empty.
    pub source_app: String,
    /// The calendar event this call belongs to, or empty.
    pub calendar_event_id: String,
}

/// One pushed chunk.
#[derive(Debug, Clone, Copy)]
pub struct ChunkInput<'a> {
    /// `mtg_…`.
    pub meeting_id: &'a str,
    /// Which side.
    pub track: Track,
    /// Per-track chunk number.
    pub seq: u32,
    /// Where the chunk starts, ms from the start of the meeting.
    pub offset_ms: i64,
    /// 16-bit PCM WAV.
    pub wav: &'a [u8],
}

/// What a chunk produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkOutcome {
    /// Segments this chunk added (or had added, on a retry).
    pub appended: Vec<SegmentRow>,
    /// The chunk was silence, or the transcriber's silence babble.
    pub dropped_as_silence: bool,
    /// A retry of a chunk already stored: `appended` is the earlier result,
    /// nothing new was added, and nobody needs telling again.
    pub replayed: bool,
}

fn started_label(started_at: i64) -> String {
    Local
        .timestamp_opt(started_at, 0)
        .single()
        .map(|t| t.format("%A %-d %B %Y, %-I:%M %p").to_string())
        .unwrap_or_default()
}

async fn load(db: &Database, id: &str) -> StoreResult<MeetingRow> {
    db.get_meeting(id)
        .await?
        .ok_or_else(|| StoreError::NotFound(id.to_string()))
}

/// Rewrite `transcript.md` from the stored segments.
async fn write_transcript(db: &Database, meeting: &MeetingRow) -> StoreResult<()> {
    // Read under the lock: two chunks finishing together must not let the
    // older snapshot of the segments overwrite the newer file.
    let _guard = FILE_LOCK.lock().await;
    let segments = db.segments_for(&meeting.id).await?;
    let md = transcript::render_markdown(
        &meeting.title,
        &started_label(meeting.started_at),
        &segments,
    );
    files::write_file(&meeting.dir, files::TRANSCRIPT_FILE, &md)?;
    Ok(())
}

/// Begin a meeting. Refuses when the feature is off or another meeting is
/// already recording. The title falls back to the calendar event's, then to
/// "<app> meeting", then "Meeting"; attendees come from the calendar event.
pub async fn start_meeting(
    db: &Database,
    config: &Config,
    params: StartParams,
    now: i64,
) -> StoreResult<MeetingRow> {
    if !config.meetings.enabled {
        return Err(StoreError::Disabled);
    }
    let _starting = START_LOCK.lock().await;
    if db.recording_meeting().await?.is_some() {
        return Err(StoreError::AlreadyRecording);
    }
    let event = if params.calendar_event_id.is_empty() {
        None
    } else {
        db.calendar_event(&params.calendar_event_id).await?
    };
    let title = [
        params.title.trim().to_string(),
        event
            .as_ref()
            .map(|e| e.title.trim().to_string())
            .unwrap_or_default(),
        if params.source_app.trim().is_empty() {
            String::new()
        } else {
            format!("{} meeting", params.source_app.trim())
        },
    ]
    .into_iter()
    .find(|t| !t.is_empty())
    .unwrap_or_else(|| "Meeting".to_string());
    let attendees: Vec<Attendee> = event.map(|e| e.attendees).unwrap_or_default();

    let root = files::meetings_dir(config)?;
    let started = Local
        .timestamp_opt(now, 0)
        .single()
        .unwrap_or_else(Local::now);
    let dir = files::create_meeting_dir(&root, &files::folder_name(&started, &title))?;
    let id = new_meeting_id();
    db.insert_meeting(&NewMeeting {
        id: &id,
        title: &title,
        source_app: params.source_app.trim(),
        calendar_event_id: &params.calendar_event_id,
        attendees: &attendees,
        started_at: now,
        dir: &dir,
    })
    .await?;
    let meeting = load(db, &id).await?;
    write_transcript(db, &meeting).await?;
    Ok(meeting)
}

/// Transcribe one chunk and add it to the meeting.
///
/// A chunk already stored for `(track, seq)` is answered from the database
/// without another transcription, so retries are free and never duplicate
/// text. Silence and the transcribers' silence babble ("Thank you.") add
/// nothing.
pub async fn append_chunk(
    db: &Database,
    config: &Config,
    transcriber: Option<&AudioTranscriber>,
    input: ChunkInput<'_>,
    now: i64,
) -> StoreResult<ChunkOutcome> {
    let meeting = load(db, input.meeting_id).await?;
    if meeting.state != MeetingState::Recording {
        return Err(StoreError::NotRecording);
    }
    if input.offset_ms < 0 {
        return Err(StoreError::Invalid("offset_ms must not be negative".into()));
    }
    if input.wav.len() as u64 > config.audio.max_file_size {
        return Err(StoreError::Invalid(format!(
            "chunk is {} bytes; the most a transcriber takes is {}",
            input.wav.len(),
            config.audio.max_file_size
        )));
    }
    if let Some(existing) = db.segment(input.meeting_id, input.track, input.seq).await? {
        let silent = existing.text.is_empty();
        return Ok(ChunkOutcome {
            appended: if silent { vec![] } else { vec![existing] },
            dropped_as_silence: silent,
            replayed: true,
        });
    }
    let info = audio::inspect_wav(input.wav).map_err(|e| StoreError::Invalid(e.to_string()))?;
    db.touch_meeting(&meeting.id, now).await?;

    let too_short =
        info.duration_ms < MIN_CHUNK_MS || (input.wav.len() as u64) < config.audio.min_file_size;
    let text = if too_short {
        String::new()
    } else {
        let transcriber = transcriber.ok_or(StoreError::TranscriptionUnavailable)?;
        let (text, _attempts) = transcriber
            .transcribe(input.wav, "audio/wav", "chunk.wav", None)
            .await
            .map_err(|e| StoreError::TranscriptionFailed(e.to_string()))?;
        if crate::media::voice_capture::is_whisper_hallucination(&text) {
            String::new()
        } else {
            text.trim().to_string()
        }
    };
    // Every outcome is stored once, silence as an empty row, so a retry of
    // this chunk is answered from the database whatever it held.
    let segment = SegmentRow {
        track: input.track,
        seq: input.seq,
        start_ms: input.offset_ms,
        end_ms: input.offset_ms + info.duration_ms,
        text,
    };
    let inserted = db.insert_segment(&meeting.id, &segment).await?;
    // Only the call that stored the chunk keeps its audio: a retry racing
    // this one must not add the same 25 seconds twice.
    if inserted && config.meetings.keep_audio {
        let (spec, samples) =
            audio::pcm_i16(input.wav).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let dir = meeting.dir.clone();
        let track = input.track;
        let _guard = FILE_LOCK.lock().await;
        tokio::task::spawn_blocking(move || files::append_track_audio(&dir, track, spec, &samples))
            .await
            .map_err(|e| anyhow::anyhow!("audio write task failed: {e}"))??;
    }
    if segment.text.is_empty() {
        return Ok(ChunkOutcome {
            appended: vec![],
            dropped_as_silence: true,
            replayed: !inserted,
        });
    }
    if inserted {
        write_transcript(db, &meeting).await?;
    }
    Ok(ChunkOutcome {
        appended: vec![segment],
        dropped_as_silence: false,
        replayed: !inserted,
    })
}

/// Replace the user's notes, and rename when `title` is given and not blank.
pub async fn update_notes(
    db: &Database,
    id: &str,
    notes: &str,
    title: Option<&str>,
    now: i64,
) -> StoreResult<MeetingRow> {
    let meeting = load(db, id).await?;
    db.set_meeting_notes(id, notes, now).await?;
    let renamed = title.map(str::trim).filter(|t| !t.is_empty());
    if let Some(t) = renamed {
        db.set_meeting_title(id, t, now).await?;
    }
    let meeting = if renamed.is_some() {
        load(db, id).await?
    } else {
        meeting
    };
    {
        let _guard = FILE_LOCK.lock().await;
        files::write_file(&meeting.dir, files::NOTES_FILE, notes)?;
    }
    if renamed.is_some() {
        write_transcript(db, &meeting).await?;
        if let Some(summary) = &meeting.summary_md {
            write_summary_file(&meeting, summary).await?;
        }
    }
    load(db, id).await
}

/// End recording. Idempotent: a meeting already past recording is returned
/// unchanged.
pub async fn stop_meeting(db: &Database, id: &str, now: i64) -> StoreResult<MeetingRow> {
    let meeting = load(db, id).await?;
    if meeting.state == MeetingState::Recording {
        db.set_meeting_state(id, MeetingState::Finalizing, Some(now), None, now)
            .await?;
    }
    load(db, id).await
}

async fn write_summary_file(meeting: &MeetingRow, summary: &str) -> StoreResult<()> {
    let md = format!(
        "# {}\n\n_{}_\n\n{summary}\n",
        meeting.title,
        started_label(meeting.started_at)
    );
    let _guard = FILE_LOCK.lock().await;
    files::write_file(&meeting.dir, files::SUMMARY_FILE, &md)?;
    Ok(())
}

/// Write the summary of a stopped meeting. On success the meeting is `done`
/// and unread; when the model fails it is `failed` with the reason, and the
/// transcript and notes stay. Usage counts against the budget either way.
pub async fn finalize_meeting(
    db: &Database,
    writer: &dyn SummaryWriter,
    id: &str,
    now: i64,
) -> StoreResult<MeetingRow> {
    let meeting = load(db, id).await?;
    if meeting.state != MeetingState::Finalizing {
        return Err(StoreError::Invalid(format!(
            "meeting is {}, not finalizing",
            meeting.state
        )));
    }
    let segments = db.segments_for(id).await?;
    write_transcript(db, &meeting).await?;
    let Some(prompt) = summary::build_prompt(&meeting, &segments) else {
        db.set_meeting_summary(id, summary::NO_SPEECH_SUMMARY, now)
            .await?;
        write_summary_file(&meeting, summary::NO_SPEECH_SUMMARY).await?;
        return load(db, id).await;
    };
    let (reply, usage) = writer.write(id, &prompt).await;
    for u in &usage {
        crate::agent::Agent::log_usage(db, u, None).await;
    }
    match reply {
        Ok(text) => {
            let md = summary::normalize(&text);
            db.set_meeting_summary(id, &md, now).await?;
            write_summary_file(&meeting, &md).await?;
        }
        Err(e) => {
            tracing::warn!(meeting = id, "meeting summary failed: {e:#}");
            db.set_meeting_state(id, MeetingState::Failed, None, Some(&format!("{e:#}")), now)
                .await?;
        }
    }
    load(db, id).await
}

/// Delete a meeting and the files this module wrote in its folder. The
/// files go first: if they cannot be removed the meeting stays listed and
/// the error says so, rather than "deleted" with the transcript still on
/// disk.
pub async fn delete_meeting(db: &Database, id: &str) -> StoreResult<()> {
    let meeting = load(db, id).await?;
    let _guard = FILE_LOCK.lock().await;
    files::remove_meeting_dir(&meeting.dir).map_err(|e| {
        StoreError::Other(anyhow::anyhow!(
            "Borg couldn't remove this meeting's files in {} ({e:#}), so it is still listed. \
             Remove them by hand, then delete it again.",
            meeting.dir.display()
        ))
    })?;
    db.delete_meeting(id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AudioModelConfig;
    use crate::llm::UsageData;
    use crate::meetings::audio::test_wav::wav_ms;
    use crate::meetings::calendar::CalendarEvent;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct Fixture {
        db: Database,
        config: Config,
        _tmp: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.meetings.enabled = true;
        config.meetings.dir = Some(tmp.path().to_string_lossy().into_owned());
        Fixture {
            db: Database::test_db().await,
            config,
            _tmp: tmp,
        }
    }

    /// A transcriber pointed at a mock Whisper endpoint that answers `text`.
    async fn whisper(config: &mut Config, text: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": text })),
            )
            .mount(&server)
            .await;
        // Dictation stays off: meetings must transcribe without it.
        config.audio.enabled = false;
        config.audio.models = vec![AudioModelConfig {
            provider: "openai".into(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: Some(5_000),
            base_url: Some(server.uri()),
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: None,
        }];
        // The key Settings would save; no process-global env var involved.
        config.stored_api_keys.insert(
            "openai".into(),
            crate::security::secrets_resolve::ApiKeyRouting::Literal {
                value: "sk-test".into(),
            },
        );
        server
    }

    fn chunk<'a>(
        meeting_id: &'a str,
        track: Track,
        seq: u32,
        offset_ms: i64,
        wav: &'a [u8],
    ) -> ChunkInput<'a> {
        ChunkInput {
            meeting_id,
            track,
            seq,
            offset_ms,
            wav,
        }
    }

    #[tokio::test]
    async fn start_refuses_when_off_or_already_recording() {
        let mut f = fixture().await;
        f.config.meetings.enabled = false;
        assert!(matches!(
            start_meeting(&f.db, &f.config, StartParams::default(), 1_000).await,
            Err(StoreError::Disabled)
        ));
        f.config.meetings.enabled = true;
        start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        assert!(matches!(
            start_meeting(&f.db, &f.config, StartParams::default(), 1_001).await,
            Err(StoreError::AlreadyRecording)
        ));
    }

    #[tokio::test]
    async fn start_takes_title_and_attendees_from_the_calendar_event() {
        let f = fixture().await;
        f.db.replace_calendar_events(
            "google",
            &[CalendarEvent {
                id: "google:1".into(),
                source: "google".into(),
                title: "Adelphi Daily Stand Up".into(),
                start_unix: 1_000,
                end_unix: 2_800,
                all_day: false,
                attendees: vec![Attendee {
                    name: "Juan".into(),
                    email: "j@x".into(),
                }],
                join_url: "https://meet.google.com/abc-defg-hij".into(),
                provider: "meet".into(),
            }],
            1,
        )
        .await
        .unwrap();
        let m = start_meeting(
            &f.db,
            &f.config,
            StartParams {
                source_app: "Chrome".into(),
                calendar_event_id: "google:1".into(),
                ..Default::default()
            },
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(m.title, "Adelphi Daily Stand Up");
        assert_eq!(m.attendees[0].name, "Juan");
        assert!(m.dir.join(files::TRANSCRIPT_FILE).exists());
        assert!(m
            .dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_adelphi-daily-stand-up"));
    }

    #[tokio::test]
    async fn untitled_meetings_are_named_after_the_app() {
        let f = fixture().await;
        let m = start_meeting(
            &f.db,
            &f.config,
            StartParams {
                source_app: "Zoom".into(),
                ..Default::default()
            },
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(m.title, "Zoom meeting");
    }

    #[tokio::test]
    async fn chunks_transcribe_with_only_audio_models_set_and_retries_are_free() {
        let mut f = fixture().await;
        let server = whisper(&mut f.config, "we patch weekly").await;
        let transcriber = AudioTranscriber::from_models(&f.config).expect("chain builds");
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        let wav = wav_ms(2_000);
        let out = append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&m.id, Track::System, 0, 0, &wav),
            1_001,
        )
        .await
        .unwrap();
        assert_eq!(out.appended[0].text, "we patch weekly");
        assert_eq!(out.appended[0].end_ms, 2_000);
        // Same (track, seq) again: answered from the database.
        let again = append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&m.id, Track::System, 0, 0, &wav),
            1_002,
        )
        .await
        .unwrap();
        assert_eq!(again.appended, out.appended);
        assert!(
            !out.replayed && again.replayed,
            "only the retry is a replay"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(f.db.segments_for(&m.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn silence_babble_adds_nothing() {
        let mut f = fixture().await;
        let _server = whisper(&mut f.config, "Thank you.").await;
        let transcriber = AudioTranscriber::from_models(&f.config).unwrap();
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        let wav = wav_ms(2_000);
        let out = append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&m.id, Track::Mic, 0, 0, &wav),
            1_001,
        )
        .await
        .unwrap();
        assert!(out.dropped_as_silence && out.appended.is_empty());
        assert!(f.db.segments_for(&m.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn transcript_file_is_in_time_order_whatever_the_arrival_order() {
        let mut f = fixture().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text": "later words"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text": "first words"})),
            )
            .mount(&server)
            .await;
        whisper(&mut f.config, "unused").await;
        f.config.audio.models[0].base_url = Some(server.uri());
        let transcriber = AudioTranscriber::from_models(&f.config).unwrap();
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        let wav = wav_ms(1_000);
        // The other side's chunk at 30 s lands before the user's at 0 s.
        append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&m.id, Track::System, 1, 30_000, &wav),
            1_001,
        )
        .await
        .unwrap();
        append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&m.id, Track::Mic, 0, 0, &wav),
            1_002,
        )
        .await
        .unwrap();
        let md = std::fs::read_to_string(m.dir.join(files::TRANSCRIPT_FILE)).unwrap();
        let first = md.find("Me:** first words").expect(&md);
        let later = md.find("Them:** later words").expect(&md);
        assert!(first < later, "{md}");
    }

    #[tokio::test]
    async fn chunks_are_refused_for_stopped_oversized_or_unknown_meetings() {
        let mut f = fixture().await;
        let wav = wav_ms(1_000);
        assert!(matches!(
            append_chunk(
                &f.db,
                &f.config,
                None,
                chunk("mtg_missing", Track::Mic, 0, 0, &wav),
                1
            )
            .await,
            Err(StoreError::NotFound(_))
        ));
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        // No transcriber at all.
        assert!(matches!(
            append_chunk(
                &f.db,
                &f.config,
                None,
                chunk(&m.id, Track::Mic, 0, 0, &wav),
                1_001
            )
            .await,
            Err(StoreError::TranscriptionUnavailable)
        ));
        f.config.audio.max_file_size = 1_000;
        assert!(matches!(
            append_chunk(
                &f.db,
                &f.config,
                None,
                chunk(&m.id, Track::Mic, 1, 0, &wav),
                1_001
            )
            .await,
            Err(StoreError::Invalid(_))
        ));
        stop_meeting(&f.db, &m.id, 1_002).await.unwrap();
        assert!(matches!(
            append_chunk(
                &f.db,
                &f.config,
                None,
                chunk(&m.id, Track::Mic, 2, 0, &wav),
                1_003
            )
            .await,
            Err(StoreError::NotRecording)
        ));
    }

    #[tokio::test]
    async fn audio_is_kept_only_when_asked() {
        let mut f = fixture().await;
        let _server = whisper(&mut f.config, "hello there").await;
        let transcriber = AudioTranscriber::from_models(&f.config).unwrap();
        let wav = wav_ms(1_000);

        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&m.id, Track::Mic, 0, 0, &wav),
            1_001,
        )
        .await
        .unwrap();
        assert!(!m.dir.join("audio-mic.wav").exists());
        stop_meeting(&f.db, &m.id, 1_002).await.unwrap();

        f.config.meetings.keep_audio = true;
        let k = start_meeting(&f.db, &f.config, StartParams::default(), 2_000)
            .await
            .unwrap();
        append_chunk(
            &f.db,
            &f.config,
            Some(&transcriber),
            chunk(&k.id, Track::Mic, 0, 0, &wav),
            2_001,
        )
        .await
        .unwrap();
        assert!(k.dir.join("audio-mic.wav").exists());
    }

    #[tokio::test]
    async fn notes_and_rename_reach_the_row_and_the_files() {
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        let after = update_notes(&f.db, &m.id, "ask about IL5", Some("ISSM interview"), 1_001)
            .await
            .unwrap();
        assert_eq!(after.notes, "ask about IL5");
        assert_eq!(after.title, "ISSM interview");
        assert_eq!(
            std::fs::read_to_string(m.dir.join(files::NOTES_FILE)).unwrap(),
            "ask about IL5"
        );
        assert!(std::fs::read_to_string(m.dir.join(files::TRANSCRIPT_FILE))
            .unwrap()
            .starts_with("# ISSM interview"));
        // A blank title leaves the name alone.
        let kept = update_notes(&f.db, &m.id, "x", Some("  "), 1_002)
            .await
            .unwrap();
        assert_eq!(kept.title, "ISSM interview");
    }

    struct Fake {
        reply: Result<&'static str, &'static str>,
        usage_tokens: u64,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SummaryWriter for Fake {
        async fn write(
            &self,
            _id: &str,
            _prompt: &str,
        ) -> (anyhow::Result<String>, Vec<UsageData>) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let usage = vec![UsageData {
                prompt_tokens: self.usage_tokens,
                completion_tokens: 10,
                total_tokens: self.usage_tokens + 10,
                provider: "openrouter".into(),
                model: "test/model".into(),
                ..Default::default()
            }];
            (
                self.reply
                    .map(str::to_string)
                    .map_err(|e| anyhow::anyhow!(e)),
                usage,
            )
        }
    }

    fn fake(reply: Result<&'static str, &'static str>) -> Fake {
        Fake {
            reply,
            usage_tokens: 500,
            calls: Default::default(),
        }
    }

    async fn total_logged_tokens(db: &Database) -> i64 {
        let mut rows = db
            .conn()
            .query("SELECT COALESCE(SUM(total_tokens), 0) FROM token_usage", ())
            .await
            .unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    }

    #[tokio::test]
    async fn finalize_writes_the_summary_marks_unread_and_counts_usage() {
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        update_notes(&f.db, &m.id, "decided to ship", None, 1_001)
            .await
            .unwrap();
        stop_meeting(&f.db, &m.id, 1_002).await.unwrap();
        let writer = fake(Ok("```\n# Decisions\n- Ship it\n```"));
        let done = finalize_meeting(&f.db, &writer, &m.id, 1_003)
            .await
            .unwrap();
        assert_eq!(done.state, MeetingState::Done);
        assert!(done.unread);
        assert_eq!(done.summary_md.as_deref(), Some("**Decisions**\n- Ship it"));
        let file = std::fs::read_to_string(m.dir.join(files::SUMMARY_FILE)).unwrap();
        assert!(file.contains("**Decisions**\n- Ship it"));
        assert_eq!(total_logged_tokens(&f.db).await, 510);
    }

    #[tokio::test]
    async fn finalize_without_speech_or_notes_never_asks_the_model() {
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        stop_meeting(&f.db, &m.id, 1_001).await.unwrap();
        let writer = fake(Ok("should not be used"));
        let done = finalize_meeting(&f.db, &writer, &m.id, 1_002)
            .await
            .unwrap();
        assert_eq!(writer.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(done.state, MeetingState::Done);
        assert_eq!(done.summary_md.as_deref(), Some(summary::NO_SPEECH_SUMMARY));
    }

    #[tokio::test]
    async fn a_failed_summary_keeps_the_transcript_and_says_why() {
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        update_notes(&f.db, &m.id, "something", None, 1_001)
            .await
            .unwrap();
        stop_meeting(&f.db, &m.id, 1_002).await.unwrap();
        let failed = finalize_meeting(&f.db, &fake(Err("model is down")), &m.id, 1_003)
            .await
            .unwrap();
        assert_eq!(failed.state, MeetingState::Failed);
        assert!(failed.error.unwrap().contains("model is down"));
        assert!(m.dir.join(files::TRANSCRIPT_FILE).exists());
        assert!(m.dir.join(files::NOTES_FILE).exists());
        // Usage of a failed call still counts.
        assert_eq!(total_logged_tokens(&f.db).await, 510);
    }

    #[tokio::test]
    async fn finalize_refuses_a_meeting_that_is_still_recording() {
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        assert!(matches!(
            finalize_meeting(&f.db, &fake(Ok("x")), &m.id, 1_001).await,
            Err(StoreError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_delete_removes_row_and_folder() {
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        let stopped = stop_meeting(&f.db, &m.id, 1_500).await.unwrap();
        assert_eq!(stopped.state, MeetingState::Finalizing);
        assert_eq!(stopped.ended_at, Some(1_500));
        let again = stop_meeting(&f.db, &m.id, 1_900).await.unwrap();
        assert_eq!(again.ended_at, Some(1_500));
        delete_meeting(&f.db, &m.id).await.unwrap();
        assert!(f.db.get_meeting(&m.id).await.unwrap().is_none());
        assert!(!m.dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_delete_that_cannot_remove_the_files_says_so_and_keeps_the_meeting() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture().await;
        let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
            .await
            .unwrap();
        // Files inside a read-only folder cannot be removed.
        std::fs::set_permissions(&m.dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let err = delete_meeting(&f.db, &m.id).await.unwrap_err().to_string();
        std::fs::set_permissions(&m.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(err.contains("still listed"), "{err}");
        assert!(
            f.db.get_meeting(&m.id).await.unwrap().is_some(),
            "never 'deleted' while the transcript is still on disk"
        );
        assert!(m.dir.join(files::TRANSCRIPT_FILE).exists());
        delete_meeting(&f.db, &m.id).await.unwrap();
        assert!(!m.dir.exists());
    }

    #[tokio::test]
    async fn two_starts_at_the_same_moment_record_one_meeting() {
        let f = fixture().await;
        let (a, b) = tokio::join!(
            start_meeting(&f.db, &f.config, StartParams::default(), 1_000),
            start_meeting(&f.db, &f.config, StartParams::default(), 1_000),
        );
        assert_eq!(
            [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "one start wins, the other is told a meeting is recording"
        );
        assert!(matches!(
            a.err().or(b.err()),
            Some(StoreError::AlreadyRecording)
        ));
    }

    #[tokio::test]
    async fn kept_audio_holds_each_chunk_once_however_often_it_is_sent() {
        let frames = |p: &std::path::Path| hound::WavReader::open(p).unwrap().duration();
        let wav = wav_ms(2_000);
        for (said, speech) in [("Thank you.", false), ("Real words here.", true)] {
            let mut f = fixture().await;
            let _server = whisper(&mut f.config, said).await;
            f.config.meetings.keep_audio = true;
            let transcriber = AudioTranscriber::from_models(&f.config).unwrap();
            let m = start_meeting(&f.db, &f.config, StartParams::default(), 1_000)
                .await
                .unwrap();
            let push = || {
                append_chunk(
                    &f.db,
                    &f.config,
                    Some(&transcriber),
                    chunk(&m.id, Track::Mic, 0, 0, &wav),
                    1_001,
                )
            };
            let first = push().await.unwrap();
            let once = frames(&m.dir.join("audio-mic.wav"));
            // A client that never saw the reply sends the chunk again.
            let retry = push().await.unwrap();
            assert!(retry.replayed, "{said}");
            assert_eq!(first.dropped_as_silence, !speech);
            assert_eq!(retry.dropped_as_silence, !speech, "{said}");
            assert_eq!(
                frames(&m.dir.join("audio-mic.wav")),
                once,
                "{said}: the retry added the audio again"
            );
            assert_eq!(
                f.db.segments_for(&m.id).await.unwrap().len(),
                usize::from(speech),
                "{said}: silence is not a transcript line"
            );
        }
    }
}
