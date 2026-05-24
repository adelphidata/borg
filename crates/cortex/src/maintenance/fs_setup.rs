//! Idempotent filesystem setup for the runtime data directory.
//!
//! Seeds the subdirectories the runtime expects under `~/.borg/` and
//! the `global/INDEX` memory entry so a fresh install passes every
//! doctor check on boot instead of warning until something happens to
//! populate them.

use anyhow::Result;
use std::fs;
use std::path::Path;

use crate::db::Database;

/// Subdirectories of `~/.borg/` that the runtime expects to exist. They are
/// created lazily by their owners (channels on first script-channel install,
/// backups on first backup, etc.); seeding them here means a fresh install
/// passes every doctor check on boot instead of warning until something has
/// happened to populate them.
///
/// `secrets/` is intentionally NOT in this list. The identity layer
/// (`crates/identity/src/keychain.rs`) creates it with mode `0o700` on
/// first KEK write; pre-creating it here with `create_dir_all` would leave
/// it `0o755 & ~umask` — world/group readable — exposing the encrypted
/// KEK file's parent on multi-user hosts. `FileKekStore` owns that path.
const ENSURED_SUBDIRS: &[&str] = &["channels", "skills", "hooks", "backups", "logs"];

/// Idempotently create the runtime subdirectories under `data_dir`. Public
/// so the daemon can call this on startup as well as the daily sweep —
/// either path produces the same end state.
pub fn ensure_data_subdirs(data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir)?;
    for sub in ENSURED_SUBDIRS {
        let path = data_dir.join(sub);
        if let Err(e) = fs::create_dir_all(&path) {
            // Don't abort the sweep over one missing subdir — log and try
            // the rest.
            tracing::warn!("maintenance: could not create {path:?}: {e}");
        }
    }
    Ok(())
}

/// Seed `global/INDEX` if other `global` entries exist but no INDEX. The
/// nightly memory consolidation crons will rewrite the body later — this
/// just makes sure the doctor "no INDEX entry" check stops firing on a
/// fresh install. Returns `true` if a new INDEX was written.
pub async fn ensure_global_index(db: &Database) -> Result<bool> {
    let entries = db.list_memory_entries("global").await?;
    if entries.is_empty() {
        // No memory at all yet — doctor's check_memory passes ("no entries
        // in DB yet"), so nothing to seed.
        return Ok(false);
    }
    if entries.iter().any(|e| e.name == "INDEX") {
        return Ok(false);
    }
    let mut body = String::from("# Memory Index\n\n");
    for e in &entries {
        body.push_str(&format!("- {}\n", e.name));
    }
    db.upsert_memory_entry("global", "INDEX", &body).await?;
    tracing::info!(
        "maintenance: seeded global/INDEX with {} entry/entries",
        entries.len()
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_data_subdirs_creates_missing_dirs_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        ensure_data_subdirs(root).expect("first call");
        for sub in ENSURED_SUBDIRS {
            assert!(root.join(sub).is_dir(), "{sub} should be created");
        }
        // Pre-populate one subdir with content, then re-run: must not delete or error.
        let marker = root.join("channels").join("preexisting");
        fs::create_dir_all(&marker).unwrap();
        ensure_data_subdirs(root).expect("second call should not error");
        assert!(marker.is_dir(), "second call should not blow away contents");
    }

    #[tokio::test]
    async fn ensure_global_index_seeds_when_entries_exist_without_index() {
        let db = Database::test_db().await;
        db.upsert_memory_entry("global", "notes", "stuff")
            .await
            .unwrap();
        let seeded = ensure_global_index(&db).await.unwrap();
        assert!(seeded, "should write a fresh INDEX");
        let idx = db
            .get_memory_entry("global", "INDEX")
            .await
            .unwrap()
            .expect("INDEX must exist");
        assert!(idx.content.contains("- notes"));
    }

    #[tokio::test]
    async fn ensure_global_index_noop_when_no_entries() {
        let db = Database::test_db().await;
        let seeded = ensure_global_index(&db).await.unwrap();
        assert!(!seeded, "no entries means nothing to seed");
        assert!(
            db.get_memory_entry("global", "INDEX")
                .await
                .unwrap()
                .is_none(),
            "must not write an INDEX into an empty scope"
        );
    }

    #[tokio::test]
    async fn ensure_global_index_noop_when_index_already_exists() {
        let db = Database::test_db().await;
        db.upsert_memory_entry("global", "INDEX", "user-curated body")
            .await
            .unwrap();
        db.upsert_memory_entry("global", "notes", "x")
            .await
            .unwrap();
        let seeded = ensure_global_index(&db).await.unwrap();
        assert!(!seeded, "must not overwrite an existing INDEX");
        let idx = db
            .get_memory_entry("global", "INDEX")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            idx.content, "user-curated body",
            "existing INDEX content must be preserved verbatim"
        );
    }
}
