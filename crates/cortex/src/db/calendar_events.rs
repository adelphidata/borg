//! The calendar cache: events the sync worker fetched, read by the "Coming
//! up" view and the call detector. Each source is replaced wholesale on every
//! sync, so a deleted or moved event never lingers.

use anyhow::Result;
use libsql::params;

use super::Database;
use crate::meetings::calendar::CalendarEvent;

const COLS: &str =
    "id, source, title, start_unix, end_unix, all_day, attendees_json, join_url, provider";

fn map_row(row: &libsql::Row) -> Result<CalendarEvent> {
    let attendees: String = row.get(6)?;
    Ok(CalendarEvent {
        id: row.get(0)?,
        source: row.get(1)?,
        title: row.get(2)?,
        start_unix: row.get(3)?,
        end_unix: row.get(4)?,
        all_day: row.get::<i64>(5)? != 0,
        attendees: serde_json::from_str(&attendees).unwrap_or_default(),
        join_url: row.get(7)?,
        provider: row.get(8)?,
    })
}

impl Database {
    /// Replace every cached event from `source` with `events`.
    pub async fn replace_calendar_events(
        &self,
        source: &str,
        events: &[CalendarEvent],
        synced_at: i64,
    ) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result = async {
            self.conn
                .execute(
                    "DELETE FROM calendar_events WHERE source = ?1",
                    params!(source),
                )
                .await?;
            for e in events {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO calendar_events
                            (id, source, title, start_unix, end_unix, all_day,
                             attendees_json, join_url, provider, synced_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params!(
                            e.id.as_str(),
                            source,
                            e.title.as_str(),
                            e.start_unix,
                            e.end_unix,
                            i64::from(e.all_day),
                            serde_json::to_string(&e.attendees)?,
                            e.join_url.as_str(),
                            e.provider.as_str(),
                            synced_at
                        ),
                    )
                    .await?;
            }
            Ok(())
        }
        .await;
        self.finish_immediate_tx(result, "replace calendar events")
            .await
    }

    /// Cached events that overlap `[from, to)`, by start time.
    pub async fn calendar_events_between(&self, from: i64, to: i64) -> Result<Vec<CalendarEvent>> {
        let sql = format!(
            "SELECT {COLS} FROM calendar_events
             WHERE start_unix < ?2 AND end_unix > ?1
             ORDER BY start_unix, id"
        );
        let mut rows = self.conn.query(&sql, params!(from, to)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map_row(&row)?);
        }
        Ok(out)
    }

    /// One cached event.
    pub async fn calendar_event(&self, id: &str) -> Result<Option<CalendarEvent>> {
        let sql = format!("SELECT {COLS} FROM calendar_events WHERE id = ?1");
        let mut rows = self.conn.query(&sql, params!(id)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(map_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Forget every cached event.
    pub async fn clear_calendar_events(&self) -> Result<()> {
        self.conn.execute("DELETE FROM calendar_events", ()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meetings::Attendee;

    fn ev(id: &str, source: &str, start: i64, end: i64) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            source: source.into(),
            title: id.into(),
            start_unix: start,
            end_unix: end,
            all_day: false,
            attendees: vec![Attendee {
                name: "Jae".into(),
                email: String::new(),
            }],
            join_url: String::new(),
            provider: String::new(),
        }
    }

    #[tokio::test]
    async fn replacing_one_source_leaves_the_other_alone() {
        let db = Database::test_db().await;
        db.replace_calendar_events("ics", &[ev("ics:a", "ics", 100, 200)], 1)
            .await
            .unwrap();
        db.replace_calendar_events("google", &[ev("google:x", "google", 100, 200)], 1)
            .await
            .unwrap();
        // A Google re-sync that dropped x leaves only the ICS row.
        db.replace_calendar_events("google", &[], 2).await.unwrap();
        let all = db.calendar_events_between(0, 1_000).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "ics:a");
        assert_eq!(all[0].attendees[0].name, "Jae");
    }

    #[tokio::test]
    async fn between_returns_events_that_overlap_the_window() {
        let db = Database::test_db().await;
        db.replace_calendar_events(
            "ics",
            &[
                ev("before", "ics", 0, 100),
                ev("spans", "ics", 50, 250),
                ev("inside", "ics", 120, 180),
                ev("after", "ics", 300, 400),
            ],
            1,
        )
        .await
        .unwrap();
        let ids: Vec<_> = db
            .calendar_events_between(100, 300)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        // "before" ends exactly at the window start; "after" starts at its end.
        assert_eq!(ids, ["spans", "inside"]);
        assert!(db.calendar_event("inside").await.unwrap().is_some());
        db.clear_calendar_events().await.unwrap();
        assert!(db.calendar_event("inside").await.unwrap().is_none());
    }
}
