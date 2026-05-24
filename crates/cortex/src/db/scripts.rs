use anyhow::Result;
use libsql::params;

use super::models::{NewScript, ScriptRow};
use super::Database;

impl Database {
    // ── Scripts CRUD ──

    pub async fn create_script(&self, s: &NewScript<'_>) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO scripts (id, name, description, runtime, entrypoint, sandbox_profile,
             network_access, fs_read, fs_write, ephemeral, hmac, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params!(
                    s.id,
                    s.name,
                    s.description,
                    s.runtime,
                    s.entrypoint,
                    s.sandbox_profile,
                    s.network_access as i32,
                    s.fs_read,
                    s.fs_write,
                    s.ephemeral as i32,
                    s.hmac,
                    s.created_at,
                    s.updated_at,
                ),
            )
            .await?;
        Ok(())
    }

    const SCRIPTS_SELECT: &'static str =
        "SELECT id, name, description, runtime, entrypoint, sandbox_profile,
                network_access, fs_read, fs_write, ephemeral, hmac,
                created_at, updated_at, last_run_at, run_count
         FROM scripts";

    fn script_row_from_row(row: &libsql::Row) -> Result<ScriptRow> {
        Ok(ScriptRow {
            id: row.get::<String>(0)?,
            name: row.get::<String>(1)?,
            description: row.get::<String>(2)?,
            runtime: row.get::<String>(3)?,
            entrypoint: row.get::<String>(4)?,
            sandbox_profile: row.get::<String>(5)?,
            network_access: row.get::<i32>(6)? != 0,
            fs_read: row.get::<String>(7)?,
            fs_write: row.get::<String>(8)?,
            ephemeral: row.get::<i32>(9)? != 0,
            hmac: row.get::<String>(10)?,
            created_at: row.get::<i64>(11)?,
            updated_at: row.get::<i64>(12)?,
            last_run_at: row.get::<Option<i64>>(13)?,
            run_count: row.get::<i64>(14)?,
        })
    }

    pub async fn get_script_by_name(&self, name: &str) -> Result<Option<ScriptRow>> {
        let sql = format!("{} WHERE name = ?1", Self::SCRIPTS_SELECT);
        let mut rows = self.conn.query(&sql, params!(name)).await?;
        match rows.next().await? {
            Some(row) => Ok(Some(Self::script_row_from_row(&row)?)),
            None => Ok(None),
        }
    }

    pub async fn list_scripts(&self) -> Result<Vec<ScriptRow>> {
        let sql = format!("{} ORDER BY name", Self::SCRIPTS_SELECT);
        let mut rows = self.conn.query(&sql, ()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(Self::script_row_from_row(&row)?);
        }
        Ok(out)
    }

    pub async fn update_script_hmac(&self, id: &str, hmac: &str, updated_at: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE scripts SET hmac = ?1, updated_at = ?2 WHERE id = ?3",
                params!(hmac, updated_at, id),
            )
            .await?;
        Ok(())
    }

    pub async fn record_script_run(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "UPDATE scripts SET run_count = run_count + 1, last_run_at = ?1 WHERE id = ?2",
                params!(now, id),
            )
            .await?;
        Ok(())
    }

    pub async fn delete_script(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM scripts WHERE id = ?1", params!(id))
            .await?;
        Ok(())
    }

    pub async fn delete_ephemeral_scripts_older_than(&self, cutoff: i64) -> Result<u64> {
        let count = self
            .conn
            .execute(
                "DELETE FROM scripts WHERE ephemeral = 1 AND created_at < ?1",
                params!(cutoff),
            )
            .await?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns<'a>(
        id: &'a str,
        name: &'a str,
        ephemeral: bool,
        created_at: i64,
    ) -> NewScript<'a> {
        NewScript {
            id,
            name,
            description: "test",
            runtime: "bash",
            entrypoint: "/tmp/x.sh",
            sandbox_profile: "default",
            network_access: false,
            fs_read: "/tmp",
            fs_write: "/tmp",
            ephemeral,
            hmac: "deadbeef",
            created_at,
            updated_at: created_at,
        }
    }

    #[tokio::test]
    async fn create_then_get_by_name_round_trips_all_fields() {
        // Spec: insert + read must preserve every column. A regression
        // that swapped column order or dropped network_access would
        // silently let an unsandboxed script run.
        let db = Database::test_db().await;
        let mut s = ns("id-1", "my-script", false, 1_000);
        s.network_access = true;
        s.description = "important";
        db.create_script(&s).await.unwrap();
        let row = db
            .get_script_by_name("my-script")
            .await
            .unwrap()
            .expect("must round-trip");
        assert_eq!(row.id, "id-1");
        assert_eq!(row.description, "important");
        assert!(row.network_access);
        assert_eq!(row.runtime, "bash");
        assert_eq!(row.hmac, "deadbeef");
        // Counters start at zero.
        assert_eq!(row.run_count, 0);
        assert_eq!(row.last_run_at, None);
    }

    #[tokio::test]
    async fn get_by_name_returns_none_for_missing() {
        let db = Database::test_db().await;
        assert!(db.get_script_by_name("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_scripts_returns_rows_sorted_by_name() {
        let db = Database::test_db().await;
        db.create_script(&ns("c", "charlie", false, 1)).await.unwrap();
        db.create_script(&ns("a", "alpha", false, 2)).await.unwrap();
        db.create_script(&ns("b", "bravo", false, 3)).await.unwrap();
        let rows = db.list_scripts().await.unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[tokio::test]
    async fn record_script_run_increments_count_and_sets_last_run() {
        // Real-world failure: a refactor that swapped UPDATE for INSERT
        // would silently double-count and reset the counter. Encode the
        // increment + non-null last_run contract.
        let db = Database::test_db().await;
        db.create_script(&ns("id-x", "x", false, 1_000)).await.unwrap();

        for expected_count in 1..=3 {
            db.record_script_run("id-x").await.unwrap();
            let row = db.get_script_by_name("x").await.unwrap().unwrap();
            assert_eq!(row.run_count, expected_count, "iteration {expected_count}");
            assert!(row.last_run_at.is_some(), "last_run_at must be set");
        }
    }

    #[tokio::test]
    async fn update_script_hmac_changes_hmac_and_updated_at() {
        let db = Database::test_db().await;
        db.create_script(&ns("id-h", "hmacable", false, 1_000))
            .await
            .unwrap();
        db.update_script_hmac("id-h", "ffeeddcc", 5_000)
            .await
            .unwrap();
        let row = db.get_script_by_name("hmacable").await.unwrap().unwrap();
        assert_eq!(row.hmac, "ffeeddcc");
        assert_eq!(row.updated_at, 5_000);
        // created_at must NOT change — only updated_at moves.
        assert_eq!(row.created_at, 1_000);
    }

    #[tokio::test]
    async fn delete_script_removes_row_and_is_idempotent() {
        let db = Database::test_db().await;
        db.create_script(&ns("id-d", "delete-me", false, 1)).await.unwrap();
        db.delete_script("id-d").await.unwrap();
        assert!(db.get_script_by_name("delete-me").await.unwrap().is_none());
        // Idempotent — deleting a non-existent id must not error.
        db.delete_script("id-d").await.unwrap();
    }

    #[tokio::test]
    async fn delete_ephemeral_only_removes_ephemeral_and_old() {
        // Spec: the cleanup sweep must NEVER touch non-ephemeral scripts
        // (operator-installed) regardless of age, AND must keep fresh
        // ephemerals (the agent might still be writing them). Encode the
        // AND condition with a 2×2 fixture.
        let db = Database::test_db().await;
        db.create_script(&ns("e-old", "e-old", true, 100)).await.unwrap();
        db.create_script(&ns("e-new", "e-new", true, 1_000)).await.unwrap();
        db.create_script(&ns("p-old", "p-old", false, 100)).await.unwrap();
        db.create_script(&ns("p-new", "p-new", false, 1_000)).await.unwrap();

        // cutoff = 500: only `e-old` matches both conditions.
        let deleted = db.delete_ephemeral_scripts_older_than(500).await.unwrap();
        assert_eq!(deleted, 1, "must delete exactly e-old");
        assert!(db.get_script_by_name("e-old").await.unwrap().is_none());
        assert!(db.get_script_by_name("e-new").await.unwrap().is_some());
        assert!(db.get_script_by_name("p-old").await.unwrap().is_some());
        assert!(db.get_script_by_name("p-new").await.unwrap().is_some());
    }
}
