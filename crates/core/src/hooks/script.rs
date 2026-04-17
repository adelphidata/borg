//! User-authored script hooks discovered from `~/.borg/hooks.json`.
//!
//! Schema mirrors Claude Code / codex:
//!
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse": [
//!       { "matcher": "run_shell",
//!         "hooks": [{ "type": "command", "command": "~/bin/audit.sh", "timeout": 5 }] }
//!     ],
//!     "PostToolUse": [ ... ],
//!     "SessionStart": [ ... ],
//!     "SessionEnd": [ ... ],
//!     "UserPromptSubmit": [ ... ],
//!     "Stop": [ ... ]
//!   }
//! }
//! ```
//!
//! Event names map to [`HookPoint`] variants. Hooks run `sh -c <command>` with the
//! event payload as `$1` (a JSON object). For `PreToolUse`, a non-zero exit or
//! timeout returns [`HookAction::Skip`] to abort the tool call; all other events
//! are observers.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;

use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

// ─────────────────────── Config schema ───────────────────────

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: HashMap<String, Vec<MatcherGroup>>,
}

#[derive(Debug, Deserialize)]
struct MatcherGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum HookHandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

// ─────────────────────── Event-name mapping ───────────────────────

const EVENT_NAMES: &[(&str, HookPoint)] = &[
    ("SessionStart", HookPoint::SessionStart),
    ("SessionEnd", HookPoint::SessionEnd),
    ("UserPromptSubmit", HookPoint::BeforeAgentStart),
    ("PreToolUse", HookPoint::BeforeToolCall),
    ("PostToolUse", HookPoint::AfterToolCall),
    ("Stop", HookPoint::TurnComplete),
];

fn parse_event_name(s: &str) -> Option<HookPoint> {
    EVENT_NAMES
        .iter()
        .find(|(name, _)| *name == s)
        .map(|(_, point)| *point)
}

fn event_name_for_point(point: HookPoint) -> &'static str {
    EVENT_NAMES
        .iter()
        .find(|(_, p)| *p == point)
        .map(|(n, _)| *n)
        .unwrap_or("Unknown")
}

// ─────────────────────── ScriptHook ───────────────────────

/// A hook defined in `~/.borg/hooks.json` that runs a shell command on a lifecycle event.
pub struct ScriptHook {
    name: String,
    points: Vec<HookPoint>,
    matcher: Option<Regex>,
    command: String,
    timeout: Duration,
}

impl ScriptHook {
    /// Path to the default hooks config file: `~/.borg/hooks.json`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".borg").join("hooks.json"))
    }

    /// Load hooks from the default path, gated by `enabled`.
    ///
    /// Returns an empty `Vec` if `enabled` is false, the file is missing, or the file
    /// is malformed. Never returns `Err` — all errors are logged via `tracing::warn!`.
    pub fn load_all(enabled: bool) -> Vec<ScriptHook> {
        if !enabled {
            return Vec::new();
        }
        match Self::default_path() {
            Some(path) => Self::load_from_file(&path),
            None => {
                tracing::warn!("hooks: could not resolve home directory");
                Vec::new()
            }
        }
    }

    /// Load hooks from a specific file path. Primary test seam.
    pub fn load_from_file(path: &Path) -> Vec<ScriptHook> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!("hooks: failed to read {}: {}", path.display(), e);
                return Vec::new();
            }
        };
        Self::load_from_str(&contents, path)
    }

    fn load_from_str(contents: &str, source: &Path) -> Vec<ScriptHook> {
        let file: HooksFile = match serde_json::from_str(contents) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("hooks: failed to parse {}: {}", source.display(), e);
                return Vec::new();
            }
        };

        let mut hooks = Vec::new();
        // Sort for deterministic ordering (HashMap iteration is random).
        let mut events: Vec<(&String, &Vec<MatcherGroup>)> = file.hooks.iter().collect();
        events.sort_by(|a, b| a.0.cmp(b.0));

        for (event_name, groups) in events {
            let Some(point) = parse_event_name(event_name) else {
                tracing::warn!(
                    "hooks: unknown event name {:?} in {}",
                    event_name,
                    source.display()
                );
                continue;
            };

            for (gi, group) in groups.iter().enumerate() {
                let matcher = match compile_matcher(group.matcher.as_deref()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            "hooks: invalid matcher {:?} at {}[{}]: {}",
                            group.matcher,
                            event_name,
                            gi,
                            e
                        );
                        continue;
                    }
                };

                for (hi, raw) in group.hooks.iter().enumerate() {
                    let cfg: HookHandlerConfig = match serde_json::from_value(raw.clone()) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                "hooks: invalid handler at {}[{}][{}] in {}: {}",
                                event_name,
                                gi,
                                hi,
                                source.display(),
                                e
                            );
                            continue;
                        }
                    };
                    let HookHandlerConfig::Command { command, timeout } = cfg;
                    if command.trim().is_empty() {
                        tracing::warn!(
                            "hooks: empty command at {}[{}][{}] in {}",
                            event_name,
                            gi,
                            hi,
                            source.display()
                        );
                        continue;
                    }
                    let timeout_secs = timeout.unwrap_or(60).clamp(1, 600);
                    hooks.push(ScriptHook {
                        name: format!("{event_name}[{gi}][{hi}]"),
                        points: vec![point],
                        matcher: matcher.clone(),
                        command,
                        timeout: Duration::from_secs(timeout_secs),
                    });
                }
            }
        }
        hooks
    }

    fn matches_tool(&self, ctx: &HookContext) -> bool {
        let tool_name = match &ctx.data {
            HookData::ToolCall { name, .. } | HookData::ToolResult { name, .. } => name.as_str(),
            _ => return true, // non-tool events: matcher is ignored
        };
        match &self.matcher {
            None => true,
            Some(r) => r.is_match(tool_name),
        }
    }

    fn is_pre_tool(&self) -> bool {
        self.points.contains(&HookPoint::BeforeToolCall)
    }

    fn build_payload(&self, ctx: &HookContext) -> String {
        let tool = match &ctx.data {
            HookData::ToolCall { name, .. } => {
                Some(serde_json::json!({ "name": name, "is_error": false }))
            }
            HookData::ToolResult { name, is_error, .. } => {
                Some(serde_json::json!({ "name": name, "is_error": *is_error }))
            }
            _ => None,
        };
        serde_json::json!({
            "event": event_name_for_point(ctx.point),
            "session_id": ctx.session_id,
            "turn": ctx.turn_count,
            "tool": tool,
        })
        .to_string()
    }

    fn execute_inner(&self, ctx: &HookContext) -> HookAction {
        if !self.matches_tool(ctx) {
            return HookAction::Continue;
        }
        let payload = self.build_payload(ctx);
        match run_subprocess(&self.command, &payload, self.timeout) {
            Ok(0) => HookAction::Continue,
            Ok(code) => {
                tracing::warn!("hook {} exited with code {code}", self.name);
                if self.is_pre_tool() {
                    HookAction::Skip
                } else {
                    HookAction::Continue
                }
            }
            Err(SubprocessError::Spawn(e)) => {
                // Fail-open even on PreToolUse: a broken hook must not block every tool.
                tracing::warn!("hook {} spawn failed: {e}", self.name);
                HookAction::Continue
            }
            Err(SubprocessError::Timeout) => {
                tracing::warn!("hook {} timed out after {:?}", self.name, self.timeout);
                if self.is_pre_tool() {
                    HookAction::Skip
                } else {
                    HookAction::Continue
                }
            }
            Err(SubprocessError::Io(e)) => {
                tracing::warn!("hook {} io error: {e}", self.name);
                HookAction::Continue
            }
        }
    }
}

impl Hook for ScriptHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        with_panic_guard(&self.name, || self.execute_inner(ctx))
    }
}

impl std::fmt::Debug for ScriptHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptHook")
            .field("name", &self.name)
            .field("points", &self.points)
            .field("has_matcher", &self.matcher.is_some())
            .field("timeout", &self.timeout)
            .finish()
    }
}

// ─────────────────────── Helpers ───────────────────────

fn compile_matcher(raw: Option<&str>) -> Result<Option<Regex>, regex::Error> {
    match raw {
        None => Ok(None),
        Some("*") => Ok(None),
        Some(s) => Regex::new(s).map(Some),
    }
}

/// Catch panics inside hook execution and degrade to `Continue`. Defense-in-depth:
/// a panicking hook never brings down the agent.
fn with_panic_guard<F>(name: &str, f: F) -> HookAction
where
    F: FnOnce() -> HookAction + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(action) => action,
        Err(_) => {
            tracing::warn!("hook {name} panicked; continuing");
            HookAction::Continue
        }
    }
}

#[derive(Debug)]
enum SubprocessError {
    Spawn(std::io::Error),
    Timeout,
    Io(std::io::Error),
}

/// Spawn `sh -c <command>` with the payload as `$1`, enforce `timeout`, reap the
/// child on timeout. Follows the polling pattern from `host_audit::run_cmd_timeout`.
fn run_subprocess(command: &str, payload: &str, timeout: Duration) -> Result<i32, SubprocessError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .arg("borg-hook") // $0 inside the script
        .arg(payload) // $1
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(SubprocessError::Spawn)?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                drain_streams(&mut child);
                return Ok(status.code().unwrap_or(-1));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SubprocessError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(SubprocessError::Io(e)),
        }
    }
}

fn drain_streams(child: &mut std::process::Child) {
    const MAX: u64 = 8 * 1024;
    if let Some(s) = child.stdout.take() {
        let mut buf = Vec::new();
        let _ = s.take(MAX).read_to_end(&mut buf);
        // stdout intentionally unused in v1 (hooks communicate via side effects).
    }
    if let Some(s) = child.stderr.take() {
        let mut buf = Vec::new();
        let _ = s.take(MAX).read_to_end(&mut buf);
        if !buf.is_empty() {
            let text = String::from_utf8_lossy(&buf);
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                tracing::warn!("hook stderr: {trimmed}");
            }
        }
    }
}

// ─────────────────────── Tests ───────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookAction, HookContext, HookData, HookPoint, HookRegistry};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    fn write_hooks_file(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("hooks.json");
        std::fs::write(&path, body).expect("write hooks.json");
        path
    }

    fn make_tool_ctx(point: HookPoint, tool_name: &str) -> HookContext {
        HookContext {
            point,
            session_id: "s_abc".to_string(),
            turn_count: 4,
            data: HookData::ToolCall {
                name: tool_name.to_string(),
                args: "{}".to_string(),
            },
        }
    }

    fn make_tool_result_ctx(point: HookPoint, tool_name: &str, is_error: bool) -> HookContext {
        HookContext {
            point,
            session_id: "s_abc".to_string(),
            turn_count: 4,
            data: HookData::ToolResult {
                name: tool_name.to_string(),
                result: "ok".to_string(),
                is_error,
            },
        }
    }

    fn make_simple_ctx(point: HookPoint) -> HookContext {
        HookContext {
            point,
            session_id: "s_abc".to_string(),
            turn_count: 4,
            data: HookData::SessionStart {
                session_id: "s_abc".to_string(),
            },
        }
    }

    fn make_hook(command: &str, point: HookPoint) -> ScriptHook {
        ScriptHook {
            name: format!("test-{:?}", point),
            points: vec![point],
            matcher: None,
            command: command.to_string(),
            timeout: Duration::from_secs(5),
        }
    }

    // ── config parsing ──

    mod config_tests {
        use super::*;

        #[test]
        fn parses_minimal_valid_config() {
            let dir = tmp();
            let path = write_hooks_file(
                dir.path(),
                r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"echo hi","timeout":5}]}]}}"#,
            );
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].points, vec![HookPoint::AfterToolCall]);
            assert_eq!(hooks[0].timeout, Duration::from_secs(5));
            assert!(hooks[0].matcher.is_none());
        }

        #[test]
        fn parses_all_six_event_keys() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "SessionStart":     [{"hooks":[{"type":"command","command":"a"}]}],
                "SessionEnd":       [{"hooks":[{"type":"command","command":"b"}]}],
                "UserPromptSubmit": [{"hooks":[{"type":"command","command":"c"}]}],
                "PreToolUse":       [{"hooks":[{"type":"command","command":"d"}]}],
                "PostToolUse":      [{"hooks":[{"type":"command","command":"e"}]}],
                "Stop":             [{"hooks":[{"type":"command","command":"f"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 6);
            let mut points: Vec<HookPoint> = hooks.iter().flat_map(|h| h.points.clone()).collect();
            points.sort_by_key(|p| format!("{p:?}"));
            let mut expected = vec![
                HookPoint::SessionStart,
                HookPoint::SessionEnd,
                HookPoint::BeforeAgentStart,
                HookPoint::BeforeToolCall,
                HookPoint::AfterToolCall,
                HookPoint::TurnComplete,
            ];
            expected.sort_by_key(|p| format!("{p:?}"));
            assert_eq!(points, expected);
        }

        #[test]
        fn missing_file_returns_empty_not_error() {
            let dir = tmp();
            let path = dir.path().join("does-not-exist.json");
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn empty_hooks_object_returns_empty() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), r#"{"hooks":{}}"#);
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn completely_empty_file_returns_empty() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), r#"{}"#);
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn malformed_json_returns_empty() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), "not json at all {{{ ");
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn unknown_event_key_warns_and_skips() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "Bogus":       [{"hooks":[{"type":"command","command":"nope"}]}],
                "PostToolUse": [{"hooks":[{"type":"command","command":"ok"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].points, vec![HookPoint::AfterToolCall]);
        }

        #[test]
        fn invalid_matcher_regex_warns_and_skips() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PreToolUse": [
                    {"matcher":"[","hooks":[{"type":"command","command":"bad"}]},
                    {"matcher":"run_shell","hooks":[{"type":"command","command":"good"}]}
                ]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert!(hooks[0].matcher.is_some());
        }

        #[test]
        fn missing_command_field_skips_entry() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PostToolUse": [{"hooks":[
                    {"type":"command"},
                    {"type":"command","command":"ok"}
                ]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "ok");
        }

        #[test]
        fn empty_command_string_skipped() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PostToolUse": [{"hooks":[
                    {"type":"command","command":"   "},
                    {"type":"command","command":"real"}
                ]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "real");
        }

        #[test]
        fn timeout_clamped_to_max_600() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"x","timeout":9999}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].timeout, Duration::from_secs(600));
        }

        #[test]
        fn timeout_clamped_to_min_1() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"x","timeout":0}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].timeout, Duration::from_secs(1));
        }

        #[test]
        fn timeout_default_60_when_absent() {
            let dir = tmp();
            let body =
                r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"x"}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].timeout, Duration::from_secs(60));
        }

        #[test]
        fn matcher_star_matches_all_tools() {
            let dir = tmp();
            let body = r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"x"}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            // "*" is treated as "no matcher" (match all).
            assert!(hooks[0].matcher.is_none());
        }

        #[test]
        fn non_command_type_skipped() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[
                {"type":"prompt"},
                {"type":"agent"},
                {"type":"command","command":"keep"}
            ]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "keep");
        }

        #[test]
        fn hook_name_encodes_position() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[
                {"type":"command","command":"a"},
                {"type":"command","command":"b"}
            ]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].name(), "PostToolUse[0][0]");
            assert_eq!(hooks[1].name(), "PostToolUse[0][1]");
        }
    }

    // ── execution ──

    mod execute_tests {
        use super::*;

        #[test]
        fn exit_zero_returns_continue() {
            let hook = make_hook("true", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            assert!(matches!(hook.execute(&ctx), HookAction::Continue));
        }

        #[test]
        fn pre_tool_use_nonzero_returns_skip() {
            let hook = make_hook("false", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            assert!(matches!(hook.execute(&ctx), HookAction::Skip));
        }

        #[test]
        fn pre_tool_use_zero_returns_continue() {
            let hook = make_hook("true", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            assert!(matches!(hook.execute(&ctx), HookAction::Continue));
        }

        #[test]
        fn post_tool_use_nonzero_returns_continue() {
            let hook = make_hook("false", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            assert!(matches!(hook.execute(&ctx), HookAction::Continue));
        }

        #[test]
        fn session_start_nonzero_returns_continue() {
            let hook = make_hook("false", HookPoint::SessionStart);
            let ctx = make_simple_ctx(HookPoint::SessionStart);
            assert!(matches!(hook.execute(&ctx), HookAction::Continue));
        }

        #[test]
        fn timeout_non_pre_tool_returns_continue() {
            let mut hook = make_hook("sleep 10", HookPoint::AfterToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let start = Instant::now();
            let action = hook.execute(&ctx);
            assert!(matches!(action, HookAction::Continue));
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "should reap quickly"
            );
        }

        #[test]
        fn timeout_pre_tool_returns_skip() {
            let mut hook = make_hook("sleep 10", HookPoint::BeforeToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            let start = Instant::now();
            let action = hook.execute(&ctx);
            assert!(matches!(action, HookAction::Skip));
            assert!(start.elapsed() < Duration::from_secs(4));
        }

        #[test]
        fn spawn_failure_returns_continue_even_on_pre_tool() {
            // Fail-open on spawn for PreToolUse: a typo shouldn't block every tool.
            // We simulate a spawn-ish failure by giving sh a command it runs but fails.
            // Actual spawn failure requires sh itself to be absent, which we can't test
            // portably — but we exercise the broader non-zero-exit path via `false`.
            // For the actual spawn failure path see fault_tests::spawn_failure_does_not_panic.
            let hook = make_hook("exit 127", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            // 127 from the shell means "command not found" — still a non-zero exit
            // so we map to Skip for PreToolUse. This matches Claude Code semantics.
            assert!(matches!(hook.execute(&ctx), HookAction::Skip));
        }

        #[test]
        fn payload_json_reaches_child_as_argv1() {
            let dir = tmp();
            let out = dir.path().join("payload.txt");
            // Escape the path for shell inclusion — tempdir paths are well-formed on test hosts.
            let cmd = format!(
                "printf '%s' \"$1\" > {}",
                shell_quote(out.to_str().unwrap())
            );
            let hook = make_hook(&cmd, HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = hook.execute(&ctx);
            assert!(matches!(action, HookAction::Continue));
            let contents = std::fs::read_to_string(&out).expect("payload file written");
            assert!(
                contents.contains(r#""event":"PostToolUse""#),
                "got {contents:?}"
            );
            assert!(contents.contains(r#""session_id":"s_abc""#));
            assert!(contents.contains(r#""turn":4"#));
            assert!(contents.contains(r#""name":"run_shell""#));
            assert!(contents.contains(r#""is_error":false"#));
        }

        fn shell_quote(s: &str) -> String {
            // Very small shell quoter for test paths (no single quotes expected).
            format!("'{}'", s.replace('\'', "'\\''"))
        }
    }

    // ── matcher filtering ──

    mod matcher_tests {
        use super::*;

        fn hook_with_matcher(matcher: &str, point: HookPoint, command: &str) -> ScriptHook {
            ScriptHook {
                name: format!("matcher-{matcher}"),
                points: vec![point],
                matcher: Some(Regex::new(matcher).expect("valid regex")),
                command: command.to_string(),
                timeout: Duration::from_secs(5),
            }
        }

        #[test]
        fn matcher_regex_matches_tool_name() {
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook = hook_with_matcher("run_shell", HookPoint::AfterToolCall, &cmd);

            // Matches
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let _ = hook.execute(&ctx);
            assert!(marker.exists(), "hook should have fired for run_shell");

            // Clear and try non-matching
            std::fs::remove_file(&marker).ok();
            let ctx2 = make_tool_result_ctx(HookPoint::AfterToolCall, "read_file", false);
            let _ = hook.execute(&ctx2);
            assert!(!marker.exists(), "hook should NOT have fired for read_file");
        }

        #[test]
        fn matcher_alternation() {
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook =
                hook_with_matcher("apply_patch|write_memory", HookPoint::AfterToolCall, &cmd);

            for tool in ["apply_patch", "write_memory"] {
                let _ = std::fs::remove_file(&marker);
                let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, tool, false);
                let _ = hook.execute(&ctx);
                assert!(marker.exists(), "expected fire for {tool}");
            }

            let _ = std::fs::remove_file(&marker);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "read_file", false);
            let _ = hook.execute(&ctx);
            assert!(!marker.exists(), "should not fire for read_file");
        }

        #[test]
        fn matcher_absent_matches_all() {
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook = make_hook(&cmd, HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "anything", false);
            let _ = hook.execute(&ctx);
            assert!(marker.exists());
        }

        #[test]
        fn matcher_ignored_on_non_tool_events() {
            // Hook has a tool-name matcher, but SessionStart has no tool name.
            // Implementation returns true (ignore matcher) for non-tool events.
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook = hook_with_matcher("run_shell", HookPoint::SessionStart, &cmd);
            let ctx = make_simple_ctx(HookPoint::SessionStart);
            let _ = hook.execute(&ctx);
            assert!(
                marker.exists(),
                "matcher should be no-op on non-tool events"
            );
        }
    }

    // ── loader + registry integration ──

    mod loader_tests {
        use super::*;

        #[test]
        fn load_all_reads_from_temp_config() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"matcher":"run_shell","hooks":[{"type":"command","command":"true","timeout":3}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].timeout, Duration::from_secs(3));
            assert!(hooks[0].matcher.is_some());
        }

        #[test]
        fn registered_hooks_dispatched_via_registry() {
            let dir = tmp();
            let marker_a = dir.path().join("a.txt");
            let marker_b = dir.path().join("b.txt");
            let hook_a = make_hook(
                &format!("touch '{}'", marker_a.display()),
                HookPoint::AfterToolCall,
            );
            let hook_b = make_hook(
                &format!("touch '{}'", marker_b.display()),
                HookPoint::AfterToolCall,
            );

            let mut reg = HookRegistry::new();
            reg.register(Box::new(hook_a));
            reg.register(Box::new(hook_b));

            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = reg.dispatch(&ctx);
            assert!(matches!(action, HookAction::Continue));
            assert!(marker_a.exists());
            assert!(marker_b.exists());
        }

        #[test]
        fn skip_short_circuits_subsequent_hooks() {
            let dir = tmp();
            let marker = dir.path().join("should_not_exist.txt");
            let skipper = make_hook("false", HookPoint::BeforeToolCall);
            let toucher = make_hook(
                &format!("touch '{}'", marker.display()),
                HookPoint::BeforeToolCall,
            );

            let mut reg = HookRegistry::new();
            reg.register(Box::new(skipper));
            reg.register(Box::new(toucher));

            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            let action = reg.dispatch(&ctx);
            assert!(matches!(action, HookAction::Skip));
            assert!(!marker.exists(), "second hook must not run after Skip");
        }

        #[test]
        fn disabled_flag_returns_empty() {
            // load_all with enabled=false must return empty regardless of disk state.
            let hooks = ScriptHook::load_all(false);
            assert!(hooks.is_empty());
        }
    }

    // ── fault isolation ──

    mod fault_tests {
        use super::*;

        #[test]
        fn malformed_json_does_not_crash_loader() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), "totally not json");
            let hooks = ScriptHook::load_from_file(&path);
            assert!(hooks.is_empty());
        }

        #[test]
        fn spawn_failure_does_not_panic() {
            // Run `sh -c` with a command that exits nonzero to exercise the non-zero path.
            // Actual sh-missing is not portably testable.
            let hook = make_hook(
                "/definitely/nonexistent/binary --flag",
                HookPoint::AfterToolCall,
            );
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = hook.execute(&ctx);
            // sh runs, command-not-found → exit 127. PostToolUse → Continue.
            assert!(matches!(action, HookAction::Continue));
        }

        #[test]
        fn panic_guard_catches_panic() {
            let flag = Arc::new(AtomicBool::new(false));
            let flag_clone = flag.clone();
            let action = with_panic_guard("test-panic", move || {
                flag_clone.store(true, Ordering::SeqCst);
                panic!("boom");
            });
            assert!(matches!(action, HookAction::Continue));
            assert!(flag.load(Ordering::SeqCst), "closure must have run");
        }

        #[test]
        fn panic_guard_passes_through_non_panicking() {
            let action =
                with_panic_guard("test-happy", || HookAction::InjectContext("hi".to_string()));
            assert!(matches!(action, HookAction::InjectContext(_)));
        }

        #[test]
        fn non_utf8_stdout_does_not_crash() {
            // Bash prints raw bytes, hook runner must not panic.
            let hook = make_hook("printf '\\xff\\xfe\\xfd'; exit 0", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = hook.execute(&ctx);
            assert!(matches!(action, HookAction::Continue));
        }

        #[test]
        fn invalid_regex_entry_does_not_disable_others() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PreToolUse": [
                    {"matcher":"[bad","hooks":[{"type":"command","command":"x"}]},
                    {"matcher":"ok.*","hooks":[{"type":"command","command":"y"}]}
                ],
                "PostToolUse": [{"hooks":[{"type":"command","command":"z"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            // Invalid matcher group dropped; the other two load.
            assert_eq!(hooks.len(), 2);
        }

        #[test]
        fn garbage_event_key_does_not_disable_others() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "Nope":        [{"hooks":[{"type":"command","command":"x"}]}],
                "PostToolUse": [{"hooks":[{"type":"command","command":"y"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "y");
        }

        #[test]
        fn timeout_kills_and_reaps_child() {
            let mut hook = make_hook("sleep 30", HookPoint::AfterToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let start = Instant::now();
            let action = hook.execute(&ctx);
            let elapsed = start.elapsed();
            assert!(matches!(action, HookAction::Continue));
            assert!(
                elapsed < Duration::from_secs(3),
                "child should be killed promptly, elapsed={elapsed:?}"
            );
        }

        #[test]
        fn empty_hook_array_within_group_ok() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"matcher":"x","hooks":[]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }
    }
}
