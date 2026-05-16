use anyhow::Result;
use turso::params;

use super::models::{DeliveryRow, NewDelivery};
use super::Database;

impl Database {
    // ── Delivery Queue ──

    pub async fn enqueue_delivery(&self, d: &NewDelivery<'_>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO delivery_queue (id, channel_name, sender_id, channel_id, session_id, payload_json, status, retry_count, max_retries, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?8, ?8)",
            params!(d.id, d.channel_name, d.sender_id, d.channel_id.map(std::string::ToString::to_string), d.session_id.map(std::string::ToString::to_string), d.payload_json, d.max_retries, now),
        ).await?;
        Ok(())
    }

    pub async fn claim_pending_deliveries(&self, limit: u32) -> Result<Vec<DeliveryRow>> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute_batch("BEGIN IMMEDIATE").await?;
        let result: anyhow::Result<Vec<DeliveryRow>> = async {
            let mut stmt = self.conn.prepare(
                "SELECT id, channel_name, sender_id, channel_id, session_id, payload_json, status, retry_count, max_retries, next_retry_at, created_at, updated_at, error
             FROM delivery_queue
             WHERE status = 'pending' AND (next_retry_at IS NULL OR next_retry_at <= ?1)
             ORDER BY created_at ASC
             LIMIT ?2",
            ).await?;
            let mut rows_iter = stmt.query(params!(now, limit as i64)).await?;
            let mut rows = Vec::new();
            while let Some(row) = rows_iter.next().await? {
                rows.push(DeliveryRow {
                    id: row.get::<String>(0)?,
                    channel_name: row.get::<String>(1)?,
                    sender_id: row.get::<String>(2)?,
                    channel_id: row.get::<Option<String>>(3)?,
                    session_id: row.get::<Option<String>>(4)?,
                    payload_json: row.get::<String>(5)?,
                    status: row.get::<String>(6)?,
                    retry_count: row.get::<i64>(7)? as i32,
                    max_retries: row.get::<i64>(8)? as i32,
                    next_retry_at: row.get::<Option<i64>>(9)?,
                    created_at: row.get::<i64>(10)?,
                    updated_at: row.get::<i64>(11)?,
                    error: row.get::<Option<String>>(12)?,
                });
            }
            drop(rows_iter);
            drop(stmt);

            // Mark claimed rows as in_progress.
            for row in &rows {
                self.conn.execute(
                    "UPDATE delivery_queue SET status = 'in_progress', updated_at = ?1 WHERE id = ?2",
                    params!(now, row.id.clone()),
                )
                .await?;
            }

            Ok(rows)
        }.await;
        match result {
            Ok(rows) => {
                self.conn.execute_batch("COMMIT").await?;
                Ok(rows)
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK").await {
                    tracing::warn!("delivery: rollback after claim failure failed: {re}");
                }
                Err(e)
            }
        }
    }

    pub async fn mark_delivered(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE delivery_queue SET status = 'delivered', updated_at = ?1 WHERE id = ?2",
                params!(now, id),
            )
            .await?;
        Ok(())
    }

    pub async fn mark_failed(
        &self,
        id: &str,
        error: &str,
        next_retry_at: Option<i64>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE delivery_queue SET status = CASE WHEN retry_count + 1 >= max_retries THEN 'exhausted' ELSE 'pending' END, retry_count = retry_count + 1, error = ?1, next_retry_at = ?2, updated_at = ?3 WHERE id = ?4",
            params!(error, next_retry_at, now, id),
        ).await?;
        Ok(())
    }

    pub async fn replay_unfinished(&self) -> Result<u32> {
        let now = chrono::Utc::now().timestamp();
        let count = self.conn.execute(
            "UPDATE delivery_queue SET status = 'pending', updated_at = ?1 WHERE status = 'in_progress'",
            params!(now),
        ).await?;
        Ok(count as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::test_db().await
    }

    fn sample_delivery<'a>(id: &'a str, channel: &'a str) -> NewDelivery<'a> {
        NewDelivery {
            id,
            channel_name: channel,
            sender_id: "user1",
            channel_id: Some("ch1"),
            session_id: Some("sess1"),
            payload_json: r#"{"text":"hello"}"#,
            max_retries: 3,
        }
    }

    #[tokio::test]
    async fn test_enqueue_and_claim() {
        let db = test_db().await;
        db.enqueue_delivery(&sample_delivery("d1", "telegram"))
            .await
            .unwrap();
        db.enqueue_delivery(&sample_delivery("d2", "telegram"))
            .await
            .unwrap();

        let claimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].id, "d1");
        assert_eq!(claimed[1].id, "d2");
        // Rows are returned with their pre-claim status
        assert_eq!(claimed[0].status, "pending");

        // Re-claim should return empty (both are in_progress now)
        let empty = db.claim_pending_deliveries(10).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_claim_respects_limit() {
        let db = test_db().await;
        for i in 0..3 {
            db.enqueue_delivery(&sample_delivery(&format!("d{i}"), "slack"))
                .await
                .unwrap();
        }

        let first = db.claim_pending_deliveries(1).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "d0");

        let second = db.claim_pending_deliveries(1).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, "d1");
    }

    #[tokio::test]
    async fn test_mark_delivered() {
        let db = test_db().await;
        db.enqueue_delivery(&sample_delivery("d1", "telegram"))
            .await
            .unwrap();
        let claimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        db.mark_delivered("d1").await.unwrap();

        // Delivered items are not claimable
        let empty = db.claim_pending_deliveries(10).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_mark_failed_retries_and_exhaustion() {
        let db = test_db().await;
        let mut d = sample_delivery("d1", "telegram");
        d.max_retries = 2;
        db.enqueue_delivery(&d).await.unwrap();

        // Claim and fail once — should go back to pending (retry_count=1 < max_retries=2)
        let claimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        db.mark_failed("d1", "timeout", None).await.unwrap();

        // Should be claimable again (back to pending)
        let claimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].retry_count, 1);

        // Fail again — retry_count+1 (2) >= max_retries (2), status becomes exhausted
        db.mark_failed("d1", "timeout again", None).await.unwrap();

        let empty = db.claim_pending_deliveries(10).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_mark_failed_with_future_retry() {
        let db = test_db().await;
        db.enqueue_delivery(&sample_delivery("d1", "telegram"))
            .await
            .unwrap();
        let claimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // Mark failed with next_retry_at far in the future
        let future = chrono::Utc::now().timestamp() + 999_999;
        db.mark_failed("d1", "transient", Some(future))
            .await
            .unwrap();

        // Should not be claimable (next_retry_at > now)
        let empty = db.claim_pending_deliveries(10).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_replay_unfinished() {
        let db = test_db().await;
        db.enqueue_delivery(&sample_delivery("d1", "telegram"))
            .await
            .unwrap();
        db.enqueue_delivery(&sample_delivery("d2", "slack"))
            .await
            .unwrap();

        let claimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 2);

        // Replay resets in_progress back to pending
        let count = db.replay_unfinished().await.unwrap();
        assert_eq!(count, 2);

        // Both should be claimable again
        let reclaimed = db.claim_pending_deliveries(10).await.unwrap();
        assert_eq!(reclaimed.len(), 2);
    }
}
