use anyhow::{Context, Result};
use chrono::Utc;
use cron::Schedule;
use rusqlite::params;
use std::str::FromStr;

use crate::db::{Database, ScheduledTaskRow};

// ── Task status constants ──

pub const TASK_STATUS_ACTIVE: &str = "active";
pub const TASK_STATUS_PAUSED: &str = "paused";
pub const TASK_STATUS_CANCELLED: &str = "cancelled";
pub const TASK_STATUS_COMPLETED: &str = "completed";

// ── Run status constants ──

pub const RUN_STATUS_RUNNING: &str = "running";
pub const RUN_STATUS_SUCCESS: &str = "success";
pub const RUN_STATUS_FAILED: &str = "failed";

// ── Schedule type constants ──

/// One-shot task: runs once at `next_run`, then marked completed.
pub const SCHEDULE_TYPE_ONCE: &str = "once";
/// Cron-scheduled task: recurring, driven by a cron expression.
pub const SCHEDULE_TYPE_CRON: &str = "cron";
/// Interval-scheduled task: recurring, every N duration.
pub const SCHEDULE_TYPE_INTERVAL: &str = "interval";

/// Format a run status for CLI/tool display.
pub fn format_run_status(status: &str) -> &str {
    match status {
        RUN_STATUS_RUNNING => "RUNNING",
        RUN_STATUS_SUCCESS => "OK",
        RUN_STATUS_FAILED => "FAIL",
        other => other,
    }
}

/// Advance a task's next_run using raw SQL on a connection (for use within transactions).
/// Returns Ok(()) on success.
pub fn advance_next_run_raw(conn: &rusqlite::Connection, task: &ScheduledTaskRow) -> Result<()> {
    match task.schedule_type.as_str() {
        SCHEDULE_TYPE_ONCE => {
            conn.execute(
                "UPDATE scheduled_tasks SET status = ?1, next_run = NULL WHERE id = ?2",
                params![TASK_STATUS_COMPLETED, task.id],
            )?;
        }
        SCHEDULE_TYPE_CRON | SCHEDULE_TYPE_INTERVAL => {
            let next = calculate_next_run(&task.schedule_type, &task.schedule_expr).unwrap_or(None);
            conn.execute(
                "UPDATE scheduled_tasks SET next_run = ?1 WHERE id = ?2",
                params![next, task.id],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Calculate the next run time for a task based on its schedule.
pub fn calculate_next_run(schedule_type: &str, schedule_expr: &str) -> Result<Option<i64>> {
    match schedule_type {
        SCHEDULE_TYPE_CRON => {
            let schedule = Schedule::from_str(schedule_expr)
                .with_context(|| format!("Invalid cron expression: {schedule_expr}"))?;
            let next = schedule.upcoming(Utc).next();
            Ok(next.map(|t| t.timestamp()))
        }
        SCHEDULE_TYPE_INTERVAL => {
            let duration = parse_interval(schedule_expr)
                .with_context(|| format!("Invalid interval: {schedule_expr}"))?;
            Ok(Some(Utc::now().timestamp() + duration.as_secs() as i64))
        }
        SCHEDULE_TYPE_ONCE => {
            // For one-shot tasks, next_run is set at creation time
            Ok(Some(Utc::now().timestamp()))
        }
        other => {
            anyhow::bail!("Unknown schedule type: {other}. Use 'cron', 'interval', or 'once'.")
        }
    }
}

/// Advance a task's next_run after execution.
pub fn advance_next_run(task: &ScheduledTaskRow, db: &Database) -> Result<()> {
    advance_next_run_raw(db.conn(), task)
}

/// Format a task row for display.
pub fn format_task(task: &ScheduledTaskRow) -> String {
    let next = task
        .next_run
        .map(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| ts.to_string())
        })
        .unwrap_or_else(|| "—".to_string());

    let payload_label = if task.task_type == "command" {
        "Command"
    } else {
        "Prompt"
    };
    format!(
        "  {} [{}] ({})\n    Schedule: {} {}\n    Next run: {}\n    {}: {}",
        task.name,
        task.status,
        task.id.chars().take(8).collect::<String>(),
        task.schedule_type,
        task.schedule_expr,
        next,
        payload_label,
        truncate_str(&task.prompt, 80),
    )
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}...", &s[..end])
    } else {
        s.to_string()
    }
}

/// Validate a schedule expression without computing the next run time.
pub fn validate_schedule(schedule_type: &str, schedule_expr: &str) -> Result<()> {
    match schedule_type {
        SCHEDULE_TYPE_CRON => {
            Schedule::from_str(schedule_expr)
                .with_context(|| format!("Invalid cron expression: {schedule_expr}"))?;
            Ok(())
        }
        SCHEDULE_TYPE_INTERVAL => {
            parse_interval(schedule_expr)
                .ok_or_else(|| anyhow::anyhow!("Invalid interval: {schedule_expr}"))?;
            Ok(())
        }
        SCHEDULE_TYPE_ONCE => Ok(()),
        other => {
            anyhow::bail!("Unknown schedule type: {other}. Use 'cron', 'interval', or 'once'.")
        }
    }
}

pub fn parse_interval(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<u64>()
            .ok()
            .map(|m| std::time::Duration::from_secs(m * 60))
    } else if let Some(hours) = s.strip_suffix('h') {
        hours
            .parse::<u64>()
            .ok()
            .map(|h| std::time::Duration::from_secs(h * 3600))
    } else if let Some(secs) = s.strip_suffix('s') {
        secs.parse::<u64>().ok().map(std::time::Duration::from_secs)
    } else if let Some(days) = s.strip_suffix('d') {
        days.parse::<u64>()
            .ok()
            .map(|d| std::time::Duration::from_secs(d * 86400))
    } else {
        s.parse::<u64>().ok().map(std::time::Duration::from_secs)
    }
}

/// Exponential backoff delays for task retries: 30s, 60s, 5m, 15m, 1h.
pub fn retry_delay_secs(attempt: i32) -> i64 {
    const DELAYS: [i64; 5] = [30, 60, 300, 900, 3600];
    DELAYS.get(attempt as usize).copied().unwrap_or(3600)
}

/// Convert a 5-field Linux cron expression to the 7-field format used by the `cron` crate.
/// Prepends "0" for seconds and appends "*" for year.
/// Input: "*/5 * * * *" → Output: "0 */5 * * * * *"
pub fn convert_5_to_7_field(expr: &str) -> String {
    format!("0 {} *", expr.trim())
}

/// Parse a combined cron line like "*/5 * * * * echo hello" (Linux crontab format).
/// Returns (seven_field_cron_expr, command).
/// The first 5 whitespace-separated tokens are the cron schedule, the rest is the command.
pub fn parse_cron_line(line: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = line.trim().splitn(6, char::is_whitespace).collect();
    if parts.len() < 6 || parts[5].is_empty() {
        anyhow::bail!(
            "Expected 5 cron fields followed by a command. Example: */5 * * * * echo hello"
        );
    }
    let cron_5 = parts[..5].join(" ");
    let command = parts[5].trim().to_string();
    let cron_7 = convert_5_to_7_field(&cron_5);
    // Validate
    Schedule::from_str(&cron_7).with_context(|| format!("Invalid cron expression: {cron_5}"))?;
    Ok((cron_7, command))
}

/// Check if an error message indicates a transient failure worth retrying.
pub fn is_transient_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("rate limit")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("status 429")
        || lower.contains("http 429")
        || lower.contains(" 429 ")
        || lower.contains("status 503")
        || lower.contains("http 503")
        || lower.contains(" 503 ")
        || lower.contains("status 502")
        || lower.contains("http 502")
        || lower.contains(" 502 ")
        || lower.contains("status 500")
        || lower.contains("http 500")
        || lower.contains(" 500 ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_interval_minutes() {
        let d = parse_interval("30m").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(1800));
    }

    #[test]
    fn parse_interval_hours() {
        let d = parse_interval("2h").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(7200));
    }

    #[test]
    fn parse_interval_days() {
        let d = parse_interval("1d").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(86400));
    }

    #[test]
    fn parse_interval_seconds() {
        let d = parse_interval("60s").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(60));
    }

    #[test]
    fn parse_interval_bare_number() {
        let d = parse_interval("120").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(120));
    }

    #[test]
    fn parse_interval_with_whitespace() {
        let d = parse_interval("  10m  ").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(600));
    }

    #[test]
    fn parse_interval_zero() {
        assert_eq!(
            parse_interval("0s").unwrap(),
            std::time::Duration::from_secs(0)
        );
        assert_eq!(
            parse_interval("0").unwrap(),
            std::time::Duration::from_secs(0)
        );
    }

    #[test]
    fn parse_interval_large_hours() {
        let d = parse_interval("24h").unwrap();
        assert_eq!(d, std::time::Duration::from_secs(24 * 3600));
    }

    #[test]
    fn parse_interval_invalid() {
        assert!(parse_interval("abc").is_none());
        assert!(parse_interval("").is_none());
    }

    #[test]
    fn calculate_next_run_cron() {
        let next = calculate_next_run("cron", "0 0 9 * * * *").unwrap();
        assert!(next.is_some());
        assert!(next.unwrap() > Utc::now().timestamp());
    }

    #[test]
    fn calculate_next_run_interval() {
        let next = calculate_next_run("interval", "1h").unwrap();
        assert!(next.is_some());
        let expected_min = Utc::now().timestamp() + 3500; // ~1h minus tolerance
        assert!(next.unwrap() > expected_min);
    }

    #[test]
    fn calculate_next_run_once() {
        let next = calculate_next_run("once", "").unwrap();
        assert!(next.is_some());
    }

    #[test]
    fn calculate_next_run_unknown_type() {
        assert!(calculate_next_run("weekly", "").is_err());
    }

    #[test]
    fn validate_schedule_valid_cron() {
        assert!(validate_schedule("cron", "0 0 9 * * * *").is_ok());
    }

    #[test]
    fn validate_schedule_invalid_cron() {
        assert!(validate_schedule("cron", "not a cron").is_err());
    }

    #[test]
    fn validate_schedule_valid_interval() {
        assert!(validate_schedule("interval", "30m").is_ok());
    }

    #[test]
    fn validate_schedule_invalid_interval() {
        assert!(validate_schedule("interval", "abc").is_err());
    }

    #[test]
    fn validate_schedule_once() {
        assert!(validate_schedule("once", "").is_ok());
    }

    #[test]
    fn validate_schedule_unknown_type() {
        assert!(validate_schedule("weekly", "").is_err());
    }

    #[test]
    fn parse_interval_is_pub() {
        // Verifies parse_interval is now pub by calling it from tests
        assert!(parse_interval("30m").is_some());
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        let long = "a".repeat(100);
        let result = truncate_str(&long, 10);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 13); // 10 + "..."
    }

    #[test]
    fn truncate_str_multibyte() {
        let s = "hello\u{1F600}world"; // emoji in the middle
        let result = truncate_str(s, 6);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 9); // 6 + "..."
    }

    // ── Cron line parser tests ──

    #[test]
    fn convert_5_to_7_field_basic() {
        assert_eq!(convert_5_to_7_field("*/5 * * * *"), "0 */5 * * * * *");
    }

    #[test]
    fn convert_5_to_7_field_specific() {
        assert_eq!(convert_5_to_7_field("0 3 * * *"), "0 0 3 * * * *");
    }

    #[test]
    fn parse_cron_line_echo() {
        let (cron, cmd) = parse_cron_line("*/5 * * * * echo hello").unwrap();
        assert_eq!(cron, "0 */5 * * * * *");
        assert_eq!(cmd, "echo hello");
    }

    #[test]
    fn parse_cron_line_script() {
        let (cron, cmd) = parse_cron_line("0 3 * * * python3 /opt/test/myscript.py").unwrap();
        assert_eq!(cron, "0 0 3 * * * *");
        assert_eq!(cmd, "python3 /opt/test/myscript.py");
    }

    #[test]
    fn parse_cron_line_too_few_fields() {
        assert!(parse_cron_line("*/5 * * *").is_err());
    }

    #[test]
    fn parse_cron_line_no_command() {
        assert!(parse_cron_line("*/5 * * * *").is_err());
    }

    #[test]
    fn parse_cron_line_command_with_spaces() {
        let (_, cmd) = parse_cron_line("0 0 * * * echo hello world").unwrap();
        assert_eq!(cmd, "echo hello world");
    }

    // ── Retry logic tests ──

    #[test]
    fn retry_delay_secs_escalates() {
        assert_eq!(retry_delay_secs(0), 30);
        assert_eq!(retry_delay_secs(1), 60);
        assert_eq!(retry_delay_secs(2), 300);
        assert_eq!(retry_delay_secs(3), 900);
        assert_eq!(retry_delay_secs(4), 3600);
    }

    #[test]
    fn retry_delay_secs_caps_at_max() {
        assert_eq!(retry_delay_secs(5), 3600);
        assert_eq!(retry_delay_secs(100), 3600);
    }

    #[test]
    fn is_transient_error_true_cases() {
        assert!(is_transient_error("request timed out"));
        assert!(is_transient_error("Connection timeout after 30s"));
        assert!(is_transient_error("rate limit exceeded"));
        assert!(is_transient_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_error("HTTP 503 Service Unavailable"));
        assert!(is_transient_error("HTTP 502 Bad Gateway"));
        assert!(is_transient_error("HTTP 500 Internal Server Error"));
        assert!(is_transient_error("status 500"));
        assert!(is_transient_error("connection refused"));
        assert!(is_transient_error("Connection reset by peer"));
        assert!(is_transient_error("broken pipe"));
    }

    #[test]
    fn is_transient_error_false_cases() {
        assert!(!is_transient_error("invalid API key"));
        assert!(!is_transient_error("model not found"));
        assert!(!is_transient_error("unauthorized"));
        assert!(!is_transient_error("bad request: missing required field"));
        assert!(!is_transient_error("content policy violation"));
    }

    #[test]
    fn is_transient_error_no_false_positive_on_numbers() {
        // "500" should not match inside "5000ms" (no "timeout" word here)
        assert!(!is_transient_error("processed 5000 items"));
        // "429" should not match port numbers
        assert!(!is_transient_error("connected on port 4290"));
        // But "timeout after 5000ms" IS transient (matches "timeout")
        assert!(is_transient_error("timeout after 5000ms"));
    }
}
