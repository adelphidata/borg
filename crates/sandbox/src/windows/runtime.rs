//! Tier 1 sandbox runtime — the trampoline subcommand's body.
//!
//! Called from `crates/interface/src/main.rs` after it has detected
//! the `__windows-sandbox-spawn` token and parsed argv into a
//! [`TrampolineInvocation`](super::TrampolineInvocation).
//!
//! Sequence (every step fail-closed):
//!
//! 1. Generate a fresh capability SID string + resolve to PSID.
//! 2. Build a restricted primary token from the current process token,
//!    adding the cap SID as a restricting SID.
//! 3. Resolve the policy into the concrete ACE batch.
//! 4. Apply ACEs to the filesystem (DACL DENY/ALLOW). Take the guard.
//! 5. Spawn the wrapped command under the restricted token, inheriting
//!    the trampoline's stdio.
//! 6. Wait for the child to exit.
//! 7. Drop the guard (reverts ACEs).
//! 8. Re-emit the child's exit code as our own.
//!
//! If any step fails, the trampoline returns an error and exits
//! non-zero — the wrapped command never runs unsandboxed.

use std::io;
use std::time::Instant;

use super::acl::apply_for_paths;
use super::conpty::{pump_parent_stdio, OwnedPseudoConsole};
use super::elevated::account::SandboxAccount;
use super::elevated::mode::{self, Tier};
use super::elevated::runner_client;
use super::elevated::runner_pipe;
use super::elevated::setup::SetupRecord;
use super::policy_resolve::resolve;
use super::process::{spawn_with_token, SpawnStdio};
use super::sid::{generate_random_cap_sid_string, LocalSid};
use super::token::for_current_with_cap_sid;
use super::{TrampolineInvocation, AUDIT_TARGET, RUNNER_SUBCOMMAND};

/// Runner-side entry point for the Tier 2 elevated backend. Dispatched
/// by `crates/interface/src/main.rs` when invoked as
/// `borg.exe __windows-sandbox-runner --pipe <name>` (which the operator
/// launched as the sandbox account). Connects to the pipe, brokers one
/// sandboxed child, and returns its exit code.
pub fn run_elevated_runner(pipe_name: &str) -> io::Result<u32> {
    tracing::info!(
        event = "sandbox_runner_started",
        target = AUDIT_TARGET,
        "Tier 2 runner connecting to operator pipe",
    );
    runner_pipe::serve(pipe_name)
}

/// Run the trampoline body, returning the wrapped child's exit code on
/// success.
pub fn run(invocation: &TrampolineInvocation) -> io::Result<u32> {
    let started = Instant::now();
    let policy_hash = policy_fingerprint(&invocation.policy);
    tracing::info!(
        event = "sandbox_spawn_started",
        target = AUDIT_TARGET,
        policy_hash = %policy_hash,
        program = %invocation.program,
        // arg_count + tool_dir give an operator-readable handle on
        // *which* sandbox call this was without exposing arg contents
        // (which may contain inline secrets like
        // `--password=...`).
        arg_count = invocation.args.len(),
        tool_dir = %invocation.tool_dir.display(),
    );
    let result = run_inner(invocation);
    let duration_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(code) => tracing::info!(
            event = "sandbox_spawn_completed",
            target = AUDIT_TARGET,
            policy_hash = %policy_hash,
            program = %invocation.program,
            exit_code = code,
            duration_ms,
        ),
        Err(e) => tracing::warn!(
            event = "sandbox_spawn_failed",
            target = AUDIT_TARGET,
            policy_hash = %policy_hash,
            program = %invocation.program,
            duration_ms,
            error = %e,
        ),
    }
    result
}

/// Cheap fingerprint of a SandboxPolicy for audit-trail correlation.
/// Same policy → same hash; different policy → different hash with
/// overwhelming probability. Truncated to 16 hex chars so audit logs
/// stay readable; this is not a cryptographic commitment, so the
/// SHA-256 truncation is fine.
fn policy_fingerprint(policy: &crate::policy::SandboxPolicy) -> String {
    use sha2::{Digest, Sha256};
    let body = serde_json::to_vec(policy).unwrap_or_default();
    let digest = Sha256::digest(&body);
    let mut out = String::with_capacity(16);
    for b in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Route the spawn through Tier 2 (elevated runner + sandbox account)
/// when `setup.json` is present and valid, otherwise Tier 1. Tier 2 is
/// strictly additive: any failure setting it up or running the session
/// falls back to the proven Tier 1 path so a half-rolled-out or broken
/// Tier 2 install never breaks `run_shell`.
fn run_inner(invocation: &TrampolineInvocation) -> io::Result<u32> {
    if let Tier::Tier2(record) = mode::detect() {
        match run_tier2(&record, invocation) {
            Ok(code) => return Ok(code),
            // Fall back to Tier 1 ONLY if Tier 2 failed before the child
            // was launched. A post-spawn failure means the command
            // already ran under the runner — re-running it here would
            // double-execute, so propagate the error instead.
            Err(e) if e.is_recoverable() => {
                tracing::warn!(
                    event = "sandbox_tier2_fallback",
                    target = AUDIT_TARGET,
                    error = %e.into_inner(),
                    "Tier 2 setup failed before spawn; falling back to Tier 1",
                );
            }
            Err(e) => {
                let err = e.into_inner();
                tracing::warn!(
                    event = "sandbox_tier2_failed",
                    target = AUDIT_TARGET,
                    error = %err,
                    "Tier 2 session failed after the child launched; not re-running under Tier 1",
                );
                return Err(err);
            }
        }
    }
    run_tier1(invocation)
}

/// Tier 2: launch the wrapped command under a dedicated sandbox account
/// via the elevated runner, brokered over a named pipe.
fn run_tier2(
    record: &SetupRecord,
    invocation: &TrampolineInvocation,
) -> Result<u32, runner_client::SessionError> {
    // current_exe failing is a pre-spawn setup error → recoverable.
    let self_exe = std::env::current_exe().map_err(|e| {
        runner_client::SessionError::PreSpawn(io::Error::other(format!("current_exe failed: {e}")))
    })?;
    let account = SandboxAccount::for_policy(&invocation.policy);
    let account_sid = match account {
        SandboxAccount::Offline => record.offline_account_sid.as_str(),
        SandboxAccount::Online => record.online_account_sid.as_str(),
    };
    tracing::debug!(
        account = account.account_name(),
        wfp = record.wfp_filters_installed,
        "routing spawn through Tier 2 elevated runner",
    );
    runner_client::run_session(
        &self_exe,
        RUNNER_SUBCOMMAND,
        account.account_name(),
        account_sid,
        invocation,
    )
}

fn run_tier1(invocation: &TrampolineInvocation) -> io::Result<u32> {
    // 1. Cap SID.
    let cap_sid_str = generate_random_cap_sid_string();
    let cap_sid = LocalSid::from_str_sid(&cap_sid_str)
        .map_err(|e| io::Error::other(format!("cap SID resolve failed: {e}")))?;
    tracing::debug!(sid = %cap_sid_str, "cap SID generated");

    // 2. Restricted primary token.
    let token = for_current_with_cap_sid(&cap_sid)
        .map_err(|e| io::Error::other(format!("restricted token build failed: {e}")))?;
    tracing::debug!("restricted token built");

    // 3. Resolve policy → ACE batch.
    let aces = resolve(&invocation.policy);
    tracing::debug!(count = aces.len(), "ACEs to apply");

    // 4. Apply ACEs. The guard drops at the end of this scope and
    //    reverts every ACE — placement matters: it must outlive the
    //    spawn+wait so the child sees the policy in effect, but not
    //    longer than necessary. We rely on the function-scope drop
    //    order: `_acl_guard` drops AFTER `child` because it was
    //    declared first (Rust drops in reverse declaration order).
    let _acl_guard = apply_for_paths(&cap_sid_str, &cap_sid, &aces)
        .map_err(|e| io::Error::other(format!("ACE apply failed: {e}")))?;

    // 5. Spawn under the restricted token. CWD = the parent's CWD
    //    (passed through by inheritance because we pass None).
    //    Environment block = inherit (None). The parent has already
    //    filtered env vars via the SAFE_ENV_VARS allowlist before
    //    re-execing us, so inheritance carries the filtered set.
    //
    //    stdio: when the trampoline's own stdout is a real console
    //    (interactive REPL), wrap the child in a pseudo-console so it
    //    gets a sandboxed terminal view rather than the operator's raw
    //    console, and pump bytes through. When stdout is a pipe (the
    //    common agent-driven tool call, where borgd captures output),
    //    fall back to inheriting handles — byte-for-byte the prior
    //    behavior, so non-interactive tools cannot regress.
    // When stdout is a real console (interactive REPL) wrap the child in
    // a pseudo-console so it gets a sandboxed terminal view and pump
    // bytes through; otherwise (the common captured-output tool call)
    // inherit handles byte-for-byte as before.
    let pty = if stdout_is_console() {
        Some(
            OwnedPseudoConsole::new()
                .map_err(|e| io::Error::other(format!("pseudo-console create failed: {e}")))?,
        )
    } else {
        None
    };
    let code = spawn_and_wait(&token, invocation, pty.as_ref())?;

    Ok(code)
    // 7-8. _acl_guard + `pty` drop here; caller maps `code` to
    //      process::exit().
}

/// Spawn the child and wait for exit. With `pty`, the child is attached
/// to the pseudo-console and the operator's stdio is pumped through it
/// for the child's lifetime; without it, the child inherits the
/// trampoline's stdio handles directly.
fn spawn_and_wait(
    token: &super::token::OwnedHandle,
    invocation: &TrampolineInvocation,
    pty: Option<&OwnedPseudoConsole>,
) -> io::Result<u32> {
    let stdio = match pty {
        Some(p) => SpawnStdio::Pty(p),
        None => SpawnStdio::Inherit,
    };
    // SAFETY: `token` (and `pty`, if present) outlive this call; the
    // child + its handles are owned by the returned SpawnedChild which we
    // wait on then drop.
    let child = unsafe {
        spawn_with_token(
            token.as_handle(),
            &invocation.program,
            &invocation.args,
            None,
            None,
            stdio,
        )
    }
    .map_err(|e| io::Error::other(format!("spawn failed: {e}")))?;
    tracing::debug!(pid = child.pid(), program = %invocation.program, pty = pty.is_some(), "child spawned");
    // Start the stdio pump (PTY path only) before waiting so output
    // drains live; join it after exit so the tail isn't dropped.
    let pump = pty.map(pump_parent_stdio);
    let code = child
        .wait_for_exit()
        .map_err(|e| io::Error::other(format!("wait failed: {e}")))?;
    if let Some(pump) = pump {
        pump.join();
    }
    tracing::debug!(pid = child.pid(), code, "child exited");
    Ok(code)
}

/// True if the trampoline's own stdout is a console (not a pipe/file).
/// `GetConsoleMode` succeeds only on console handles, so it's the
/// canonical "is this a TTY" probe on Windows.
fn stdout_is_console() -> bool {
    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: STD_OUTPUT_HANDLE is a documented constant; mode is a
    // stack-local out-param.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode: u32 = 0;
        GetConsoleMode(h, &mut mode) != FALSE
    }
}
