//! The notes written after a meeting: the auxiliary model reads the user's
//! notes and the transcript and answers in Granola's shape — bold topic
//! headings over bullets, then decisions and action items.

use crate::config::Config;
use crate::db::{MeetingRow, SegmentRow};
use crate::llm::{LlmClient, UsageData};
use crate::meetings::transcript;
use crate::types::Message;

/// Instructions for the model.
pub const SUMMARY_PROMPT: &str = include_str!("../../templates/tasks/MEETING_SUMMARY.md");

/// How much transcript the prompt carries. About 12k tokens: an hour-long
/// call fits whole, a three-hour one keeps its opening and its end.
pub const TRANSCRIPT_CHAR_BUDGET: usize = 48_000;
/// How much of the user's own notes the prompt carries.
pub const NOTES_CHAR_BUDGET: usize = 8_000;

/// Written when nothing was said and nothing was typed: there is nothing to
/// summarise, and asking a model would only invent something.
pub const NO_SPEECH_SUMMARY: &str = "_No speech was captured in this meeting._";

/// The user message for the model, or `None` when the meeting holds neither
/// speech nor notes.
pub fn build_prompt(meeting: &MeetingRow, segments: &[SegmentRow]) -> Option<String> {
    let transcript = transcript::render_for_llm(segments, TRANSCRIPT_CHAR_BUDGET);
    let notes = meeting.notes.trim();
    if transcript.trim().is_empty() && notes.is_empty() {
        return None;
    }
    let notes: String = notes.chars().take(NOTES_CHAR_BUDGET).collect();
    let attendees = meeting
        .attendees
        .iter()
        .map(|a| {
            if a.name.is_empty() {
                a.email.as_str()
            } else {
                a.name.as_str()
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!("Meeting: {}\n", meeting.title);
    if !attendees.is_empty() {
        out.push_str(&format!("Invited: {attendees}\n"));
    }
    out.push_str("\n## The user's notes\n\n");
    out.push_str(if notes.is_empty() { "(none)" } else { &notes });
    out.push_str("\n\n## Transcript\n\n");
    out.push_str(if transcript.trim().is_empty() {
        "(nothing was transcribed)"
    } else {
        &transcript
    });
    Some(out)
}

/// Tidy the model's reply into the shape the notes page renders: no wrapping
/// code fence, and `#` headings turned into bold lines.
pub fn normalize(reply: &str) -> String {
    let mut text = reply.trim();
    if let Some(inner) = text.strip_prefix("```") {
        // Drop the opening fence's language tag, then the closing fence.
        let inner = inner.split_once('\n').map_or("", |(_, rest)| rest);
        text = inner.trim_end().strip_suffix("```").unwrap_or(inner).trim();
    }
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                let heading = trimmed.trim_start_matches('#').trim();
                if heading.is_empty() {
                    String::new()
                } else {
                    format!("**{}**", heading.trim_matches('*'))
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Writes a summary from a prompt. The production writer is
/// [`AuxSummaryWriter`]; the seam exists so the meeting lifecycle around it
/// can be tested without a model.
#[async_trait::async_trait]
pub trait SummaryWriter: Send + Sync {
    /// The reply, and the usage it spent (a failed call's included).
    async fn write(
        &self,
        meeting_id: &str,
        prompt: &str,
    ) -> (anyhow::Result<String>, Vec<UsageData>);
}

/// The auxiliary model (`llm.aux`, falling back to the main chain).
pub struct AuxSummaryWriter<'a> {
    /// Config the client is built from.
    pub config: &'a Config,
}

#[async_trait::async_trait]
impl SummaryWriter for AuxSummaryWriter<'_> {
    async fn write(
        &self,
        meeting_id: &str,
        prompt: &str,
    ) -> (anyhow::Result<String>, Vec<UsageData>) {
        let mut llm = match LlmClient::for_auxiliary(self.config).await {
            Ok(c) => c
                .with_prompt_cache_key(format!("meeting:{meeting_id}"))
                .with_one_shot(),
            Err(e) => return (Err(e.context("could not reach the model")), Vec::new()),
        };
        let messages = [Message::system(SUMMARY_PROMPT), Message::user(prompt)];
        // Streamed: the timeout is inactivity-based, so a long summary of a
        // long call finishes while a wedged connection still dies.
        let reply = llm.chat_streamed(&messages).await;
        let usage = llm.take_usage();
        let text = reply.and_then(|m| {
            m.text_content()
                .map(str::to_string)
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("the model returned an empty summary"))
        });
        (text, usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meetings::{Attendee, MeetingState, Track};

    fn meeting(notes: &str) -> MeetingRow {
        MeetingRow {
            id: "mtg_1".into(),
            title: "PSP7 Tiger Sync".into(),
            state: MeetingState::Finalizing,
            source_app: "Zoom".into(),
            calendar_event_id: String::new(),
            attendees: vec![
                Attendee {
                    name: "Juan".into(),
                    email: String::new(),
                },
                Attendee {
                    name: String::new(),
                    email: "jae@example.com".into(),
                },
            ],
            started_at: 0,
            ended_at: Some(60),
            dir: "/tmp".into(),
            notes: notes.into(),
            summary_md: None,
            unread: false,
            error: None,
            updated_at: 0,
        }
    }

    fn seg(text: &str) -> SegmentRow {
        SegmentRow {
            track: Track::System,
            seq: 0,
            start_ms: 0,
            end_ms: 25_000,
            text: text.into(),
        }
    }

    #[test]
    fn nothing_said_and_nothing_typed_needs_no_model() {
        assert!(build_prompt(&meeting("  "), &[]).is_none());
        assert!(build_prompt(&meeting(""), &[seg("   ")]).is_none());
    }

    #[test]
    fn prompt_carries_notes_before_transcript_and_names_attendees() {
        let p = build_prompt(
            &meeting("ask about IL5 timelines"),
            &[seg("we patch weekly")],
        )
        .unwrap();
        let notes_at = p.find("ask about IL5 timelines").unwrap();
        let transcript_at = p.find("Them: we patch weekly").unwrap();
        assert!(notes_at < transcript_at);
        assert!(p.contains("Invited: Juan, jae@example.com"));
        // Notes alone still make a summary worth asking for.
        let notes_only = build_prompt(&meeting("decided to ship"), &[]).unwrap();
        assert!(notes_only.contains("(nothing was transcribed)"));
    }

    #[test]
    fn prompt_respects_the_notes_budget() {
        let long = "n".repeat(NOTES_CHAR_BUDGET * 2);
        let p = build_prompt(&meeting(&long), &[]).unwrap();
        assert!(p.matches('n').count() <= NOTES_CHAR_BUDGET + 20);
    }

    #[test]
    fn normalize_unwraps_fences_and_demotes_hash_headings() {
        let reply = "```markdown\n# Context and Setup\n- one\n\n## **Decisions**\n- ship\n```";
        assert_eq!(
            normalize(reply),
            "**Context and Setup**\n- one\n\n**Decisions**\n- ship"
        );
        // Already in shape: unchanged.
        let clean = "**Overview**\n- a\n  - b";
        assert_eq!(normalize(clean), clean);
    }
}
