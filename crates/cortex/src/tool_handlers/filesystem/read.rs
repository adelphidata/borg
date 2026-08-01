//! `read_file` (text + image multimodal) and `read_pdf` tool handlers.

use anyhow::Result;
use base64::Engine as _;
use tracing::instrument;

use crate::config::Config;
use crate::constants;
use crate::execution_context::ExecutionContext;
use crate::types::{ContentPart, MediaData, ToolOutput};

use super::blocked::is_blocked_path;
use super::default_execution_context;
use crate::tool_handlers::{optional_u64_param, require_str_param};

/// Image file extensions that should be returned as multimodal content.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif", "svg",
];

pub fn handle_read_pdf(args: &serde_json::Value) -> Result<String> {
    let file_path = require_str_param(args, "file_path")?;
    let max_chars = args["max_chars"]
        .as_u64()
        .unwrap_or(constants::DEFAULT_READ_MAX_CHARS as u64) as usize;
    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Ok(format!("File not found: {file_path}"));
    }
    match pdf_extract::extract_text(path) {
        Ok(text) => {
            if text.len() > max_chars {
                let truncated: String = text.chars().take(max_chars).collect();
                Ok(format!(
                    "{truncated}\n\n[truncated — {max_chars}/{} chars shown]",
                    text.len()
                ))
            } else {
                Ok(text)
            }
        }
        Err(e) => Ok(format!("Error reading PDF: {e}")),
    }
}

#[instrument(skip_all, fields(tool.name = "read_file"))]
pub fn handle_read_file(args: &serde_json::Value, config: &Config) -> Result<ToolOutput> {
    let ctx = default_execution_context()?;
    handle_read_file_with_context(args, config, &ctx)
}

pub fn handle_read_file_with_context(
    args: &serde_json::Value,
    config: &Config,
    ctx: &ExecutionContext,
) -> Result<ToolOutput> {
    let raw_path = require_str_param(args, "path")?;
    let offset = optional_u64_param(args, "offset", 1).max(1) as usize;
    let limit = optional_u64_param(args, "limit", 0) as usize;
    let max_chars =
        optional_u64_param(args, "max_chars", constants::DEFAULT_READ_MAX_CHARS as u64) as usize;

    // Resolve path: expand ~ and resolve relative paths
    let bridge = ctx.text_file_bridge().cloned();
    let canonical = if bridge
        .as_ref()
        .is_some_and(|bridge| bridge.supports_read_text_file())
    {
        match ctx.resolve_path_maybe_missing(raw_path) {
            Ok(p) => p,
            Err(_) => return Ok(ToolOutput::Text(not_found_message(ctx, raw_path))),
        }
    } else {
        match ctx.resolve_existing_path(raw_path) {
            Ok(p) => p,
            Err(_) => {
                return Ok(ToolOutput::Text(not_found_message(ctx, raw_path)));
            }
        }
    };

    if !canonical.exists()
        && !bridge
            .as_ref()
            .is_some_and(|bridge| bridge.supports_read_text_file())
    {
        return Ok(ToolOutput::Text(not_found_message(ctx, raw_path)));
    }

    if canonical.is_dir() {
        return Ok(ToolOutput::Text(format!(
            "Path is a directory, not a file: {raw_path}. Use run_shell with ls to list directory contents."
        )));
    }

    // Security: check blocked paths
    if is_blocked_path(
        &canonical,
        &config.security.blocked_paths,
        &config.security.allowed_paths,
    ) {
        return Ok(ToolOutput::Text(format!(
            "Access denied: {raw_path} is in a blocked path."
        )));
    }

    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "pdf" {
        let pdf_args =
            serde_json::json!({"file_path": canonical.to_string_lossy(), "max_chars": max_chars});
        return Ok(ToolOutput::Text(handle_read_pdf(&pdf_args)?));
    }

    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        // Guard against huge images (50MB max)
        if let Ok(meta) = std::fs::metadata(&canonical) {
            if meta.len() > constants::MAX_IMAGE_FILE_SIZE as u64 {
                return Ok(ToolOutput::Text(format!(
                    "Image too large ({} MB). Max 50 MB.",
                    meta.len() / (1024 * 1024)
                )));
            }
        }

        let raw_bytes = match std::fs::read(&canonical) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolOutput::Text(format!("Error reading file: {e}")));
            }
        };

        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(&raw_bytes);
        let mime = mime_for_extension(&ext);

        // Compress if needed (1MB threshold)
        let (final_b64, final_mime) =
            crate::media::compress_image(&b64, mime, constants::IMAGE_COMPRESSION_TARGET)
                .unwrap_or((b64, mime.to_string()));

        let summary = format!(
            "Image: {} ({} bytes)",
            canonical.file_name().unwrap_or_default().to_string_lossy(),
            raw_bytes.len()
        );

        return Ok(ToolOutput::Multimodal {
            text: summary.clone(),
            parts: vec![
                ContentPart::Text(summary),
                ContentPart::ImageBase64 {
                    media: MediaData {
                        mime_type: final_mime,
                        data: final_b64,
                        filename: canonical
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string()),
                    },
                },
            ],
        });
    }

    // Text file: read raw content (no line-number gutter). Emitting a per-line
    // gutter costs input tokens on every line for orientation the model rarely
    // needs — edits anchor on exact content + patch context markers, not line
    // numbers. The partial-read footer below preserves line orientation.
    let content = if let Some(bridge) = bridge
        .as_ref()
        .filter(|bridge| bridge.supports_read_text_file())
    {
        match bridge.read_text_file(&canonical) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::Text(format!(
                    "ACP fs read_text_file failed: {e}"
                )));
            }
        }
    } else {
        match std::fs::read_to_string(&canonical) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::Text(format!(
                    "Error reading file: {e}. The file may be binary."
                )));
            }
        }
    };

    if content.is_empty() {
        return Ok(ToolOutput::Text(format!("[File is empty: {raw_path}]")));
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    // Apply offset (1-based) and limit
    let start = (offset - 1).min(total_lines);
    let end = if limit > 0 {
        (start + limit).min(total_lines)
    } else {
        total_lines
    };

    let mut output = String::new();
    for line in &lines[start..end] {
        output.push_str(line);
        output.push('\n');
    }

    // Truncate if too long (safe for multi-byte UTF-8)
    if output.len() > max_chars {
        let truncate_at = output
            .char_indices()
            .take_while(|(i, _)| *i <= max_chars)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        output.truncate(truncate_at);
        output.push_str(&format!(
            "\n\n[truncated — {max_chars} chars shown, {total_lines} total lines]"
        ));
    } else if end < total_lines {
        output.push_str(&format!(
            "\n[showing lines {offset}–{end} of {total_lines}]"
        ));
    }

    Ok(ToolOutput::Text(output))
}

/// Build a "file not found" message that surfaces the *resolved* absolute path
/// when it differs from the raw input. A relative path resolves against the
/// execution context's workspace cwd (not the process cwd), so when a worktree
/// or remote session sets a different cwd, reporting only the relative path
/// hides where the agent actually looked. Showing the resolved path makes a
/// wrong-cwd mismatch visible instead of silent.
fn not_found_message(ctx: &ExecutionContext, raw_path: &str) -> String {
    if let Ok(resolved) = ctx.resolve_path_maybe_missing(raw_path) {
        let resolved = resolved.to_string_lossy();
        if resolved != raw_path {
            return format!("File not found: {raw_path} (resolved: {resolved})");
        }
    }
    format!("File not found: {raw_path}")
}

/// Map a file extension to its MIME type.
fn mime_for_extension(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeTextBridge {
        reads: Mutex<HashMap<std::path::PathBuf, String>>,
        writes: Mutex<HashMap<std::path::PathBuf, String>>,
    }

    impl crate::execution_context::TextFileBridge for FakeTextBridge {
        fn supports_read_text_file(&self) -> bool {
            true
        }

        fn supports_write_text_file(&self) -> bool {
            true
        }

        fn read_text_file(&self, path: &std::path::Path) -> Result<String> {
            self.reads
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing fake read buffer"))
        }

        fn write_text_file(&self, path: &std::path::Path, content: &str) -> Result<()> {
            self.writes
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), content.to_string());
            Ok(())
        }
    }

    #[test]
    fn handle_read_pdf_missing_file() {
        let result = handle_read_pdf(&json!({"file_path": "/nonexistent/path.pdf"})).unwrap();
        assert!(result.contains("not found"));
    }

    #[test]
    fn handle_read_pdf_missing_param() {
        let result = handle_read_pdf(&json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn handle_read_file_missing_file() {
        let config = Config::default();
        let result = handle_read_file(&json!({"path": "/nonexistent/file.txt"}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => assert!(s.contains("not found")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_missing_param() {
        let config = Config::default();
        let result = handle_read_file(&json!({}), &config);
        assert!(result.is_err());
    }

    #[test]
    fn handle_read_file_text_raw_no_gutter() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "fn main() {\n    let x = 1;\n    foo(x);\n").unwrap();
        let result = handle_read_file(&json!({"path": path.to_string_lossy()}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => {
                // Raw line content, no line-number gutter: no leading spaces,
                // no tab separator. (Line endings are normalized to `\n`, as
                // they were before the gutter was dropped — not a byte-for-byte
                // copy of arbitrary CRLF / trailing-newline variations.)
                assert_eq!(s, "fn main() {\n    let x = 1;\n    foo(x);\n");
                assert!(!s.contains('\t'), "must not emit a tab gutter");
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_offset_limit() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.txt");
        std::fs::write(&path, "alpha\nbravo\ncharlie\ndelta\n").unwrap();
        let result = handle_read_file(
            &json!({"path": path.to_string_lossy(), "offset": 2, "limit": 2}),
            &config,
        )
        .unwrap();
        match result {
            ToolOutput::Text(s) => {
                // Lines 2-3 only, raw, followed by the partial-read footer that
                // preserves line orientation now that the gutter is gone.
                assert!(
                    s.starts_with("bravo\ncharlie\n"),
                    "should return lines 2-3: {s}"
                );
                assert!(!s.contains("alpha"), "should not include line 1");
                assert!(!s.contains("delta"), "should stop at limit");
                assert!(
                    s.contains("[showing lines 2–3 of 4]"),
                    "partial-read footer should report the window: {s}"
                );
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_blocked_path() {
        let config = Config::default();
        let home = dirs::home_dir().unwrap();
        let blocked = home.join(".ssh/id_rsa");
        let result =
            handle_read_file(&json!({"path": blocked.to_string_lossy()}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => assert!(s.contains("denied") || s.contains("not found")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_directory_rejected() {
        let config = Config::default();
        let result = handle_read_file(&json!({"path": "."}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => assert!(s.contains("directory")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_missing_reports_resolved_absolute_path() {
        // A relative path resolves against the execution context's workspace
        // cwd, which can differ from the process cwd (worktree / remote
        // session). The not-found message must surface that resolved absolute
        // path so a wrong-cwd lookup is visible, not just the bare relative name.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).unwrap();
        let result =
            handle_read_file_with_context(&json!({"path": "ghost.txt"}), &config, &ctx).unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert!(s.contains("not found"), "got: {s}");
                assert!(
                    s.contains(root.to_string_lossy().as_ref()),
                    "resolved absolute path missing from: {s}"
                );
                assert!(s.contains("ghost.txt"), "raw path missing from: {s}");
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_with_bridge_prefers_unsaved_buffer() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("demo.txt");
        std::fs::write(&file, "disk\n").unwrap();
        let bridge = Arc::new(FakeTextBridge::default());
        bridge
            .reads
            .lock()
            .unwrap()
            .insert(file.clone(), "buffered\ntext\n".to_string());
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone())
            .unwrap()
            .with_text_file_bridge(Some(bridge));
        let result =
            handle_read_file_with_context(&json!({"path": file.to_string_lossy()}), &config, &ctx)
                .unwrap();
        match result {
            ToolOutput::Text(s) => assert!(s.contains("buffered")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_empty_file() {
        let tmp = std::env::temp_dir().join(format!("borg_empty_{}", std::process::id()));
        std::fs::write(&tmp, "").unwrap();
        let config = Config::default();
        let result =
            handle_read_file(&json!({"path": tmp.to_string_lossy().as_ref()}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => assert!(s.contains("empty"), "expected 'empty' in: {s}"),
            _ => panic!("expected Text"),
        }
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn handle_read_file_tilde_expansion() {
        let config = Config::default();
        let result = handle_read_file(
            &json!({"path": "~/nonexistent_borg_test_file_xyz.txt"}),
            &config,
        )
        .unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert!(s.contains("not found"), "expected 'not found' in: {s}")
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_truncation() {
        let tmp = std::env::temp_dir().join(format!("borg_trunc_{}", std::process::id()));
        let content = "x\n".repeat(1000);
        std::fs::write(&tmp, &content).unwrap();
        let config = Config::default();
        let result = handle_read_file(
            &json!({"path": tmp.to_string_lossy().as_ref(), "max_chars": 100}),
            &config,
        )
        .unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert!(s.contains("truncated"), "expected 'truncated' in: {s}")
            }
            _ => panic!("expected Text"),
        }
        std::fs::remove_file(&tmp).ok();
    }
}
