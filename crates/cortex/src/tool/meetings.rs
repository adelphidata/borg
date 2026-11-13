//! `meetings` tool: read the user's meeting notes, summaries and transcripts.
//!
//! The data lives in `borg.db` and (by default) under Borg's data directory,
//! which the sandbox keeps from the agent, and memory search does not index
//! it — so this read-only tool is the agent's only way in. Offered only
//! while `meetings.enabled` is on.

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{Database, MeetingRow};
use crate::meetings::transcript;

/// Default and ceiling for `limit`.
const DEFAULT_LIMIT: u32 = 10;
const MAX_LIMIT: u32 = 50;
/// Default and ceiling for the transcript a `get` returns.
const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_MAX_CHARS: usize = 60_000;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MeetingsArgs {
    /// `list`, `get` or `search`.
    pub action: String,
    /// The meeting, for `get`.
    #[serde(default)]
    pub id: Option<String>,
    /// Text to find, for `search`.
    #[serde(default)]
    pub query: Option<String>,
    /// How many rows `list` and `search` return.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Transcript budget for `get`.
    #[serde(default)]
    pub max_chars: Option<usize>,
}

pub(crate) async fn handle(args: &MeetingsArgs, db: &Database) -> Result<String> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let out = match args.action.as_str() {
        "list" => {
            let rows = db.list_meetings(limit, None).await?;
            json!({ "meetings": rows.iter().map(summary).collect::<Vec<_>>() })
        }
        "get" => {
            let Some(id) = args.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                bail!("'get' needs the meeting 'id' (from action 'list' or 'search')");
            };
            let Some(row) = db.get_meeting(id).await? else {
                bail!("no meeting with id {id}; use action 'list' or 'search' to find it");
            };
            let segments = db.segments_for(id).await?;
            let budget = args
                .max_chars
                .unwrap_or(DEFAULT_MAX_CHARS)
                .clamp(1_000, MAX_MAX_CHARS);
            let mut v = summary(&row);
            v["notes"] = json!(row.notes);
            v["summary"] = json!(row.summary_md);
            if let Some(error) = &row.error {
                v["error"] = json!(error);
            }
            v["transcript"] = json!(transcript::render_for_llm(&segments, budget));
            v
        }
        "search" => {
            let Some(query) = args
                .query
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                bail!("'search' needs a 'query'");
            };
            let hits = db.search_meetings(query, limit).await?;
            json!({
                "results": hits.iter().map(|h| json!({
                    "id": h.id,
                    "title": h.title,
                    "started_at": iso(h.started_at),
                    "matched_in": h.field,
                    "snippet": h.snippet,
                })).collect::<Vec<_>>()
            })
        }
        other => bail!("unknown action '{other}'; use list, get or search"),
    };
    Ok(out.to_string())
}

fn summary(m: &MeetingRow) -> Value {
    json!({
        "id": m.id,
        "title": m.title,
        "state": m.state.as_str(),
        "started_at": iso(m.started_at),
        "ended_at": m.ended_at.map(iso),
        "app": m.source_app,
        "attendees": m.attendees.iter().map(|a| {
            if a.name.is_empty() { a.email.clone() } else { a.name.clone() }
        }).collect::<Vec<_>>(),
        "has_summary": m.summary_md.as_deref().is_some_and(|s| !s.trim().is_empty()),
    })
}

/// Local time with offset: the model answers "when was it" in the user's
/// clock, not in Unix seconds.
fn iso(ts: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map_or_else(|| ts.to_string(), |t| t.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewMeeting, SegmentRow};
    use crate::meetings::{Attendee, Track};

    async fn seed(db: &Database, id: &str, title: &str, started_at: i64) {
        db.insert_meeting(&NewMeeting {
            id,
            title,
            source_app: "Zoom",
            calendar_event_id: "",
            attendees: &[Attendee {
                name: "Juan".into(),
                email: "juan@example.com".into(),
            }],
            started_at,
            dir: std::path::Path::new("/tmp/none"),
        })
        .await
        .unwrap();
    }

    fn args(action: &str) -> MeetingsArgs {
        MeetingsArgs {
            action: action.into(),
            id: None,
            query: None,
            limit: None,
            max_chars: None,
        }
    }

    #[tokio::test]
    async fn list_is_newest_first_and_says_which_have_notes() {
        let db = Database::test_db().await;
        seed(&db, "mtg_old", "Kickoff", 1_000).await;
        seed(&db, "mtg_new", "Standup", 2_000).await;
        db.set_meeting_summary("mtg_old", "**Decisions**\n- ship", 3_000)
            .await
            .unwrap();
        let out: Value = serde_json::from_str(&handle(&args("list"), &db).await.unwrap()).unwrap();
        let rows = out["meetings"].as_array().unwrap();
        let got: Vec<(&str, bool)> = rows
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap(),
                    r["has_summary"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, [("mtg_new", false), ("mtg_old", true)]);
        assert_eq!(rows[0]["attendees"], json!(["Juan"]));
    }

    #[tokio::test]
    async fn get_returns_notes_summary_and_a_capped_transcript() {
        let db = Database::test_db().await;
        seed(&db, "mtg_1", "Tiger Sync", 1_000).await;
        db.set_meeting_notes("mtg_1", "ask about IL5", 1_001)
            .await
            .unwrap();
        db.set_meeting_summary("mtg_1", "**Decisions**\n- weekly patching", 1_002)
            .await
            .unwrap();
        for seq in 0..400u32 {
            let seg = SegmentRow {
                track: if seq % 2 == 0 {
                    Track::Mic
                } else {
                    Track::System
                },
                seq,
                start_ms: i64::from(seq) * 25_000,
                end_ms: i64::from(seq) * 25_000 + 20_000,
                text: format!("line {seq} with some words to fill the transcript budget"),
            };
            db.insert_segment("mtg_1", &seg).await.unwrap();
        }
        let a = MeetingsArgs {
            id: Some("mtg_1".into()),
            max_chars: Some(2_000),
            ..args("get")
        };
        let out: Value = serde_json::from_str(&handle(&a, &db).await.unwrap()).unwrap();
        assert_eq!(out["notes"], "ask about IL5");
        assert_eq!(out["summary"], "**Decisions**\n- weekly patching");
        let t = out["transcript"].as_str().unwrap();
        assert!(
            t.chars().count() <= 2_200,
            "transcript respects max_chars: {}",
            t.len()
        );
        assert!(t.contains("line 0 "), "keeps the opening");
        assert!(t.contains("line 399 "), "keeps the end");
        assert!(t.contains("omitted"));
    }

    #[tokio::test]
    async fn search_finds_text_in_notes_and_get_explains_a_missing_id() {
        let db = Database::test_db().await;
        seed(&db, "mtg_1", "Tiger Sync", 1_000).await;
        db.set_meeting_notes("mtg_1", "budget for Q3 approved", 1_001)
            .await
            .unwrap();
        let a = MeetingsArgs {
            query: Some("Q3".into()),
            ..args("search")
        };
        let out: Value = serde_json::from_str(&handle(&a, &db).await.unwrap()).unwrap();
        assert_eq!(out["results"][0]["id"], "mtg_1");
        assert_eq!(out["results"][0]["matched_in"], "notes");

        let missing = MeetingsArgs {
            id: Some("mtg_nope".into()),
            ..args("get")
        };
        let err = handle(&missing, &db).await.unwrap_err().to_string();
        assert!(err.contains("no meeting with id mtg_nope"), "{err}");
        let err = handle(&args("delete"), &db).await.unwrap_err().to_string();
        assert!(err.contains("unknown action"), "{err}");
    }
}
