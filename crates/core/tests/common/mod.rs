//! Shared helpers for integration tests.
//!
//! Cargo treats files under `tests/` as standalone binaries, but anything
//! under `tests/common/` is loaded as a module via `mod common;` and is
//! therefore not compiled as its own test binary.

#![allow(dead_code)]

use borg_core::db::Database;
use rusqlite::Connection;
use tempfile::TempDir;

/// Create a fresh in-memory database with migrations applied.
pub fn test_db() -> Database {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    Database::from_connection(conn).expect("init test db")
}

/// Create a temp directory the integration test can use as a scratch dir.
pub fn test_tempdir() -> TempDir {
    tempfile::tempdir().expect("create temp dir")
}
