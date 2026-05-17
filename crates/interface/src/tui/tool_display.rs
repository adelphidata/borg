use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::theme;

/// A sub-entry inside an "Explored" group.
#[derive(Debug, Clone, PartialEq)]
pub enum ExploreEntry {
    /// One or more file reads, showing basenames.
    Read(Vec<String>),
    /// A directory listing.
    List(String),
}

/// Semantic display category for a tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDisplayCategory {
    Explored { entries: Vec<ExploreEntry> },
    Ran { command: String },
    Edited { files: Vec<String>, count: usize },
    WroteMemory { target: String },
    ReadMemory { target: String },
    SearchedMemory { query: String },
    Browsed { action: String, url: Option<String> },
    Tasks { action: String },
    Listed { what: String },
    Generic { name: String, preview: String },
}

/// Extract the last path component (basename) from a file path.
fn extract_basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Extract filenames from patch DSL content (*** Add/Update/Delete File: lines).
fn extract_patch_filenames(patch: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim();
        for prefix in &["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(name) = trimmed.strip_prefix(prefix) {
                let name = name.trim();
                if !name.is_empty() {
                    files.push(name.to_string());
                }
            }
        }
    }
    files
}

/// Truncate to at most `max` characters total, with "..." suffix when
/// truncation occurs. UTF-8 safe.
fn truncate(s: &str, max: usize) -> String {
    cortex::util::truncate::truncate_to_width_with(s, max, "...")
}

/// Truncate a `/`-separated path so the trailing segment (filename) stays
/// intact and as many leading segments as fit are retained, joined to the
/// tail by `/…/`. Falls back to end-truncate for non-path strings or when a
/// single segment alone exceeds `max`. Preserves the part of the path the
/// user actually identifies the file by.
fn truncate_path(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if !s.contains('/') {
        return truncate(s, max);
    }
    let segments: Vec<&str> = s.split('/').collect();
    let last = *segments.last().unwrap_or(&"");
    let last_w = UnicodeWidthStr::width(last);
    // If the filename alone won't fit with the "…/" prefix, give up and
    // end-truncate so we at least preserve the start.
    if last_w + 2 >= max {
        return truncate(s, max);
    }
    let separator = "/…/";
    let sep_w = UnicodeWidthStr::width(separator);
    // Greedy: take leading segments until adding another would overflow.
    let mut head = String::new();
    let budget = max.saturating_sub(last_w + sep_w);
    for seg in segments.iter().take(segments.len().saturating_sub(1)) {
        let candidate_w = if head.is_empty() {
            UnicodeWidthStr::width(*seg)
        } else {
            UnicodeWidthStr::width(head.as_str()) + 1 + UnicodeWidthStr::width(*seg)
        };
        if candidate_w > budget {
            break;
        }
        if !head.is_empty() {
            head.push('/');
        }
        head.push_str(seg);
    }
    if head.is_empty() {
        // Couldn't fit any leading segment — show "…/filename" instead.
        format!("…/{last}")
    } else {
        format!("{head}{separator}{last}")
    }
}

/// Classify a tool call into a semantic display category.
pub fn classify_tool(name: &str, args_json: &str) -> ToolDisplayCategory {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => {
            return ToolDisplayCategory::Generic {
                name: name.to_string(),
                preview: truncate(args_json, 77),
            };
        }
    };

    match name {
        "read_file" => {
            let path = args["path"].as_str().unwrap_or("?");
            ToolDisplayCategory::Explored {
                entries: vec![ExploreEntry::Read(vec![extract_basename(path)])],
            }
        }
        "list_dir" => {
            let path = args["path"].as_str().unwrap_or(".");
            ToolDisplayCategory::Explored {
                entries: vec![ExploreEntry::List(path.to_string())],
            }
        }
        "run_shell" => {
            let command = args["command"].as_str().unwrap_or("?").to_string();
            ToolDisplayCategory::Ran { command }
        }
        "apply_patch" | "apply_skill_patch" | "create_channel" => {
            let patch = args["patch"].as_str().unwrap_or("");
            let files = extract_patch_filenames(patch);
            let count = files.len();
            ToolDisplayCategory::Edited { files, count }
        }
        "write_memory" => {
            let target = args["filename"].as_str().unwrap_or("?").to_string();
            ToolDisplayCategory::WroteMemory { target }
        }
        "read_memory" => {
            let target = args["filename"].as_str().unwrap_or("?").to_string();
            ToolDisplayCategory::ReadMemory { target }
        }
        "memory_search" => {
            let query = args["query"].as_str().unwrap_or("?").to_string();
            ToolDisplayCategory::SearchedMemory { query }
        }
        "browser" => {
            let action = args["action"].as_str().unwrap_or("?").to_string();
            let url = args["url"].as_str().map(ToString::to_string);
            ToolDisplayCategory::Browsed { action, url }
        }
        "schedule" | "manage_tasks" | "manage_cron" => {
            let action = args["action"].as_str().unwrap_or("?").to_string();
            ToolDisplayCategory::Tasks { action }
        }
        "list_skills" => ToolDisplayCategory::Listed {
            what: "skills".to_string(),
        },
        "list_channels" => ToolDisplayCategory::Listed {
            what: "channels".to_string(),
        },
        _ => {
            let preview = truncate(args_json, 77);
            ToolDisplayCategory::Generic {
                name: name.to_string(),
                preview,
            }
        }
    }
}

/// Build styled spans for the ToolStart header line (after the bullet).
pub fn tool_header_spans(cat: &ToolDisplayCategory) -> Vec<Span<'static>> {
    let bold = theme::bold();
    let code = theme::code_style();
    let dim = theme::dim();

    match cat {
        ToolDisplayCategory::Explored { .. } => {
            vec![Span::styled("Explored", bold)]
        }
        ToolDisplayCategory::Ran { command } => {
            let cmd_display = truncate(command, 60);
            vec![
                Span::styled("Ran", bold),
                Span::styled(format!(" `{cmd_display}`"), code),
            ]
        }
        ToolDisplayCategory::Edited { .. } => {
            vec![Span::styled("Edited", bold)]
        }
        ToolDisplayCategory::WroteMemory { target } => {
            vec![
                Span::styled("Wrote memory", bold),
                Span::styled(format!(" {target}"), dim),
            ]
        }
        ToolDisplayCategory::ReadMemory { target } => {
            vec![
                Span::styled("Read memory", bold),
                Span::styled(format!(" {target}"), dim),
            ]
        }
        ToolDisplayCategory::SearchedMemory { query } => {
            let q = truncate(query, 40);
            vec![
                Span::styled("Searched memory", bold),
                Span::styled(format!(" \"{q}\""), dim),
            ]
        }
        ToolDisplayCategory::Browsed { action, url } => {
            let mut spans = vec![
                Span::styled("Browsed", bold),
                Span::styled(format!(" {action}"), dim),
            ];
            if let Some(u) = url {
                let u = truncate(u, 50);
                spans.push(Span::styled(format!(" {u}"), code));
            }
            spans
        }
        ToolDisplayCategory::Tasks { action } => {
            vec![
                Span::styled("Tasks", bold),
                Span::styled(format!(" {action}"), dim),
            ]
        }
        ToolDisplayCategory::Listed { what } => {
            vec![Span::styled(format!("Listed {what}"), bold)]
        }
        ToolDisplayCategory::Generic { name, preview } => {
            let name_style = theme::code_style().add_modifier(ratatui::style::Modifier::BOLD);
            let mut spans = vec![Span::styled(name.clone(), name_style)];
            if !preview.is_empty() {
                spans.push(Span::styled(format!(" {preview}"), dim));
            }
            spans
        }
    }
}

/// Build the detail sub-line content (shown with └ prefix). Returns None if no detail.
pub fn tool_detail_line(cat: &ToolDisplayCategory) -> Option<Vec<Span<'static>>> {
    let dim = theme::dim();
    let cyan = theme::code_style();

    match cat {
        ToolDisplayCategory::Explored { entries } => {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled("  ", dim));
                }
                match entry {
                    ExploreEntry::Read(files) => {
                        spans.push(Span::styled("Read ", cyan));
                        let joined = files.join(", ");
                        spans.push(Span::styled(joined, dim));
                    }
                    ExploreEntry::List(path) => {
                        spans.push(Span::styled("List ", cyan));
                        spans.push(Span::styled(truncate_path(path, 60), dim));
                    }
                }
            }
            Some(spans)
        }
        ToolDisplayCategory::Edited { files, count } => {
            if *count == 0 {
                return None;
            }
            // Per-file center-truncate so deep paths still expose the
            // filename — `crates/.../foo.rs` instead of `crates/core/...`.
            let per_file_max = if *count == 1 { 64 } else { 32 };
            let names = files
                .iter()
                .map(|f| truncate_path(f, per_file_max))
                .collect::<Vec<_>>()
                .join(", ");
            Some(vec![Span::styled(
                format!("{names} ({count} file(s))"),
                dim,
            )])
        }
        ToolDisplayCategory::Browsed {
            url: Some(url),
            action,
            ..
        } if action == "navigate" => {
            let u = truncate(url, 80);
            Some(vec![Span::styled(u, dim)])
        }
        // `$ command` echo for run_shell. The magenta `$` reads as a shell
        // prompt; the command body is dim so the focus is the command
        // itself, not the leading prompt char.
        ToolDisplayCategory::Ran { command } => {
            let cmd = truncate(command, 120);
            Some(vec![
                Span::styled("$ ", theme::shell_prompt_style()),
                Span::styled(cmd, dim),
            ])
        }
        // Listed and most others don't have a detail line from ToolStart
        _ => None,
    }
}

/// Build the label used in the ToolResult status line.
pub fn tool_result_label(cat: &ToolDisplayCategory) -> String {
    match cat {
        ToolDisplayCategory::Explored { .. } => "Explored".to_string(),
        ToolDisplayCategory::Ran { command } => {
            let cmd = truncate(command, 40);
            format!("Ran `{cmd}`")
        }
        ToolDisplayCategory::Edited { .. } => "Edited".to_string(),
        ToolDisplayCategory::WroteMemory { target } => format!("Wrote {target}"),
        ToolDisplayCategory::ReadMemory { target } => format!("Read {target}"),
        ToolDisplayCategory::SearchedMemory { .. } => "Searched memory".to_string(),
        ToolDisplayCategory::Browsed { action, .. } => format!("Browsed {action}"),
        ToolDisplayCategory::Tasks { action } => format!("Tasks {action}"),
        ToolDisplayCategory::Listed { what } => format!("Listed {what}"),
        ToolDisplayCategory::Generic { name, .. } => format!("Ran {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- extract_basename --

    /// `extract_basename` is plain `rsplit('/').next()` — it must return the
    /// trailing path segment for relative, absolute, and slashless inputs,
    /// and intentionally returns "" for a trailing-slash path so the caller
    /// can distinguish "directory" from "file".
    #[test]
    fn basename_handles_all_path_shapes() {
        let cases: &[(&str, &str)] = &[
            ("src/main.rs", "main.rs"),
            ("/home/user/project/lib.rs", "lib.rs"),
            ("file.txt", "file.txt"),
            ("dir/", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(extract_basename(input), *expected, "input: {input:?}");
        }
    }

    // -- extract_patch_filenames --

    #[test]
    fn patch_filenames_extracts_add_update_delete_in_order() {
        let patch = "*** Begin Patch\n*** Add File: foo.rs\n+content\n*** Update File: bar.rs\n@@\n*** Delete File: old.rs\n*** End Patch";
        let files = extract_patch_filenames(patch);
        assert_eq!(files, vec!["foo.rs", "bar.rs", "old.rs"]);
    }

    #[test]
    fn patch_filenames_empty_patch_returns_empty() {
        let files = extract_patch_filenames("*** Begin Patch\n*** End Patch");
        assert!(files.is_empty());
    }

    #[test]
    fn patch_filenames_preserves_full_paths_not_basenames() {
        // Filenames must keep their directory prefix — they're shown verbatim
        // in the tool-display, and truncation is later/elsewhere.
        let patch = "*** Add File: tools/my-tool/main.py\n*** Update File: tools/my-tool/tool.toml";
        assert_eq!(
            extract_patch_filenames(patch),
            vec!["tools/my-tool/main.py", "tools/my-tool/tool.toml"]
        );
    }

    // -- classify_tool --

    /// `classify_tool` is the one-stop mapper from `(tool_name, args_json)`
    /// to the user-facing display category. Each row exercises a distinct
    /// per-tool branch and the field the renderer keys on.
    #[test]
    fn classify_tool_maps_each_tool_to_its_display_category() {
        use ToolDisplayCategory::*;

        let cases: &[(&str, &str, ToolDisplayCategory)] = &[
            (
                "read_file",
                r#"{"path":"src/main.rs"}"#,
                Explored {
                    entries: vec![ExploreEntry::Read(vec!["main.rs".to_string()])],
                },
            ),
            (
                // Absolute path: basename only is surfaced.
                "read_file",
                r#"{"path":"/home/user/project/lib.rs"}"#,
                Explored {
                    entries: vec![ExploreEntry::Read(vec!["lib.rs".to_string()])],
                },
            ),
            (
                "list_dir",
                r#"{"path":"src"}"#,
                Explored {
                    entries: vec![ExploreEntry::List("src".to_string())],
                },
            ),
            (
                // Default-path branch: absent `path` becomes `.`.
                "list_dir",
                r#"{}"#,
                Explored {
                    entries: vec![ExploreEntry::List(".".to_string())],
                },
            ),
            (
                "run_shell",
                r#"{"command":"ls -la"}"#,
                Ran {
                    command: "ls -la".to_string(),
                },
            ),
            (
                "write_memory",
                r#"{"filename":"MEMORY.md","content":"stuff"}"#,
                WroteMemory {
                    target: "MEMORY.md".to_string(),
                },
            ),
            (
                "read_memory",
                r#"{"filename":"notes.md"}"#,
                ReadMemory {
                    target: "notes.md".to_string(),
                },
            ),
            (
                "browser",
                r#"{"action":"navigate","url":"https://example.com"}"#,
                Browsed {
                    action: "navigate".to_string(),
                    url: Some("https://example.com".to_string()),
                },
            ),
            (
                "schedule",
                r#"{"action":"list"}"#,
                Tasks {
                    action: "list".to_string(),
                },
            ),
            (
                "list_skills",
                r#"{}"#,
                Listed {
                    what: "skills".to_string(),
                },
            ),
            (
                "list_channels",
                r#"{}"#,
                Listed {
                    what: "channels".to_string(),
                },
            ),
        ];
        for (tool, args, expected) in cases {
            assert_eq!(
                classify_tool(tool, args),
                *expected,
                "tool={tool} args={args}"
            );
        }
    }

    /// `apply_patch` is special-cased: the patch DSL is parsed and the
    /// affected filenames + count are surfaced, so a multi-file patch shows
    /// "Edited 2 files" not "Ran apply_patch".
    #[test]
    fn classify_apply_patch_extracts_filenames_and_count() {
        let patch = r#"{"patch":"*** Begin Patch\n*** Add File: foo.rs\n+hello\n*** Update File: bar.rs\n@@\n*** End Patch"}"#;
        let cat = classify_tool("apply_patch", patch);
        if let ToolDisplayCategory::Edited { files, count } = &cat {
            assert_eq!(*count, 2);
            assert_eq!(files, &["foo.rs", "bar.rs"]);
        } else {
            panic!("expected Edited category, got {:?}", cat);
        }

        // Empty patch is still classified as Edited (preserving the bullet
        // header), with count=0 so the renderer can skip the file list.
        let empty = classify_tool(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** End Patch"}"#,
        );
        if let ToolDisplayCategory::Edited { count, files } = &empty {
            assert_eq!(*count, 0);
            assert!(files.is_empty());
        } else {
            panic!("expected Edited category for empty patch");
        }
    }

    /// Unknown tools and malformed JSON both fall through to `Generic` so
    /// the renderer never crashes on a stranger; the tool name is preserved
    /// so the user still sees what ran.
    #[test]
    fn classify_unknown_and_invalid_json_fall_through_to_generic() {
        let unknown = classify_tool("my_custom_tool", r#"{"x":1}"#);
        if let ToolDisplayCategory::Generic { name, .. } = &unknown {
            assert_eq!(name, "my_custom_tool");
        } else {
            panic!("expected Generic, got {unknown:?}");
        }

        // Invalid JSON for a *known* tool also fails over to Generic instead
        // of panicking — this is the regression-guard for the rendering path.
        let bad_json = classify_tool("read_file", "not valid json");
        assert!(matches!(bad_json, ToolDisplayCategory::Generic { .. }));
    }

    // -- tool_header_spans --

    /// One-line headers shown on the bullet row. Each category contributes a
    /// distinct verb/format; this table is the source of truth for the
    /// rendered string and guards against header drift.
    #[test]
    fn tool_header_spans_render_per_category_verb() {
        let cases: Vec<(ToolDisplayCategory, &str)> = vec![
            (
                ToolDisplayCategory::Explored {
                    entries: vec![ExploreEntry::Read(vec!["main.rs".to_string()])],
                },
                "Explored",
            ),
            (
                ToolDisplayCategory::Ran {
                    command: "cargo test".to_string(),
                },
                "Ran `cargo test`",
            ),
            (
                ToolDisplayCategory::Edited {
                    files: vec!["a.rs".to_string()],
                    count: 1,
                },
                "Edited",
            ),
            (
                ToolDisplayCategory::Listed {
                    what: "tools".to_string(),
                },
                "Listed tools",
            ),
            (
                ToolDisplayCategory::Generic {
                    name: "my_tool".to_string(),
                    preview: "args".to_string(),
                },
                "my_tool args",
            ),
        ];
        for (cat, expected) in &cases {
            let spans = tool_header_spans(cat);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(text, *expected, "category: {cat:?}");
        }
    }

    // -- tool_detail_line --

    #[test]
    fn detail_explored_read() {
        let cat = ToolDisplayCategory::Explored {
            entries: vec![ExploreEntry::Read(vec![
                "main.rs".to_string(),
                "lib.rs".to_string(),
            ])],
        };
        let spans = tool_detail_line(&cat).expect("should have detail");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Read main.rs, lib.rs");
    }

    #[test]
    fn detail_explored_list() {
        let cat = ToolDisplayCategory::Explored {
            entries: vec![ExploreEntry::List("src".to_string())],
        };
        let spans = tool_detail_line(&cat).expect("should have detail");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "List src");
    }

    #[test]
    fn detail_edited() {
        let cat = ToolDisplayCategory::Edited {
            files: vec!["a.rs".to_string(), "b.rs".to_string()],
            count: 2,
        };
        let spans = tool_detail_line(&cat).expect("should have detail");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a.rs, b.rs (2 file(s))");
    }

    #[test]
    fn detail_edited_empty() {
        let cat = ToolDisplayCategory::Edited {
            files: vec![],
            count: 0,
        };
        assert!(tool_detail_line(&cat).is_none());
    }

    #[test]
    fn detail_ran_emits_shell_prompt() {
        // run_shell tool start emits a `$ <cmd>` echo line under the bullet
        // header so the user sees what's about to run.
        let cat = ToolDisplayCategory::Ran {
            command: "ls -la".to_string(),
        };
        let spans = tool_detail_line(&cat).expect("Ran emits a detail line");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with("$ ") && text.contains("ls -la"),
            "got: {text:?}"
        );
        // Magenta `$` prompt followed by dim command body.
        assert_eq!(spans[0].style, theme::shell_prompt_style());
    }

    #[test]
    fn detail_listed_none() {
        let cat = ToolDisplayCategory::Listed {
            what: "tools".to_string(),
        };
        assert!(tool_detail_line(&cat).is_none());
    }

    // -- tool_result_label --

    /// `tool_result_label` is the after-the-fact summary line. Generic falls
    /// back to "Ran <name>" (note the verb change vs. the live header).
    #[test]
    fn tool_result_label_per_category() {
        let cases: Vec<(ToolDisplayCategory, &str)> = vec![
            (
                ToolDisplayCategory::Explored { entries: vec![] },
                "Explored",
            ),
            (
                ToolDisplayCategory::Ran {
                    command: "cargo build".to_string(),
                },
                "Ran `cargo build`",
            ),
            (
                ToolDisplayCategory::Edited {
                    files: vec![],
                    count: 0,
                },
                "Edited",
            ),
            (
                ToolDisplayCategory::Generic {
                    name: "custom".to_string(),
                    preview: String::new(),
                },
                "Ran custom",
            ),
        ];
        for (cat, expected) in &cases {
            assert_eq!(tool_result_label(cat), *expected, "category: {cat:?}");
        }
    }

    // -- truncate --

    /// `truncate` is the under/at-limit pass-through plus the over-limit
    /// ellipsis case. The char-count cap is non-negotiable — a wider string
    /// would overflow the bullet line.
    #[test]
    fn truncate_passes_through_or_ellipsizes() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello"); // exact fit
        let result = truncate("a]very long string here", 10);
        assert!(result.ends_with("..."), "got: {result}");
        assert!(
            result.chars().count() <= 10,
            "got len: {}",
            result.chars().count()
        );
    }

    // -- truncate_path --

    #[test]
    fn truncate_path_short_unchanged() {
        assert_eq!(truncate_path("src/main.rs", 40), "src/main.rs");
    }

    #[test]
    fn truncate_path_preserves_filename() {
        let p = "crates/cli/src/tui/tool_display.rs";
        let out = truncate_path(p, 25);
        assert!(out.ends_with("tool_display.rs"), "got: {out}");
        assert!(out.contains("/…/"), "should mark elided middle, got: {out}");
        assert!(
            out.chars().count() <= 25,
            "got len {}: {out}",
            out.chars().count()
        );
    }

    #[test]
    fn truncate_path_falls_back_when_filename_too_long() {
        // Filename alone (`extremely_long_filename.rs` = 26 chars) doesn't fit
        // in max=15; fall back to end-truncate so we at least preserve the
        // start of the original string.
        let out = truncate_path("a/b/extremely_long_filename.rs", 15);
        assert!(out.ends_with("..."), "should end-truncate, got: {out}");
        assert_eq!(out.chars().count(), 15);
    }

    #[test]
    fn truncate_path_no_slash_falls_back() {
        let out = truncate_path("just_a_file_name_with_no_slash.rs", 12);
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), 12);
    }

    #[test]
    fn truncate_path_keeps_at_least_one_leading_segment() {
        // 60 chars budget should fit 'crates' + '/…/' + 'tool_display.rs' (25)
        let p = "crates/cli/src/tui/sub/deeper/tool_display.rs";
        let out = truncate_path(p, 30);
        assert!(out.ends_with("tool_display.rs"));
        assert!(
            out.starts_with("crates"),
            "leading segment kept, got: {out}"
        );
    }
}
