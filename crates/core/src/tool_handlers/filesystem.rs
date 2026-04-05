use anyhow::{Context, Result};
use base64::Engine as _;
use tracing::instrument;

use crate::config::Config;
use crate::constants;
use crate::types::{ContentPart, MediaData, ToolOutput};

use super::{optional_bool_param, optional_str_param, optional_u64_param, require_str_param};

/// Apply a patch to a directory, returning a formatted result message.
fn apply_patch_to(
    args: &serde_json::Value,
    base_dir: &std::path::Path,
    label: &str,
) -> Result<String> {
    let patch = require_str_param(args, "patch")?;
    std::fs::create_dir_all(base_dir)?;
    match borg_apply_patch::apply_patch_to_dir(patch, base_dir) {
        Ok(affected) => Ok(format!(
            "{label} patch applied successfully.\n{}",
            affected.format_summary()
        )),
        Err(e) => Ok(format!("Error applying {label} patch: {e}")),
    }
}

pub fn handle_apply_skill_patch(args: &serde_json::Value) -> Result<String> {
    apply_patch_to(args, &Config::skills_dir()?, "Skill")
}

/// Unified apply_patch handler with `target` parameter.
/// Supports: cwd (default), skills, channels.
pub fn handle_apply_patch_unified(args: &serde_json::Value) -> Result<String> {
    // Validate patch param exists before dispatching
    let _patch = require_str_param(args, "patch")?;
    let target = optional_str_param(args, "target").unwrap_or("cwd");

    match target {
        "cwd" => handle_apply_patch(args),
        "skills" => handle_apply_skill_patch(args),
        "channels" => handle_create_channel(args),
        other => Ok(format!(
            "Unknown target: {other}. Use: cwd, skills, channels."
        )),
    }
}

pub fn handle_apply_patch(args: &serde_json::Value) -> Result<String> {
    let patch = require_str_param(args, "patch")?;
    let base_dir =
        std::env::current_dir().context("Failed to determine current working directory")?;
    match borg_apply_patch::apply_patch_to_dir(patch, &base_dir) {
        Ok(affected) => Ok(format!(
            "Patch applied successfully.\n{}",
            affected.format_summary()
        )),
        Err(e) => Ok(format!("Error applying patch: {e}")),
    }
}

pub fn handle_create_channel(args: &serde_json::Value) -> Result<String> {
    apply_patch_to(args, &Config::channels_dir()?, "Channel")
}

pub fn handle_list_dir(args: &serde_json::Value, config: &Config) -> Result<String> {
    let path_str = optional_str_param(args, "path").unwrap_or(".");
    let depth = optional_u64_param(args, "depth", 1).min(3) as usize;
    let include_hidden = optional_bool_param(args, "include_hidden", false);

    let base = if path_str.starts_with('/') || path_str.starts_with('~') {
        std::path::PathBuf::from(shellexpand::tilde(path_str).as_ref())
    } else {
        std::env::current_dir()?.join(path_str)
    };

    let canonical = base.canonicalize().unwrap_or_else(|_| base.clone());

    // Security: reuse the same blocked-path check as read_file
    if is_blocked_path(
        &canonical,
        &config.security.blocked_paths,
        &config.security.allowed_paths,
    ) {
        return Ok(format!("Access denied: {path_str} is in a blocked path"));
    }

    if !canonical.is_dir() {
        return Ok(format!("Not a directory: {path_str}"));
    }

    let mut output = String::new();
    list_dir_recursive(
        &canonical,
        depth,
        0,
        include_hidden,
        &config.security.blocked_paths,
        &config.security.allowed_paths,
        &mut output,
    )?;
    if output.is_empty() {
        output = "(empty directory)".to_string();
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn list_dir_recursive(
    dir: &std::path::Path,
    max_depth: usize,
    current_depth: usize,
    include_hidden: bool,
    blocked_paths: &[String],
    allowed_paths: &[String],
    output: &mut String,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let indent = "  ".repeat(current_depth);
    for entry in &entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !include_hidden && name_str.starts_with('.') {
            continue;
        }

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        // Security: check each entry against blocked paths before displaying/
        // recursing. For symlinks we must resolve the target too — an
        // unresolved link would be a bypass (e.g. `decoy -> ~/.ssh/id_rsa`).
        // If canonicalization fails for a symlink, treat it as blocked
        // rather than falling through to the raw path.
        let raw_path = entry.path();
        let is_link = ft.is_symlink();
        let blocked = if is_link {
            match raw_path.canonicalize() {
                Ok(target) => is_blocked_path(&target, blocked_paths, allowed_paths),
                // Broken/unresolvable symlink: hide it rather than leaking
                // the raw link target string.
                Err(_) => true,
            }
        } else {
            let entry_canonical = raw_path.canonicalize().unwrap_or_else(|_| raw_path.clone());
            is_blocked_path(&entry_canonical, blocked_paths, allowed_paths)
        };

        if blocked {
            output.push_str(&format!("{indent}[blocked] {name_str}\n"));
            continue;
        }

        if ft.is_dir() {
            output.push_str(&format!("{indent}[dir]  {name_str}/\n"));
            if current_depth < max_depth {
                list_dir_recursive(
                    &raw_path,
                    max_depth,
                    current_depth + 1,
                    include_hidden,
                    blocked_paths,
                    allowed_paths,
                    output,
                )?;
            }
        } else if is_link {
            let target = std::fs::read_link(&raw_path)
                .map(|t| t.to_string_lossy().to_string())
                .unwrap_or_else(|_| "?".to_string());
            output.push_str(&format!("{indent}[link] {name_str} -> {target}\n"));
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let size_str = format_size(size);
            output.push_str(&format!("{indent}[file] {name_str} ({size_str})\n"));
        }
    }
    Ok(())
}

pub(crate) fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

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

/// Image file extensions that should be returned as multimodal content.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif", "svg",
];

/// Check whether `path` is denied by the configured blocklist, taking the
/// allow list into account.
///
/// Matching rules:
/// 1. The path is canonicalized first (resolves symlinks, `..`, and
///    tilde-expanded entries on the allow list). If canonicalization fails
///    for a path that doesn't exist, the original path is used — this is
///    fine because nonexistent paths are handled separately by callers.
/// 2. If the canonical path starts with any entry in `allowed_paths`,
///    access is allowed regardless of blocklist matches. This is the
///    escape hatch for specific `.env` files or `.aws/` directories that
///    the user legitimately wants the agent to read.
/// 3. Otherwise, access is denied if **any path component** of the
///    canonical path matches an entry in `blocked`. Blocked entries may be
///    single components (e.g. `.ssh`) or multi-component suffixes (e.g.
///    `.config/gh`) — both forms are supported.
///
/// This deliberately does **not** jail access to `$HOME` or CWD: absolute
/// paths to non-sensitive files elsewhere on the system
/// (`/etc/hosts`, `/Users/you/other-project/...`, `../sibling/file.rs`)
/// remain fully accessible, as does cross-directory navigation via `@`.
pub fn is_blocked_path(path: &std::path::Path, blocked: &[String], allowed: &[String]) -> bool {
    // Canonicalize so we make decisions on the real on-disk path. If
    // canonicalization fails (path doesn't exist, permissions), fall back to
    // the literal path — callers handle nonexistent paths separately.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    // 1. Allow list: if the canonical path is under any allowed prefix,
    //    short-circuit to allow. Entries support tilde expansion.
    for raw_entry in allowed {
        let expanded = shellexpand::tilde(raw_entry).into_owned();
        let entry_path = std::path::PathBuf::from(&expanded);
        // Canonicalize the allow entry too, so `~/work/.aws` and
        // `/Users/me/work/.aws` compare equal.
        let entry_canonical = entry_path.canonicalize().unwrap_or(entry_path);
        if canonical.starts_with(&entry_canonical) {
            return false;
        }
    }

    // 2. Block list: match any component or multi-component suffix against
    //    the canonical path's components.
    for raw_entry in blocked {
        if path_contains_blocked_entry(&canonical, raw_entry) {
            return true;
        }
    }

    false
}

/// Returns `true` if `path` contains `entry` as a contiguous sequence of
/// components. `entry` may itself be multi-component (e.g. `.config/gh`).
fn path_contains_blocked_entry(path: &std::path::Path, entry: &str) -> bool {
    // Split the entry into components, tolerating both forward slashes and
    // the platform separator.
    let entry_components: Vec<&std::ffi::OsStr> = std::path::Path::new(entry)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    if entry_components.is_empty() {
        return false;
    }

    let path_components: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    if path_components.len() < entry_components.len() {
        return false;
    }

    // Sliding window match.
    for start in 0..=path_components.len() - entry_components.len() {
        let window = &path_components[start..start + entry_components.len()];
        if window == entry_components.as_slice() {
            return true;
        }
    }

    false
}

#[instrument(skip_all, fields(tool.name = "read_file"))]
pub fn handle_read_file(args: &serde_json::Value, config: &Config) -> Result<ToolOutput> {
    let raw_path = require_str_param(args, "path")?;
    let offset = optional_u64_param(args, "offset", 1).max(1) as usize;
    let limit = optional_u64_param(args, "limit", 0) as usize;
    let max_chars =
        optional_u64_param(args, "max_chars", constants::DEFAULT_READ_MAX_CHARS as u64) as usize;

    // Resolve path: expand ~ and resolve relative paths
    let expanded = shellexpand::tilde(raw_path).to_string();
    let resolved = if std::path::Path::new(&expanded).is_absolute() {
        std::path::PathBuf::from(&expanded)
    } else {
        std::env::current_dir().unwrap_or_default().join(&expanded)
    };

    // Canonicalize to prevent traversal
    let canonical = match resolved.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return Ok(ToolOutput::Text(format!("File not found: {raw_path}")));
        }
    };

    if !canonical.exists() {
        return Ok(ToolOutput::Text(format!("File not found: {raw_path}")));
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

    // Dispatch by extension
    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "pdf" {
        // Delegate to existing PDF handler
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

        // Read image bytes, compress, return as multimodal
        let raw_bytes = match std::fs::read(&canonical) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolOutput::Text(format!("Error reading file: {e}")));
            }
        };

        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(&raw_bytes);
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "heic" | "heif" => "image/heic",
            "svg" => "image/svg+xml",
            _ => "application/octet-stream",
        };

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

    // Text file: read with line numbers
    let content = match std::fs::read_to_string(&canonical) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ToolOutput::Text(format!(
                "Error reading file: {e}. The file may be binary."
            )));
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
    for (i, line) in lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        output.push_str(&format!("{line_no:>6}\t{line}\n"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn handle_read_file_text_with_line_numbers() {
        let config = Config::default();
        let result = handle_read_file(&json!({"path": "Cargo.toml", "limit": 3}), &config).unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert!(s.contains("     1\t"), "should have line numbers");
                assert!(s.contains("     2\t"));
                assert!(s.contains("     3\t"));
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn handle_read_file_offset_limit() {
        let config = Config::default();
        let result = handle_read_file(
            &json!({"path": "Cargo.toml", "offset": 2, "limit": 2}),
            &config,
        )
        .unwrap();
        match result {
            ToolOutput::Text(s) => {
                assert!(!s.contains("     1\t"), "should not include line 1");
                assert!(s.contains("     2\t"), "should start at line 2");
                assert!(s.contains("     3\t"), "should include line 3");
                assert!(!s.contains("     4\t"), "should stop at limit");
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
    fn handle_list_dir_current_directory() {
        let config = Config::default();
        let result = handle_list_dir(&json!({}), &config).unwrap();
        assert!(result.contains("[dir]") || result.contains("[file]"));
    }

    #[test]
    fn handle_list_dir_not_a_directory() {
        let config = Config::default();
        let tmp = std::env::temp_dir().join(format!("borg_listdir_file_{}", std::process::id()));
        std::fs::write(&tmp, "hello").unwrap();
        let result =
            handle_list_dir(&json!({"path": tmp.to_string_lossy().as_ref()}), &config).unwrap();
        assert!(result.contains("Not a directory"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn handle_list_dir_hidden_files_excluded_by_default() {
        let config = Config::default();
        let tmp = std::env::temp_dir().join(format!("borg_listdir_hidden_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".hidden"), "secret").unwrap();
        std::fs::write(tmp.join("visible.txt"), "hello").unwrap();

        let result =
            handle_list_dir(&json!({"path": tmp.to_string_lossy().as_ref()}), &config).unwrap();
        assert!(result.contains("visible.txt"));
        assert!(!result.contains(".hidden"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_hidden_files_included_when_requested() {
        let config = Config::default();
        let tmp =
            std::env::temp_dir().join(format!("borg_listdir_showhidden_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".hidden"), "secret").unwrap();
        std::fs::write(tmp.join("visible.txt"), "hello").unwrap();

        let result = handle_list_dir(
            &json!({"path": tmp.to_string_lossy().as_ref(), "include_hidden": true}),
            &config,
        )
        .unwrap();
        assert!(result.contains("visible.txt"));
        assert!(result.contains(".hidden"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_depth_limiting() {
        let config = Config::default();
        let tmp = std::env::temp_dir().join(format!("borg_listdir_depth_{}", std::process::id()));
        let deep = tmp.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.txt"), "deep").unwrap();

        let result = handle_list_dir(
            &json!({"path": tmp.to_string_lossy().as_ref(), "depth": 1}),
            &config,
        )
        .unwrap();
        assert!(result.contains("a/"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_empty_directory() {
        let config = Config::default();
        let tmp = std::env::temp_dir().join(format!("borg_listdir_empty_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let result =
            handle_list_dir(&json!({"path": tmp.to_string_lossy().as_ref()}), &config).unwrap();
        assert_eq!(result, "(empty directory)");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
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

    #[test]
    fn is_blocked_path_matches_blocked_dir() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".ssh/id_rsa");
        let blocked = vec![".ssh".to_string()];
        assert!(is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_rejects_non_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join("Documents/safe.txt");
        let blocked = vec![".ssh".to_string(), ".aws".to_string()];
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_nested_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".aws/credentials/secret");
        let blocked = vec![".aws".to_string()];
        assert!(is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_empty_blocked_list() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".ssh/id_rsa");
        let blocked: Vec<String> = vec![];
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_outside_home() {
        let blocked = vec![".ssh".to_string()];
        let path = std::path::Path::new("/tmp/.ssh/id_rsa");
        assert!(is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_allowed_overrides_blocked() {
        let home = dirs::home_dir().unwrap();
        let path = home.join(".env.example");
        let blocked = vec![".env".to_string()];
        // The path has ".env" as a substring of the component ".env.example"
        // but component matching should only match exact components
        assert!(!is_blocked_path(&path, &blocked, &[]));
    }

    #[test]
    fn is_blocked_path_absolute_path_not_under_home() {
        let blocked = vec![".aws".to_string()];
        let path = std::path::Path::new("/etc/hosts");
        assert!(!is_blocked_path(path, &blocked, &[]));
    }

    #[test]
    fn apply_patch_unified_unknown_target() {
        let args = json!({"patch": "*** Begin Patch\n*** End Patch", "target": "invalid"});
        let result = handle_apply_patch_unified(&args).unwrap();
        assert!(result.contains("Unknown target"));
    }

    #[test]
    fn apply_patch_unified_missing_patch() {
        let args = json!({"target": "cwd"});
        let result = handle_apply_patch_unified(&args);
        assert!(result.is_err());
    }

    #[test]
    fn apply_patch_unified_default_target_is_cwd() {
        let args = json!({"patch": "*** Begin Patch\n*** End Patch"});
        let result = handle_apply_patch_unified(&args);
        assert!(result.is_ok());
    }

    #[test]
    fn apply_skill_patch_missing_patch_param() {
        let args = json!({});
        let result = handle_apply_skill_patch(&args);
        assert!(result.is_err(), "should error on missing patch param");
    }
}
