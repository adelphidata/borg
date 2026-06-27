use super::*;
use ratatui::style::Style;

/// `@mention` recognition: only `@` at start-of-line or after whitespace
/// counts; in-word `@` (emails) doesn't. Bare `@` followed by space is
/// not a mention. The mention span gets the `mention` style; everything
/// else uses `normal`.
#[test]
fn parse_at_mentions_recognizes_only_word_boundary_at() {
    let normal = Style::default();
    let mention = Style::default().add_modifier(ratatui::style::Modifier::BOLD);

    // (input, expected span texts, expected mention indices)
    let cases: &[(&str, &[&str], &[usize])] = &[
        (
            "hello @file.rs world",
            &["hello ", "@file.rs", " world"],
            &[1],
        ),
        ("no mentions here", &["no mentions here"], &[]),
        ("user@example.com", &["user@", "example.com"], &[]),
        ("@path rest", &["@path", " rest"], &[0]),
        ("@ alone", &["@", " alone"], &[]),
    ];

    for (input, expected_texts, mention_indices) in cases {
        let spans = parse_at_mentions(input, normal, mention);
        let actual: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(&actual[..], *expected_texts, "input: {input:?}");
        for (i, span) in spans.iter().enumerate() {
            let is_mention = mention_indices.contains(&i);
            let expected_style = if is_mention { mention } else { normal };
            assert_eq!(
                span.style, expected_style,
                "input: {input:?} span {i} ({:?})",
                span.content
            );
        }
    }
}

#[test]
fn render_user_cell() {
    let cell = HistoryCell::User {
        text: "hello".to_string(),
    };
    let lines = cell.render(40, None);
    assert_eq!(lines.len(), 3);
    let text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("hello"));
    assert!(text.contains(super::theme::CHEVRON));
}

#[test]
fn render_user_cell_padded_box() {
    let cell = HistoryCell::User {
        text: "hello world".to_string(),
    };
    let lines = cell.render(80, None);
    // The chevron should appear on the content line (after the unstyled spacer).
    let chevron_line_idx = lines.iter().position(|l| {
        l.spans
            .iter()
            .any(|s| s.content.as_ref().contains(super::theme::CHEVRON))
    });
    assert!(chevron_line_idx.is_some(), "chevron must be present");
    let idx = chevron_line_idx.unwrap();
    assert_eq!(idx, 1, "chevron should be on the middle filled line");
}

#[test]
fn render_user_cell_wraps_long_line_with_hanging_indent() {
    // At width 30, a long single-line user message must wrap; continuation
    // lines start with the 2-space bg-styled prefix (under the chevron's
    // content column) instead of overflowing the terminal.
    let cell = HistoryCell::User {
        text: "alpha beta gamma delta epsilon zeta eta theta iota".to_string(),
    };
    let lines = cell.render(30, None);
    // Find first line with chevron and first continuation after it.
    let chev_idx = lines
        .iter()
        .position(|l| {
            l.spans
                .iter()
                .any(|s| s.content.as_ref().contains(super::theme::CHEVRON))
        })
        .expect("chevron line");
    assert!(lines.len() > chev_idx + 1, "expected wrap, got {:?}", lines);
    let cont = &lines[chev_idx + 1];
    assert_eq!(cont.spans[0].content.as_ref(), "  ");
    assert!(!cont
        .spans
        .iter()
        .any(|s| s.content.as_ref().contains(super::theme::CHEVRON)));
}

#[test]
fn render_user_cell_paints_full_interior_width() {
    let cell = HistoryCell::User {
        text: "hello?".to_string(),
    };
    let width = 40;
    let lines = cell.render(width, None);
    let row = &lines[1];
    assert_eq!(row.style.bg, theme::user_message_style().bg);
    let rendered_width: usize = row
        .spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    assert_eq!(rendered_width, width as usize);
    if let Some(bubble_bg) = theme::user_message_style().bg {
        let fill_width: usize = row
            .spans
            .iter()
            .filter(|span| span.style.bg == Some(bubble_bg))
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        assert_eq!(fill_width, width as usize);
    }
}

#[test]
fn render_assistant_streaming() {
    let cell = HistoryCell::assistant("partial".to_string(), true);
    let lines = cell.render(40, None);
    let rendered = flatten_plain(&lines);
    assert!(rendered.contains("partial"));
    assert!(!rendered.contains('▊'));
}

#[test]
fn render_assistant_not_streaming() {
    let cell = HistoryCell::assistant("done".to_string(), false);
    let lines = cell.render(40, None);
    // No trailing blank line; last line should contain the rendered text.
    let last = &lines[lines.len() - 1];
    let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("done"),
        "last line should contain rendered text, got: {text:?}"
    );
}

#[test]
fn user_cell_does_not_emit_leading_blank() {
    let cell = HistoryCell::User {
        text: "hello".to_string(),
    };
    let lines = cell.render(40, None);
    assert!(!lines.is_empty(), "User render produced no lines");
    let first = &lines[1];
    assert!(
        !first.spans.is_empty(),
        "content User line is unexpectedly blank: {first:?}"
    );
    let text: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("hello"),
        "content User line should contain content, got: {text:?}"
    );
}

#[test]
fn tool_flow_classification() {
    // The render loop hides the inter-cell spacer when both neighbors
    // are tool-flow cells, so back-to-back command runs hug. Lock the
    // classification down so adding a future variant doesn't silently
    // break the spacing rule.
    let tool_start = HistoryCell::ToolStart {
        name: "run_shell".to_string(),
        args: "{}".to_string(),
        completed: false,
        start_time: None,
    };
    let tool_result = HistoryCell::ToolResult {
        output: "ok".to_string(),
        is_error: false,
        duration_ms: Some(10),
        display_label: "run_shell".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    let tool_streaming = HistoryCell::ToolStreaming { lines: vec![] };
    assert!(tool_start.is_tool_flow());
    assert!(tool_result.is_tool_flow());
    assert!(tool_streaming.is_tool_flow());

    let assistant = HistoryCell::assistant("hi".to_string(), false);
    let user = HistoryCell::User {
        text: "hey".to_string(),
    };
    assert!(!assistant.is_tool_flow());
    assert!(!user.is_tool_flow());
}

#[test]
fn stream_continuation_classification() {
    let tool_start = HistoryCell::ToolStart {
        name: "run_shell".to_string(),
        args: "{}".to_string(),
        completed: false,
        start_time: None,
    };
    assert!(!tool_start.is_stream_continuation());

    let tool_result = HistoryCell::ToolResult {
        output: "ok".to_string(),
        is_error: false,
        duration_ms: Some(10),
        display_label: "run_shell".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    assert!(tool_result.is_stream_continuation());

    let tool_streaming = HistoryCell::ToolStreaming { lines: vec![] };
    assert!(tool_streaming.is_stream_continuation());

    let assistant = HistoryCell::assistant("hi".to_string(), false);
    assert!(!assistant.is_stream_continuation());

    let user = HistoryCell::User {
        text: "hey".to_string(),
    };
    assert!(!user.is_stream_continuation());
}

#[test]
fn render_tool_start_patch_summary() {
    let cell = HistoryCell::ToolStart {
        name: "apply_patch".to_string(),
        args: r#"{"patch":"*** Begin Patch\n*** Add File: a.rs\n+x\n*** Update File: b.rs\n@@\n*** End Patch"}"#.to_string(),
        completed: true,
        start_time: None,
    };
    let lines = cell.render(80, None);
    let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        header.contains("Edited"),
        "header should say Edited, got: {header}"
    );
    // Detail line should show files
    assert!(lines.len() >= 2, "should have detail line");
    let detail: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        detail.contains("a.rs"),
        "detail should contain filenames, got: {detail}"
    );
    assert!(
        detail.contains("2 file(s)"),
        "detail should show count, got: {detail}"
    );
}

#[test]
fn render_tool_start_run_shell() {
    let cell = HistoryCell::ToolStart {
        name: "run_shell".to_string(),
        args: r#"{"command":"cargo test --all"}"#.to_string(),
        completed: false,
        start_time: None,
    };
    let lines = cell.render(80, None);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("Ran"), "should say Ran, got: {text}");
    assert!(
        text.contains("cargo test --all"),
        "should show command, got: {text}"
    );
}

#[test]
fn render_tool_start_read_file() {
    let cell = HistoryCell::ToolStart {
        name: "read_file".to_string(),
        args: r#"{"path":"src/main.rs"}"#.to_string(),
        completed: true,
        start_time: None,
    };
    let lines = cell.render(80, None);
    let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        header.contains("Explored"),
        "should say Explored, got: {header}"
    );
    assert!(lines.len() >= 2, "should have detail line");
    let detail: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(detail.contains("Read"), "detail should have Read label");
    assert!(detail.contains("main.rs"), "detail should have basename");
}

#[test]
fn render_tool_start_generic_truncates() {
    let cell = HistoryCell::ToolStart {
        name: "custom_tool".to_string(),
        args: format!(r#"{{"data":"{}"}}"#, "a".repeat(100)),
        completed: false,
        start_time: None,
    };
    let lines = cell.render(80, None);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("custom_tool"),
        "should show tool name, got: {text}"
    );
    assert!(
        text.contains("..."),
        "should truncate long args, got: {text}"
    );
}

#[test]
fn render_tool_result_collapsed_shows_footer() {
    let output = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cell = HistoryCell::ToolResult {
        output,
        is_error: false,
        duration_ms: Some(1500),
        display_label: "Ran test".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: true,
    };
    let lines = cell.render(80, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // 20 lines total, 8 preview → 12 hidden. The footer copy points
    // users at the transcript pager (Ctrl+T).
    assert!(all_text.contains("+12 lines"), "got: {all_text}");
    assert!(
        all_text.contains("ctrl + t"),
        "footer should point at transcript pager: {all_text}"
    );
    assert!(all_text.contains("1.5s"));
}

#[test]
fn render_tool_result_expanded_shows_all_lines() {
    let output = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cell = HistoryCell::ToolResult {
        output,
        is_error: false,
        duration_ms: Some(1500),
        display_label: "Ran test".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    let lines = cell.render(80, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !all_text.contains("ctrl + t to view transcript"),
        "should not collapse"
    );
    // All 20 lines should appear
    assert!(all_text.contains("line 0"));
    assert!(all_text.contains("line 19"));
}

#[test]
fn render_tool_result_short_output_not_collapsed() {
    // Under threshold — no footer even with collapsed=true
    let cell = HistoryCell::ToolResult {
        output: "one\ntwo\nthree".to_string(),
        is_error: false,
        duration_ms: Some(100),
        display_label: "Ran".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: true,
    };
    let lines = cell.render(80, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(!all_text.contains("ctrl + t to view transcript"));
}

#[test]
fn toggle_collapsed_flips_state() {
    let mut cell = HistoryCell::ToolResult {
        output: (0..20)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        is_error: false,
        duration_ms: None,
        display_label: "ran".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: true,
    };
    assert!(cell.toggle_collapsed());
    // Should no longer show the collapse footer
    let all_text: String = cell
        .render(80, None)
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(!all_text.contains("ctrl + t to view transcript"));
    // Toggle back
    assert!(cell.toggle_collapsed());
    let all_text2: String = cell
        .render(80, None)
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(all_text2.contains("ctrl + t to view transcript"));
}

#[test]
fn toggle_collapsed_noop_on_non_result() {
    let mut cell = HistoryCell::Separator { label: None };
    assert!(!cell.toggle_collapsed());
}

#[test]
fn is_collapsible_result_true_when_over_threshold() {
    let cell = HistoryCell::ToolResult {
        output: (0..20)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        is_error: false,
        duration_ms: None,
        display_label: "ran".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: true,
    };
    assert!(cell.is_collapsible_result());
}

#[test]
fn is_collapsible_result_false_when_under_threshold() {
    let cell = HistoryCell::ToolResult {
        output: "one\ntwo".to_string(),
        is_error: false,
        duration_ms: None,
        display_label: "ran".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    assert!(!cell.is_collapsible_result());
}

#[test]
fn render_tool_result_apply_patch_uses_diff() {
    let patch = "*** Begin Patch\n*** Add File: x.rs\n+fn foo() {}\n*** End Patch";
    let args_json = serde_json::json!({ "patch": patch }).to_string();
    let cell = HistoryCell::ToolResult {
        output: "Patch applied successfully.".to_string(),
        is_error: false,
        duration_ms: Some(50),
        display_label: "Edited 1 file".to_string(),
        tool_name: "apply_patch".to_string(),
        args_json: Some(args_json),
        collapsed: false,
    };
    let lines = cell.render(80, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // Diff header + body rendered from args, NOT the output text
    assert!(
        all_text.contains("Add:"),
        "expected diff header in: {all_text}"
    );
    assert!(all_text.contains("fn foo()"));
}

#[test]
fn render_tool_result_apply_patch_error_falls_back_to_output() {
    let cell = HistoryCell::ToolResult {
        output: "Error: patch rejected at line 3".to_string(),
        is_error: true,
        duration_ms: Some(10),
        display_label: "Edited".to_string(),
        tool_name: "apply_patch".to_string(),
        args_json: Some(r#"{"patch":"*** Begin Patch\n*** End Patch"}"#.to_string()),
        collapsed: false,
    };
    let all_text: String = cell
        .render(80, None)
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // Error should keep the raw output visible, not swap for an empty diff
    assert!(all_text.contains("Error: patch rejected"));
}

#[test]
fn render_tool_result_uses_display_label() {
    let cell = HistoryCell::ToolResult {
        output: "ok".to_string(),
        is_error: false,
        duration_ms: Some(200),
        display_label: "Ran `ls`".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    let lines = cell.render(80, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        all_text.contains("Ran `ls`"),
        "should use display_label, got: {all_text}"
    );
}

#[test]
fn render_separator() {
    let cell = HistoryCell::Separator { label: None };
    let lines = cell.render(60, None);
    // Only the rule line; surrounding spacing is handled by the render loop.
    assert_eq!(lines.len(), 1);
    // The unlabeled rule fills the full viewport width.
    let rule_w: usize = lines[0]
        .spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    assert_eq!(rule_w, 60, "unlabeled separator must span the full width");
}

#[test]
fn render_labeled_separator_rule_format() {
    let cell = HistoryCell::Separator {
        label: Some("Worked for 2m • Local tools: 3 calls".to_string()),
    };
    let lines = cell.render(60, None);
    assert_eq!(lines.len(), 1);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    // Labeled format: `─ {label} ─` then fill to the full width.
    assert!(
        text.starts_with("─ Worked for 2m • Local tools: 3 calls ─"),
        "labeled separator must use the `─ label ─` form: {text}"
    );
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(text.as_str()),
        60,
        "labeled separator must still fill the full width"
    );
}

#[test]
fn labeled_separator_falls_back_to_plain_rule_when_too_narrow() {
    // A label wider than the viewport must not overflow/truncate — the divider
    // degrades to a plain full-width rule instead.
    let cell = HistoryCell::Separator {
        label: Some("Worked for 2m 3s • Local tools: 12 calls".to_string()),
    };
    let lines = cell.render(20, None);
    assert_eq!(lines.len(), 1);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        !text.contains("Worked for"),
        "narrow divider must drop the label, got: {text}"
    );
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(text.as_str()),
        20,
        "fallback rule still spans the full width"
    );
    assert!(
        text.chars().all(|c| c.to_string() == theme::SEPARATOR),
        "fallback must be a pure rule of separator glyphs"
    );
}

#[test]
fn turn_separator_label_policy() {
    let label = |c, s| match HistoryCell::turn_separator(c, s) {
        HistoryCell::Separator { label } => label,
        _ => panic!("turn_separator must produce a Separator cell"),
    };
    // Elapsed only counts when over a minute; under the threshold it is dropped.
    assert_eq!(
        label(0, Some(45)),
        None,
        "no work + short turn → plain rule"
    );
    assert_eq!(label(0, Some(90)), Some("Worked for 1m 30s".to_string()));
    // Tool count uses singular/plural and combines with notable elapsed.
    assert_eq!(label(1, Some(10)), Some("Local tools: 1 call".to_string()));
    assert_eq!(
        label(3, Some(125)),
        Some("Worked for 2m 5s • Local tools: 3 calls".to_string())
    );
}

#[test]
fn render_thinking_empty() {
    let cell = HistoryCell::Thinking {
        text: String::new(),
    };
    let lines = cell.render(40, None);
    assert!(lines.is_empty());
}

#[test]
fn render_thinking_content() {
    let cell = HistoryCell::Thinking {
        text: "pondering".to_string(),
    };
    let lines = cell.render(40, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(all_text.contains("thinking"));
    assert!(all_text.contains("pondering"));
}

#[test]
fn render_thinking_is_bullet_style_not_boxed() {
    // Dim+italic bullet block, NOT a bordered box. Regression
    // guard against the old `render_boxed_lines` rendering. Asserts the
    // header reads `• thinking` and no box-drawing characters appear.
    let cell = HistoryCell::Thinking {
        text: "step one".to_string(),
    };
    let lines = cell.render(40, None);
    let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(header_text.starts_with(theme::BULLET));
    assert!(header_text.contains("thinking"));
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    for forbidden in ['│', '└', '╭', '╮', '╰', '╯'] {
        assert!(
            !all_text.contains(forbidden),
            "box-drawing char {forbidden:?} leaked into thinking render"
        );
    }
}

#[test]
fn render_heartbeat() {
    let cell = HistoryCell::Heartbeat {
        text: "check-in".to_string(),
    };
    let lines = cell.render(40, None);
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("[heartbeat]"));
    assert!(text.contains("check-in"));
}

#[test]
fn heartbeat_inline_backticks_styled_teal() {
    let cell = HistoryCell::Heartbeat {
        text: "run `/plugins` to add an integration".to_string(),
    };
    let lines = cell.render(80, None);
    let code_span = lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "`/plugins`")
        .expect("inline-code span present with backticks intact");
    assert_eq!(code_span.style, theme::code_style());
}

#[test]
fn style_inline_backticks_unmatched_trailing_tick() {
    let spans = style_inline_backticks("foo `bar baz", Style::default());
    let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(combined, "foo `bar baz");
    // unmatched run keeps the leading backtick but isn't styled as code.
    assert!(spans.iter().all(|s| s.style != theme::code_style()));
}

#[test]
fn render_plan_cell_with_steps() {
    let cell = HistoryCell::Plan {
        steps: vec![
            PlanStep {
                title: "Read files".into(),
                status: PlanStepStatus::Completed,
            },
            PlanStep {
                title: "Write code".into(),
                status: PlanStepStatus::InProgress,
            },
            PlanStep {
                title: "Run tests".into(),
                status: PlanStepStatus::Pending,
            },
        ],
    };
    let lines = cell.render(80, None);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(all_text.contains("Updated Plan"));
    assert!(all_text.contains("Read files"));
    assert!(all_text.contains("Write code"));
    assert!(all_text.contains("Run tests"));
    // Step markers: ✔ for completed, □ for in-progress/pending,
    // with the block carrying the `└` tree prefix on its first row.
    assert!(all_text.contains('✔'), "completed step uses ✔: {all_text}");
    assert!(
        all_text.contains('□'),
        "in-progress/pending steps use □: {all_text}"
    );
    assert!(
        all_text.contains('└'),
        "plan steps carry the └ prefix: {all_text}"
    );
}

#[test]
fn render_plan_cell_empty() {
    let cell = HistoryCell::Plan { steps: vec![] };
    let lines = cell.render(80, None);
    // Should have at least the "Updated Plan" header
    assert!(!lines.is_empty());
}

#[test]
fn render_tool_streaming_truncates() {
    let tool_lines: Vec<(String, bool)> = (0..12)
        .map(|i| (format!("output line {i}"), i % 3 == 0))
        .collect();
    let cell = HistoryCell::ToolStreaming { lines: tool_lines };
    let rendered = cell.render(80, None);
    let all_text: String = rendered
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // Should show "lines above" indicator for truncated lines
    assert!(all_text.contains("lines above"));
}

// -- style_system_line tests --

/// Branches that emit a single dim span (no header/icon emphasis).
#[test]
fn style_system_line_dim_only_branches() {
    let cases = &[
        "───────────",      // separator
        "",                 // empty
        "  some body text", // plain indented prose
    ];
    for input in cases {
        let line = style_system_line(input);
        assert_eq!(line.spans.len(), 1, "input: {input:?}");
        assert_eq!(line.spans[0].style, theme::dim(), "input: {input:?}");
    }
}

/// Branches that emit a single header-styled span.
#[test]
fn style_system_line_single_header_span_branches() {
    let cases = &[
        "Borg Doctor",                     // plain header line
        "<memory_file name=\"notes.md\">", // XML tag
        "# My Memory Topic",               // markdown header
    ];
    for input in cases {
        let line = style_system_line(input);
        assert_eq!(line.spans.len(), 1, "input: {input:?}");
        assert_eq!(
            line.spans[0].style,
            theme::header_style(),
            "input: {input:?}"
        );
    }
}

/// Icon-prefixed lines must apply the correct semantic style to the icon
/// glyph (success/warning/error), with the prefix indent and trailing
/// body kept separate. Regression: a previous render concatenated the
/// icon into the body span, losing the colored emphasis.
#[test]
fn style_system_line_icon_branches_use_semantic_style_for_glyph() {
    let cases: &[(&str, &str, Style)] = &[
        ("  ✓ sandbox enabled", "✓", theme::icon_style()),
        ("  ⚠ updates available", "⚠", theme::warning_style()),
        ("  ✗ provider missing", "✗", theme::error_style()),
    ];
    for (input, glyph, expected_style) in cases {
        let line = style_system_line(input);
        assert_eq!(line.spans.len(), 3, "input: {input:?}");
        assert_eq!(line.spans[1].content.as_ref(), *glyph, "input: {input:?}");
        assert_eq!(line.spans[1].style, *expected_style, "input: {input:?}");
    }
}

/// History-prefix lines emit `[timestamp] <Role:> <body>` with the role
/// span carrying header emphasis. `You:` keeps a bold modifier; the
/// others use the plain header style.
#[test]
fn style_system_line_history_prefix_branches() {
    let line = style_system_line("[13:05] You: hello");
    assert_eq!(line.spans.len(), 3);
    assert_eq!(line.spans[0].content.as_ref(), "[13:05] ");
    assert_eq!(line.spans[1].content.as_ref(), "You:");
    assert!(line.spans[1]
        .style
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));

    for (input, role) in &[
        ("[13:05] Assistant: hi there", "Assistant:"),
        ("[13:13] Tool (toolu_vr): Exit code: 0", "Tool (toolu_vr):"),
        ("  /help      - Show this help", "/help"),
    ] {
        let line = style_system_line(input);
        assert_eq!(line.spans.len(), 3, "input: {input:?}");
        assert_eq!(line.spans[1].content.as_ref(), *role, "input: {input:?}");
        assert_eq!(
            line.spans[1].style,
            theme::header_style(),
            "input: {input:?}"
        );
    }
}

/// `tracing` log lines have `LEVEL` prefix bolded and the rest dim, so
/// the level scans at a glance.
#[test]
fn style_system_line_log_level_branches() {
    for (input, level) in &[
        (
            "2026-04-03T17:05:25.065853Z  WARN Failed to resolve credential",
            "WARN ",
        ),
        (
            "2026-04-03T17:05:25.096265Z ERROR Gateway exited with error",
            "ERROR ",
        ),
    ] {
        let line = style_system_line(input);
        assert_eq!(line.spans.len(), 2, "input: {input:?}");
        assert_eq!(line.spans[0].style, theme::header_style());
        assert!(line.spans[0].content.contains(level), "input: {input:?}");
        assert_eq!(line.spans[1].style, theme::dim());
    }
}

#[test]
fn style_system_line_label_content() {
    let line = style_system_line("Session: 58 messages, ~11399 estimated tokens");
    assert_eq!(line.spans.len(), 2);
    assert_eq!(line.spans[0].content.as_ref(), "Session:");
    assert_eq!(line.spans[0].style, theme::header_style());
    assert_eq!(line.spans[1].style, theme::dim());
}

#[test]
fn style_system_line_tool_listing() {
    let line = style_system_line("  write_memory       Write/append long-term memory entries");
    assert_eq!(line.spans.len(), 3);
    assert_eq!(line.spans[1].content.as_ref(), "write_memory");
    assert_eq!(line.spans[1].style.fg, Some(ratatui::style::Color::White));
    // Description stays dim — no bold/white emphasis on the description body.
    assert_eq!(line.spans[2].style, theme::dim());
}

#[test]
fn parse_ansi_line_extracts_colors() {
    // Red SGR around "error", reset, then plain text.
    let raw = "\x1b[31merror\x1b[0m: failed";
    let line = parse_ansi_line(raw, theme::dim());
    // Should produce ≥2 spans with the first one carrying red fg.
    assert!(line.spans.len() >= 2, "spans: {:?}", line.spans);
    let red_span = line
        .spans
        .iter()
        .find(|s| s.content.contains("error"))
        .expect("error span");
    // Must have an explicit fg that is NOT the Reset sentinel — that's
    // the contract the dim-fallback merging relies on.
    assert!(
        red_span.style.fg.is_some() && red_span.style.fg != Some(ratatui::style::Color::Reset),
        "expected explicit non-Reset fg on the colored span, got: {:?}",
        red_span.style
    );
    // Trailing plain text should fall back to dim.
    let plain = line
        .spans
        .iter()
        .find(|s| s.content.contains("failed"))
        .expect("plain span");
    assert_eq!(plain.style.fg, theme::dim().fg);
}

#[test]
fn parse_ansi_line_strips_non_sgr_escapes() {
    // Hostile sequence: clear-screen + cursor-home embedded in output.
    // Must not survive into span text or it'll repaint the terminal.
    let raw = "safe\x1b[2J\x1b[Hhostile";
    let line = parse_ansi_line(raw, theme::dim());
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!joined.contains('\x1b'), "ESC byte leaked: {joined:?}");
    assert!(joined.contains("safe"));
    assert!(joined.contains("hostile"));
}

#[test]
fn parse_ansi_line_no_escapes_uses_fallback() {
    let line = parse_ansi_line("plain output", theme::error_style());
    let all: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(all, "plain output");
    assert_eq!(line.spans[0].style.fg, theme::error_style().fg);
}

#[test]
fn render_tool_result_error_is_boxed() {
    let cell = HistoryCell::ToolResult {
        output: "boom".to_string(),
        is_error: true,
        duration_ms: Some(50),
        display_label: "Ran `false`".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    let lines = cell.render(40, None);
    // First line should be the box top, last meaningful body row uses │,
    // bottom rule precedes the status line.
    let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(first.contains(theme::BOX_TOP_LEFT), "first: {first}");
    assert!(first.contains(theme::BOX_TOP_RIGHT));
    let has_vertical = lines.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content.contains(theme::BOX_VERTICAL))
    });
    assert!(has_vertical, "expected box side glyphs in body");
    let has_bottom = lines.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content.contains(theme::BOX_BOTTOM_LEFT))
    });
    assert!(has_bottom);
    // Status line still shows the cross + label.
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(all.contains(theme::CROSS));
    assert!(all.contains("Ran `false`"));
}

#[test]
fn render_boxed_lines_all_rows_equal_width() {
    // Regression for the off-by-one width-math bug in `render_boxed_lines`:
    // top, body, and bottom rows must all render at exactly `width` cells
    // so the box rules align vertically. Tests indent=0 (Thinking) and
    // indent=2 (boxed error ToolResult).
    for indent in [0usize, 2] {
        let inner = vec![
            Line::from(Span::raw("hi")),
            Line::from(Span::raw("longer body line here")),
            Line::from(Span::raw("")),
        ];
        for width in [40u16, 60, 80] {
            let lines = render_boxed_lines(
                inner.clone(),
                theme::error_style(),
                Some(" error "),
                width,
                indent,
            );
            let widths: Vec<usize> = lines.iter().map(line_utils::line_width).collect();
            let target = width as usize;
            assert!(
                widths.iter().all(|w| *w == target),
                "width={width} indent={indent} → row widths {widths:?} should all equal {target}"
            );
        }
    }
}

#[test]
fn render_tool_result_success_not_boxed() {
    // Sanity: success path still uses the tree-end prefix, not a box.
    let cell = HistoryCell::ToolResult {
        output: "ok".to_string(),
        is_error: false,
        duration_ms: Some(10),
        display_label: "Ran".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: None,
        collapsed: false,
    };
    let lines = cell.render(40, None);
    let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!first.contains(theme::BOX_TOP_LEFT));
    assert!(first.contains(theme::TREE_END));
}

// --- Streaming markdown collector tests (newline-gated cache) ---

fn assistant_cache_snapshot(cell: &HistoryCell) -> (Option<u16>, usize, usize) {
    match cell {
        HistoryCell::Assistant { cache, .. } => {
            let c = cache.borrow();
            (
                c.cached_width,
                c.committed_byte_len,
                c.committed_lines.len(),
            )
        }
        _ => panic!("expected Assistant"),
    }
}

fn flatten_plain(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn streaming_pending_without_newline_is_not_committed() {
    let cell = HistoryCell::assistant("Hello, world".to_string(), true);
    let _ = cell.render(40, None);
    let (w, committed_len, committed_count) = assistant_cache_snapshot(&cell);
    assert_eq!(committed_len, 0, "no newline => nothing committed");
    assert_eq!(committed_count, 0);
    assert_eq!(w, Some(38), "width - LIVE_PREFIX_COLS");
}

#[test]
fn streaming_newline_commits_and_second_render_hits_cache() {
    let cell = HistoryCell::assistant("Hello!\n".to_string(), true);
    let first = cell.render(40, None);
    let (_, committed_len, committed_count) = assistant_cache_snapshot(&cell);
    assert_eq!(committed_len, 7, "committed_byte_len == last newline + 1");
    assert!(committed_count >= 1);

    // Second render at same width: cache hit (no panic, equivalent output).
    let second = cell.render(40, None);
    assert_eq!(flatten_plain(&first), flatten_plain(&second));
    let (_, committed_len2, _) = assistant_cache_snapshot(&cell);
    assert_eq!(committed_len2, 7);
}

#[test]
fn streaming_width_change_invalidates_cache() {
    let long = "This is a long line that should wrap across different widths differently to make the cache width-sensitive.\n".to_string();
    let cell = HistoryCell::assistant(long, true);
    let _ = cell.render(40, None);
    let (w1, _, _) = assistant_cache_snapshot(&cell);
    assert_eq!(w1, Some(38));

    let _ = cell.render(60, None);
    let (w2, _, _) = assistant_cache_snapshot(&cell);
    assert_eq!(w2, Some(58), "cache width reflects new render width");
}

#[test]
fn streaming_mid_fence_pending_renders_without_panic() {
    // Open fence with language tag, content line, no closing fence.
    // The only newline is after "rust", so commit_end == 8; the pending
    // tail "fn main() {" is rendered per-frame.
    let cell = HistoryCell::assistant("```rust\nfn main() {".to_string(), true);
    let lines = cell.render(40, None);
    let (_, committed_len, _) = assistant_cache_snapshot(&cell);
    assert_eq!(committed_len, 8, "committed up to newline after ```rust");
    let rendered = flatten_plain(&lines);
    assert!(
        rendered.contains("fn main"),
        "pending tail should be rendered each frame, got: {rendered:?}"
    );
}

#[test]
fn streaming_finalize_flushes_pending_without_cursor() {
    let cell = HistoryCell::assistant("Line A\nLine B".to_string(), false);
    let lines = cell.render(40, None);
    let rendered = flatten_plain(&lines);
    assert!(rendered.contains("Line A"));
    assert!(rendered.contains("Line B"));
    assert!(
        !rendered.contains('▊'),
        "no streaming cursor when streaming=false"
    );
}

#[test]
fn streaming_empty_while_streaming_renders_cursor_only() {
    let cell = HistoryCell::assistant(String::new(), true);
    let lines = cell.render(40, None);
    assert!(
        lines.is_empty(),
        "empty streaming cell should stay invisible"
    );
}

#[test]
fn reply_directive_prefix_is_hidden_from_assistant_render() {
    let cell = HistoryCell::assistant("[[reply_to_current]]\nHello there".to_string(), true);
    let rendered = flatten_plain(&cell.render(60, None));
    assert!(!rendered.contains("[[reply_to_current]]"));
    assert!(rendered.contains("Hello there"));
}

fn shell_tool_result(output: &str, collapsed: bool) -> HistoryCell {
    HistoryCell::ToolResult {
        output: output.to_string(),
        is_error: false,
        duration_ms: Some(3400),
        display_label: "Ran `cat foo`".to_string(),
        tool_name: "run_shell".to_string(),
        args_json: Some(r#"{"command":"cat foo"}"#.to_string()),
        collapsed,
    }
}

#[test]
fn render_shell_result_skips_empty_streams() {
    // mkdir-style call: zero-byte stdout, zero-byte stderr — neither
    // section header should leak through, only Exit code + status.
    let cell = shell_tool_result("Exit code: 0\n--- stdout ---\n\n--- stderr ---\n", true);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(text.contains("└ Exit code: 0"), "got: {text}");
    assert!(!text.contains("--- stdout ---"), "got: {text}");
    assert!(!text.contains("--- stderr ---"), "got: {text}");
    assert!(text.contains("✓"), "status row missing: {text}");
    assert!(text.contains("• 3.4s"), "duration missing: {text}");
}

#[test]
fn render_shell_result_truncates_long_stdout() {
    let body: String = (0..30).map(|i| format!("line{i}\n")).collect();
    let raw = format!("Exit code: 0\n--- stdout ---\n{body}--- stderr ---\n");
    let cell = shell_tool_result(&raw, true);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(text.contains("--- stdout ---"));
    assert!(text.contains("line0"));
    assert!(text.contains("line4"));
    assert!(!text.contains("line29"), "tail should be elided");
    assert!(
        text.contains("ctrl + t to view transcript"),
        "expected truncation footer, got: {text}"
    );
}

#[test]
fn render_shell_result_expanded_shows_all_lines() {
    let body: String = (0..30).map(|i| format!("line{i}\n")).collect();
    let raw = format!("Exit code: 0\n--- stdout ---\n{body}--- stderr ---\n");
    let cell = shell_tool_result(&raw, false);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(text.contains("line29"), "all rows expected when expanded");
    assert!(
        !text.contains("ctrl + t to view transcript"),
        "no footer when expanded, got: {text}"
    );
}

#[test]
fn render_shell_result_shows_stderr_when_present() {
    let raw = "Exit code: 0\n--- stdout ---\nok\n--- stderr ---\nwarn: deprecated\n";
    let cell = shell_tool_result(raw, true);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(text.contains("--- stdout ---"));
    assert!(text.contains("ok"));
    assert!(text.contains("--- stderr ---"));
    assert!(text.contains("warn: deprecated"));
}

#[test]
fn render_shell_result_falls_back_on_unparseable() {
    // No "Exit code:" prefix — the structured renderer must bail and
    // hand the raw text to the generic body path so we don't lose it.
    let cell = shell_tool_result("totally unstructured output text\n", true);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(
        text.contains("totally unstructured output text"),
        "fallback must preserve raw text, got: {text}"
    );
}

#[test]
fn parse_shell_output_returns_none_when_sentinels_missing() {
    assert!(parse_shell_output("hello").is_none());
    assert!(parse_shell_output("Exit code: 0\nno markers").is_none());
    assert!(parse_shell_output("Exit code: 0\n--- stdout ---\nno stderr marker").is_none());
    let parsed = parse_shell_output("Exit code: 137\n--- stdout ---\nout\n--- stderr ---\nerr\n")
        .expect("envelope should parse");
    assert_eq!(parsed.exit_code, "137");
    assert_eq!(parsed.stdout, "out");
    assert_eq!(parsed.stderr, "err\n");
}

/// Builds a multi-line plain-text body. Each line is non-empty and unique
/// so the renderer doesn't merge them and we can recognize head vs tail.
fn many_line_text(n: usize) -> String {
    (0..n)
        .map(|i| format!("line-{i:04}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn assistant_short_message_not_collapsed() {
    // 30 source lines, well under the 40-line collapse threshold.
    let cell = HistoryCell::assistant(many_line_text(30), false);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(
        !text.contains("Ctrl+T to view transcript"),
        "short message must not show ellipsis hint, got: {text}"
    );
    // All 30 source lines must survive verbatim.
    for i in 0..30 {
        let needle = format!("line-{i:04}");
        assert!(
            text.contains(&needle),
            "missing {needle} in short-message render"
        );
    }
}

#[test]
fn assistant_long_message_collapsed_to_head_tail() {
    let cell = HistoryCell::assistant(many_line_text(100), false);
    let lines = cell.render(80, None);

    // 20 head + 1 ellipsis + 20 tail.
    assert_eq!(
        lines.len(),
        ASSISTANT_PREVIEW_LINES * 2 + 1,
        "expected head+ellipsis+tail layout"
    );

    let text = flatten_plain(&lines);
    assert!(
        text.contains("line-0000"),
        "first source line must appear in head, got: {text}"
    );
    assert!(
        text.contains("line-0099"),
        "last source line must appear in tail, got: {text}"
    );
    // Lines pruned from the middle are gone.
    assert!(
        !text.contains("line-0050"),
        "middle source line must be omitted, got: {text}"
    );

    // Exactly one ellipsis hint line.
    let hint_lines = lines
        .iter()
        .filter(|l| {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            s.contains("… +") && s.contains("Ctrl+T")
        })
        .count();
    assert_eq!(hint_lines, 1, "expected exactly one ellipsis hint line");
}

#[test]
fn assistant_collapse_reports_correct_omitted_count() {
    // 100 source lines => 100 - 40 = 60 omitted.
    let cell = HistoryCell::assistant(many_line_text(100), false);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(
        text.contains("… +60 lines"),
        "expected '+60 lines' in hint, got: {text}"
    );

    // 41 source lines => 41 - 40 = 1 omitted (boundary above threshold).
    let cell = HistoryCell::assistant(many_line_text(41), false);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(
        text.contains("… +1 lines"),
        "expected '+1 lines' at boundary, got: {text}"
    );

    // 40 source lines => exactly at threshold, no collapse.
    let cell = HistoryCell::assistant(many_line_text(40), false);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(
        !text.contains("Ctrl+T to view transcript"),
        "40-line message must not collapse, got: {text}"
    );
}

#[test]
fn assistant_streaming_message_not_collapsed() {
    // Streaming cells must render in full so the user sees live tokens.
    let cell = HistoryCell::assistant(many_line_text(100), true);
    let lines = cell.render(80, None);
    let text = flatten_plain(&lines);
    assert!(
        !text.contains("Ctrl+T to view transcript"),
        "streaming render must not collapse, got hint in: {text}"
    );
    assert!(
        lines.len() >= 100,
        "streaming render dropped lines: got {} for 100 source lines",
        lines.len()
    );
}

#[test]
fn collapse_long_lines_preserves_styling() {
    // Build a styled input where head and tail carry a distinctive
    // modifier. Guards against accidentally rebuilding lines as plain
    // strings during truncation.
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut input: Vec<Line<'static>> = Vec::new();
    for i in 0..ASSISTANT_COLLAPSE_THRESHOLD + 5 {
        input.push(Line::from(Span::styled(format!("row-{i}"), bold)));
    }
    let out = collapse_long_lines(input);
    assert_eq!(out.len(), ASSISTANT_COLLAPSE_THRESHOLD + 1);

    let head_first = &out[0];
    assert!(
        head_first
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "head line lost BOLD: {head_first:?}"
    );
    let tail_last = &out[out.len() - 1];
    assert!(
        tail_last
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "tail line lost BOLD: {tail_last:?}"
    );
}

// ── Output-redaction chokepoint (assistant + tool result) ──────────────
//
// These tests rely on the process-wide global redaction policy. They run
// in the same test binary as other tests that may touch the global, so
// each test installs a known state before asserting and restores the
// default afterwards. The lock prevents interleaving with other
// policy-sensitive tests in this module.
use std::sync::Mutex as StdMutex;
static REDACTION_TEST_LOCK: StdMutex<()> = StdMutex::new(());

struct PolicyGuard {
    prev: cortex::output_redaction::RedactionPolicy,
    _g: std::sync::MutexGuard<'static, ()>,
}
impl PolicyGuard {
    fn install(policy: cortex::output_redaction::RedactionPolicy) -> Self {
        let g = REDACTION_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = cortex::output_redaction::install_global(policy);
        Self { prev, _g: g }
    }
}
impl Drop for PolicyGuard {
    fn drop(&mut self) {
        cortex::output_redaction::install_global(self.prev.clone());
    }
}

#[test]
fn assistant_cell_redacts_secrets_in_transcript() {
    // Positive: an assistant message that echoes a real-looking AWS key
    // must be stripped before reaching the rendered Lines. Catches the
    // shared-screen leak vector this whole feature exists for.
    let _guard =
        PolicyGuard::install(cortex::output_redaction::RedactionPolicy::enabled_no_audit());
    let cell = HistoryCell::assistant(
        "here is the key: AKIAIOSFODNN7EXAMPLE end.".to_string(),
        false,
    );
    let lines = cell.render(80, None);
    let rendered = flatten_plain(&lines);
    assert!(
        !rendered.contains("AKIAIOSFODNN7EXAMPLE"),
        "raw secret leaked into transcript: {rendered:?}"
    );
    assert!(
        rendered.contains("[REDACTED AWS Access Key]"),
        "expected redaction marker, got: {rendered:?}"
    );
}

#[test]
fn assistant_cell_passes_through_when_policy_disabled() {
    // Negative / operator-toggle contract: when the policy is off,
    // assistant content reaches the transcript verbatim. This is what
    // an operator opts in to when they flip `security.output_redaction`
    // — confirms the toggle actually disables redaction in the TUI sink.
    let _guard = PolicyGuard::install(cortex::output_redaction::RedactionPolicy::disabled());
    let cell = HistoryCell::assistant(
        "here is the key: AKIAIOSFODNN7EXAMPLE end.".to_string(),
        false,
    );
    let lines = cell.render(80, None);
    let rendered = flatten_plain(&lines);
    assert!(
        rendered.contains("AKIAIOSFODNN7EXAMPLE"),
        "policy-disabled must let the original text through, got: {rendered:?}"
    );
    assert!(!rendered.contains("[REDACTED"));
}

#[test]
fn tool_result_cell_redacts_secrets() {
    let _guard =
        PolicyGuard::install(cortex::output_redaction::RedactionPolicy::enabled_no_audit());
    let cell = HistoryCell::ToolResult {
        output: "GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk done".to_string(),
        is_error: false,
        duration_ms: Some(10),
        display_label: "cat .env".to_string(),
        tool_name: "read_file".to_string(),
        args_json: None,
        collapsed: false,
    };
    let lines = cell.render(80, None);
    let rendered = flatten_plain(&lines);
    assert!(
        !rendered.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk"),
        "raw GitHub token leaked into tool result render: {rendered:?}"
    );
    assert!(
        rendered.contains("[REDACTED GitHub Token]"),
        "expected redaction marker on tool result, got: {rendered:?}"
    );
}

#[test]
fn user_cell_is_never_redacted() {
    // Critical contract: the user sees their own input verbatim. If a
    // user pastes an AKIA-shaped fixture (e.g. asking the agent to
    // search for a leaked key), redacting their own message would be
    // confusing and lose information they explicitly typed.
    let _guard =
        PolicyGuard::install(cortex::output_redaction::RedactionPolicy::enabled_no_audit());
    let cell = HistoryCell::User {
        text: "AKIAIOSFODNN7EXAMPLE".to_string(),
    };
    let lines = cell.render(80, None);
    let rendered = flatten_plain(&lines);
    assert!(
        rendered.contains("AKIAIOSFODNN7EXAMPLE"),
        "user input must NOT be redacted; got: {rendered:?}"
    );
    assert!(!rendered.contains("[REDACTED"));
}

/// `style_system_line` must be total over all UTF-8 input.
///
/// Regression: streaming a `request_user_input` tool call whose option
/// labels contained `"2. Warm & Playful — Warm, ..."` panicked the TUI
/// at history.rs:520 because the em-dash (`—`, 3 bytes) straddled the
/// hard-coded byte index 20 used by the log-line heuristic. Any digit-
/// prefixed line with a multi-byte char crossing byte 20 will crash.
///
/// Invariant: for any `&str`, `style_system_line` returns a `Line`
/// without panicking, and the joined span text equals the input
/// (styling splits the line into spans but never drops or corrupts it).
#[test]
fn style_system_line_handles_multibyte_at_log_heuristic_boundary() {
    let cases: &[&str] = &[
        // Exact reported crash: em-dash crosses byte 20.
        "2. Warm & Playful — Warm, supportive, and a bit playful",
        // Another em-dash straddling byte 20.
        "1234567890123456789—tail",
        // Real timestamped log line — must still take the heuristic path.
        "2026-04-03T17:05:25.123Z  WARN something happened",
        // Digit-prefixed but ≤20 bytes — heuristic must not match.
        "1. short",
        // Pure ASCII non-log line.
        "hello world",
    ];

    for input in cases {
        let line = style_system_line(input);
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            joined, *input,
            "style_system_line must preserve text exactly; input: {input:?}"
        );
    }

    // The log-line case must actually exercise the timestamp-styling
    // branch — i.e. produce >1 span with a header-styled prefix ending
    // at the log level. Otherwise the fix could silently regress the
    // heuristic into a no-op and this test would still pass.
    let log_line = style_system_line("2026-04-03T17:05:25.123Z  WARN something happened");
    assert!(
        log_line.spans.len() >= 2,
        "log-line heuristic must split into multiple spans; got {} span(s)",
        log_line.spans.len()
    );
    assert!(
        log_line.spans[0].content.contains(" WARN "),
        "first span should include the log-level prefix; got {:?}",
        log_line.spans[0].content
    );
}
