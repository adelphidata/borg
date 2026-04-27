use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::config::Config;
use crate::secrets::redact_secrets;
use crate::types::{Message, MessageContent};

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

pub fn log_message(message: &Message) {
    let timestamp = Local::now().to_rfc3339();

    let kind = match message.role {
        crate::types::Role::User => {
            let full = message
                .content
                .as_ref()
                .map_or_else(String::new, MessageContent::full_text);
            let content = redact_secrets(&full);
            LogEntryKind::UserMessage { content }
        }
        crate::types::Role::Assistant => LogEntryKind::AssistantMessage {
            content: message.text_content().map(redact_secrets),
            tool_calls: message.tool_calls.as_ref().map(|tcs| {
                tcs.iter()
                    .map(|tc| LogToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: redact_secrets(&tc.function.arguments),
                    })
                    .collect()
            }),
        },
        crate::types::Role::Tool => LogEntryKind::ToolResult {
            tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
            content: redact_secrets(message.text_content().unwrap_or("")),
        },
        crate::types::Role::System => return,
    };

    let entry = LogEntry { timestamp, kind };
    if let Err(e) = append_entry(&entry) {
        tracing::warn!("Failed to write conversation log: {e}");
    }
}

#[derive(Debug, Default)]
pub struct UsageStats {
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
}

pub fn count_messages_for_period(days: i64) -> Result<UsageStats> {
    let dir = log_dir()?;
    let today = Local::now().date_naive();
    count_messages_in_dir(&dir, today, days)
}

fn count_messages_in_dir(
    dir: &std::path::Path,
    today: chrono::NaiveDate,
    days: i64,
) -> Result<UsageStats> {
    let mut stats = UsageStats::default();

    for d in 0..days {
        let date = today - chrono::Duration::days(d);
        let path = dir.join(format!("{}.jsonl", date.format("%Y-%m-%d")));
        if !path.exists() {
            continue;
        }
        let content = fs::read_to_string(&path)?;
        for line in content.lines() {
            if line.contains("\"user_message\"") {
                stats.user_messages += 1;
            } else if line.contains("\"assistant_message\"") {
                stats.assistant_messages += 1;
            } else if line.contains("\"tool_result\"") {
                stats.tool_calls += 1;
            }
        }
    }

    Ok(stats)
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
    if first_line.len() > max {
        format!("{}…", &first_line[..max])
    } else {
        first_line.to_string()
    }
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
            if let Some(text) = content.as_deref() {
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
            if let Some(text) = content.as_deref() {
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
    fn count_messages_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let today = Local::now().date_naive();
        let stats = count_messages_in_dir(dir.path(), today, 7).unwrap();
        assert_eq!(stats.user_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
        assert_eq!(stats.tool_calls, 0);
    }

    #[test]
    fn count_messages_single_day() {
        let dir = tempfile::tempdir().unwrap();
        let today = Local::now().date_naive();
        let filename = format!("{}.jsonl", today.format("%Y-%m-%d"));
        let path = dir.path().join(filename);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user_message","content":"hello"}}"#).unwrap();
        writeln!(f, r#"{{"type":"assistant_message","content":"hi"}}"#).unwrap();
        writeln!(f, r#"{{"type":"user_message","content":"do stuff"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"tool_result","tool_call_id":"c1","content":"ok"}}"#
        )
        .unwrap();

        let stats = count_messages_in_dir(dir.path(), today, 1).unwrap();
        assert_eq!(stats.user_messages, 2);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_calls, 1);
    }

    #[test]
    fn count_messages_multi_day() {
        let dir = tempfile::tempdir().unwrap();
        let today = Local::now().date_naive();

        // Today
        let path = dir
            .path()
            .join(format!("{}.jsonl", today.format("%Y-%m-%d")));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user_message","content":"today"}}"#).unwrap();

        // Yesterday
        let yesterday = today - chrono::Duration::days(1);
        let path = dir
            .path()
            .join(format!("{}.jsonl", yesterday.format("%Y-%m-%d")));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user_message","content":"yesterday"}}"#).unwrap();
        writeln!(f, r#"{{"type":"assistant_message","content":"resp"}}"#).unwrap();

        let stats = count_messages_in_dir(dir.path(), today, 2).unwrap();
        assert_eq!(stats.user_messages, 2);
        assert_eq!(stats.assistant_messages, 1);

        // Only 1 day should exclude yesterday
        let stats = count_messages_in_dir(dir.path(), today, 1).unwrap();
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 0);
    }

    #[test]
    fn count_messages_zero_days() {
        let dir = tempfile::tempdir().unwrap();
        let today = Local::now().date_naive();
        let stats = count_messages_in_dir(dir.path(), today, 0).unwrap();
        assert_eq!(stats.user_messages, 0);
    }

    #[test]
    fn usage_stats_default() {
        let stats = UsageStats::default();
        assert_eq!(stats.user_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
        assert_eq!(stats.tool_calls, 0);
    }

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
}
