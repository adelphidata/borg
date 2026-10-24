#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! `borg memory write` then `borg memory search` against a temp home.
//!
//! Search reads the chunk index, which the agent's memory tool fills on every
//! write. The CLI write skipped it, so an entry written from the shell could
//! not be found until the nightly repair sweep re-indexed it.
//!
//! The embedding cache is seeded with a vector for each piece of text the
//! write embeds, so indexing never reaches the network. No real `borgd`
//! starts: auto-start goes to a stub that exits at once, and
//! `BORG_ACP_NO_AUTOSPAWN` stops `connect_or_spawn` from starting one.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const KEY_ENV: &str = "BORG_MEMORY_CLI_TEST_EMBED_KEY";
const ENTRY: &str = "The QA user's favourite colour is teal.";

fn borg(home: &std::path::Path, stub: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_borg"))
        .args(args)
        .env("BORG_HOME", home)
        .env("BORG_DAEMON_BIN", stub)
        .env("BORG_ACP_NO_AUTOSPAWN", "1")
        .env("BORG_KEK_PASSPHRASE", "memory-cli-test-passphrase")
        .env(KEY_ENV, "not-a-real-key")
        .output()
        .expect("run borg");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "`borg {}` failed: {}\n{stdout}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[tokio::test]
async fn a_written_entry_is_found_by_search() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let stub = tmp.path().join("stub-borgd");
    std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).expect("chmod");

    // SAFETY: the only test in this binary.
    unsafe {
        std::env::set_var("BORG_HOME", tmp.path());
        std::env::set_var(KEY_ENV, "not-a-real-key");
    }
    let db = cortex::db::Database::open().await.expect("db");
    db.set_setting("memory.embeddings.provider", "openai")
        .await
        .expect("provider");
    db.set_setting("memory.embeddings.api_key_env", KEY_ENV)
        .await
        .expect("key env");

    let config = cortex::config::Config::load_from_db()
        .await
        .expect("config");
    let embeddings = &config.memory.embeddings;
    let provider = cortex::memory::embeddings::EmbeddingProvider::from_config(embeddings)
        .expect("provider resolves from the seeded settings");
    let vector = cortex::memory::embeddings::embedding_to_bytes(&vec![0.5; provider.dimension]);
    let chunks = cortex::memory::chunker::chunk_content(
        ENTRY,
        embeddings.chunk_size_tokens,
        embeddings.chunk_overlap_tokens,
    );
    for text in std::iter::once(ENTRY.to_string()).chain(chunks.into_iter().map(|c| c.content)) {
        db.cache_embedding(
            &provider.endpoint,
            &provider.cache_model_key(),
            &cortex::memory::embeddings::content_hash(&text),
            &vector,
            provider.dimension,
        )
        .await
        .expect("seed embedding cache");
    }
    drop(db);

    borg(
        tmp.path(),
        &stub,
        &["memory", "write", "global", "qa-fact", "--content", ENTRY],
    );
    let found = borg(tmp.path(), &stub, &["memory", "search", "teal"]);
    assert!(
        found.contains("global/qa-fact"),
        "search must find the entry just written:\n{found}"
    );
}
