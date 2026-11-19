//! `Meetings` gRPC service: the desktop app pushes audio here, every
//! surface reads meetings and the calendar from here.
//!
//! Thin by design: each RPC loads the live settings and the database, calls
//! `cortex::meetings`, maps the result to the wire, and announces what
//! changed on the [`MeetingEventBus`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cortex::config::Config;
use cortex::db::{Database, MeetingRow, SegmentRow};
use cortex::media::understanding::AudioTranscriber;
use cortex::meetings::calendar::{self, google::GoogleEndpoints, sync};
use cortex::meetings::store::{self, ChunkInput, StartParams, StoreError};
use cortex::meetings::{now_unix, MeetingState, Track};
use protocols::meetings::meeting_event::Kind;
use protocols::meetings::meetings_server::Meetings;
use protocols::meetings::{
    Attendee as PbAttendee, CalendarChanged, CalendarEvent as PbCalendarEvent, CalendarEvents,
    CalendarStatus, ConnectCalendarRequest, ConnectCalendarResponse, Empty, ListMeetingsRequest,
    Meeting as PbMeeting, MeetingEvent, MeetingId, MeetingList, MeetingStarted, MeetingStopped,
    MeetingSummary as PbMeetingSummary, MeetingsStatus, PushAudioRequest, PushAudioResponse,
    Segment as PbSegment, StartMeetingRequest, TranscriptAppended, UpcomingRequest,
    UpdateNotesRequest,
};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status as TStatus};

use crate::meetings::{oauth_loopback, MeetingsHandles};

/// Largest chunk `PushAudio` decodes: 25 s of 16 kHz mono is ~800 KB, so
/// this leaves room for a client that sends 48 kHz stereo.
pub const MAX_CHUNK_BYTES: usize = crate::grpc::voice::MAX_RECORDING_BYTES;

/// Default and ceiling for `ListMeetings.limit`.
const DEFAULT_PAGE: u32 = 30;
const MAX_PAGE: u32 = 200;

/// The service.
pub struct MeetingsSvc {
    handles: MeetingsHandles,
    google: GoogleEndpoints,
    /// Stops the listener of a Google sign-in still waiting, when a newer
    /// one starts.
    pending_sign_in: Arc<Mutex<Option<CancellationToken>>>,
}

impl MeetingsSvc {
    /// A service publishing to `handles`.
    pub fn new(handles: MeetingsHandles) -> Self {
        Self {
            handles,
            google: GoogleEndpoints::default(),
            pending_sign_in: Arc::new(Mutex::new(None)),
        }
    }

    /// Point Google calls somewhere else (tests).
    pub fn with_google_endpoints(mut self, endpoints: GoogleEndpoints) -> Self {
        self.google = endpoints;
        self
    }

    fn publish(&self, kind: Kind) {
        self.handles.bus.publish(kind);
    }
}

async fn deps() -> Result<(Config, Database), TStatus> {
    let config = Config::load_from_db()
        .await
        .map_err(|e| internal("load settings", e))?;
    let db = Database::open()
        .await
        .map_err(|e| internal("open database", e))?;
    Ok((config, db))
}

/// The event that tells a waiting Connections page its Google sign-in did
/// not finish, and why.
fn sign_in_failed(why: String) -> Kind {
    Kind::CalendarChanged(CalendarChanged {
        connected: false,
        error: why,
    })
}

fn internal(what: &str, e: impl std::fmt::Display) -> TStatus {
    tracing::warn!("meetings: {what} failed: {e}");
    TStatus::internal(format!("{what} failed: {e}"))
}

/// Each store refusal has one status, so clients branch on the code.
fn status(e: StoreError) -> TStatus {
    match e {
        StoreError::NotFound(_) => TStatus::not_found(e.to_string()),
        StoreError::AlreadyRecording => TStatus::already_exists(e.to_string()),
        StoreError::NotRecording
        | StoreError::StillWriting
        | StoreError::Disabled
        | StoreError::TranscriptionUnavailable => TStatus::failed_precondition(e.to_string()),
        StoreError::TranscriptionFailed(_) => TStatus::unavailable(e.to_string()),
        // Retrying will not help; the message names the fix.
        StoreError::TranscriptionRefused(_) => TStatus::failed_precondition(e.to_string()),
        StoreError::Invalid(_) => TStatus::invalid_argument(e.to_string()),
        StoreError::Other(err) => {
            // The store's own errors are written for the person; no prefix.
            tracing::warn!("meetings: {err:#}");
            TStatus::internal(format!("{err:#}"))
        }
    }
}

fn track_of(raw: i32) -> Result<Track, TStatus> {
    match protocols::meetings::Track::try_from(raw) {
        Ok(protocols::meetings::Track::Mic) => Ok(Track::Mic),
        Ok(protocols::meetings::Track::System) => Ok(Track::System),
        _ => Err(TStatus::invalid_argument("track must be MIC or SYSTEM")),
    }
}

fn to_attendees(list: &[cortex::meetings::Attendee]) -> Vec<PbAttendee> {
    list.iter()
        .map(|a| PbAttendee {
            name: a.name.clone(),
            email: a.email.clone(),
        })
        .collect()
}

fn has_summary(m: &MeetingRow) -> bool {
    m.summary_md
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
}

pub(crate) fn to_summary(m: &MeetingRow) -> PbMeetingSummary {
    PbMeetingSummary {
        id: m.id.clone(),
        title: m.title.clone(),
        started_at: m.started_at,
        ended_at: m.ended_at.unwrap_or(0),
        state: m.state.as_str().to_string(),
        source_app: m.source_app.clone(),
        attendees: to_attendees(&m.attendees),
        unread: m.unread,
        has_summary: has_summary(m),
        calendar_event_id: m.calendar_event_id.clone(),
    }
}

pub(crate) fn to_segment(s: &SegmentRow) -> PbSegment {
    PbSegment {
        track: s.track.as_str().to_string(),
        start_ms: s.start_ms,
        end_ms: s.end_ms,
        text: s.text.clone(),
    }
}

fn to_meeting(m: &MeetingRow, segments: &[SegmentRow]) -> PbMeeting {
    PbMeeting {
        summary: Some(to_summary(m)),
        segments: segments.iter().map(to_segment).collect(),
        notes: m.notes.clone(),
        summary_markdown: m.summary_md.clone().unwrap_or_default(),
        error: m.error.clone().unwrap_or_default(),
        dir: m.dir.display().to_string(),
    }
}

/// A cached calendar event on the wire.
pub(crate) fn to_pb_event(e: &calendar::CalendarEvent) -> PbCalendarEvent {
    PbCalendarEvent {
        id: e.id.clone(),
        title: e.title.clone(),
        start_unix: e.start_unix,
        end_unix: e.end_unix,
        attendees: to_attendees(&e.attendees),
        join_url: e.join_url.clone(),
        provider: e.provider.clone(),
        all_day: e.all_day,
    }
}

async fn load_meeting(db: &Database, id: &str) -> Result<PbMeeting, TStatus> {
    let row = db
        .get_meeting(id)
        .await
        .map_err(|e| internal("read meeting", e))?
        .ok_or_else(|| TStatus::not_found(format!("there is no meeting {id}")))?;
    let segments = db
        .segments_for(id)
        .await
        .map_err(|e| internal("read transcript", e))?;
    Ok(to_meeting(&row, &segments))
}

fn required_id(id: &str) -> Result<&str, TStatus> {
    let id = id.trim();
    if id.is_empty() {
        return Err(TStatus::invalid_argument("meeting id required"));
    }
    Ok(id)
}

#[tonic::async_trait]
impl Meetings for MeetingsSvc {
    async fn get_meetings_status(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<MeetingsStatus>, TStatus> {
        let (config, db) = deps().await?;
        let conn = calendar::connection(&config);
        let recordings_dir = cortex::meetings::files::meetings_dir(&config)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Ok(Response::new(MeetingsStatus {
            enabled: config.meetings.enabled,
            transcription_ready: AudioTranscriber::from_models(&config).is_some(),
            detection_supported: self.handles.detection_supported.load(Ordering::Relaxed),
            calendar: Some(CalendarStatus {
                connected: conn.connected,
                kind: conn.kind.to_string(),
                account_label: conn.account_label,
            }),
            recordings_dir,
            unread_count: db
                .unread_meeting_count()
                .await
                .map_err(|e| internal("count unread", e))?,
        }))
    }

    async fn list_meetings(
        &self,
        req: Request<ListMeetingsRequest>,
    ) -> Result<Response<MeetingList>, TStatus> {
        let req = req.into_inner();
        let (_, db) = deps().await?;
        let limit = u32::try_from(req.limit)
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PAGE)
            .min(MAX_PAGE);
        let before = (req.before_unix > 0).then_some(req.before_unix);
        let rows = db
            .list_meetings(limit, before)
            .await
            .map_err(|e| internal("list meetings", e))?;
        Ok(Response::new(MeetingList {
            meetings: rows.iter().map(to_summary).collect(),
        }))
    }

    async fn get_meeting(&self, req: Request<MeetingId>) -> Result<Response<PbMeeting>, TStatus> {
        let id = req.into_inner().id;
        let (_, db) = deps().await?;
        Ok(Response::new(load_meeting(&db, required_id(&id)?).await?))
    }

    async fn start_meeting(
        &self,
        req: Request<StartMeetingRequest>,
    ) -> Result<Response<PbMeeting>, TStatus> {
        let req = req.into_inner();
        let (config, db) = deps().await?;
        let row = store::start_meeting(
            &db,
            &config,
            StartParams {
                title: req.title,
                source_app: req.source_app,
                calendar_event_id: req.calendar_event_id,
            },
            now_unix(),
        )
        .await
        .map_err(status)?;
        self.publish(Kind::Started(MeetingStarted {
            id: row.id.clone(),
            auto_started: false,
        }));
        Ok(Response::new(to_meeting(&row, &[])))
    }

    async fn push_audio(
        &self,
        req: Request<PushAudioRequest>,
    ) -> Result<Response<PushAudioResponse>, TStatus> {
        let req = req.into_inner();
        let track = track_of(req.track)?;
        let id = required_id(&req.meeting_id)?.to_string();
        let (config, db) = deps().await?;
        if !config.meetings.enabled {
            return Err(status(StoreError::Disabled));
        }
        let transcriber = AudioTranscriber::from_models(&config);
        let outcome = store::append_chunk(
            &db,
            &config,
            transcriber.as_ref(),
            ChunkInput {
                meeting_id: &id,
                track,
                seq: req.seq,
                offset_ms: req.offset_ms,
                wav: &req.wav,
            },
            now_unix(),
        )
        .await
        .map_err(status)?;
        let appended: Vec<PbSegment> = outcome.appended.iter().map(to_segment).collect();
        if !appended.is_empty() && !outcome.replayed {
            self.publish(Kind::TranscriptAppended(TranscriptAppended {
                meeting_id: id,
                segments: appended.clone(),
            }));
        }
        Ok(Response::new(PushAudioResponse { appended }))
    }

    async fn update_notes(
        &self,
        req: Request<UpdateNotesRequest>,
    ) -> Result<Response<Empty>, TStatus> {
        let req = req.into_inner();
        let (_, db) = deps().await?;
        store::update_notes(
            &db,
            required_id(&req.meeting_id)?,
            &req.notes,
            req.title.as_deref(),
            now_unix(),
        )
        .await
        .map_err(status)?;
        Ok(Response::new(Empty {}))
    }

    async fn stop_meeting(&self, req: Request<MeetingId>) -> Result<Response<PbMeeting>, TStatus> {
        let id = req.into_inner().id;
        let id = required_id(&id)?;
        let (_, db) = deps().await?;
        let row = store::stop_meeting(&db, id, now_unix())
            .await
            .map_err(status)?;
        if row.state == MeetingState::Finalizing {
            self.publish(Kind::Stopped(MeetingStopped { id: id.to_string() }));
            // A full queue is fine: the finalizer's scan finds it anyway.
            if let Err(e) = self.handles.finalize_tx.try_send(id.to_string()) {
                tracing::warn!(
                    meeting = id,
                    "finalize queue: {e}; the next scan will pick it up"
                );
            }
        }
        Ok(Response::new(load_meeting(&db, id).await?))
    }

    async fn resume_meeting(
        &self,
        req: Request<MeetingId>,
    ) -> Result<Response<PbMeeting>, TStatus> {
        let id = req.into_inner().id;
        let id = required_id(&id)?;
        let (config, db) = deps().await?;
        let row = store::resume_meeting(&db, &config, id, now_unix())
            .await
            .map_err(status)?;
        // The desktop opens (or keeps) the notes panel on this, which is what
        // starts capturing again.
        self.publish(Kind::Started(MeetingStarted {
            id: row.id.clone(),
            auto_started: false,
        }));
        Ok(Response::new(load_meeting(&db, id).await?))
    }

    async fn delete_meeting(&self, req: Request<MeetingId>) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        let id = required_id(&id)?;
        let (_, db) = deps().await?;
        let was_recording = db
            .get_meeting(id)
            .await
            .map_err(|e| internal("read meeting", e))?
            .is_some_and(|m| m.state == MeetingState::Recording);
        store::delete_meeting(&db, id).await.map_err(status)?;
        if was_recording {
            // The capturing panel must stop pushing into a meeting that is gone.
            self.publish(Kind::Stopped(MeetingStopped { id: id.to_string() }));
        }
        Ok(Response::new(Empty {}))
    }

    async fn mark_meeting_read(&self, req: Request<MeetingId>) -> Result<Response<Empty>, TStatus> {
        let id = req.into_inner().id;
        let (_, db) = deps().await?;
        db.mark_meeting_read(required_id(&id)?)
            .await
            .map_err(|e| internal("mark read", e))?;
        Ok(Response::new(Empty {}))
    }

    async fn list_upcoming_events(
        &self,
        req: Request<UpcomingRequest>,
    ) -> Result<Response<CalendarEvents>, TStatus> {
        let req = req.into_inner();
        if req.to_unix <= req.from_unix {
            return Err(TStatus::invalid_argument("to_unix must be after from_unix"));
        }
        let (_, db) = deps().await?;
        let events = db
            .calendar_events_between(req.from_unix, req.to_unix)
            .await
            .map_err(|e| internal("read calendar", e))?;
        Ok(Response::new(CalendarEvents {
            events: events.iter().map(to_pb_event).collect(),
        }))
    }

    async fn connect_calendar(
        &self,
        req: Request<ConnectCalendarRequest>,
    ) -> Result<Response<ConnectCalendarResponse>, TStatus> {
        let req = req.into_inner();
        let (config, db) = deps().await?;
        match req.kind.as_str() {
            "ics" => {
                let count = sync::connect_ics(&db, &config, &req.ics_url, now_unix())
                    .await
                    .map_err(|e| TStatus::invalid_argument(format!("{e:#}")))?;
                self.publish(Kind::CalendarChanged(CalendarChanged {
                    connected: true,
                    error: String::new(),
                }));
                self.handles.calendar_poke.notify_one();
                let plural = if count == 1 { "" } else { "s" };
                Ok(Response::new(ConnectCalendarResponse {
                    auth_url: String::new(),
                    status_message: format!("Connected. {count} event{plural} in the next 7 days."),
                }))
            }
            "google" => {
                let client_id = req.google_client_id.trim().to_string();
                let client_secret = req.google_client_secret.trim().to_string();
                if client_id.is_empty() || client_secret.is_empty() {
                    return Err(TStatus::invalid_argument(
                        "paste the client ID and client secret of your Google OAuth client",
                    ));
                }
                let loopback = oauth_loopback::bind()
                    .await
                    .map_err(|e| internal("start the sign-in listener", e))?;
                let pending =
                    calendar::google::begin_auth(&self.google, &client_id, &loopback.redirect_uri);
                loopback.expect_state(&pending.state);
                if let Some(old) = self
                    .pending_sign_in
                    .lock()
                    .await
                    .replace(loopback.stop_token())
                {
                    old.cancel();
                }
                let auth_url = pending.url.clone();
                let bus = self.handles.bus.clone();
                let poke = self.handles.calendar_poke.clone();
                let endpoints = self.google.clone();
                // A bounded one-off: it ends on the redirect, a newer
                // sign-in, or the ten-minute timeout.
                tokio::spawn(async move {
                    let code = match loopback.wait(oauth_loopback::SIGN_IN_TIMEOUT).await {
                        Ok(code) => code,
                        // A newer sign-in took over; that one reports instead.
                        Err(oauth_loopback::SignInEnd::Replaced) => return,
                        Err(oauth_loopback::SignInEnd::Failed(why)) => {
                            tracing::info!("Google Calendar sign-in did not finish: {why}");
                            bus.publish(sign_in_failed(why));
                            return;
                        }
                    };
                    match sync::complete_google(
                        &db,
                        &config,
                        cortex::http::shared(),
                        &endpoints,
                        &client_id,
                        &client_secret,
                        &code,
                        &pending,
                        now_unix(),
                    )
                    .await
                    {
                        Ok(account) => {
                            tracing::info!("Google Calendar connected for {account}");
                            bus.publish(Kind::CalendarChanged(CalendarChanged {
                                connected: true,
                                error: String::new(),
                            }));
                            poke.notify_one();
                        }
                        Err(e) => {
                            tracing::warn!("Google Calendar sign-in failed: {e:#}");
                            let why = format!("{e:#}");
                            bus.publish(sign_in_failed(format!(
                                "{}. Check the client ID and secret, then connect again.",
                                why.trim_end_matches('.')
                            )));
                        }
                    }
                });
                Ok(Response::new(ConnectCalendarResponse {
                    auth_url,
                    status_message: "Approve access in your browser, then come back here."
                        .to_string(),
                }))
            }
            other => Err(TStatus::invalid_argument(format!(
                "unknown calendar kind {other:?}; use \"google\" or \"ics\""
            ))),
        }
    }

    async fn disconnect_calendar(&self, _req: Request<Empty>) -> Result<Response<Empty>, TStatus> {
        let (_, db) = deps().await?;
        if let Some(pending) = self.pending_sign_in.lock().await.take() {
            pending.cancel();
        }
        sync::disconnect(&db)
            .await
            .map_err(|e| internal("disconnect calendar", format!("{e:#}")))?;
        self.publish(Kind::CalendarChanged(CalendarChanged {
            connected: false,
            error: String::new(),
        }));
        self.handles.calendar_poke.notify_one();
        Ok(Response::new(Empty {}))
    }

    type SubscribeMeetingEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<MeetingEvent, TStatus>> + Send + 'static>,
    >;

    async fn subscribe_meeting_events(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeMeetingEventsStream>, TStatus> {
        let mut sub = self.handles.bus.subscribe();
        let (tx, rx) = mpsc::channel::<Result<MeetingEvent, TStatus>>(32);
        tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(evt) => {
                        if tx.send(Ok(evt)).await.is_err() {
                            return; // client gone
                        }
                    }
                    // Every event is a hint to refetch; a lagging client
                    // catches up from the RPCs, so keep the feed alive.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("meeting events subscriber lagged by {n}; continuing");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
