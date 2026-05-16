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

use super::acl::apply_for_paths;
use super::policy_resolve::resolve;
use super::process::spawn_with_token;
use super::sid::{generate_random_cap_sid_string, LocalSid};
use super::token::for_current_with_cap_sid;
use super::TrampolineInvocation;

/// Run the trampoline body, returning the wrapped child's exit code on
/// success.
pub fn run(invocation: &TrampolineInvocation) -> io::Result<u32> {
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
    // SAFETY: `token` is the OwnedHandle returned by
    // for_current_with_cap_sid above; it lives for the rest of this
    // function and is closed only when `token` drops, which happens
    // after `child` (declared later) drops.
    let child = unsafe {
        spawn_with_token(
            token.as_handle(),
            &invocation.program,
            &invocation.args,
            None,
            None,
        )
    }
    .map_err(|e| io::Error::other(format!("spawn failed: {e}")))?;
    tracing::debug!(pid = child.pid(), program = %invocation.program, "child spawned");

    // 6. Wait for exit.
    let code = child
        .wait_for_exit()
        .map_err(|e| io::Error::other(format!("wait failed: {e}")))?;
    tracing::debug!(pid = child.pid(), code, "child exited");

    Ok(code)
    // 7-8. _acl_guard drop here reverts ACEs; caller maps `code` to
    //      process::exit().
}
