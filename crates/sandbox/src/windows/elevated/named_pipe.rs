//! Named-pipe IPC between trampoline and elevated runner.
//!
//! Replaces the anonymous-pipe transport once Tier 2 is
//! active. Named pipes carry their own security descriptor, which
//! the trampoline can use to *authenticate* the caller — closing the
//! same-session same-integrity `DuplicateHandle` gap that the
//! anonymous-pipe transport leaves open.
//!
//! # Wire format
//!
//! Same `SpawnRequest` framing as [`super::super::ipc`]: `[u32 LE
//! length][JSON body]`. Reusing the framing means a future code path
//! that switches transports doesn't have to touch the serializer.
//!
//! # Pipe naming
//!
//! Pipes are named `\\.\pipe\borg-sandbox-{server-pid}-{nonce}`. The
//! server-PID prefix is purely diagnostic (lets an operator
//! `Get-CimInstance Win32_NamedPipe` and see which borgd owns the
//! pipe); the nonce is the actual unguessability primitive (96 bits
//! of entropy).
//!
//! # SDDL
//!
//! The pipe's security descriptor (built in
//! [`super::runner_client::create_control_server`]) is a single-ACE DACL
//! granting full access to the sandbox account SID only, so a
//! different-principal local process cannot `CreateFileW` the pipe at
//! all even if it guesses the name. Combined with the nonce in the name
//! and the connecting-PID check, this authenticates the caller without a
//! separate handshake.
//!
//! # Status
//!
//! This module ships the pipe name-generation helper; the
//! `CreateNamedPipeW` / `ConnectNamedPipe` / framed-IO plumbing lives in
//! [`super::runner_client`] (operator side) and [`super::runner_pipe`]
//! (runner side).

/// Per-spawn random nonce embedded in the pipe name. 96 bits of
/// entropy from the rand RNG — collision-resistant against a
/// concurrent same-host attacker enumerating pipe names.
pub const PIPE_NONCE_BYTES: usize = 12;

/// Build a unique pipe name for this trampoline invocation. Returns
/// the full `\\.\pipe\...` form ready for `CreateNamedPipeW`.
///
/// `server_pid` is for diagnostic readability only; the nonce is the
/// real entropy. Same-PID re-use is fine because the nonce is fresh.
pub fn pipe_name(server_pid: u32) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut nonce = [0u8; PIPE_NONCE_BYTES];
    for b in nonce.iter_mut() {
        *b = rng.random();
    }
    let mut hex = String::with_capacity(PIPE_NONCE_BYTES * 2);
    for byte in nonce {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!(r"\\.\pipe\borg-sandbox-{server_pid}-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_has_full_unc_prefix() {
        // Spec: `CreateNamedPipeW` requires the `\\.\pipe\` prefix
        // verbatim. A relative or short name would fail with
        // `ERROR_INVALID_NAME`, which is recoverable but adds noise.
        let n = pipe_name(42);
        assert!(n.starts_with(r"\\.\pipe\borg-sandbox-"), "got: {n}");
    }

    #[test]
    fn pipe_name_includes_server_pid_for_diagnostics() {
        // Spec: the server PID in the name is for operator readability
        // (running `Get-CimInstance Win32_NamedPipe` should show which
        // process owns which pipe). A PID-less name would still work,
        // but the diagnostic affordance is the reason it's there.
        let n = pipe_name(31415);
        assert!(n.contains("31415"), "expected PID 31415 in pipe name: {n}");
    }

    #[test]
    fn pipe_name_nonce_is_lowercase_hex_of_correct_length() {
        // Spec: 12 random bytes → 24 lowercase hex chars at the tail
        // of the name. A short / non-hex nonce would silently weaken
        // collision resistance.
        let n = pipe_name(1);
        let nonce = n.rsplit('-').next().expect("trailing nonce");
        assert_eq!(nonce.len(), PIPE_NONCE_BYTES * 2, "got: {nonce:?}");
        assert!(
            nonce
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "non-hex chars: {nonce:?}",
        );
    }

    #[test]
    fn two_consecutive_pipe_names_differ() {
        // Spec: nonce uniqueness is what guards against a malicious
        // local process pre-creating a pipe with the same name to
        // intercept the request. Reused nonces would defeat that.
        let a = pipe_name(1);
        let b = pipe_name(1);
        assert_ne!(a, b);
    }
}
