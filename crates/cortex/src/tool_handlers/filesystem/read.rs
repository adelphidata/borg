//! `read_file` (text, documents, and image multimodal) and `read_pdf` tool
//! handlers.

use anyhow::Result;
use base64::Engine as _;
use tracing::instrument;

use crate::config::Config;
use crate::constants;
use crate::execution_context::ExecutionContext;
use crate::types::{ContentPart, MediaData, ToolOutput};

use super::blocked::is_blocked_path;
use super::default_execution_context;
use super::document;
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
    match document::convert(path) {
        Ok(doc) => {
            // `coverage_warning` stays silent when *every* page is empty,
            // deferring to this message — without it a fully-scanned PDF
            // returns an empty string, which reads as "the document is blank".
            if doc.markdown.trim().is_empty() {
                return Ok(format!(
                    "[No extractable text in {file_path} — the document may be empty, or scanned images with no text layer]"
                ));
            }
            // Same disclosure as the read_file path: a part-scanned PDF must
            // not present its partial text as the whole document.
            let mut output = document::coverage_warning(&doc).unwrap_or_default();
            output.push_str(&window_text(&doc.markdown, 1, 0, max_chars));
            Ok(output)
        }
        Err(e) => Ok(format!("Error reading PDF: {e}")),
    }
}

/// Convert a document named by a document extension, reporting the failure
/// when it cannot be read — the caller asked for a `.docx`, so "this `.docx`
/// is unreadable" is the useful answer.
fn read_as_document(
    path: &std::path::Path,
    raw_path: &str,
    offset: usize,
    limit: usize,
    max_chars: usize,
) -> ToolOutput {
    let doc = match document::convert(path) {
        Ok(doc) => doc,
        Err(e) => return ToolOutput::Text(format!("Error reading document {raw_path}: {e}")),
    };

    if doc.markdown.trim().is_empty() {
        return ToolOutput::Text(format!(
            "[No extractable text in {raw_path} — the document may be empty, or scanned images with no text layer]"
        ));
    }

    // The extension can still be the wrong one *within* the document family
    // (a `.doc` that is really a `.docx`), which the reader should know.
    let mismatch = document::Format::from_path(path).is_some_and(|claimed| claimed != doc.format);
    ToolOutput::Text(frame(&doc, mismatch, offset, limit, max_chars))
}

/// Try to read a file that failed a text read as a document, for the case
/// where nothing about its name suggested one. Returns `None` when it is not
/// a document, so the caller can report the original read failure instead of
/// masking it with a conversion error.
///
/// Detection needs the whole file, not a prefix: ZIP packages (`.docx`,
/// `.xlsx`, `.odt`, `.epub`) are identified through their central directory,
/// which sits at the *end* of the container, and OLE files are opened whole.
fn try_read_as_document(
    path: &std::path::Path,
    offset: usize,
    limit: usize,
    max_chars: usize,
) -> Option<ToolOutput> {
    let doc = document::convert(path).ok()?;
    if doc.markdown.trim().is_empty() {
        return None;
    }
    Some(ToolOutput::Text(frame(
        &doc, true, offset, limit, max_chars,
    )))
}

/// Prefix converted Markdown with the notice that says what it came from and
/// that it is a read-only view.
fn frame(
    doc: &document::ConvertedDocument,
    extension_mismatch: bool,
    offset: usize,
    limit: usize,
    max_chars: usize,
) -> String {
    let mut output = document::conversion_header(doc, extension_mismatch);
    // A part-scanned PDF converts "successfully" while whole sections are
    // silently missing; the coverage warning discloses the gap up front.
    if let Some(warning) = document::coverage_warning(doc) {
        output.push_str(&warning);
    }
    output.push_str(&window_text(&doc.markdown, offset, limit, max_chars));
    output
}

/// Name the file kind when `path` is something a byte read cannot finish:
/// a FIFO blocks until a writer appears, a socket refuses, and a device read
/// can hang or stream forever. The existing guards are name-based (`/proc`
/// leaves), which cannot see an arbitrary special file in the workspace, so
/// the check is a single `stat` on the resolved path.
///
/// Follows symlinks, matching the `is_dir()` check it sits beside. A `stat`
/// error is never a refusal — the caller falls through to the ordinary
/// not-found and read handling, which already report failures usefully.
fn special_file_note(path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        let file_type = std::fs::metadata(path).ok()?.file_type();
        let kind = if file_type.is_fifo() {
            "a named pipe (FIFO)"
        } else if file_type.is_socket() {
            "a socket"
        } else if file_type.is_char_device() {
            "a character device"
        } else if file_type.is_block_device() {
            "a block device"
        } else {
            return None;
        };
        Some(format!(
            "{} is {kind}, not a regular file — no read was attempted, because \
             reading it can block indefinitely.",
            path.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Apply the 1-based `offset`/`limit` line window and the `max_chars` ceiling,
/// annotating the result whenever the caller is not seeing all of it.
fn window_text(content: &str, offset: usize, limit: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let start = (offset - 1).min(total_lines);
    let end = if limit > 0 {
        (start + limit).min(total_lines)
    } else {
        total_lines
    };

    // Past the end: the window is empty for a reason the caller cannot infer
    // from an empty string, which reads as "the file has no content here".
    // A genuinely empty file is reported by its own message upstream.
    if total_lines > 0 && start >= total_lines {
        return format!("[no lines at offset {offset} — the file ends at line {total_lines}]\n");
    }

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

    output
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

    // After the blocked-path check, so a denied path reports the denial rather
    // than disclosing its file kind — and before every branch that opens the
    // file, since document, image, and plain text all reach a blocking read.
    if let Some(note) = special_file_note(&canonical) {
        return Ok(ToolOutput::Text(note));
    }

    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Word, PowerPoint, Excel, OpenDocument, RTF, EPUB and PDF are opaque to a
    // text read, so they are converted to Markdown the moment they are read.
    if document::is_convertible_extension(&ext) {
        return Ok(read_as_document(
            &canonical, raw_path, offset, limit, max_chars,
        ));
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
                // The extension promised text but the bytes are not UTF-8.
                // Before giving up, check whether this is a document wearing
                // the wrong name — format detection reads the content
                // signature, so a `.docx` saved as `.txt` still converts.
                if let Some(converted) = try_read_as_document(&canonical, offset, limit, max_chars)
                {
                    return Ok(converted);
                }
                return Ok(ToolOutput::Text(format!(
                    "Error reading file: {e}. The file may be binary."
                )));
            }
        }
    };

    if content.is_empty() {
        return Ok(ToolOutput::Text(format!("[File is empty: {raw_path}]")));
    }

    Ok(ToolOutput::Text(window_text(
        &content, offset, limit, max_chars,
    )))
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
    fn handle_read_file_non_utf8_flagged_binary_not_mojibake() {
        // A latin-1 (non-UTF-8) file must be refused as binary, never served
        // as lossy text: U+FFFD replacement chars are "printable", so if the
        // read path ever switched to a lossy decode, the model would edit the
        // mojibake and write it back, permanently destroying the original
        // bytes. A valid UTF-8 file with multibyte chars must still read fine.
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();

        let latin1 = dir.path().join("latin1.txt");
        // "café" in latin-1: 0xE9 is invalid as UTF-8 here.
        std::fs::write(&latin1, [b'c', b'a', b'f', 0xE9, b'\n']).unwrap();
        let result = handle_read_file(&json!({"path": latin1.to_string_lossy()}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert!(
                    !s.contains('\u{FFFD}'),
                    "must never serve replacement-char mojibake as content: {s}"
                );
                assert!(
                    s.contains("binary") || s.contains("Error reading file"),
                    "non-UTF-8 file must be flagged unreadable/binary, got: {s}"
                );
                assert!(
                    !s.starts_with("caf"),
                    "must not serve the file as ordinary editable text: {s}"
                );
            }
            _ => panic!("expected Text"),
        }

        let utf8 = dir.path().join("utf8.txt");
        std::fs::write(&utf8, "café — 日本語\n").unwrap();
        let result = handle_read_file(&json!({"path": utf8.to_string_lossy()}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert_eq!(
                    s, "café — 日本語\n",
                    "valid multibyte UTF-8 must read as-is"
                )
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

    /// Run `read_file` on another thread so a regressed guard fails the test
    /// with a timeout instead of hanging the whole suite: a FIFO with no
    /// writer blocks `read_to_string` forever, which is the bug being pinned.
    #[cfg(unix)]
    fn read_with_deadline(path: &std::path::Path) -> String {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel();
        let owned = path.to_path_buf();
        std::thread::spawn(move || {
            let config = Config::default();
            let out = handle_read_file(&json!({"path": owned.to_string_lossy()}), &config);
            // A closed receiver means the deadline already fired; the panic
            // below is the real failure, so drop this send quietly.
            let _ = tx.send(out.map(|o| match o {
                ToolOutput::Text(s) => s,
                other => format!("{other:?}"),
            }));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => result.expect("read_file returned an error"),
            Err(_) => panic!("read_file blocked on a special file — the stat guard is missing"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_file_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must be available on unix CI");
        assert!(status.success(), "mkfifo failed");

        let out = read_with_deadline(&fifo);
        assert!(
            out.contains("named pipe"),
            "expected the FIFO kind to be named: {out}"
        );
        assert!(
            out.contains("no read was attempted"),
            "the model must be told nothing was read: {out}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_file_refuses_a_socket_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sock");
        // Bind holds the socket file for the length of the test.
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

        let out = read_with_deadline(&sock);
        assert!(
            out.contains("socket"),
            "expected the socket kind to be named: {out}"
        );
        assert!(
            out.contains("no read was attempted"),
            "the model must be told nothing was read: {out}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_file_still_reads_a_regular_file_beside_a_special_one() {
        // The guard must not become a blanket refusal: a plain file in the
        // same directory reads exactly as before.
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        let plain = dir.path().join("notes.txt");
        std::fs::write(&plain, "hello\n").unwrap();

        let out = read_with_deadline(&plain);
        assert!(out.contains("hello"), "expected the file content: {out}");
        assert!(
            !out.contains("no read was attempted"),
            "a regular file must not trip the guard: {out}"
        );
    }

    #[test]
    fn window_text_past_eof_says_where_the_file_ends() {
        // Returning "" here reads as "this part of the file is blank", which
        // is a different fact from "you asked past the last line".
        let out = window_text("a\nb\nc\n", 10, 0, 10_000);
        assert!(
            out.contains("offset 10"),
            "the requested offset must be echoed: {out}"
        );
        assert!(
            out.contains("line 3"),
            "the last real line must be named: {out}"
        );
    }

    #[test]
    fn window_text_at_the_last_line_still_returns_it() {
        // Off-by-one guard: line 3 of a 3-line file is in range, not past EOF.
        let out = window_text("a\nb\nc\n", 3, 0, 10_000);
        assert_eq!(out, "c\n", "expected the last line, got: {out}");
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
