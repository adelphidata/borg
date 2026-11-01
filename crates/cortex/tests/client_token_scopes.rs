//! Client-token scopes gate each network surface.
//!
//! The in-crate `auth::tokens` tests are feature-gated off, so the scope
//! contract lives here where it runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use cortex::auth::tokens::{issue, validate_scopes, verify_for, Surface};
use cortex::db::Database;

#[tokio::test]
async fn scopes_gate_each_surface_and_empty_keeps_the_browser_out() {
    let db = Database::test_db().await;
    let chat = issue(&db, "chat", "openai_compat").await.unwrap();
    let both = issue(&db, "both", "mcp, webui").await.unwrap();
    let legacy = issue(&db, "legacy", "").await.unwrap();

    assert!(verify_for(&db, &chat.plaintext, Surface::OpenAiCompat)
        .await
        .is_ok());
    assert!(verify_for(&db, &chat.plaintext, Surface::Mcp)
        .await
        .is_err());
    assert!(verify_for(&db, &chat.plaintext, Surface::WebUi)
        .await
        .is_err());

    assert!(verify_for(&db, &both.plaintext, Surface::Mcp).await.is_ok());
    assert!(verify_for(&db, &both.plaintext, Surface::WebUi)
        .await
        .is_ok());
    assert!(verify_for(&db, &both.plaintext, Surface::OpenAiCompat)
        .await
        .is_err());

    // A pre-enforcement token keeps the chat and MCP surfaces but may not
    // reach the Admin service behind the browser mount.
    assert!(verify_for(&db, &legacy.plaintext, Surface::OpenAiCompat)
        .await
        .is_ok());
    assert!(verify_for(&db, &legacy.plaintext, Surface::Mcp)
        .await
        .is_ok());
    assert!(verify_for(&db, &legacy.plaintext, Surface::WebUi)
        .await
        .is_err());
}

#[tokio::test]
async fn a_scoped_token_without_the_surface_fails_like_an_unknown_one() {
    let db = Database::test_db().await;
    let chat = issue(&db, "chat", "openai_compat").await.unwrap();
    let scoped_out = verify_for(&db, &chat.plaintext, Surface::WebUi)
        .await
        .unwrap_err()
        .to_string();
    let unknown = verify_for(&db, "not-a-token", Surface::WebUi)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(scoped_out, unknown, "no scope leak through the error text");
}

#[tokio::test]
async fn issue_rejects_an_unknown_scope() {
    let db = Database::test_db().await;
    let err = issue(&db, "typo", "webui,admin").await.unwrap_err();
    assert!(
        err.to_string().contains("unknown token scope `admin`"),
        "{err}"
    );
    assert!(validate_scopes("").is_ok());
    assert!(validate_scopes(" mcp , webui ").is_ok());
}
