//! Bearer-token issuance, verification, and revocation.
//!
//! Used by the OpenAI-compat HTTP surface and the MCP HTTP transport.
//! Tokens are 32 random bytes, base64url-encoded; only the
//! SHA-256 hash is persisted. Plaintext is shown to the issuer once at
//! creation and is never recoverable thereafter — same model as
//! GitHub PATs. Revocation is a tombstone (`revoked_at` timestamp), not
//! a delete, so audit replay can still observe issued-then-revoked
//! tokens.

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::db::Database;

/// Length of the random portion of a token, in bytes (256 bits of entropy).
const TOKEN_BYTES: usize = 32;

/// One row in `client_tokens`.
#[derive(Debug, Clone)]
pub struct TokenRow {
    /// Database autoincrement id — stable across renames.
    pub id: i64,
    /// Human-readable label (e.g. `"chatbox-laptop"`). Unique.
    pub label: String,
    /// Comma-separated scopes. v1 has no scope enforcement; the field
    /// exists so future surfaces can restrict tokens to read-only MCP,
    /// chat-only OpenAI-compat, etc. without a schema migration.
    pub scopes: String,
    /// Unix seconds when the token was issued.
    pub created_at: i64,
    /// Unix seconds of the most recent successful [`verify`]. `None`
    /// for never-used tokens.
    pub last_used_at: Option<i64>,
    /// Unix seconds the token was revoked. `None` while live; `Some`
    /// after [`revoke`]. A revoked token will never verify again.
    pub revoked_at: Option<i64>,
}

/// Result of [`issue`]: the row that was written, plus the plaintext
/// token shown to the user. The plaintext is **only** available here —
/// it's not stored and cannot be recovered from the database.
#[derive(Debug)]
pub struct IssuedToken {
    /// Persistent row metadata.
    pub row: TokenRow,
    /// The token string the user copies into their HTTP client. Treat
    /// as a credential: never log, never persist outside the user's
    /// secrets store.
    pub plaintext: String,
}

/// Generate a fresh token, write its hash to the DB, and return the
/// plaintext + row.
///
/// Fails with `AlreadyExists` if `label` is taken — labels are unique
/// so users can revoke by name without remembering ids.
pub fn issue(db: &Database, label: &str, scopes: &str) -> Result<IssuedToken> {
    if label.trim().is_empty() {
        return Err(anyhow!("token label must not be empty"));
    }

    let plaintext = generate_plaintext();
    let hash = hash_token(&plaintext);
    let now = chrono::Utc::now().timestamp();

    let conn = db.conn();
    let id = conn
        .query_row(
            "INSERT INTO client_tokens (token_hash, label, scopes, created_at)
             VALUES (?1, ?2, ?3, ?4)
             RETURNING id",
            rusqlite::params![hash, label, scopes, now],
            |r| r.get::<_, i64>(0),
        )
        .with_context(|| format!("failed to insert client token `{label}`"))?;

    Ok(IssuedToken {
        row: TokenRow {
            id,
            label: label.to_string(),
            scopes: scopes.to_string(),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
        },
        plaintext,
    })
}

/// Verify a plaintext token against the DB.
///
/// On success, bumps `last_used_at` to "now" and returns the row.
/// Errors:
/// - token unknown (no row matches the hash)
/// - token revoked (`revoked_at IS NOT NULL`)
///
/// The two error paths return the same generic error string to avoid
/// disclosing whether a presented token ever existed (anti-enumeration).
pub fn verify(db: &Database, plaintext: &str) -> Result<TokenRow> {
    let hash = hash_token(plaintext);
    let now = chrono::Utc::now().timestamp();
    let conn = db.conn();

    let row = conn.query_row(
        "SELECT id, label, scopes, created_at, last_used_at, revoked_at
         FROM client_tokens
         WHERE token_hash = ?1",
        rusqlite::params![hash],
        |r| {
            Ok(TokenRow {
                id: r.get(0)?,
                label: r.get(1)?,
                scopes: r.get(2)?,
                created_at: r.get(3)?,
                last_used_at: r.get(4)?,
                revoked_at: r.get(5)?,
            })
        },
    );

    let mut row = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(anyhow!("invalid client token"));
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context("client token lookup failed"));
        }
    };

    if row.revoked_at.is_some() {
        return Err(anyhow!("invalid client token"));
    }

    conn.execute(
        "UPDATE client_tokens SET last_used_at = ?1 WHERE id = ?2",
        rusqlite::params![now, row.id],
    )
    .context("failed to update last_used_at")?;
    row.last_used_at = Some(now);

    Ok(row)
}

/// Revoke a token by label. Subsequent [`verify`] calls for that token
/// return `Err`. Idempotent: revoking an already-revoked token leaves
/// `revoked_at` at its earliest value.
pub fn revoke_by_label(db: &Database, label: &str) -> Result<TokenRow> {
    let now = chrono::Utc::now().timestamp();
    let conn = db.conn();

    let updated = conn.execute(
        "UPDATE client_tokens SET revoked_at = ?1
         WHERE label = ?2 AND revoked_at IS NULL",
        rusqlite::params![now, label],
    )?;

    let row = conn
        .query_row(
            "SELECT id, label, scopes, created_at, last_used_at, revoked_at
             FROM client_tokens WHERE label = ?1",
            rusqlite::params![label],
            |r| {
                Ok(TokenRow {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    scopes: r.get(2)?,
                    created_at: r.get(3)?,
                    last_used_at: r.get(4)?,
                    revoked_at: r.get(5)?,
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow!("no token with label `{label}`"),
            other => anyhow::Error::from(other).context("client token lookup failed"),
        })?;

    if updated == 0 && row.revoked_at.is_none() {
        // Defensive: SELECT-after-UPDATE saw a live row, but UPDATE
        // affected zero rows. Should be unreachable in single-writer
        // sqlite, but report rather than silently swallow.
        return Err(anyhow!(
            "revocation race: row for `{label}` mutated between update and read"
        ));
    }

    Ok(row)
}

/// List all tokens (live and revoked). Caller decides whether to filter
/// the revoked tombstones for display. Ordered newest-first by
/// `created_at`.
pub fn list(db: &Database) -> Result<Vec<TokenRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, label, scopes, created_at, last_used_at, revoked_at
         FROM client_tokens
         ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(TokenRow {
                id: r.get(0)?,
                label: r.get(1)?,
                scopes: r.get(2)?,
                created_at: r.get(3)?,
                last_used_at: r.get(4)?,
                revoked_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Generate a fresh URL-safe base64 token string. 32 bytes of randomness
/// → 43 chars of base64url (no padding).
fn generate_plaintext() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::rng().fill(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// SHA-256 of the token plaintext. The hashed form is what gets stored;
/// it's also what `verify` looks up. Pre-image resistance means a DB
/// dump cannot be replayed against the OpenAI-compat surface without
/// also stealing the user's plaintext copy.
fn hash_token(plaintext: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::test_db()
    }

    #[test]
    fn issue_returns_plaintext_and_persists_only_hash() {
        // The contract every bearer-token system must hold: the DB
        // sees only the hash, the user sees only the plaintext at
        // creation, and the two are linked one-way. Regressing this
        // means a DB exfiltration grants attacker access to the
        // OpenAI-compat surface.
        let db = db();
        let issued = issue(&db, "test-token", "openai_compat").expect("issue");

        assert!(
            !issued.plaintext.is_empty(),
            "plaintext must be returned at issue time"
        );

        let stored: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT token_hash FROM client_tokens WHERE id = ?1",
                rusqlite::params![issued.row.id],
                |r| r.get(0),
            )
            .expect("row exists");

        assert_ne!(
            stored,
            issued.plaintext.as_bytes(),
            "stored hash must NOT equal plaintext"
        );
        assert_eq!(stored.len(), 32, "SHA-256 output is 32 bytes");
        assert_eq!(stored, hash_token(&issued.plaintext));
    }

    #[test]
    fn verify_accepts_valid_token_and_bumps_last_used_at() {
        // Real round-trip: issue produces plaintext, verify accepts it
        // and updates last_used_at. The bump is the audit signal an
        // operator uses to find dormant tokens to revoke.
        let db = db();
        let issued = issue(&db, "live", "").expect("issue");
        assert!(issued.row.last_used_at.is_none());

        let row = verify(&db, &issued.plaintext).expect("verify");
        assert_eq!(row.id, issued.row.id);
        assert_eq!(row.label, "live");
        assert!(
            row.last_used_at.is_some(),
            "verify must bump last_used_at on success"
        );

        let row2 = verify(&db, &issued.plaintext).expect("verify second time");
        assert!(
            row2.last_used_at.unwrap() >= row.last_used_at.unwrap(),
            "subsequent verify must not move last_used_at backward"
        );
    }

    #[test]
    fn verify_rejects_wrong_plaintext() {
        // An attacker presenting a guessed token must be rejected,
        // and last_used_at on no row must move (no oracle).
        let db = db();
        let issued = issue(&db, "live", "").expect("issue");

        let err = verify(&db, "completely-fabricated-token").expect_err("must reject");
        assert!(
            err.to_string().contains("invalid client token"),
            "rejection message must be generic to avoid existence oracle, got: {err}"
        );

        let row: Option<i64> = db
            .conn()
            .query_row(
                "SELECT last_used_at FROM client_tokens WHERE id = ?1",
                rusqlite::params![issued.row.id],
                |r| r.get(0),
            )
            .expect("row exists");
        assert!(
            row.is_none(),
            "failed verify on a different token must not move last_used_at on the live row"
        );
    }

    #[test]
    fn verify_rejects_revoked_token() {
        // Revocation is the only way to disable a token. Once revoked,
        // verify must return Err — even with the original plaintext.
        // Regression here is the entire point of having revocation.
        let db = db();
        let issued = issue(&db, "doomed", "").expect("issue");
        verify(&db, &issued.plaintext).expect("works pre-revocation");

        revoke_by_label(&db, "doomed").expect("revoke");
        let err = verify(&db, &issued.plaintext).expect_err("revoked token must fail verify");
        assert!(err.to_string().contains("invalid client token"));
    }

    #[test]
    fn revoke_is_idempotent_and_preserves_first_revocation_timestamp() {
        // Operator runs `borg client-token revoke` twice by mistake —
        // second call must succeed (no spurious error) but must NOT
        // overwrite revoked_at, since the audit chain treats the first
        // revocation as the authoritative event.
        let db = db();
        issue(&db, "revoked-twice", "").expect("issue");

        let first = revoke_by_label(&db, "revoked-twice").expect("first revoke");
        let first_ts = first.revoked_at.expect("first revoke sets timestamp");

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = revoke_by_label(&db, "revoked-twice").expect("second revoke is idempotent");
        assert_eq!(
            second.revoked_at.expect("still revoked"),
            first_ts,
            "second revoke must NOT overwrite the original revocation timestamp"
        );
    }

    #[test]
    fn duplicate_label_is_rejected() {
        // Labels are the operator's primary handle for revocation —
        // collisions would let one issuer's revoke command silently
        // disable another's token. UNIQUE constraint enforces this.
        let db = db();
        issue(&db, "shared-name", "").expect("first issue");
        let err = issue(&db, "shared-name", "").expect_err("second must conflict");
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("client token")
                || err.to_string().to_ascii_lowercase().contains("unique"),
            "duplicate-label error should reference the conflict, got: {err}"
        );
    }

    #[test]
    fn list_returns_live_and_revoked_in_creation_order() {
        // The CLI's `list` view shows operators what's live; revoked
        // tombstones must appear too so they can audit history.
        // Ordering is newest-first so recently-issued tokens are up
        // top.
        let db = db();
        issue(&db, "old", "").expect("issue");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let middle = issue(&db, "middle", "").expect("issue");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        issue(&db, "new", "").expect("issue");

        revoke_by_label(&db, "middle").expect("revoke");

        let rows = list(&db).expect("list");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label, "new");
        assert_eq!(rows[1].label, "middle");
        assert!(rows[1].revoked_at.is_some(), "revoked tombstone preserved");
        assert_eq!(rows[2].label, "old");
        assert_eq!(middle.row.id, rows[1].id);
    }

    #[test]
    fn issue_rejects_empty_label() {
        // Operator typo guard. An empty label would also collide via
        // UNIQUE on the second attempt, but explicit early rejection
        // gives a clearer error than "UNIQUE constraint failed".
        let db = db();
        let err = issue(&db, "   ", "").expect_err("empty label rejected");
        assert!(err.to_string().contains("label"));
    }

    #[test]
    fn distinct_tokens_have_distinct_hashes() {
        // 32 bytes of CSPRNG output should never collide — but if the
        // hash function or the random source were broken (deterministic
        // seed, length truncation), distinct issuances would share a
        // hash and verify would map any one plaintext to the wrong
        // row. This test catches that class of regression.
        let db = db();
        let a = issue(&db, "a", "").expect("issue");
        let b = issue(&db, "b", "").expect("issue");
        assert_ne!(a.plaintext, b.plaintext);
        assert_ne!(hash_token(&a.plaintext), hash_token(&b.plaintext));
    }
}
