use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::config::Config;
use crate::constants;
use crate::output_redaction::{current_policy, OutputSink, RedactionPolicy};
use crate::types::{Message, MessageContent};
use crate::util::truncate::floor_char_boundary;

#[derive(Debug, Serialize, Deserialize)]
struct LogEntry {
    timestamp: String,
    #[serde(flatten)]
    kind: LogEntryKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum LogEntryKind {
    #[serde(rename = "user_message")]
    UserMessage { content: String },
    #[serde(rename = "assistant_message")]
    AssistantMessage {
        content: Option<String>,
        tool_calls: Option<Vec<LogToolCall>>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct LogToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn log_dir() -> Result<PathBuf> {
    let dir = Config::logs_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn log_path() -> Result<PathBuf> {
    let date = Local::now().format("%Y-%m-%d");
    Ok(log_dir()?.join(format!("{date}.jsonl")))
}

fn append_entry(entry: &LogEntry) -> Result<()> {
    let path = log_path()?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let line = serde_json::to_string(entry)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Bound one free-text log field to [`constants::MAX_LOG_FIELD_BYTES`].
///
/// Truncation is announced rather than silent: a reader who finds a tool
/// result ending mid-sentence needs to know the log cut it, not the tool.
/// Splitting happens on a character boundary so the JSONL line stays valid
/// UTF-8 — a multi-byte character cut in half would make the row unreadable
/// for every consumer, which is worse than the growth being bounded.
fn bound_log_field(text: &str) -> String {
    if text.len() <= constants::MAX_LOG_FIELD_BYTES {
        return text.to_string();
    }
    let end = floor_char_boundary(text, constants::MAX_LOG_FIELD_BYTES);
    let omitted = text.len() - end;
    format!(
        "{}\n[log truncated — {omitted} bytes omitted]",
        &text[..end]
    )
}

pub fn log_message(message: &Message) {
    log_message_with_policy(message, &current_policy())
}

/// Same as [`log_message`] but uses the caller-supplied policy. Test seam:
/// production code always goes through [`log_message`], which consults the
/// global policy installed during daemon startup.
pub fn log_message_with_policy(message: &Message, policy: &RedactionPolicy) {
    let timestamp = Local::now().to_rfc3339();

    // Logs are a persisted, shareable surface — unlike TUI cells where the
    // user's own typed input is rendered verbatim, the log file scrubs every
    // role including User. A user who pastes a credential into the prompt
    // still benefits from output-side redaction once their message lands in
    // `~/.borg/logs/*.jsonl`. There is no per-sender thread context for log
    // lines (they're session-scoped at the JSONL row level elsewhere).
    // Redact first, then bound. Truncating first could cut a credential in
    // half and leave the surviving prefix unmatched by the redaction patterns.
    let redact = |s: &str| bound_log_field(&policy.apply(OutputSink::Log, None, s));

    let kind = match message.role {
        crate::types::Role::User => {
            let full = message
                .content
                .as_ref()
                .map_or_else(String::new, MessageContent::full_text);
            LogEntryKind::UserMessage {
                content: redact(&full),
            }
        }
        crate::types::Role::Assistant => LogEntryKind::AssistantMessage {
            content: message.text_content().map(redact),
            tool_calls: message.tool_calls.as_ref().map(|tcs| {
                tcs.iter()
                    .map(|tc| LogToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: redact(&tc.function.arguments),
                    })
                    .collect()
            }),
        },
        crate::types::Role::Tool => LogEntryKind::ToolResult {
            tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
            content: redact(message.text_content().unwrap_or("")),
        },
        crate::types::Role::System => return,
    };

    let entry = LogEntry { timestamp, kind };
    if let Err(e) = append_entry(&entry) {
        tracing::warn!("Failed to write conversation log: {e}");
    }
}

pub fn read_history(lines: usize) -> Result<Vec<String>> {
    let path = log_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path)?;
    let all_lines: Vec<String> = content.lines().map(String::from).collect();
    let start = all_lines.len().saturating_sub(lines);
    Ok(all_lines[start..].to_vec())
}

fn format_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|_| "??:??".to_string())
}

fn truncate(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    crate::util::truncate::truncate_with_ellipsis(first_line, max, "…")
}

fn strip_tool_output_wrapper(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("<tool_output") {
        // Ensure the tag name is exactly "tool_output" (followed by space or >)
        if !rest.starts_with(' ') && !rest.starts_with('>') {
            return s;
        }
        // Find the end of the opening tag
        if let Some(tag_end) = rest.find('>') {
            let inner = &rest[tag_end + 1..];
            // Strip closing tag
            let inner = if let Some(pos) = inner.rfind("</tool_output>") {
                &inner[..pos]
            } else {
                inner
            };
            return inner.trim();
        }
    }
    s
}

fn format_tool_call_summary(tc: &LogToolCall) -> String {
    let args_preview = truncate(&tc.arguments, 60);
    format!("[{}({})]", tc.name, args_preview)
}

fn format_entry(entry: &LogEntry) -> String {
    let time = format_time(&entry.timestamp);
    match &entry.kind {
        LogEntryKind::UserMessage { content } => {
            format!("[{time}] You: {}", truncate(content, 200))
        }
        LogEntryKind::AssistantMessage {
            content,
            tool_calls,
        } => {
            let mut parts = Vec::new();
            // Models often open a reply with blank lines; they would leave
            // the `Assistant:` line itself looking empty.
            if let Some(text) = content.as_deref().map(str::trim) {
                if !text.is_empty() {
                    parts.push(truncate(text, 200));
                }
            }
            if let Some(tcs) = tool_calls {
                for tc in tcs {
                    parts.push(format_tool_call_summary(tc));
                }
            }
            if parts.is_empty() {
                format!("[{time}] Assistant:")
            } else {
                format!("[{time}] Assistant: {}", parts.join(" "))
            }
        }
        LogEntryKind::ToolResult {
            tool_call_id,
            content,
        } => {
            let short_id = if tool_call_id.len() > 8 {
                &tool_call_id[..8]
            } else {
                tool_call_id
            };
            let stripped = strip_tool_output_wrapper(content);
            format!("[{time}] Tool ({short_id}): {}", truncate(stripped, 200))
        }
    }
}

fn format_entry_verbose(entry: &LogEntry) -> String {
    let time = format_time(&entry.timestamp);
    match &entry.kind {
        LogEntryKind::UserMessage { content } => {
            format!("[{time}] You: {content}")
        }
        LogEntryKind::AssistantMessage {
            content,
            tool_calls,
        } => {
            let mut parts = Vec::new();
            // Models often open a reply with blank lines; they would leave
            // the `Assistant:` line itself looking empty.
            if let Some(text) = content.as_deref().map(str::trim) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
            if let Some(tcs) = tool_calls {
                for tc in tcs {
                    parts.push(format!("[{}({})]", tc.name, tc.arguments));
                }
            }
            if parts.is_empty() {
                format!("[{time}] Assistant:")
            } else {
                format!("[{time}] Assistant: {}", parts.join(" "))
            }
        }
        LogEntryKind::ToolResult {
            tool_call_id,
            content,
        } => {
            let short_id = if tool_call_id.len() > 8 {
                &tool_call_id[..8]
            } else {
                tool_call_id
            };
            let stripped = strip_tool_output_wrapper(content);
            format!("[{time}] Tool ({short_id}): {stripped}")
        }
    }
}

pub fn read_history_formatted(count: usize, verbose: bool) -> Result<Vec<String>> {
    let raw = read_history(count)?;
    let formatter = if verbose {
        format_entry_verbose
    } else {
        format_entry
    };
    Ok(raw
        .iter()
        .map(|line| {
            serde_json::from_str::<LogEntry>(line)
                .map(|entry| formatter(&entry))
                .unwrap_or_else(|_| line.clone())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn format_entry_user_message() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:30:00+00:00","type":"user_message","content":"hello world"}"#,
        ).unwrap();
        assert_eq!(format_entry(&entry), "[10:30] You: hello world");
    }

    #[test]
    fn format_entry_assistant_message() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:31:00+00:00","type":"assistant_message","content":"Sure, I can help."}"#,
        ).unwrap();
        assert_eq!(format_entry(&entry), "[10:31] Assistant: Sure, I can help.");
    }

    #[test]
    fn format_entry_assistant_message_drops_leading_blank_lines() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:31:00+00:00","type":"assistant_message","content":"\n\npong\n"}"#,
        ).unwrap();
        assert_eq!(format_entry(&entry), "[10:31] Assistant: pong");
        assert_eq!(format_entry_verbose(&entry), "[10:31] Assistant: pong");
    }

    #[test]
    fn format_entry_assistant_tool_call() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:32:00+00:00","type":"assistant_message","content":null,"tool_calls":[{"id":"call_abc123","name":"run_shell","arguments":"{}"}]}"#,
        ).unwrap();
        assert_eq!(format_entry(&entry), "[10:32] Assistant: [run_shell({})]");
    }

    #[test]
    fn format_entry_tool_result() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:33:00+00:00","type":"tool_result","tool_call_id":"call_abc123def","content":"command output here"}"#,
        ).unwrap();
        assert_eq!(
            format_entry(&entry),
            "[10:33] Tool (call_abc): command output here"
        );
    }

    #[test]
    fn format_entry_truncates_long_content() {
        let long = "x".repeat(300);
        let json = format!(
            r#"{{"timestamp":"2026-03-14T10:30:00+00:00","type":"user_message","content":"{long}"}}"#
        );
        let entry: LogEntry = serde_json::from_str(&json).unwrap();
        let formatted = format_entry(&entry);
        assert!(formatted.ends_with('…'));
        assert!(formatted.len() < 250);
    }

    #[test]
    fn strip_tool_output_wrapper_basic() {
        let input =
            "<tool_output name=\"web_search\" trust=\"external\">\nresults here\n</tool_output>";
        assert_eq!(strip_tool_output_wrapper(input), "results here");
    }

    #[test]
    fn strip_tool_output_wrapper_no_tags() {
        assert_eq!(strip_tool_output_wrapper("plain text"), "plain text");
    }

    #[test]
    fn format_entry_assistant_with_text_and_tool_calls() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:32:00+00:00","type":"assistant_message","content":"I'll search","tool_calls":[{"id":"call_1","name":"web_search","arguments":"{\"query\": \"test\"}"}]}"#,
        ).unwrap();
        let formatted = format_entry(&entry);
        assert!(formatted.contains("I'll search"));
        assert!(formatted.contains("[web_search("));
    }

    #[test]
    fn format_entry_tool_result_strips_xml() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:33:00+00:00","type":"tool_result","tool_call_id":"call_abc123def","content":"<tool_output name=\"web_search\" trust=\"external\">\nSearch results here\n</tool_output>"}"#,
        ).unwrap();
        let formatted = format_entry(&entry);
        assert!(!formatted.contains("<tool_output"));
        assert!(formatted.contains("Search results here"));
    }

    #[test]
    fn format_entry_verbose_user() {
        let long = "x".repeat(300);
        let json = format!(
            r#"{{"timestamp":"2026-03-14T10:30:00+00:00","type":"user_message","content":"{long}"}}"#
        );
        let entry: LogEntry = serde_json::from_str(&json).unwrap();
        let formatted = format_entry_verbose(&entry);
        assert!(formatted.contains(&long));
        assert!(!formatted.ends_with('…'));
    }

    #[test]
    fn format_entry_verbose_assistant_tool_call() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:32:00+00:00","type":"assistant_message","content":null,"tool_calls":[{"id":"call_1","name":"run_shell","arguments":"{\"command\": \"ls -la /very/long/path\"}"}]}"#,
        ).unwrap();
        let formatted = format_entry_verbose(&entry);
        assert!(formatted.contains(r#"{"command": "ls -la /very/long/path"}"#));
    }

    #[test]
    fn format_entry_verbose_tool_result() {
        let entry: LogEntry = serde_json::from_str(
            r#"{"timestamp":"2026-03-14T10:33:00+00:00","type":"tool_result","tool_call_id":"call_abc123def","content":"<tool_output name=\"web_search\" trust=\"external\">\nFull results here\n</tool_output>"}"#,
        ).unwrap();
        let formatted = format_entry_verbose(&entry);
        assert!(!formatted.contains("<tool_output"));
        assert!(formatted.contains("Full results here"));
    }

    #[test]
    fn verbose_produces_longer_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let long_content = "y".repeat(300);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"2026-03-14T10:30:00+00:00","type":"user_message","content":"{long_content}"}}"#
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<String> = content.lines().map(String::from).collect();

        let default_fmt: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<LogEntry>(l)
                    .map(|e| format_entry(&e))
                    .unwrap_or_else(|_| l.clone())
            })
            .collect();
        let verbose_fmt: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<LogEntry>(l)
                    .map(|e| format_entry_verbose(&e))
                    .unwrap_or_else(|_| l.clone())
            })
            .collect();

        assert!(verbose_fmt[0].len() > default_fmt[0].len());
    }

    const TRUNCATION_MARKER: &str = "[log truncated";

    #[test]
    fn a_field_within_the_cap_is_returned_byte_identical() {
        let ordinary = "a tool result that nobody needs to truncate";
        assert_eq!(bound_log_field(ordinary), ordinary);

        // The boundary itself must not truncate: `<=` not `<`.
        let exactly_at_cap = "x".repeat(constants::MAX_LOG_FIELD_BYTES);
        assert_eq!(bound_log_field(&exactly_at_cap), exactly_at_cap);
    }

    #[test]
    fn an_oversized_field_is_truncated_and_says_how_much_it_dropped() {
        let overflow = 5_000;
        let huge = "x".repeat(constants::MAX_LOG_FIELD_BYTES + overflow);

        let bounded = bound_log_field(&huge);

        assert!(
            bounded.contains(TRUNCATION_MARKER),
            "a reader must be able to tell the log cut the value, not the tool: {bounded:?}"
        );
        assert!(
            bounded.contains(&format!("{overflow} bytes omitted")),
            "the marker must report the real omitted count: {bounded:?}"
        );
        let body = bounded
            .split_once('\n')
            .expect("marker is on its own line")
            .0;
        assert_eq!(body.len(), constants::MAX_LOG_FIELD_BYTES);
        assert!(huge.starts_with(body), "the kept prefix must be verbatim");
    }

    #[test]
    fn truncation_splits_on_a_character_boundary() {
        // One ASCII byte then two-byte characters, so the cap lands mid-character.
        let multibyte_count = constants::MAX_LOG_FIELD_BYTES;
        let huge = format!("a{}", "é".repeat(multibyte_count));
        assert!(!huge.is_char_boundary(constants::MAX_LOG_FIELD_BYTES));

        let bounded = bound_log_field(&huge);
        let body = bounded
            .split_once('\n')
            .expect("marker is on its own line")
            .0;

        assert_eq!(
            body.len(),
            constants::MAX_LOG_FIELD_BYTES - 1,
            "the split backs off to the last whole character"
        );
        assert!(body.ends_with('é'));
        assert!(!body.contains('\u{fffd}'), "no half character survived");
    }

    #[test]
    fn a_truncated_field_still_writes_one_valid_json_line() {
        let entry = LogEntry {
            timestamp: "2026-03-14T10:30:00+00:00".to_string(),
            kind: LogEntryKind::ToolResult {
                tool_call_id: "call_1".to_string(),
                content: bound_log_field(&"é".repeat(constants::MAX_LOG_FIELD_BYTES)),
            },
        };

        let line = serde_json::to_string(&entry).expect("truncated field serializes");
        assert!(!line.contains('\n'), "one entry must stay on one JSONL row");

        let parsed: LogEntry = serde_json::from_str(&line).expect("row round-trips");
        let LogEntryKind::ToolResult { content, .. } = parsed.kind else {
            panic!("wrong variant");
        };
        assert!(content.contains(TRUNCATION_MARKER));
        assert!(content.len() < constants::MAX_LOG_FIELD_BYTES + 100);
    }
}
