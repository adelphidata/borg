//! Persistence for meetings and their transcript segments.
//!
//! A meeting row carries its state, notes and summary; the readable files in
//! the meeting's folder are written from these rows (`crate::meetings::store`),
//! never the other way round, so the database is the source of truth.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use libsql::params;

use super::Database;
use crate::meetings::{Attendee, MeetingState, Track};

/// A stored meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingRow {
    /// `mtg_…`.
    pub id: String,
    /// Shown title; never empty once started.
    pub title: String,
    /// Where it is in its life.
    pub state: MeetingState,
    /// The app the call was in ("Zoom", "Chrome"), or empty.
    pub source_app: String,
    /// The calendar event it was started for, or empty.
    pub calendar_event_id: String,
    /// Invitees copied from the calendar event.
    pub attendees: Vec<Attendee>,
    /// Unix seconds.
    pub started_at: i64,
    /// Unix seconds; `None` while recording.
    pub ended_at: Option<i64>,
    /// The meeting's folder.
    pub dir: PathBuf,
    /// What the user typed.
    pub notes: String,
    /// The written summary, once there is one.
    pub summary_md: Option<String>,
    /// A finished summary nobody has opened.
    pub unread: bool,
    /// Why the summary failed.
    pub error: Option<String>,
    /// Last write, Unix seconds. A recording meeting that stops moving is
    /// recovered from this.
    pub updated_at: i64,
}

/// What [`Database::insert_meeting`] needs.
#[derive(Debug, Clone)]
pub struct NewMeeting<'a> {
    /// `mtg_…`.
    pub id: &'a str,
    /// Shown title.
    pub title: &'a str,
    /// The app the call was in.
    pub source_app: &'a str,
    /// The calendar event, or empty.
    pub calendar_event_id: &'a str,
    /// Invitees.
    pub attendees: &'a [Attendee],
    /// Unix seconds.
    pub started_at: i64,
    /// The meeting's folder.
    pub dir: &'a Path,
}

/// One transcribed chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    /// Who was speaking.
    pub track: Track,
    /// Per-track chunk number.
    pub seq: u32,
    /// Milliseconds from the start of the meeting.
    pub start_ms: i64,
    /// Milliseconds from the start of the meeting.
    pub end_ms: i64,
    /// The words.
    pub text: String,
}

/// A meeting that matched a search, with where and what matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingSearchHit {
    /// `mtg_…`.
    pub id: String,
    /// Shown title.
    pub title: String,
    /// Unix seconds.
    pub started_at: i64,
    /// `title`, `notes`, `summary` or `transcript`.
    pub field: &'static str,
    /// The text around the match.
    pub snippet: String,
}

const COLS: &str = "id, title, state, source_app, calendar_event_id, attendees_json, \
                    started_at, ended_at, dir, notes, summary_md, unread, error, updated_at";

fn map_row(row: &libsql::Row) -> Result<MeetingRow> {
    let state: String = row.get(2)?;
    let attendees_json: String = row.get(5)?;
    Ok(MeetingRow {
        id: row.get(0)?,
        title: row.get(1)?,
        state: state.parse()?,
        source_app: row.get(3)?,
        calendar_event_id: row.get(4)?,
        attendees: serde_json::from_str(&attendees_json).unwrap_or_default(),
        started_at: row.get(6)?,
        ended_at: row.get::<Option<i64>>(7)?,
        dir: PathBuf::from(row.get::<String>(8)?),
        notes: row.get(9)?,
        summary_md: row.get::<Option<String>>(10)?,
        unread: row.get::<i64>(11)? != 0,
        error: row.get::<Option<String>>(12)?,
        updated_at: row.get(13)?,
    })
}

/// Escape `%`, `_` and the escape character itself for a `LIKE … ESCAPE '\'`.
fn like_pattern(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// About `width` characters of `text` around the first case-insensitive
/// occurrence of `query`.
fn snippet(text: &str, query: &str, width: usize) -> String {
    let lower = text.to_lowercase();
    let at = lower.find(&query.to_lowercase()).unwrap_or(0);
    // Work in chars so a multi-byte letter is never split.
    let char_at = lower[..at].chars().count();
    let start = char_at.saturating_sub(width / 2);
    let chars: Vec<char> = text.chars().collect();
    let end = (start + width).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

impl Database {
    /// Store a new meeting in the `recording` state.
    pub async fn insert_meeting(&self, m: &NewMeeting<'_>) -> Result<()> {
        let attendees = serde_json::to_string(m.attendees)?;
        let dir = m.dir.to_string_lossy().into_owned();
        self.conn
            .execute(
                "INSERT INTO meetings
                    (id, title, state, source_app, calendar_event_id, attendees_json,
                     started_at, dir, created_at, updated_at)
                 VALUES (?1, ?2, 'recording', ?3, ?4, ?5, ?6, ?7, ?6, ?6)",
                params!(
                    m.id,
                    m.title,
                    m.source_app,
                    m.calendar_event_id,
                    attendees,
                    m.started_at,
                    dir
                ),
            )
            .await?;
        Ok(())
    }

    /// One meeting.
    pub async fn get_meeting(&self, id: &str) -> Result<Option<MeetingRow>> {
        let sql = format!("SELECT {COLS} FROM meetings WHERE id = ?1");
        let mut rows = self.conn.query(&sql, params!(id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Meetings that started before `before_unix` (all when `None`), newest
    /// first.
    pub async fn list_meetings(
        &self,
        limit: u32,
        before_unix: Option<i64>,
    ) -> Result<Vec<MeetingRow>> {
        let sql = format!(
            "SELECT {COLS} FROM meetings WHERE started_at < ?1
             ORDER BY started_at DESC, id DESC LIMIT ?2"
        );
        let before = before_unix.unwrap_or(i64::MAX);
        let mut rows = self.conn.query(&sql, params!(before, limit)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_row(&row)?);
        }
        Ok(out)
    }

    /// Meetings in `state`, oldest first.
    pub async fn meetings_in_state(&self, state: MeetingState) -> Result<Vec<MeetingRow>> {
        let sql = format!("SELECT {COLS} FROM meetings WHERE state = ?1 ORDER BY started_at");
        let mut rows = self.conn.query(&sql, params!(state.as_str())).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_row(&row)?);
        }
        Ok(out)
    }

    /// The meeting that is recording now, if any.
    pub async fn recording_meeting(&self) -> Result<Option<MeetingRow>> {
        Ok(self
            .meetings_in_state(MeetingState::Recording)
            .await?
            .into_iter()
            .next())
    }

    /// Recording meetings nothing has written to since `older_than`.
    pub async fn stale_recordings(&self, older_than: i64) -> Result<Vec<MeetingRow>> {
        let sql =
            format!("SELECT {COLS} FROM meetings WHERE state = 'recording' AND updated_at < ?1");
        let mut rows = self.conn.query(&sql, params!(older_than)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_row(&row)?);
        }
        Ok(out)
    }

    /// Move a meeting to `state`. `ended_at` and `error` are written only
    /// when given.
    pub async fn set_meeting_state(
        &self,
        id: &str,
        state: MeetingState,
        ended_at: Option<i64>,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE meetings SET state = ?2,
                    ended_at = COALESCE(?3, ended_at),
                    error = COALESCE(?4, error),
                    updated_at = ?5
                 WHERE id = ?1",
                params!(id, state.as_str(), ended_at, error, now),
            )
            .await?;
        Ok(())
    }

    /// Record activity on a recording meeting without changing anything else.
    pub async fn touch_meeting(&self, id: &str, now: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE meetings SET updated_at = ?2 WHERE id = ?1",
                params!(id, now),
            )
            .await?;
        Ok(())
    }

    /// Replace the user's notes.
    pub async fn set_meeting_notes(&self, id: &str, notes: &str, now: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE meetings SET notes = ?2, updated_at = ?3 WHERE id = ?1",
                params!(id, notes, now),
            )
            .await?;
        Ok(())
    }

    /// Rename.
    pub async fn set_meeting_title(&self, id: &str, title: &str, now: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE meetings SET title = ?2, updated_at = ?3 WHERE id = ?1",
                params!(id, title, now),
            )
            .await?;
        Ok(())
    }

    /// Store the summary: the meeting is `done` and unread.
    pub async fn set_meeting_summary(&self, id: &str, summary_md: &str, now: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE meetings SET summary_md = ?2, state = 'done', unread = 1,
                    error = NULL, updated_at = ?3
                 WHERE id = ?1",
                params!(id, summary_md, now),
            )
            .await?;
        Ok(())
    }

    /// Clear the unread mark.
    pub async fn mark_meeting_read(&self, id: &str) -> Result<()> {
        self.conn
            .execute("UPDATE meetings SET unread = 0 WHERE id = ?1", params!(id))
            .await?;
        Ok(())
    }

    /// Finished summaries nobody has opened.
    pub async fn unread_meeting_count(&self) -> Result<u32> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM meetings WHERE unread = 1 AND summary_md IS NOT NULL",
                (),
            )
            .await?;
        let row = rows.next().await?.context("COUNT returned no row")?;
        Ok(u32::try_from(row.get::<i64>(0)?).unwrap_or(u32::MAX))
    }

    /// Remove a meeting and its segments. Returns whether it existed.
    pub async fn delete_meeting(&self, id: &str) -> Result<bool> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = async {
            self.conn
                .execute(
                    "DELETE FROM meeting_segments WHERE meeting_id = ?1",
                    params!(id),
                )
                .await?;
            let n = self
                .conn
                .execute("DELETE FROM meetings WHERE id = ?1", params!(id))
                .await?;
            Ok(n > 0)
        }
        .await;
        self.finish_immediate_tx(result, "delete meeting").await
    }

    /// Store one segment. Returns `false` when `(meeting, track, seq)` was
    /// already stored, which is what makes a retried push harmless.
    pub async fn insert_segment(&self, meeting_id: &str, seg: &SegmentRow) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO meeting_segments
                    (meeting_id, track, seq, start_ms, end_ms, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params!(
                    meeting_id,
                    seg.track.as_str(),
                    seg.seq,
                    seg.start_ms,
                    seg.end_ms,
                    seg.text.as_str()
                ),
            )
            .await?;
        Ok(n > 0)
    }

    /// The stored segment for `(meeting, track, seq)`, if any.
    pub async fn segment(
        &self,
        meeting_id: &str,
        track: Track,
        seq: u32,
    ) -> Result<Option<SegmentRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT track, seq, start_ms, end_ms, text FROM meeting_segments
                 WHERE meeting_id = ?1 AND track = ?2 AND seq = ?3",
                params!(meeting_id, track.as_str(), seq),
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_segment(&row)?)),
            None => Ok(None),
        }
    }

    /// Every segment of a meeting, in time order (mic first on a tie).
    pub async fn segments_for(&self, meeting_id: &str) -> Result<Vec<SegmentRow>> {
        let mut rows = self
            .conn
            .query(
                // An empty row marks a chunk that held only silence: it
                // makes a retry of that chunk a no-op, and is not speech.
                "SELECT track, seq, start_ms, end_ms, text FROM meeting_segments
                 WHERE meeting_id = ?1 AND text != '' ORDER BY start_ms, track, seq",
                params!(meeting_id),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_segment(&row)?);
        }
        Ok(out)
    }

    /// Meetings whose title, notes, summary or transcript contain `query`
    /// (case-insensitive for ASCII), newest first, one hit per meeting.
    pub async fn search_meetings(&self, query: &str, limit: u32) -> Result<Vec<MeetingSearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = like_pattern(query);
        let mut rows = self
            .conn
            .query(
                "SELECT m.id, m.title, m.started_at, m.notes, COALESCE(m.summary_md, ''),
                        (SELECT s.text FROM meeting_segments s
                          WHERE s.meeting_id = m.id AND s.text LIKE ?1 ESCAPE '\\'
                          ORDER BY s.start_ms LIMIT 1)
                   FROM meetings m
                  WHERE m.title LIKE ?1 ESCAPE '\\'
                     OR m.notes LIKE ?1 ESCAPE '\\'
                     OR m.summary_md LIKE ?1 ESCAPE '\\'
                     OR EXISTS (SELECT 1 FROM meeting_segments s
                                 WHERE s.meeting_id = m.id AND s.text LIKE ?1 ESCAPE '\\')
                  ORDER BY m.started_at DESC LIMIT ?2",
                params!(pattern, limit),
            )
            .await?;
        let needle = query.to_lowercase();
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let title: String = row.get(1)?;
            let notes: String = row.get(3)?;
            let summary: String = row.get(4)?;
            let segment: Option<String> = row.get(5)?;
            let (field, text) = [
                ("title", title.as_str()),
                ("notes", notes.as_str()),
                ("summary", summary.as_str()),
                ("transcript", segment.as_deref().unwrap_or("")),
            ]
            .into_iter()
            .find(|(_, t)| t.to_lowercase().contains(&needle))
            .unwrap_or(("title", title.as_str()));
            out.push(MeetingSearchHit {
                id: row.get(0)?,
                title: title.clone(),
                started_at: row.get(2)?,
                field,
                snippet: snippet(text, query, 160),
            });
        }
        Ok(out)
    }
}

fn map_segment(row: &libsql::Row) -> Result<SegmentRow> {
    let track: String = row.get(0)?;
    Ok(SegmentRow {
        track: track.parse()?,
        seq: u32::try_from(row.get::<i64>(1)?).unwrap_or(0),
        start_ms: row.get(2)?,
        end_ms: row.get(3)?,
        text: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn meeting(db: &Database, id: &str, started_at: i64) {
        db.insert_meeting(&NewMeeting {
            id,
            title: &format!("Meeting {id}"),
            source_app: "Zoom",
            calendar_event_id: "",
            attendees: &[Attendee {
                name: "Juan".into(),
                email: "juan@example.com".into(),
            }],
            started_at,
            dir: Path::new("/tmp/m"),
        })
        .await
        .unwrap();
    }

    fn seg(track: Track, seq: u32, start_ms: i64, text: &str) -> SegmentRow {
        SegmentRow {
            track,
            seq,
            start_ms,
            end_ms: start_ms + 25_000,
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn a_new_meeting_round_trips_and_starts_recording() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_a", 1_000).await;
        let row = db.get_meeting("mtg_a").await.unwrap().unwrap();
        assert_eq!(row.state, MeetingState::Recording);
        assert_eq!(row.attendees[0].name, "Juan");
        assert_eq!(row.ended_at, None);
        assert_eq!(row.dir, PathBuf::from("/tmp/m"));
        assert_eq!(db.recording_meeting().await.unwrap().unwrap().id, "mtg_a");
    }

    #[tokio::test]
    async fn insert_segment_is_idempotent_per_track_and_seq() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_a", 1_000).await;
        assert!(db
            .insert_segment("mtg_a", &seg(Track::Mic, 0, 0, "hello"))
            .await
            .unwrap());
        // A retried push carries the same (track, seq): nothing new.
        assert!(!db
            .insert_segment("mtg_a", &seg(Track::Mic, 0, 0, "hello"))
            .await
            .unwrap());
        // The other track's chunk 0 is a different segment.
        assert!(db
            .insert_segment("mtg_a", &seg(Track::System, 0, 0, "hi"))
            .await
            .unwrap());
        let segs = db.segments_for("mtg_a").await.unwrap();
        assert_eq!(segs.len(), 2);
        // Mic sorts before system at the same start.
        assert_eq!(segs[0].track, Track::Mic);
        assert_eq!(
            db.segment("mtg_a", Track::System, 0)
                .await
                .unwrap()
                .unwrap()
                .text,
            "hi"
        );
    }

    #[tokio::test]
    async fn list_meetings_pages_backwards_from_before_unix() {
        let db = Database::test_db().await;
        for (id, at) in [("mtg_1", 100), ("mtg_2", 200), ("mtg_3", 300)] {
            meeting(&db, id, at).await;
        }
        let ids = |rows: Vec<MeetingRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.list_meetings(2, None).await.unwrap()),
            ["mtg_3", "mtg_2"]
        );
        assert_eq!(
            ids(db.list_meetings(2, Some(200)).await.unwrap()),
            ["mtg_1"]
        );
    }

    #[tokio::test]
    async fn summary_marks_done_and_unread_until_read() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_a", 1_000).await;
        meeting(&db, "mtg_b", 2_000).await;
        // An unread flag without a summary does not count.
        assert_eq!(db.unread_meeting_count().await.unwrap(), 0);
        db.set_meeting_summary("mtg_a", "**Overview**", 3_000)
            .await
            .unwrap();
        let row = db.get_meeting("mtg_a").await.unwrap().unwrap();
        assert_eq!(row.state, MeetingState::Done);
        assert!(row.unread);
        assert_eq!(db.unread_meeting_count().await.unwrap(), 1);
        db.mark_meeting_read("mtg_a").await.unwrap();
        assert_eq!(db.unread_meeting_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn state_changes_keep_earlier_end_and_error_unless_given() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_a", 1_000).await;
        db.set_meeting_state("mtg_a", MeetingState::Finalizing, Some(1_900), None, 1_900)
            .await
            .unwrap();
        db.set_meeting_state(
            "mtg_a",
            MeetingState::Failed,
            None,
            Some("model down"),
            2_000,
        )
        .await
        .unwrap();
        let row = db.get_meeting("mtg_a").await.unwrap().unwrap();
        assert_eq!(row.state, MeetingState::Failed);
        assert_eq!(row.ended_at, Some(1_900));
        assert_eq!(row.error.as_deref(), Some("model down"));
    }

    #[tokio::test]
    async fn stale_recordings_skips_recent_and_finished_meetings() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_old", 1_000).await;
        meeting(&db, "mtg_new", 1_000).await;
        meeting(&db, "mtg_done", 1_000).await;
        db.touch_meeting("mtg_new", 5_000).await.unwrap();
        db.set_meeting_state(
            "mtg_done",
            MeetingState::Finalizing,
            Some(1_100),
            None,
            1_100,
        )
        .await
        .unwrap();
        let stale = db.stale_recordings(4_000).await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, "mtg_old");
    }

    #[tokio::test]
    async fn delete_meeting_removes_its_segments() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_a", 1_000).await;
        db.insert_segment("mtg_a", &seg(Track::Mic, 0, 0, "x"))
            .await
            .unwrap();
        assert!(db.delete_meeting("mtg_a").await.unwrap());
        assert!(db.get_meeting("mtg_a").await.unwrap().is_none());
        assert!(db.segments_for("mtg_a").await.unwrap().is_empty());
        assert!(!db.delete_meeting("mtg_a").await.unwrap());
    }

    #[tokio::test]
    async fn search_finds_notes_summary_and_transcript_and_treats_wildcards_literally() {
        let db = Database::test_db().await;
        meeting(&db, "mtg_a", 1_000).await;
        meeting(&db, "mtg_b", 2_000).await;
        meeting(&db, "mtg_c", 3_000).await;
        db.set_meeting_notes("mtg_a", "budget is 100% approved", 1_000)
            .await
            .unwrap();
        db.set_meeting_summary("mtg_b", "**Decisions**\n- Ship the pilot", 2_000)
            .await
            .unwrap();
        db.insert_segment("mtg_c", &seg(Track::System, 0, 0, "let's ship on Friday"))
            .await
            .unwrap();

        let hits = db.search_meetings("SHIP", 10).await.unwrap();
        let found: Vec<_> = hits.iter().map(|h| (h.id.as_str(), h.field)).collect();
        assert_eq!(found, [("mtg_c", "transcript"), ("mtg_b", "summary")]);
        assert!(
            hits[0].snippet.contains("ship on Friday"),
            "{}",
            hits[0].snippet
        );

        // `%` is a literal percent sign, not "anything".
        let pct = db.search_meetings("100%", 10).await.unwrap();
        assert_eq!(pct.len(), 1);
        assert_eq!(pct[0].field, "notes");
        assert!(db.search_meetings("1%0", 10).await.unwrap().is_empty());
        assert!(db.search_meetings("   ", 10).await.unwrap().is_empty());
    }

    #[test]
    fn snippet_centres_on_the_match_and_marks_cuts() {
        let text = format!("{} needle {}", "a ".repeat(100), "b ".repeat(100));
        let s = snippet(&text, "NEEDLE", 40);
        assert!(s.contains("needle"));
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        assert_eq!(snippet("short needle", "needle", 40), "short needle");
    }
}
