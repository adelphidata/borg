//! Memory system integration tests.
//!
//! Tests write/read/list lifecycle, path traversal prevention, token budgeting,
//! and extra path scanning.
//!
//! NOTE: Tests that depend on `BORG_DATA_DIR` are combined into a single test
//! to avoid env var races when running in parallel. Tests that do not depend
//! on `BORG_DATA_DIR` (like scan_extra_paths) are separate.

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
use borg_core::memory::WriteMode;

mod common;

// ── Test: full memory lifecycle ──
// Combined into one test to avoid BORG_DATA_DIR race conditions.

#[test]
fn memory_lifecycle() {
    let tmp = common::test_tempdir();
    let mem_dir = tmp.path().join("memory");
    fs::create_dir_all(&mem_dir).expect("create memory dir");
    std::env::set_var("BORG_DATA_DIR", tmp.path());

    // --- Write then read round-trip ---
    let result = memory::write_memory("test_note.md", "Hello from test", WriteMode::Overwrite)
        .expect("write memory");
    assert!(
        result.contains("test_note.md"),
        "Write result should mention filename"
    );
    let content = memory::read_memory("test_note.md").expect("read memory");
    assert_eq!(content.trim(), "Hello from test");

    // --- Append mode accumulates ---
    memory::write_memory("append_test.md", "Line 1\n", WriteMode::Overwrite).expect("write first");
    memory::write_memory("append_test.md", "Line 2\n", WriteMode::Append).expect("write second");
    let content = memory::read_memory("append_test.md").expect("read append");
    assert!(content.contains("Line 1"));
    assert!(content.contains("Line 2"));

    // --- Overwrite replaces content ---
    memory::write_memory("overwrite.md", "old content", WriteMode::Overwrite).expect("write old");
    memory::write_memory("overwrite.md", "new content", WriteMode::Overwrite).expect("write new");
    let content = memory::read_memory("overwrite.md").expect("read overwrite");
    assert!(!content.contains("old content"));
    assert!(content.contains("new content"));

    // --- List memory files ---
    let files = memory::list_memory_files().expect("list");
    let names: Vec<&str> = files.iter().map(|f| f.filename.as_str()).collect();
    assert!(names.contains(&"test_note.md"), "Should list test_note.md");
    assert!(
        names.contains(&"append_test.md"),
        "Should list append_test.md"
    );
    assert!(names.contains(&"overwrite.md"), "Should list overwrite.md");

    // --- MemoryFileInfo has expected fields ---
    let info = files
        .iter()
        .find(|f| f.filename == "test_note.md")
        .expect("find file");
    assert!(info.size_bytes > 0, "Size should be non-zero");
    assert!(info.modified_at.is_some(), "Should have modification time");

    // --- Read nonexistent file ---
    let result = memory::read_memory("does_not_exist.md").expect("read nonexistent");
    assert!(
        result.contains("not found"),
        "Should report not found, got: {result}"
    );

    // --- Path traversal rejected ---
    let result = memory::write_memory("../../etc/passwd", "evil", WriteMode::Overwrite);
    assert!(result.is_err(), "Path traversal should be rejected");

    // --- Token budget ---
    let index_path = tmp.path().join("MEMORY.md");
    fs::write(
        &index_path,
        "# Memory Index\n- [test_note.md](test_note.md)\n",
    )
    .expect("write index");
    let context = memory::load_memory_context(100).expect("load with small budget");
    assert!(
        !context.is_empty(),
        "Should return some context even with small budget"
    );
}

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
