//! Windows sandbox backend.
//!
//! Borg's main process never calls Windows security APIs directly. Instead,
//! `SandboxPolicy::wrap_command` produces a [`SandboxCommand`] that re-execs
//! the current `borg` binary with a hidden subcommand
//! (`__windows-sandbox-spawn`). The trampoline subcommand reads the policy
//! and the wrapped command from argv, applies the restricted token + DACL
//! ACEs (Tier 1) or routes through the elevated runner (Tier 2), spawns the
//! real child, and relays stdio + exit code.
//!
//! This mirrors how `sandbox-exec` and `bwrap` work on the Unix backends:
//! the wrapper is another process, not an in-process API. The cost is one
//! extra hop in the process tree; the win is that the existing
//! [`crate::policy::SandboxCommand`] API works unchanged for callers.
//!
//! The trampoline-arg construction in this module is intentionally
//! platform-neutral so it can be tested on every host. The actual
//! [`CreateRestrictedToken`] / [`CreateProcessAsUserW`] logic lives in
//! submodules that are `cfg(target_os = "windows")` only.
//!
//! # SECURITY — provisional wire format
//!
//! The current argv-based policy transport is INTENDED FOR DEVELOPMENT
//! ONLY and must be replaced before release:
//!
//! - **TODO:** Move the policy JSON off the trampoline's argv.
//!   Windows command lines are observable to any same-session process
//!   via `Win32_Process.CommandLine` (WMI, `tasklist /v`,
//!   `Get-CimInstance`). The policy itself is not a credential, but
//!   `deny_read` / `deny_write` paths and `allowed_domains` give a
//!   local co-tenant a map of which paths/endpoints are sensitive —
//!   useful for a different-vector attack. Replace with an inheritable
//!   anonymous-pipe handle the trampoline reads on startup, or a
//!   transient HMAC-sealed file at `~/.borg/run/sandbox-<nonce>.json`
//!   with a DACL restricted to the spawning user, referenced on argv
//!   by path + one-shot nonce only.
//!
//! - **TODO:** Add caller authentication to the
//!   `__windows-sandbox-spawn` subcommand dispatcher. Today the name
//!   alone (with the documentation convention that double underscore
//!   means "internal") is a soft check, not a security boundary. Any
//!   process that can invoke `borg.exe` can hand-craft a
//!   `--policy '{"fs_write": ["C:\\"]}'` invocation and launder
//!   execution through whatever token shaping the trampoline applies.
//!   Use parent-PID validation (same borg.exe path, same logon
//!   session) or an inheritable shared-memory nonce that only the
//!   spawning parent could have written.

use crate::policy::{SandboxCommand, SandboxPolicy};

pub mod ace;
pub mod policy_resolve;
pub mod sid;

#[cfg(target_os = "windows")]
pub mod acl;
#[cfg(target_os = "windows")]
pub mod process;
#[cfg(target_os = "windows")]
pub mod runtime;
#[cfg(target_os = "windows")]
pub mod token;

/// Hidden subcommand name dispatched by `crates/interface/src/main.rs`
/// before clap sees argv. Two leading underscores mark this as an
/// internal subcommand — invoking it directly is unsupported and will
/// fail because it expects a parent process holding inheritable
/// stdio handles.
pub const SPAWN_SUBCOMMAND: &str = "__windows-sandbox-spawn";

/// Argument name carrying the JSON-encoded [`SandboxPolicy`].
pub const POLICY_ARG: &str = "--policy";
/// Argument name carrying the tool directory path (the same `tool_dir`
/// the Unix backends use for tool-scoped bind mounts / Seatbelt scope).
pub const TOOL_DIR_ARG: &str = "--tool-dir";
/// Argument separator — everything after `--` is the wrapped program and
/// its args, verbatim.
pub const PROGRAM_SEPARATOR: &str = "--";

/// Build the trampoline command for a Windows sandboxed launch.
///
/// `self_exe` is the absolute path to the running `borg` binary
/// (resolved via [`std::env::current_exe`] by the caller — accepting it
/// as a parameter keeps this function pure and trivially testable).
///
/// The returned [`SandboxCommand`] is shaped so the existing
/// `Command::new(cmd.program).args(cmd.args).spawn()` call sites in
/// `runner.rs` Just Work: spawning `borg.exe __windows-sandbox-spawn ...`
/// inherits stdin/stdout/stderr handles into the trampoline, which then
/// passes those same handles to `CreateProcessAsUserW` so the real child
/// reads/writes the parent's pipes directly. No userspace stdio relay.
pub fn build_trampoline_command(
    self_exe: &std::path::Path,
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
    tool_dir: &std::path::Path,
) -> SandboxCommand {
    // SandboxPolicy is plain-data (Vec<String> + bool fields); serde_json
    // cannot fail to serialize it. Panicking here means a future field was
    // added with a non-serializable type — that's a compile-time-detectable
    // bug we want loud, not a runtime degraded sandbox.
    #[allow(clippy::expect_used)]
    let policy_json = serde_json::to_string(policy)
        .expect("SandboxPolicy must always serialize — non-serializable field added?");

    let mut trampoline_args = Vec::with_capacity(args.len() + 6);
    trampoline_args.push(SPAWN_SUBCOMMAND.to_string());
    trampoline_args.push(POLICY_ARG.to_string());
    trampoline_args.push(policy_json);
    trampoline_args.push(TOOL_DIR_ARG.to_string());
    trampoline_args.push(tool_dir.to_string_lossy().into_owned());
    trampoline_args.push(PROGRAM_SEPARATOR.to_string());
    trampoline_args.push(program.to_string());
    trampoline_args.extend(args.iter().cloned());

    SandboxCommand {
        program: self_exe.to_string_lossy().into_owned(),
        args: trampoline_args,
    }
}

/// Parsed form of the trampoline's argv. Constructed by the
/// `__windows-sandbox-spawn` subcommand from its own
/// [`std::env::args`] in Step 3.
#[derive(Debug, Clone)]
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

/// Parse argv as seen by the trampoline subcommand.
///
/// Accepts the slice *after* the `__windows-sandbox-spawn` token has
/// already been stripped (the main-binary dispatcher swallows that token
/// before calling us). Returns an error on any structural mismatch so the
/// trampoline can fail closed — never proceed with a partially-parsed
/// invocation that might silently drop a deny path.
pub fn parse_trampoline_args(
    argv_after_subcommand: &[String],
) -> Result<TrampolineInvocation, TrampolineError> {
    let mut policy_json: Option<&str> = None;
    let mut tool_dir: Option<&str> = None;
    let mut i = 0;
    while i < argv_after_subcommand.len() {
        let arg = &argv_after_subcommand[i];
        if arg == PROGRAM_SEPARATOR {
            i += 1;
            break;
        }
        if arg == POLICY_ARG {
            let val = argv_after_subcommand
                .get(i + 1)
                .ok_or(TrampolineError::MissingValue(POLICY_ARG))?;
            policy_json = Some(val.as_str());
            i += 2;
            continue;
        }
        if arg == TOOL_DIR_ARG {
            let val = argv_after_subcommand
                .get(i + 1)
                .ok_or(TrampolineError::MissingValue(TOOL_DIR_ARG))?;
            tool_dir = Some(val.as_str());
            i += 2;
            continue;
        }
        return Err(TrampolineError::UnknownArg(arg.clone()));
    }

    let program = argv_after_subcommand
        .get(i)
        .ok_or(TrampolineError::MissingProgram)?
        .clone();
    let args: Vec<String> = argv_after_subcommand[i + 1..].to_vec();

    let policy_json = policy_json.ok_or(TrampolineError::MissingValue(POLICY_ARG))?;
    let tool_dir = tool_dir.ok_or(TrampolineError::MissingValue(TOOL_DIR_ARG))?;
    let policy: SandboxPolicy =
        serde_json::from_str(policy_json).map_err(TrampolineError::PolicyDecode)?;

    Ok(TrampolineInvocation {
        policy,
        tool_dir: std::path::PathBuf::from(tool_dir),
        program,
        args,
    })
}

/// Errors that can occur parsing the trampoline argv.
///
/// (Encoding errors are not represented: `SandboxPolicy` is plain data
/// that `serde_json::to_string` cannot fail to serialize, so
/// `build_trampoline_command` panics on that impossible case instead of
/// returning an error variant the type system would force callers to
/// handle.)
#[derive(Debug, thiserror::Error)]
pub enum TrampolineError {
    /// `serde_json::from_str(policy_json)` failed. Treated as fatal so
    /// the trampoline never runs the child under a half-formed policy.
    #[error("failed to decode sandbox policy: {0}")]
    PolicyDecode(serde_json::Error),
    /// A flag like `--policy` appeared without a following value.
    #[error("missing value for {0}")]
    MissingValue(&'static str),
    /// The required `--` separator was reached without a program name
    /// after it. The trampoline cannot proceed without something to spawn.
    #[error("missing program after `--` separator")]
    MissingProgram,
    /// An unrecognized argument was seen before `--`. Refuse rather than
    /// ignoring — silent ignores have been a recurring source of sandbox
    /// bypasses.
    #[error("unknown trampoline argument: {0}")]
    UnknownArg(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn sample_policy() -> SandboxPolicy {
        SandboxPolicy {
            network: false,
            fs_read: vec!["/etc/ssl".into()],
            fs_write: vec!["/tmp/work".into()],
            deny_read: vec!["/home/u/.ssh".into()],
            deny_write: vec!["/home/u/.borg".into()],
            allowed_domains: vec![],
        }
    }

    #[test]
    fn build_trampoline_command_round_trips_through_parse() {
        // Spec: anything wrap_command emits must be losslessly parseable by
        // the trampoline subcommand. A drift between encoder and decoder
        // would silently drop policy fields → sandbox bypass.
        let self_exe = PathBuf::from("/opt/borg/borg");
        let tool_dir = PathBuf::from("/var/borg/tools/echo");
        let program = "cmd.exe";
        let args = vec!["/c".to_string(), "echo hi".to_string()];
        let policy = sample_policy();

        let cmd = build_trampoline_command(&self_exe, &policy, program, &args, &tool_dir);

        assert_eq!(cmd.program, self_exe.to_string_lossy());
        assert_eq!(cmd.args[0], SPAWN_SUBCOMMAND);

        // Strip the leading subcommand token (the main dispatcher does this
        // before calling parse_trampoline_args).
        let argv_after = &cmd.args[1..];
        let parsed = parse_trampoline_args(argv_after).unwrap();

        assert_eq!(parsed.program, program);
        assert_eq!(parsed.args, args);
        assert_eq!(parsed.tool_dir, tool_dir);
        assert_eq!(parsed.policy.fs_read, policy.fs_read);
        assert_eq!(parsed.policy.fs_write, policy.fs_write);
        assert_eq!(parsed.policy.deny_read, policy.deny_read);
        assert_eq!(parsed.policy.deny_write, policy.deny_write);
        assert_eq!(parsed.policy.network, policy.network);
    }

    #[test]
    fn round_trip_preserves_network_true_and_allowed_domains() {
        // Spec gap from review: SandboxPolicy uses #[serde(default)] on
        // deny_read/deny_write/allowed_domains. A field rename or removal
        // would deserialize to vec![] silently. The original round-trip
        // test had network: false + allowed_domains: vec![], so the
        // missing-field-becomes-empty failure mode wouldn't fire there.
        // Round-tripping non-default values catches it: if the encoder
        // were to drop the field entirely, the decoder's default would
        // produce an empty vec and this assertion would fail.
        let policy = SandboxPolicy {
            network: true,
            fs_read: vec!["/tmp".into()],
            fs_write: vec![],
            deny_read: vec![],
            deny_write: vec![],
            allowed_domains: vec!["api.openai.com".into(), ".github.com".into()],
        };
        let cmd =
            build_trampoline_command(Path::new("/borg"), &policy, "cmd.exe", &[], Path::new("/t"));
        let parsed = parse_trampoline_args(&cmd.args[1..]).unwrap();
        assert!(parsed.policy.network, "network=true must round-trip");
        assert_eq!(parsed.policy.allowed_domains, policy.allowed_domains);
    }

    #[test]
    fn build_trampoline_command_preserves_program_arg_order() {
        // Spec: the wrapped program comes first after `--`, then its args
        // in declaration order. Reordering would change script behavior
        // because positional args (e.g. `bash -c`) depend on it.
        let self_exe = PathBuf::from("/opt/borg/borg");
        let cmd = build_trampoline_command(
            &self_exe,
            &SandboxPolicy::default(),
            "node",
            &["script.js".into(), "--flag".into(), "value".into()],
            Path::new("/t"),
        );

        let sep_idx = cmd
            .args
            .iter()
            .position(|a| a == PROGRAM_SEPARATOR)
            .expect("separator must be present");
        assert_eq!(cmd.args[sep_idx + 1], "node");
        assert_eq!(cmd.args[sep_idx + 2], "script.js");
        assert_eq!(cmd.args[sep_idx + 3], "--flag");
        assert_eq!(cmd.args[sep_idx + 4], "value");
    }

    #[test]
    fn build_trampoline_command_with_empty_args() {
        // Spec: programs with no args (e.g. plain `whoami`) must be
        // representable. Empty args is a valid case the trampoline needs
        // to handle, not an error.
        let cmd = build_trampoline_command(
            Path::new("/opt/borg/borg"),
            &SandboxPolicy::default(),
            "whoami",
            &[],
            Path::new("/t"),
        );
        let parsed = parse_trampoline_args(&cmd.args[1..]).unwrap();
        assert_eq!(parsed.program, "whoami");
        assert!(parsed.args.is_empty());
    }

    #[test]
    fn parse_rejects_unknown_arg() {
        // Spec: unknown args fail closed. A future borg version that adds
        // `--foo` but is invoked by an older trampoline must NOT silently
        // ignore it — that would defeat whatever invariant `--foo` carries.
        let argv = vec![
            POLICY_ARG.into(),
            "{}".into(),
            "--evil".into(),
            "x".into(),
            TOOL_DIR_ARG.into(),
            "/t".into(),
            PROGRAM_SEPARATOR.into(),
            "cmd.exe".into(),
        ];
        match parse_trampoline_args(&argv) {
            Err(TrampolineError::UnknownArg(s)) => assert_eq!(s, "--evil"),
            other => panic!("expected UnknownArg, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_missing_policy_value() {
        // Spec: `--policy` with no following value cannot default to "" or
        // "{}" — that would silently produce an empty (allow-nothing or
        // allow-everything, depending on Default impl) policy.
        let argv = vec![POLICY_ARG.into()];
        match parse_trampoline_args(&argv) {
            Err(TrampolineError::MissingValue(s)) => assert_eq!(s, POLICY_ARG),
            other => panic!("expected MissingValue, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_missing_tool_dir_value() {
        let argv = vec![POLICY_ARG.into(), "{}".into(), TOOL_DIR_ARG.into()];
        match parse_trampoline_args(&argv) {
            Err(TrampolineError::MissingValue(s)) => assert_eq!(s, TOOL_DIR_ARG),
            other => panic!("expected MissingValue, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_missing_program() {
        // Spec: `--` separator with nothing after it is a structural error.
        // We must not "default" to running cmd.exe or anything else.
        let argv = vec![
            POLICY_ARG.into(),
            "{}".into(),
            TOOL_DIR_ARG.into(),
            "/t".into(),
            PROGRAM_SEPARATOR.into(),
        ];
        match parse_trampoline_args(&argv) {
            Err(TrampolineError::MissingProgram) => {}
            other => panic!("expected MissingProgram, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_malformed_policy_json() {
        // Spec: malformed JSON must error, not parse to an empty policy.
        let argv = vec![
            POLICY_ARG.into(),
            "{not valid json".into(),
            TOOL_DIR_ARG.into(),
            "/t".into(),
            PROGRAM_SEPARATOR.into(),
            "cmd.exe".into(),
        ];
        match parse_trampoline_args(&argv) {
            Err(TrampolineError::PolicyDecode(_)) => {}
            other => panic!("expected PolicyDecode, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_preserves_policy_with_special_chars_in_paths() {
        // Spec: paths with spaces, quotes, and unicode (filenames are
        // user-controlled on Windows) must survive the JSON encode/decode
        // round-trip in argv. A broken serializer would either truncate
        // the path (sandbox enforces wrong path → bypass) or fail to parse.
        let policy = SandboxPolicy {
            fs_read: vec![
                r"C:\Users\Jürgen O'Brien\Docs".into(),
                r#"C:\path with "quote" inside"#.into(),
            ],
            ..Default::default()
        };
        let cmd =
            build_trampoline_command(Path::new("/borg"), &policy, "cmd.exe", &[], Path::new("/t"));
        let parsed = parse_trampoline_args(&cmd.args[1..]).unwrap();
        assert_eq!(parsed.policy.fs_read, policy.fs_read);
    }
}
