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

/// Truncate a string to at most `max` characters, appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

/// Truncate a `/`-separated path so the trailing segment (filename) stays
/// intact and as many leading segments as fit are retained, joined to the
/// tail by `/…/`. Falls back to end-truncate for non-path strings or when a
/// single segment alone exceeds `max`.
///
/// Inspired by codex `text_formatting::center_truncate_path` — preserves the
/// part of the path the user actually identifies the file by.
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
        // Ran, Listed, and most others don't have a detail line from ToolStart
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

    #[test]
    fn basename_simple() {
        assert_eq!(extract_basename("src/main.rs"), "main.rs");
    }

    #[test]
    fn basename_absolute() {
        assert_eq!(extract_basename("/home/user/project/lib.rs"), "lib.rs");
    }

    #[test]
    fn basename_no_slash() {
        assert_eq!(extract_basename("file.txt"), "file.txt");
    }

    #[test]
    fn basename_trailing_slash() {
        assert_eq!(extract_basename("dir/"), "");
    }

    // -- extract_patch_filenames --

    #[test]
    fn patch_filenames_mixed() {
        let patch = "*** Begin Patch\n*** Add File: foo.rs\n+content\n*** Update File: bar.rs\n@@\n*** Delete File: old.rs\n*** End Patch";
        let files = extract_patch_filenames(patch);
        assert_eq!(files, vec!["foo.rs", "bar.rs", "old.rs"]);
    }

    #[test]
    fn patch_filenames_empty() {
        let patch = "*** Begin Patch\n*** End Patch";
        let files = extract_patch_filenames(patch);
        assert!(files.is_empty());
    }

    #[test]
    fn patch_filenames_with_paths() {
        let patch = "*** Add File: tools/my-tool/main.py\n*** Update File: tools/my-tool/tool.toml";
        let files = extract_patch_filenames(patch);
        assert_eq!(
            files,
            vec!["tools/my-tool/main.py", "tools/my-tool/tool.toml"]
        );
    }

    // -- classify_tool --

    #[test]
    fn classify_read_file() {
        let cat = classify_tool("read_file", r#"{"path":"src/main.rs"}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Explored {
                entries: vec![ExploreEntry::Read(vec!["main.rs".to_string()])]
            }
        );
    }

    #[test]
    fn classify_read_file_absolute() {
        let cat = classify_tool("read_file", r#"{"path":"/home/user/project/lib.rs"}"#);
        if let ToolDisplayCategory::Explored { entries } = &cat {
            if let ExploreEntry::Read(files) = &entries[0] {
                assert_eq!(files[0], "lib.rs");
            } else {
                panic!("expected Read entry");
            }
        } else {
            panic!("expected Explored category");
        }
    }

    #[test]
    fn classify_list_dir() {
        let cat = classify_tool("list_dir", r#"{"path":"src"}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Explored {
                entries: vec![ExploreEntry::List("src".to_string())]
            }
        );
    }

    #[test]
    fn classify_list_dir_default() {
        let cat = classify_tool("list_dir", r#"{}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Explored {
                entries: vec![ExploreEntry::List(".".to_string())]
            }
        );
    }

    #[test]
    fn classify_run_shell() {
        let cat = classify_tool("run_shell", r#"{"command":"ls -la"}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Ran {
                command: "ls -la".to_string()
            }
        );
    }

    #[test]
    fn classify_apply_patch() {
        let patch = r#"{"patch":"*** Begin Patch\n*** Add File: foo.rs\n+hello\n*** Update File: bar.rs\n@@\n*** End Patch"}"#;
        let cat = classify_tool("apply_patch", patch);
        if let ToolDisplayCategory::Edited { files, count } = &cat {
            assert_eq!(*count, 2);
            assert_eq!(files, &["foo.rs", "bar.rs"]);
        } else {
            panic!("expected Edited category, got {:?}", cat);
        }
    }

    #[test]
    fn classify_apply_patch_empty() {
        let cat = classify_tool(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** End Patch"}"#,
        );
        if let ToolDisplayCategory::Edited { count, .. } = &cat {
            assert_eq!(*count, 0);
        } else {
            panic!("expected Edited category");
        }
    }

    #[test]
    fn classify_write_memory() {
        let cat = classify_tool(
            "write_memory",
            r#"{"filename":"MEMORY.md","content":"stuff"}"#,
        );
        assert_eq!(
            cat,
            ToolDisplayCategory::WroteMemory {
                target: "MEMORY.md".to_string()
            }
        );
    }

    #[test]
    fn classify_read_memory() {
        let cat = classify_tool("read_memory", r#"{"filename":"notes.md"}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::ReadMemory {
                target: "notes.md".to_string()
            }
        );
    }

    #[test]
    fn classify_browser() {
        let cat = classify_tool(
            "browser",
            r#"{"action":"navigate","url":"https://example.com"}"#,
        );
        assert_eq!(
            cat,
            ToolDisplayCategory::Browsed {
                action: "navigate".to_string(),
                url: Some("https://example.com".to_string())
            }
        );
    }

    #[test]
    fn classify_schedule() {
        let cat = classify_tool("schedule", r#"{"action":"list"}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Tasks {
                action: "list".to_string()
            }
        );
    }

    #[test]
    fn classify_list_skills() {
        let cat = classify_tool("list_skills", r#"{}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Listed {
                what: "skills".to_string()
            }
        );
    }

    #[test]
    fn classify_list_channels() {
        let cat = classify_tool("list_channels", r#"{}"#);
        assert_eq!(
            cat,
            ToolDisplayCategory::Listed {
                what: "channels".to_string()
            }
        );
    }

    #[test]
    fn classify_unknown() {
        let cat = classify_tool("my_custom_tool", r#"{"x":1}"#);
        if let ToolDisplayCategory::Generic { name, .. } = &cat {
            assert_eq!(name, "my_custom_tool");
        } else {
            panic!("expected Generic category");
        }
    }

    #[test]
    fn classify_invalid_json() {
        let cat = classify_tool("read_file", "not valid json");
        assert!(matches!(cat, ToolDisplayCategory::Generic { .. }));
    }

    // -- tool_header_spans --

    #[test]
    fn header_explored() {
        let cat = ToolDisplayCategory::Explored {
            entries: vec![ExploreEntry::Read(vec!["main.rs".to_string()])],
        };
        let spans = tool_header_spans(&cat);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Explored");
    }

    #[test]
    fn header_ran() {
        let cat = ToolDisplayCategory::Ran {
            command: "cargo test".to_string(),
        };
        let spans = tool_header_spans(&cat);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Ran `cargo test`");
    }

    #[test]
    fn header_edited() {
        let cat = ToolDisplayCategory::Edited {
            files: vec!["a.rs".to_string()],
            count: 1,
        };
        let spans = tool_header_spans(&cat);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Edited");
    }

    #[test]
    fn header_listed() {
        let cat = ToolDisplayCategory::Listed {
            what: "tools".to_string(),
        };
        let spans = tool_header_spans(&cat);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Listed tools");
    }

    #[test]
    fn header_generic() {
        let cat = ToolDisplayCategory::Generic {
            name: "my_tool".to_string(),
            preview: "args".to_string(),
        };
        let spans = tool_header_spans(&cat);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "my_tool args");
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
    fn detail_ran_none() {
        let cat = ToolDisplayCategory::Ran {
            command: "ls".to_string(),
        };
        assert!(tool_detail_line(&cat).is_none());
    }

    #[test]
    fn detail_listed_none() {
        let cat = ToolDisplayCategory::Listed {
            what: "tools".to_string(),
        };
        assert!(tool_detail_line(&cat).is_none());
    }

    // -- tool_result_label --

    #[test]
    fn result_label_explored() {
        let cat = ToolDisplayCategory::Explored { entries: vec![] };
        assert_eq!(tool_result_label(&cat), "Explored");
    }

    #[test]
    fn result_label_ran() {
        let cat = ToolDisplayCategory::Ran {
            command: "cargo build".to_string(),
        };
        assert_eq!(tool_result_label(&cat), "Ran `cargo build`");
    }

    #[test]
    fn result_label_edited() {
        let cat = ToolDisplayCategory::Edited {
            files: vec![],
            count: 0,
        };
        assert_eq!(tool_result_label(&cat), "Edited");
    }

    #[test]
    fn result_label_generic() {
        let cat = ToolDisplayCategory::Generic {
            name: "custom".to_string(),
            preview: String::new(),
        };
        assert_eq!(tool_result_label(&cat), "Ran custom");
    }

    // -- truncate --

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        let result = truncate("a]very long string here", 10);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 10);
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
