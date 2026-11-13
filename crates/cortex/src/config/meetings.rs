//! Meeting notes: recording calls the user is already in, then summarising
//! them. Capture happens in the desktop app; the daemon transcribes, stores
//! and summarises (`crate::meetings`).

use serde::{Deserialize, Serialize};

use crate::security::secrets_resolve::SecretRef;

/// `meetings.*` settings.
///
/// Deliberately small: the transcriber is the shared `audio.models` chain,
/// not a knob here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MeetingsConfig {
    /// Master switch. Off: nothing is detected, nothing records, and the
    /// `meetings` tool is not offered to the model.
    pub enabled: bool,
    /// Where meeting folders go. Unset means `recordings/meetings/` under
    /// Borg's data directory; a leading `~` is expanded.
    pub dir: Option<String>,
    /// Keep each track's audio next to the transcript. Off by default: an
    /// hour of 16 kHz mono is ~110 MB per track, and the text is the point.
    pub keep_audio: bool,
    /// Show "Meeting detected" and wait for a click. Off: a detected call
    /// that lines up with a calendar event starts recording on its own.
    pub ask_before_recording: bool,
    /// The connected calendar.
    pub calendar: MeetingsCalendarConfig,
}

impl Default for MeetingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: None,
            keep_audio: false,
            ask_before_recording: true,
            calendar: MeetingsCalendarConfig::default(),
        }
    }
}

/// `meetings.calendar.*`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MeetingsCalendarConfig {
    /// The calendar's secret ICS address. A secret: anyone holding it can
    /// read the whole calendar.
    pub ics_url: Option<SecretRef>,
    /// The Google sign-in: a JSON `GoogleCalendarCredential` (client id and
    /// secret, refresh token, account) held as a secret. Written by
    /// `Meetings.ConnectCalendar`, never typed by hand, so it has no
    /// settings-page row. Kept out of `credentials.*`, whose rows are LLM
    /// and speech provider keys.
    pub google: Option<SecretRef>,
}
