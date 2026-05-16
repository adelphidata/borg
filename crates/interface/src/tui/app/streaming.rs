//! Streaming-turn UI state: the live header/details row shown while
//! `AppState::Streaming`, plus the conversation backtrack state machine.
//!
//! Kept separate from `mod.rs` so the streaming presentation rules
//! (header transitions, details ellipsis, Working reset semantics) live next
//! to their tests instead of being buried in the main `App` body.

use std::time::Instant;

use super::{App, AppState};

/// Live header + details shown in the status row while `AppState::Streaming`.
///
/// Driven by agent events (reasoning deltas, tool starts, etc.) so the user
/// has evidence the turn is still doing work — elapsed time alone is
/// indistinguishable from a hang on a multi-minute reasoning turn.
#[derive(Debug, Clone)]
pub struct StreamStatus {
    pub header: String,
    pub details: Option<String>,
}

impl Default for StreamStatus {
    fn default() -> Self {
        Self {
            header: "Working".to_string(),
            details: None,
        }
    }
}

/// Max displayed width (in chars) of the details snippet before left-ellipsis.
/// Kept small so the row still fits on narrow terminals.
pub(super) const STREAM_STATUS_DETAILS_MAX_CHARS: usize = 80;

impl StreamStatus {
    pub fn reset(&mut self) {
        self.header.clear();
        self.header.push_str("Working");
        self.details = None;
    }

    pub fn set_header(&mut self, header: impl Into<String>) {
        self.header = header.into();
    }

    /// Update the details snippet. Newlines are flattened to spaces and the
    /// string is trimmed to the tail `STREAM_STATUS_DETAILS_MAX_CHARS` chars
    /// with a leading ellipsis so the freshest bytes stay visible.
    pub fn set_details(&mut self, details: Option<String>) {
        self.details = details
            .map(|s| s.replace('\n', " ").trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| {
                let count = s.chars().count();
                if count <= STREAM_STATUS_DETAILS_MAX_CHARS {
                    s
                } else {
                    let skip = count - STREAM_STATUS_DETAILS_MAX_CHARS;
                    let tail: String = s.chars().skip(skip).collect();
                    format!("…{tail}")
                }
            });
    }
}

/// State machine for conversation backtracking (rewinding to a past user message).
pub enum BacktrackPhase {
    /// Not in backtrack mode.
    Inactive,
    /// User is selecting a past message to rewind to.
    Selecting {
        /// Indices into `App::cells` that are `HistoryCell::User` messages, ordered oldest-first.
        user_message_indices: Vec<usize>,
        /// Cursor position within `user_message_indices` (0 = most recent, at the end).
        cursor: usize,
    },
}

impl<'a> App<'a> {
    /// Transition into `AppState::Streaming` and reset the live status row.
    ///
    /// Call this for *new* turns (user submit, queued message). For resume
    /// transitions from approval/input prompts where the turn was already
    /// in flight, use `resume_streaming` so the existing header/details
    /// (e.g. "Running run_shell") aren't wiped while the tool is still
    /// running server-side.
    pub fn enter_streaming(&mut self) {
        self.state = AppState::Streaming {
            start: Instant::now(),
        };
        self.stream_status = StreamStatus::default();
    }

    /// Transition back to `AppState::Streaming` from a prompt state
    /// (approval / input request) without touching `stream_status` —
    /// the agent is still mid-turn and its header is still accurate.
    pub fn resume_streaming(&mut self) {
        self.state = AppState::Streaming {
            start: Instant::now(),
        };
    }

    pub fn tick_throbber(&mut self) {
        if !matches!(self.state, AppState::Idle) {
            self.throbber_state.calc_next();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_client::DaemonClient;
    use cortex::agent::AgentEvent;
    use cortex::config::Config;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn make_app() -> App<'static> {
        let config = Config::default();
        let client = Arc::new(Mutex::new(DaemonClient::lazy_for_tests()));
        App::new(config, None, None, None, client, None)
    }

    #[test]
    fn stream_status_set_details_passes_short_string_through() {
        let mut s = StreamStatus::default();
        s.set_details(Some("hello world".to_string()));
        assert_eq!(s.details.as_deref(), Some("hello world"));
    }

    #[test]
    fn stream_status_set_details_left_ellipsizes_when_over_max() {
        let mut s = StreamStatus::default();
        let long: String = "a".repeat(200);
        s.set_details(Some(long));
        let out = s.details.expect("details");
        assert!(out.starts_with('…'), "expected leading ellipsis, got {out}");
        assert_eq!(out.chars().count(), 1 + STREAM_STATUS_DETAILS_MAX_CHARS);
    }

    #[test]
    fn stream_status_set_details_flattens_newlines() {
        let mut s = StreamStatus::default();
        s.set_details(Some("line one\nline two".to_string()));
        assert_eq!(s.details.as_deref(), Some("line one line two"));
    }

    #[test]
    fn stream_status_set_details_none_on_empty_string() {
        let mut s = StreamStatus::default();
        s.set_details(Some(String::new()));
        assert!(s.details.is_none());
        s.set_details(Some("   \n  ".to_string()));
        assert!(s.details.is_none(), "whitespace-only should also clear");
    }

    #[test]
    fn stream_status_reset_restores_working_header_and_clears_details() {
        let mut s = StreamStatus::default();
        s.set_header("Running run_shell");
        s.set_details(Some("cargo build".to_string()));
        s.reset();
        assert_eq!(s.header, "Working");
        assert!(s.details.is_none());
    }

    #[test]
    fn enter_streaming_resets_stream_status() {
        let mut app = make_app();
        app.stream_status.set_header("Running run_shell");
        app.stream_status.set_details(Some("cargo build".into()));
        app.enter_streaming();
        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.stream_status.header, "Working");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn resume_streaming_preserves_stream_status() {
        let mut app = make_app();
        app.stream_status.set_header("Running run_shell");
        app.stream_status.set_details(Some("cargo build".into()));
        app.resume_streaming();
        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.stream_status.header, "Running run_shell");
        assert_eq!(app.stream_status.details.as_deref(), Some("cargo build"));
    }

    #[test]
    fn preparing_event_sets_preparing_header() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::Preparing);
        assert_eq!(app.stream_status.header, "Preparing");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn text_delta_flips_header_to_responding() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ThinkingDelta("reasoning about it".into()));
        assert_eq!(app.stream_status.header, "Thinking");
        app.process_agent_event(AgentEvent::TextDelta("Here's the plan".into()));
        assert_eq!(app.stream_status.header, "Responding");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn thinking_delta_populates_details() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ThinkingDelta(
            "weighing options for the migration".into(),
        ));
        assert_eq!(app.stream_status.header, "Thinking");
        assert_eq!(
            app.stream_status.details.as_deref(),
            Some("weighing options for the migration")
        );
    }

    #[test]
    fn tool_executing_sets_running_header_with_arg_summary() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ToolExecuting {
            tool_call_id: "call-1".into(),
            name: "run_shell".into(),
            args: r#"{"command":"cargo test --all"}"#.into(),
        });
        assert_eq!(app.stream_status.header, "Running run_shell");
        assert_eq!(
            app.stream_status.details.as_deref(),
            Some("cargo test --all"),
            "details should prefer run_shell.command over a raw JSON blob"
        );
    }

    #[test]
    fn tool_result_clears_details_so_turn_doesnt_look_stuck_on_old_tool() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ToolExecuting {
            tool_call_id: "call-1".into(),
            name: "run_shell".into(),
            args: r#"{"command":"ls"}"#.into(),
        });
        app.process_agent_event(AgentEvent::ToolResult {
            tool_call_id: "call-1".into(),
            name: "run_shell".into(),
            result: "file1\nfile2".into(),
        });
        assert_eq!(app.stream_status.header, "Working");
        assert!(app.stream_status.details.is_none());
    }

    #[test]
    fn tool_output_delta_surfaces_latest_nonempty_line_as_details() {
        let mut app = make_app();
        app.enter_streaming();
        app.process_agent_event(AgentEvent::ToolOutputDelta {
            tool_call_id: "call-1".into(),
            name: "run_shell".into(),
            delta: "compiling crate A\n\ncompiling crate B\n".into(),
            is_stderr: false,
        });
        assert_eq!(
            app.stream_status.details.as_deref(),
            Some("compiling crate B")
        );
    }

    #[test]
    fn summarize_tool_args_prefers_known_fields_per_tool() {
        use super::super::events::summarize_tool_args;
        assert_eq!(
            summarize_tool_args("read_file", r#"{"path":"src/main.rs","offset":0}"#),
            "src/main.rs",
            "path is preferred over offset for read_file"
        );
        assert_eq!(
            summarize_tool_args("web_fetch", r#"{"url":"https://example.com"}"#),
            "https://example.com"
        );
    }

    #[test]
    fn summarize_tool_args_falls_back_to_first_string_for_unknown_tools() {
        use super::super::events::summarize_tool_args;
        assert_eq!(
            summarize_tool_args("custom_tool", r#"{"foo":"bar"}"#),
            "bar"
        );
    }

    #[test]
    fn summarize_tool_args_returns_raw_on_parse_failure() {
        use super::super::events::summarize_tool_args;
        assert_eq!(summarize_tool_args("run_shell", "not json"), "not json");
    }

    #[test]
    fn summarize_tool_args_empty_is_empty() {
        use super::super::events::summarize_tool_args;
        assert_eq!(summarize_tool_args("run_shell", ""), "");
    }
}
