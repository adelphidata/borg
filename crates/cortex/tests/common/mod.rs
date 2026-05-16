//! Shared helpers for integration tests.
//!
//! Cargo treats files under `tests/` as standalone binaries, but anything
//! under `tests/common/` is loaded as a module via `mod common;` and is
//! therefore not compiled as its own test binary.

#![allow(dead_code)]

use cortex::db::Database;
use tempfile::TempDir;

/// Create a fresh in-memory database with migrations applied.
pub async fn test_db() -> Database {
    Database::test_db().await
}

/// Create a temp directory the integration test can use as a scratch dir.
pub fn test_tempdir() -> TempDir {
    tempfile::tempdir().expect("create temp dir")
}
