//! `list_dir` tool handler: recursive walk, fuzzy matching, pagination.

use anyhow::Result;

use crate::config::Config;
use crate::execution_context::ExecutionContext;

use super::blocked::is_blocked_path;
use super::default_execution_context;
use crate::tool_handlers::{optional_bool_param, optional_str_param, optional_u64_param};

pub fn handle_list_dir(args: &serde_json::Value, config: &Config) -> Result<String> {
    let ctx = default_execution_context()?;
    handle_list_dir_with_context(args, config, &ctx)
}

pub fn handle_list_dir_with_context(
    args: &serde_json::Value,
    config: &Config,
    ctx: &ExecutionContext,
) -> Result<String> {
    let path_str = optional_str_param(args, "path").unwrap_or(".");
    let depth = optional_u64_param(args, "depth", 1).min(3) as usize;
    let include_hidden = optional_bool_param(args, "include_hidden", false);
    let fuzzy_match = optional_str_param(args, "fuzzy_match")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let max_results = optional_u64_param(args, "max_results", 50).clamp(1, 500) as usize;
    // Optional pagination over the rendered/ranked entry list. `offset` is
    // 0-based; `limit` caps at 500 to mirror `max_results`. When neither is
    // given, the unpaged shape is preserved so existing callers/snapshots
    // don't shift.
    let offset = args
        .get("offset")
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as usize);
    let limit = args
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .map(|v| (v as usize).min(500));

    let canonical = match ctx.resolve_existing_path(path_str) {
        Ok(path) => path,
        Err(_) => return Ok(format!("Not a directory: {path_str}")),
    };

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

    if let Some(query) = fuzzy_match {
        let raw = fuzzy_list_dir(
            &canonical,
            query,
            include_hidden,
            max_results,
            &config.security.blocked_paths,
            &config.security.allowed_paths,
        );
        return Ok(paginate_listing(&raw, offset, limit));
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
        return Ok("(empty directory)".to_string());
    }
    Ok(paginate_listing(&output, offset, limit))
}

/// Paginate a newline-delimited listing. Returns the original string when no
/// offset/limit is supplied; otherwise slices line-wise and appends a footer
/// describing the visible window so the caller can request the next page.
fn paginate_listing(raw: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    if offset.is_none() && limit.is_none() {
        return raw.to_string();
    }
    let lines: Vec<&str> = raw.lines().collect();
    let total = lines.len();
    let start = offset.unwrap_or(0).min(total);
    let end = limit.map(|l| (start + l).min(total)).unwrap_or(total);
    if start >= end {
        return format!("(empty page; offset {start} of {total} lines)\n");
    }
    let mut out = lines[start..end].join("\n");
    out.push('\n');
    out.push_str(&format!("(showing {}-{} of {total} lines)", start + 1, end));
    out
}

/// Walk `root` (skipping a curated noisy-dir set) and return a ranked list of
/// file paths whose forward-slash-relative form best matches `query` under the
/// `nucleo-matcher` fuzzy-match algorithm.
///
/// Output is line-based (`<rel-path>\t<score>`) to match the rest of
/// `list_dir`'s style. The walker enforces `blocked_paths` exactly like the
/// tree-listing branch, plus a small built-in ignore set for build/VCS noise.
/// We deliberately do not pull in `ignore::Walk` so the fuzzy branch behaves
/// identically to the tree branch w.r.t. visibility rules.
fn fuzzy_list_dir(
    root: &std::path::Path,
    query: &str,
    include_hidden: bool,
    max_results: usize,
    blocked_paths: &[String],
    allowed_paths: &[String],
) -> String {
    use nucleo_matcher::{
        pattern::{CaseMatching, Normalization, Pattern},
        Config as MatcherConfig, Matcher, Utf32Str,
    };

    const NOISY_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        ".venv",
        "venv",
        "__pycache__",
        "dist",
        "build",
        ".next",
        ".cache",
    ];

    let mut paths: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                // Permission denied / vanished mid-walk: continue without the
                // subtree, but emit a warn so a quietly-truncated result is
                // attributable instead of looking like the file isn't there.
                tracing::warn!(dir = %dir.display(), error = %e, "fuzzy walk: read_dir failed");
                continue;
            }
        };
        for entry in entries.flatten() {
            let name_os = entry.file_name();
            let name = name_os.to_string_lossy();
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            if NOISY_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if is_blocked_path(&canonical, blocked_paths, allowed_paths) {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "fuzzy walk: file_type failed");
                    continue;
                }
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    // Use forward slashes so fuzzy queries work the same on
                    // every platform — the LLM types `crates/coding/parser`,
                    // not Windows-style backslashes.
                    let rel_str = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    if !rel_str.is_empty() {
                        paths.push(rel_str);
                    }
                }
            }
        }
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());

    let mut scored: Vec<(u32, String)> = paths
        .into_iter()
        .filter_map(|p| {
            let mut buf = Vec::new();
            let haystack = Utf32Str::new(&p, &mut buf);
            pattern.score(haystack, &mut matcher).map(|s| (s, p))
        })
        .collect();

    // Sort by score desc, then path asc for stable output across ties.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(max_results);

    if scored.is_empty() {
        return format!("(no files match fuzzy query: {query:?})");
    }

    let mut out = String::new();
    for (score, path) in scored {
        out.push_str(&format!("{path}\t{score}\n"));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn handle_list_dir_fuzzy_match_ranks_relevant_paths_first() {
        // fuzzy_match must return a ranked, flat list of paths whose relative
        // form best matches the query, with noise (build dirs, unrelated
        // files) suppressed or pushed to the bottom.
        let tmp = std::env::temp_dir().join(format!(
            "borg_fuzzy_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join("node_modules/lodash")).unwrap();
        std::fs::write(tmp.join("src/parser.rs"), "// parser").unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(tmp.join("README.md"), "# r").unwrap();
        std::fs::write(tmp.join("node_modules/lodash/index.js"), "// noise").unwrap();

        let config = Config::default();
        let result = handle_list_dir(
            &json!({
                "path": tmp.to_string_lossy().as_ref(),
                "fuzzy_match": "parser"
            }),
            &config,
        )
        .unwrap();

        let lines: Vec<&str> = result.lines().collect();
        assert!(!lines.is_empty(), "expected at least one ranked path");
        // First line must be the parser file — strongest match for "parser".
        assert!(
            lines[0].starts_with("src/parser.rs\t"),
            "top-ranked line should be parser.rs, got: {}",
            lines[0]
        );
        // Noisy build dir contents must be filtered out.
        assert!(
            !result.contains("node_modules"),
            "node_modules must be excluded from fuzzy walk: {result}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_fuzzy_match_respects_max_results() {
        let tmp = std::env::temp_dir().join(format!(
            "borg_fuzzy_max_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..10 {
            std::fs::write(tmp.join(format!("file_{i}.txt")), "x").unwrap();
        }

        let config = Config::default();
        let result = handle_list_dir(
            &json!({
                "path": tmp.to_string_lossy().as_ref(),
                "fuzzy_match": "file",
                "max_results": 3,
            }),
            &config,
        )
        .unwrap();
        let line_count = result.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(line_count, 3, "max_results must cap output, got: {result}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_fuzzy_match_no_results_message() {
        let tmp = std::env::temp_dir().join(format!(
            "borg_fuzzy_none_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("only.txt"), "x").unwrap();

        let config = Config::default();
        let result = handle_list_dir(
            &json!({
                "path": tmp.to_string_lossy().as_ref(),
                "fuzzy_match": "qzqzqzqzqz",
            }),
            &config,
        )
        .unwrap();
        assert!(
            result.contains("no files match"),
            "should explain empty result: {result}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_pagination_slices_lines_with_footer() {
        let tmp = std::env::temp_dir().join(format!(
            "borg_paginate_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..30 {
            std::fs::write(tmp.join(format!("f{i:02}.txt")), "x").unwrap();
        }

        let config = Config::default();
        let result = handle_list_dir(
            &json!({
                "path": tmp.to_string_lossy().as_ref(),
                "offset": 5,
                "limit": 10,
            }),
            &config,
        )
        .unwrap();

        let body_lines: Vec<&str> = result
            .lines()
            .filter(|l| !l.starts_with("(showing"))
            .collect();
        assert_eq!(
            body_lines.len(),
            10,
            "page should hold exactly 10 lines: {result}"
        );
        assert!(
            result.contains("(showing 6-15 of 30 lines)"),
            "footer mismatch: {result}"
        );
        // Sanity check that we skipped the first 5 (sorted f00.txt..f04.txt).
        assert!(
            !result.contains(" f00.txt "),
            "offset should skip f00: {result}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_pagination_offset_past_end_returns_empty_marker() {
        let tmp = std::env::temp_dir().join(format!(
            "borg_paginate_end_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("only.txt"), "x").unwrap();

        let config = Config::default();
        let result = handle_list_dir(
            &json!({
                "path": tmp.to_string_lossy().as_ref(),
                "offset": 50,
                "limit": 10,
            }),
            &config,
        )
        .unwrap();
        assert!(
            result.contains("empty page"),
            "should announce empty page: {result}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn handle_list_dir_pagination_combines_with_fuzzy_match() {
        let tmp = std::env::temp_dir().join(format!(
            "borg_paginate_fuzzy_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..20 {
            std::fs::write(tmp.join(format!("alpha_{i:02}.rs")), "x").unwrap();
        }

        let config = Config::default();
        let result = handle_list_dir(
            &json!({
                "path": tmp.to_string_lossy().as_ref(),
                "fuzzy_match": "alpha",
                "max_results": 20,
                "offset": 0,
                "limit": 5,
            }),
            &config,
        )
        .unwrap();
        let body_lines: Vec<&str> = result
            .lines()
            .filter(|l| !l.starts_with("(showing"))
            .collect();
        assert_eq!(
            body_lines.len(),
            5,
            "fuzzy page should be 5 lines: {result}"
        );
        assert!(
            result.contains("(showing 1-5 of 20 lines)"),
            "footer: {result}"
        );

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
}
