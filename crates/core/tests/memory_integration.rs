//! Memory system integration tests.
//!
//! Tests extra-path scanning (filesystem-backed, still supported for user-configured
//! memory directories). The DB-backed memory API (`write_memory_db`, `read_memory_db`,
//! `load_memory_context_db`, etc.) is covered by unit tests in `memory/mod.rs`.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use std::fs;

use borg_core::memory;

mod common;

// ── Test: scan_extra_paths finds .md files ──

#[test]
fn scan_extra_paths_finds_files() {
    let tmp = common::test_tempdir();
    let safe_dir = tmp.path().join("notes");
    fs::create_dir_all(&safe_dir).expect("create safe dir");
    fs::write(safe_dir.join("note.md"), "safe note").expect("write safe");

    let extra_paths = vec![safe_dir.to_string_lossy().to_string()];
    let blocked: Vec<String> = vec![];

    let results = memory::scan_extra_paths(&extra_paths, &blocked);
    assert!(!results.is_empty(), "Should find .md files in extra paths");
    assert!(
        results
            .iter()
            .any(|(_, p)| p.to_string_lossy().contains("note.md")),
        "Should include note.md"
    );
}

// ── Test: scan_extra_paths skips blocked dirs ──

#[test]
fn scan_extra_paths_blocked_dir() {
    let tmp = common::test_tempdir();
    let blocked_dir = tmp.path().join(".ssh");
    fs::create_dir_all(&blocked_dir).expect("create blocked dir");
    fs::write(blocked_dir.join("keys.md"), "secret keys").expect("write");

    // Use home-relative blocking: scan_extra_paths checks if path is under $HOME/<blocked>
    // So create a dir under home to test properly, or test that the function
    // at least returns nothing for a dir named .ssh
    let home = dirs::home_dir().expect("home dir");
    let extra_paths = vec![home.join(".ssh").to_string_lossy().to_string()];
    let blocked = vec![".ssh".to_string()];

    let results = memory::scan_extra_paths(&extra_paths, &blocked);
    assert!(results.is_empty(), "Blocked path should produce no results");
}
