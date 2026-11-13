//! Meeting notes: transcripts, the user's notes and a summary for calls the
//! user is already in. Nothing joins the call.
//!
//! The desktop app captures the microphone ("Me") and the system audio
//! ("Them") and pushes short WAV chunks; [`store`] transcribes each chunk
//! with the shared `audio.models` chain, keeps segments in the database and
//! the readable files in the meeting's folder, and [`summary`] writes the
//! notes with the auxiliary model once the meeting stops. [`calendar`] feeds
//! the "Coming up" view and lets the detector name a meeting.
//!
//! Everything here is platform-neutral; the macOS call detector and the gRPC
//! service live in `borgd`.

pub mod audio;
pub mod calendar;
pub mod files;
pub mod store;
pub mod summary;
pub mod transcript;

use std::fmt;
use std::str::FromStr;

/// Where a meeting is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetingState {
    /// Audio is arriving.
    Recording,
    /// Stopped; the summary is being written.
    Finalizing,
    /// Summary written.
    Done,
    /// The summary could not be written; transcript and notes are kept.
    Failed,
}

impl MeetingState {
    /// The stored and wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Finalizing => "finalizing",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for MeetingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MeetingState {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "recording" => Self::Recording,
            "finalizing" => Self::Finalizing,
            "done" => Self::Done,
            "failed" => Self::Failed,
            other => anyhow::bail!("unknown meeting state `{other}`"),
        })
    }
}

/// One side of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Track {
    /// The user's microphone.
    Mic,
    /// Everything the computer plays: the other participants.
    System,
}

impl Track {
    /// The stored and wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }

    /// Who is speaking, as a transcript shows it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mic => "Me",
            Self::System => "Them",
        }
    }
}

impl FromStr for Track {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "mic" => Self::Mic,
            "system" => Self::System,
            other => anyhow::bail!("unknown track `{other}`"),
        })
    }
}

/// Someone invited to a meeting.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attendee {
    /// Display name; may be empty.
    #[serde(default)]
    pub name: String,
    /// Email address; may be empty.
    #[serde(default)]
    pub email: String,
}

/// New meeting id: `mtg_` + 16 hex characters.
pub fn new_meeting_id() -> String {
    format!("mtg_{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_track_round_trip_through_their_stored_form() {
        for s in [
            MeetingState::Recording,
            MeetingState::Finalizing,
            MeetingState::Done,
            MeetingState::Failed,
        ] {
            assert_eq!(s.as_str().parse::<MeetingState>().unwrap(), s);
        }
        for t in [Track::Mic, Track::System] {
            assert_eq!(t.as_str().parse::<Track>().unwrap(), t);
        }
        assert!("paused".parse::<MeetingState>().is_err());
        assert!("speaker".parse::<Track>().is_err());
    }

    #[test]
    fn meeting_ids_are_prefixed_and_distinct() {
        let a = new_meeting_id();
        let b = new_meeting_id();
        assert!(a.starts_with("mtg_") && a.len() == 20, "{a}");
        assert_ne!(a, b);
    }
}
