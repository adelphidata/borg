//! CRUD on the `webhook_definitions` table.

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use cortex::db::Database;
use rand::Rng;
use sha2::{Digest, Sha256};
use turso::params;

/// One row in `webhook_definitions`.
#[derive(Debug, Clone)]
pub struct WebhookDefinition {
    /// Unique name — appears in the URL path `/v1/webhook/{name}`.
    pub name: String,
    /// SHA-256 of the HMAC secret. The plaintext is shown to the
    /// user only at creation time.
    pub secret_hash: Vec<u8>,
    /// JSON dot-path for the `sender_id` field. Required.
    pub sender_path: String,
    /// JSON dot-path for the `thread_id` field. Optional.
    pub thread_path: Option<String>,
    /// JSON dot-path for the `body` text field. Required.
    pub body_path: String,
    /// Unix seconds the row was created.
    pub created_at: i64,
    /// Unix seconds the webhook was revoked, if applicable.
    pub revoked_at: Option<i64>,
}

/// Result of [`create`]: the persisted row plus the plaintext HMAC
/// secret. The user copies the plaintext into their upstream service's
/// signing config — it is not stored and cannot be recovered.
#[derive(Debug)]
pub struct CreatedWebhook {
    /// Persisted row metadata.
    pub row: WebhookDefinition,
    /// HMAC secret plaintext (32 bytes, base64url). Use this with
    /// HMAC-SHA256 over `"{timestamp}.{body}"` to sign requests.
    pub secret: String,
}

/// Generate a fresh HMAC secret, persist its hash, return both.
pub async fn create(
    db: &Database,
    name: &str,
    sender_path: &str,
    thread_path: Option<&str>,
    body_path: &str,
) -> Result<CreatedWebhook> {
    if name.trim().is_empty() {
        return Err(anyhow!("webhook name must not be empty"));
    }
    if !is_valid_name(name) {
        return Err(anyhow!(
            "webhook name must be alphanumeric/underscore/dash only (got `{name}`)"
        ));
    }
    if sender_path.trim().is_empty() || body_path.trim().is_empty() {
        return Err(anyhow!("sender_path and body_path are required"));
    }

    let secret = generate_secret();
    let hash = hash_secret(&secret);
    let now = chrono::Utc::now().timestamp();

    db.conn()
        .execute(
            "INSERT INTO webhook_definitions
                (name, secret_hash, sender_path, thread_path, body_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params!(name, hash.clone(), sender_path, thread_path, body_path, now),
        )
        .await
        .with_context(|| format!("failed to insert webhook definition `{name}`"))?;

    Ok(CreatedWebhook {
        row: WebhookDefinition {
            name: name.to_string(),
            secret_hash: hash,
            sender_path: sender_path.to_string(),
            thread_path: thread_path.map(str::to_string),
            body_path: body_path.to_string(),
            created_at: now,
            revoked_at: None,
        },
        secret,
    })
}

/// Look up a definition by name. Returns `None` if not found OR
/// revoked — verifiers should treat both the same.
pub async fn lookup(db: &Database, name: &str) -> Result<Option<WebhookDefinition>> {
    let mut rows = db
        .conn()
        .query(
            "SELECT name, secret_hash, sender_path, thread_path, body_path, created_at, revoked_at
             FROM webhook_definitions WHERE name = ?1",
            params!(name),
        )
        .await
        .context("webhook lookup failed")?;
    let Some(r) = rows.next().await.context("webhook lookup failed")? else {
        return Ok(None);
    };
    let row = WebhookDefinition {
        name: r.get::<String>(0)?,
        secret_hash: r.get::<Vec<u8>>(1)?,
        sender_path: r.get::<String>(2)?,
        thread_path: r.get::<Option<String>>(3)?,
        body_path: r.get::<String>(4)?,
        created_at: r.get::<i64>(5)?,
        revoked_at: r.get::<Option<i64>>(6)?,
    };
    if row.revoked_at.is_some() {
        return Ok(None);
    }
    Ok(Some(row))
}

/// List all webhook definitions. Caller decides whether to filter
/// revoked rows.
pub async fn list(db: &Database) -> Result<Vec<WebhookDefinition>> {
    let mut rows = db
        .conn()
        .query(
            "SELECT name, secret_hash, sender_path, thread_path, body_path, created_at, revoked_at
             FROM webhook_definitions
             ORDER BY created_at DESC",
            (),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().await? {
        out.push(WebhookDefinition {
            name: r.get::<String>(0)?,
            secret_hash: r.get::<Vec<u8>>(1)?,
            sender_path: r.get::<String>(2)?,
            thread_path: r.get::<Option<String>>(3)?,
            body_path: r.get::<String>(4)?,
            created_at: r.get::<i64>(5)?,
            revoked_at: r.get::<Option<i64>>(6)?,
        });
    }
    Ok(out)
}

/// Revoke a webhook definition by name. Idempotent.
pub async fn revoke(db: &Database, name: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let updated = db
        .conn()
        .execute(
            "UPDATE webhook_definitions SET revoked_at = ?1
             WHERE name = ?2 AND revoked_at IS NULL",
            params!(now, name),
        )
        .await?;
    if updated == 0 {
        // Either not found or already revoked. Distinguish so the
        // CLI can give a clearer error.
        let exists: i64 = match db
            .conn()
            .query(
                "SELECT count(*) FROM webhook_definitions WHERE name = ?1",
                params!(name),
            )
            .await
        {
            Ok(mut rows) => match rows.next().await {
                Ok(Some(r)) => r.get::<i64>(0).unwrap_or(0),
                _ => 0,
            },
            Err(_) => 0,
        };
        if exists == 0 {
            return Err(anyhow!("no webhook with name `{name}`"));
        }
    }
    Ok(())
}

fn generate_secret() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// SHA-256 of the HMAC-secret plaintext. Stored at rest so a DB dump
/// alone cannot forge signatures — an attacker would also need to
/// steal the user's plaintext copy from their upstream service.
fn hash_secret(plaintext: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(plaintext.as_bytes());
    h.finalize().to_vec()
}

fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Database {
        Database::test_db().await
    }

    #[tokio::test]
    async fn create_returns_secret_and_persists_only_hash() {
        // Same property as auth tokens: plaintext shown once, only
        // the hash hits the DB. A forged signature attempt by an
        // attacker with DB read access would need to reverse SHA-256.
        let db = db().await;
        let created = create(&db, "alerts", "$.user.id", None, "$.text")
            .await
            .expect("create");
        assert!(!created.secret.is_empty());

        let mut rows = db
            .conn()
            .query(
                "SELECT secret_hash FROM webhook_definitions WHERE name = ?1",
                params!("alerts"),
            )
            .await
            .expect("query secret_hash");
        let row = rows.next().await.expect("step").expect("row exists");
        let stored: Vec<u8> = row.get::<Vec<u8>>(0).expect("get bytes");
        assert_eq!(stored.len(), 32);
        assert_ne!(stored, created.secret.as_bytes());
        assert_eq!(stored, hash_secret(&created.secret));
    }

    #[tokio::test]
    async fn lookup_returns_none_for_revoked_or_missing() {
        // Revoked rows are tombstoned (created_at preserved) but must
        // be invisible to verifiers — same effect as deletion from
        // the request-path's perspective. Listing still surfaces them
        // for audit.
        let db = db().await;
        create(&db, "live", "$.s", None, "$.b")
            .await
            .expect("create");
        create(&db, "doomed", "$.s", None, "$.b")
            .await
            .expect("create");
        revoke(&db, "doomed").await.expect("revoke");

        assert!(lookup(&db, "live").await.expect("lookup").is_some());
        assert!(
            lookup(&db, "doomed").await.expect("lookup").is_none(),
            "revoked webhook must not lookup as live"
        );
        assert!(lookup(&db, "never-existed")
            .await
            .expect("lookup")
            .is_none());

        let all = list(&db).await.expect("list");
        assert_eq!(all.len(), 2, "list must surface tombstones");
    }

    #[tokio::test]
    async fn duplicate_name_is_rejected() {
        // Names route requests via `/v1/webhook/{name}`. Allowing a
        // collision would silently shadow the original definition.
        let db = db().await;
        create(&db, "n", "$.s", None, "$.b")
            .await
            .expect("first create");
        let err = create(&db, "n", "$.s", None, "$.b")
            .await
            .expect_err("dupe must fail");
        assert!(
            err.to_string().to_ascii_lowercase().contains("webhook")
                || err.to_string().to_ascii_lowercase().contains("unique"),
        );
    }

    #[tokio::test]
    async fn invalid_names_are_rejected() {
        // The name appears unescaped in URLs; restricting to
        // alphanumeric/dash/underscore avoids URL-encoding bugs and
        // path-traversal-style mischief in the route layer.
        let db = db().await;
        for bad in ["", "  ", "with spaces", "../etc", "name?query", "name#frag"] {
            let err = create(&db, bad, "$.s", None, "$.b")
                .await
                .expect_err(&format!("invalid name `{bad}` must be rejected"));
            // Just assert it errored; the message can vary based on
            // whether `is_valid_name` or the empty-check fired first.
            let _ = err;
        }
    }

    #[tokio::test]
    async fn revoke_unknown_name_errors() {
        let db = db().await;
        let err = revoke(&db, "nothing").await.expect_err("must error");
        assert!(err.to_string().contains("no webhook"));
    }
}
