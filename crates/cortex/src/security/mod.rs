//! Trust-boundary cluster: prompt-injection scoring (`sanitize`), input
//! validation, secret detection/resolution, and skill audit. Anything that
//! sits between untrusted input and the agent loop lives here.

pub mod advisory;
pub mod env_allowlist;
pub mod sanitize;
pub mod secrets;
pub mod secrets_resolve;
pub mod skill;
pub mod startup_audit;
pub mod validation;

/// Bound the size of a command string we put in the audit chain. Keeps the
/// hash-chained payload from ballooning if the model emits a multi-kilobyte
/// shell command, and replaces control bytes so the JSON stays well-formed.
///
/// Lives here rather than beside `run_shell` because the scheduled-task runner
/// in `borgd` emits the same denial events and must produce byte-identical
/// payloads.
///
/// `MAX_CHARS` is a *char* count, not a byte count. Worst-case byte size of the
/// truncated body is `MAX_CHARS × 4` (pure 4-byte UTF-8) plus the `…<truncated>`
/// suffix — well within JSON-payload sanity.
pub fn redact_command_for_audit(command: &str) -> String {
    const MAX_CHARS: usize = 256;
    let cleaned: String = command
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_CHARS {
        cleaned
    } else {
        let mut s = cleaned.chars().take(MAX_CHARS).collect::<String>();
        s.push_str("…<truncated>");
        s
    }
}
