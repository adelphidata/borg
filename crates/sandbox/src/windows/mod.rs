//! Windows sandbox backend.
//!
//! Borg's main process never calls Windows security APIs directly. Instead,
//! `SandboxPolicy::wrap_command` produces a [`SandboxCommand`] that re-execs
//! the current `borg` binary with a hidden subcommand
//! (`__windows-sandbox-spawn`). The trampoline subcommand reads a framed
//! [`ipc::SpawnRequest`] from an inherited anonymous pipe, applies the
//! restricted token + DACL ACEs (Tier 1) or routes through the elevated
//! runner (Tier 2), spawns the real child, and relays stdio + exit code.
//!
//! This mirrors how `sandbox-exec` and `bwrap` work on the Unix backends:
//! the wrapper is another process, not an in-process API. The cost is one
//! extra hop in the process tree; the win is that the existing
//! [`crate::policy::SandboxCommand`] API works unchanged for callers.
//!
//! # Wire format
//!
//! The trampoline argv is:
//!
//! ```text
//! borg.exe __windows-sandbox-spawn --request-pipe <handle_decimal>
//! ```
//!
//! and that is *all*. The policy, tool directory, wrapped program, and
//! wrapped args travel over the pipe as a length-prefixed JSON
//! `SpawnRequest` (see [`ipc`]). The HANDLE value on argv is a
//! kernel-object reference into the parent process's handle table;
//! decoding it requires actually opening / duplicating the parent's
//! handles, which the child receives via `bInheritHandles=TRUE`.
//!
//! # SECURITY — what this transport buys and does not buy
//!
//! Moving the policy off argv defeats *passive* command-line scraping
//! (`Win32_Process.CommandLine` via WMI, `tasklist /v`,
//! `Get-CimInstance`) — a co-tenant scraping the system no longer sees
//! deny-paths or allowed-domains in cleartext. The HANDLE value on argv
//! is meaningless to a passive reader.
//!
//! It does NOT defeat an *active* same-session same-integrity attacker
//! who can `OpenProcess(PROCESS_DUP_HANDLE, ...)` on the parent borg
//! process and `DuplicateHandle` the read-end. Default Windows ACLs
//! allow that. The elevated-runner named-pipe transport closes
//! that gap by authenticating the caller via the pipe's security
//! descriptor — until then, treat same-session same-integrity isolation
//! as a soft boundary that the OS sandbox layer (restricted token +
//! DACL) is the real defense against.

use crate::policy::{SandboxCommand, SandboxPolicy};

pub mod ace;
pub mod elevated;
pub mod ipc;
pub mod policy_resolve;
pub mod sid;

#[cfg(target_os = "windows")]
pub mod acl;
#[cfg(target_os = "windows")]
pub mod conpty;
#[cfg(target_os = "windows")]
pub mod desktop;
#[cfg(target_os = "windows")]
pub mod dpapi;
#[cfg(target_os = "windows")]
pub mod mitigation;
#[cfg(target_os = "windows")]
pub mod pipe;
#[cfg(target_os = "windows")]
pub mod process;
#[cfg(target_os = "windows")]
pub mod runtime;
#[cfg(target_os = "windows")]
pub mod token;
#[cfg(target_os = "windows")]
pub mod wfp;

/// Hidden subcommand name dispatched by `crates/interface/src/main.rs`
/// before clap sees argv. Two leading underscores mark this as an
/// internal subcommand — invoking it directly is unsupported and will
/// fail because it expects a parent process holding inheritable pipe
/// and stdio handles.
pub const SPAWN_SUBCOMMAND: &str = "__windows-sandbox-spawn";

/// Argument name carrying the decimal HANDLE value of the inherited
/// pipe read-end. The trampoline reads its [`ipc::SpawnRequest`] off
/// that handle.
pub const REQUEST_PIPE_ARG: &str = "--request-pipe";

/// Build the trampoline command for a Windows sandboxed launch.
///
/// `self_exe` is the absolute path to the running `borg` binary
/// (resolved via [`std::env::current_exe`] by the caller — accepting it
/// as a parameter keeps this function's pure-data inputs auditable).
///
/// On Windows this creates an anonymous pipe, writes the framed
/// `SpawnRequest` to its write-end, closes the write-end, marks the
/// read-end inheritable, and embeds the read-end's HANDLE value on
/// argv. The returned [`SandboxCommand`] carries the parent's copy of
/// the read-end inside `_resources`; the caller's `Command::spawn()`
/// inherits the handle into the child, and the parent's copy closes
/// when the `SandboxCommand` drops shortly after.
///
/// On non-Windows hosts this function panics — it is part of the
/// Windows-only call path. Callers should reach it only via
/// `cfg!(target_os = "windows")` runtime dispatch in
/// `SandboxPolicy::wrap_command`.
#[cfg(target_os = "windows")]
pub fn build_trampoline_command(
    self_exe: &std::path::Path,
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
    tool_dir: &std::path::Path,
) -> SandboxCommand {
    let request = ipc::SpawnRequest {
        policy: policy.clone(),
        tool_dir: tool_dir.to_path_buf(),
        program: program.to_string(),
        args: args.to_vec(),
    };
    match pipe::send(&request) {
        Ok(session) => SandboxCommand {
            program: self_exe.to_string_lossy().into_owned(),
            args: vec![
                SPAWN_SUBCOMMAND.to_string(),
                REQUEST_PIPE_ARG.to_string(),
                session.handle_decimal(),
            ],
            _resources: Some(std::sync::Arc::new(session)),
        },
        Err(e) => crate::policy::fail_closed_windows_public("pipe::send failed", &e.to_string()),
    }
}

/// Non-Windows stub. `wrap_command` never calls this on non-Windows at
/// runtime — the dispatch is `cfg!(target_os = "windows")` — but the
/// function still has to compile on every target because the dispatch
/// is runtime, not `#[cfg]`-conditional. Panics if anyone reaches it.
#[cfg(not(target_os = "windows"))]
pub fn build_trampoline_command(
    _self_exe: &std::path::Path,
    _policy: &SandboxPolicy,
    _program: &str,
    _args: &[String],
    _tool_dir: &std::path::Path,
) -> SandboxCommand {
    panic!("build_trampoline_command is Windows-only");
}

/// Parsed form of the trampoline's argv plus the `SpawnRequest`
/// fetched from the pipe. Constructed by the `__windows-sandbox-spawn`
/// subcommand dispatcher.
#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct TrampolineInvocation {
    /// Policy passed by the parent.
    pub policy: SandboxPolicy,
    /// Tool directory passed by the parent.
    pub tool_dir: std::path::PathBuf,
    /// Program the trampoline must spawn under the sandboxed token.
    pub program: String,
    /// Arguments for that program.
    pub args: Vec<String>,
}

/// Parse argv as seen by the trampoline subcommand, then read the
/// `SpawnRequest` off the inherited pipe.
///
/// Accepts the slice *after* the `__windows-sandbox-spawn` token has
/// already been stripped (the main-binary dispatcher swallows that
/// token before calling us).
///
/// Fails closed: any structural mismatch on argv, any pipe-read
/// failure, and any framing or decode failure all surface as
/// `TrampolineError`, and the trampoline must exit non-zero rather
/// than proceed with a partially-parsed invocation that might
/// silently drop a deny path.
#[cfg(target_os = "windows")]
pub fn parse_trampoline_args(
    argv_after_subcommand: &[String],
) -> Result<TrampolineInvocation, TrampolineError> {
    let handle_str = extract_request_pipe_arg(argv_after_subcommand)?;
    let handle = pipe::parse_handle_decimal(handle_str).map_err(TrampolineError::HandleArg)?;
    let request = pipe::recv(handle).map_err(TrampolineError::PipeRead)?;
    Ok(TrampolineInvocation {
        policy: request.policy,
        tool_dir: request.tool_dir,
        program: request.program,
        args: request.args,
    })
}

/// Extract the value following `--request-pipe`. Platform-neutral so
/// the argv-shape contract is testable on every CI runner.
pub fn extract_request_pipe_arg(argv: &[String]) -> Result<&str, TrampolineError> {
    let Some(first) = argv.first() else {
        return Err(TrampolineError::MissingValue(REQUEST_PIPE_ARG));
    };
    if first != REQUEST_PIPE_ARG {
        // Unknown flags fail closed (defense against a parent that
        // races a half-rolled-out version of borg.exe with the
        // trampoline binary).
        return Err(TrampolineError::UnknownArg(first.clone()));
    }
    argv.get(1)
        .map(String::as_str)
        .ok_or(TrampolineError::MissingValue(REQUEST_PIPE_ARG))
}

/// Errors that can occur parsing the trampoline argv or reading the
/// inherited pipe.
#[derive(Debug, thiserror::Error)]
pub enum TrampolineError {
    /// A flag like `--request-pipe` appeared without a following value.
    #[error("missing value for {0}")]
    MissingValue(&'static str),
    /// An unrecognized argument was seen. Refuse rather than ignoring —
    /// silent ignores have been a recurring source of sandbox bypasses.
    #[error("unknown trampoline argument: {0}")]
    UnknownArg(String),
    /// `--request-pipe` value did not parse as a HANDLE decimal.
    #[cfg(target_os = "windows")]
    #[error("handle argument: {0}")]
    HandleArg(pipe::ParseHandleError),
    /// Reading or parsing the `SpawnRequest` from the inherited pipe
    /// failed. Almost always indicates the parent crashed mid-write
    /// or that the parent and trampoline binaries are at incompatible
    /// versions.
    #[cfg(target_os = "windows")]
    #[error("pipe read: {0}")]
    PipeRead(pipe::RecvError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_request_pipe_arg_finds_value() {
        // Spec: `--request-pipe N` extracts `N` losslessly. This is
        // the only argv field the trampoline cares about; everything
        // else travels through the pipe.
        let argv = vec![REQUEST_PIPE_ARG.to_string(), "12345".to_string()];
        assert_eq!(extract_request_pipe_arg(&argv).unwrap(), "12345");
    }

    #[test]
    fn extract_request_pipe_arg_missing_value_errors() {
        // Spec: `--request-pipe` with no following value must error,
        // not default to "0" (which would be a valid HANDLE — stdin).
        let argv = vec![REQUEST_PIPE_ARG.to_string()];
        match extract_request_pipe_arg(&argv) {
            Err(TrampolineError::MissingValue(s)) => assert_eq!(s, REQUEST_PIPE_ARG),
            other => panic!("expected MissingValue, got {other:?}"),
        }
    }

    #[test]
    fn extract_request_pipe_arg_missing_flag_errors() {
        // Spec: argv with no `--request-pipe` at all is a structural
        // error. We must not silently fall through to a default.
        let argv: Vec<String> = vec![];
        match extract_request_pipe_arg(&argv) {
            Err(TrampolineError::MissingValue(s)) => assert_eq!(s, REQUEST_PIPE_ARG),
            other => panic!("expected MissingValue, got {other:?}"),
        }
    }

    #[test]
    fn extract_request_pipe_arg_rejects_unknown_leading_flag() {
        // Spec: a future borg version that adds `--debug` but is
        // invoked by an older trampoline must NOT silently ignore
        // unknown flags — that would defeat whatever invariant the
        // new flag carries.
        let argv = vec![
            "--debug".to_string(),
            REQUEST_PIPE_ARG.to_string(),
            "1".to_string(),
        ];
        match extract_request_pipe_arg(&argv) {
            Err(TrampolineError::UnknownArg(s)) => assert_eq!(s, "--debug"),
            other => panic!("expected UnknownArg, got {other:?}"),
        }
    }
}
