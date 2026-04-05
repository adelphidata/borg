//! Tool handler integration tests.
//!
//! Tests tool handlers with real filesystem (temp dirs) and in-memory DB.
//! Verifies the tool execution pipeline without any LLM calls.

use std::fs;

use borg_core::config::Config;
use borg_core::tool_handlers;
use borg_core::types::ToolOutput;

// ── Test: apply_patch creates and modifies files ──

#[test]
fn apply_patch_creates_and_modifies() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    // Create a file via patch using the unified handler with cwd target
    // We need to set CWD for this handler; use apply_patch_to_dir directly instead
    let patch_create =
        "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n+Line two\n*** End Patch";
    let affected = borg_apply_patch::apply_patch_to_dir(patch_create, dir).expect("create patch");
    assert!(affected.format_summary().contains("hello.txt"));

    // Verify file exists
    let content = fs::read_to_string(dir.join("hello.txt")).expect("read created file");
    assert!(content.contains("Hello world") && content.contains("Line two"));

    // Modify the file via patch
    let patch_update = "*** Begin Patch\n*** Update File: hello.txt\n@@\n Hello world\n-Line two\n+Line two updated\n*** End Patch";
    borg_apply_patch::apply_patch_to_dir(patch_update, dir).expect("update patch");

    let content = fs::read_to_string(dir.join("hello.txt")).expect("read modified file");
    assert!(content.contains("Line two updated"));
}

// ── Test: apply_patch with delete ──

#[test]
fn apply_patch_delete_file() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    // Create a file first
    fs::write(dir.join("to_delete.txt"), "temporary\n").expect("write file");
    assert!(dir.join("to_delete.txt").exists());

    // Delete via patch
    let patch = "*** Begin Patch\n*** Delete File: to_delete.txt\n*** End Patch";
    borg_apply_patch::apply_patch_to_dir(patch, dir).expect("delete patch");
    assert!(
        !dir.join("to_delete.txt").exists(),
        "File should be deleted"
    );
}

// ── Test: is_blocked_path works for home-relative paths ──

#[test]
fn blocked_path_rejects_sensitive_dirs() {
    let home = dirs::home_dir().expect("home dir");
    let blocked = vec![".ssh".to_string(), ".aws".to_string()];

    // Path under $HOME/.ssh should be blocked
    let ssh_path = home.join(".ssh/id_rsa");
    assert!(tool_handlers::is_blocked_path(&ssh_path, &blocked, &[]));

    // Path under $HOME/.aws should be blocked
    let aws_path = home.join(".aws/credentials");
    assert!(tool_handlers::is_blocked_path(&aws_path, &blocked, &[]));

    // Path under $HOME/Documents should NOT be blocked
    let safe_path = home.join("Documents/safe.txt");
    assert!(!tool_handlers::is_blocked_path(&safe_path, &blocked, &[]));

    // Path outside home IS blocked by component matching (new behavior)
    let outside = std::path::Path::new("/tmp/.ssh/id_rsa");
    assert!(tool_handlers::is_blocked_path(outside, &blocked, &[]));
}

// ── Test: read_file with line numbers ──

#[test]
fn read_file_with_line_numbers() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("test.txt");
    fs::write(&file, "line one\nline two\nline three\n").expect("write file");

    let config = Config::default();
    let args = serde_json::json!({
        "path": file.to_str().unwrap()
    });
    let output = tool_handlers::handle_read_file(&args, &config).expect("read_file");
    let text = match &output {
        ToolOutput::Text(t) => t.as_str(),
        ToolOutput::Multimodal { text, .. } => text.as_str(),
    };

    // Output format is "{line_no:>6}\t{line}\n" (e.g. "     1\tline one")
    assert!(
        text.contains("1\tline one"),
        "Expected line-numbered output, got: {text}"
    );
    assert!(
        text.contains("3\tline three"),
        "Expected all lines in output"
    );
}

// ── Test: list_dir not a directory ──

#[test]
fn list_dir_not_a_directory() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("just_a_file.txt");
    fs::write(&file, "content").expect("write file");

    let config = Config::default();
    let args = serde_json::json!({
        "path": file.to_str().unwrap()
    });
    let result = tool_handlers::handle_list_dir(&args, &config).expect("list_dir on file");
    assert!(
        result.contains("Not a directory"),
        "Expected error, got: {result}"
    );
}
