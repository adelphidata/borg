//! Windows FFI smoke tests — Phase 12 partial coverage.
//!
//! These tests exercise the actual Win32 API calls (CreatePipe,
//! CreatePseudoConsole, CreateDesktopW, CryptProtectData, …) that
//! the unit-test layer can mock around. They only build + run on
//! Windows; the `windows-sandbox` CI lane defined in
//! `.github/workflows/ci.yml` covers the rest of the matrix.
//!
//! What this gets us:
//!
//! - **FFI signatures stay correct.** A wrong-type or wrong-feature
//!   regression that cross-compile somehow misses would surface here
//!   as a link or runtime error.
//! - **Resource lifecycle is sane.** The Drop impls + cleanup paths
//!   close handles in the right order; running them under a real
//!   kernel catches double-free / use-after-close before they reach
//!   a user.
//! - **Realistic happy paths work.** End-to-end round-trips
//!   through pipe + DPAPI + desktop creation prove the modules are
//!   compositionally correct, not just type-correct.
//!
//! What this still doesn't cover (deferred to a real Windows VM
//! validation pass, see ROADMAP "Windows sandbox — validate Tier 1
//! on a real host"):
//!
//! - **Restricted token + DACL ACE enforcement.** Spawning a child
//!   under the cap-SID token and asserting that the child cannot read
//!   `~/.ssh/...` requires a usable HKCU on the runner, a real test
//!   binary to spawn, and the ability to make file-read assertions
//!   the GH-actions sandbox doesn't necessarily allow.
//! - **WFP filter installation.** Requires admin; GitHub-hosted
//!   Windows runners run unelevated.
//! - **Sandbox-account `LogonUserW`.** Requires the sandbox accounts
//!   to exist (`borg service install --windows-sandbox` is the
//!   prerequisite, and that subcommand doesn't ship yet).

#![cfg(target_os = "windows")]

use sandbox::policy::SandboxPolicy;
use sandbox::windows::{conpty, desktop, dpapi, ipc, pipe};
use std::path::PathBuf;

#[test]
fn anonymous_pipe_spawn_request_round_trips_on_real_kernel() {
    // Spec: `pipe::send` writes a framed `SpawnRequest` to a real
    // Win32 anonymous pipe; `pipe::recv` reads it back through the
    // same kernel pipe. The in-memory `ipc::serialize_request` /
    // `deserialize_request` round-trip is unit-tested cross-platform;
    // this test is the only place we exercise the actual CreatePipe
    // + WriteFile + ReadFile + SetHandleInformation calls on real
    // hardware.
    let req = ipc::SpawnRequest {
        policy: SandboxPolicy {
            network: true,
            fs_read: vec![r"C:\Windows".into()],
            fs_write: vec![r"C:\temp\borg".into()],
            deny_read: vec![],
            deny_write: vec![],
            allowed_domains: vec!["api.github.com".into()],
        },
        tool_dir: PathBuf::from(r"C:\tools\test"),
        program: "cmd.exe".into(),
        args: vec!["/c".into(), "echo hi".into()],
    };
    let session = pipe::send(&req).expect("pipe::send");
    let parsed = pipe::recv(session.handle_value()).expect("pipe::recv");
    assert_eq!(parsed.policy.network, true);
    assert_eq!(parsed.policy.fs_read, req.policy.fs_read);
    assert_eq!(parsed.policy.allowed_domains, req.policy.allowed_domains);
    assert_eq!(parsed.program, req.program);
    assert_eq!(parsed.args, req.args);
    assert_eq!(parsed.tool_dir, req.tool_dir);
}

#[test]
fn isolated_desktop_create_and_drop_succeeds() {
    // Spec: `desktop::create_isolated` calls `CreateDesktopW` against
    // the real window-station and gets back a valid HDESK. The Drop
    // closes it cleanly. Per-spawn uniqueness is by 64 bits of
    // suffix entropy; creating two in a row should not collide.
    let a = desktop::create_isolated().expect("first desktop create");
    let b = desktop::create_isolated().expect("second desktop create");
    // Both should drop without panic; the kernel rejects close of a
    // desktop with bound threads, but we have none.
    drop(a);
    drop(b);
}

#[test]
fn dpapi_seal_unseal_round_trips_on_real_machine_key() {
    // Spec: `CryptProtectData(CRYPTPROTECT_LOCAL_MACHINE)` followed
    // by `CryptUnprotectData` returns the original bytes. The flag
    // matters — without it, a service-mode borgd running under a
    // different identity than the installer would fail to unseal.
    let plaintext = b"correct horse battery staple";
    let entropy = b"borg.sandbox.smoke.v1";
    let sealed = dpapi::seal(plaintext, entropy).expect("dpapi::seal");
    assert_ne!(
        sealed.as_slice(),
        plaintext,
        "ciphertext must not equal plaintext"
    );
    let recovered = dpapi::unseal(&sealed, entropy).expect("dpapi::unseal");
    assert_eq!(recovered.as_slice(), plaintext);
}

#[test]
fn dpapi_entropy_mismatch_is_an_error_not_a_wrong_decrypt() {
    // Spec: unsealing with a different `entropy` must error rather
    // than silently returning garbage. Otherwise the domain-separation
    // guarantee we rely on (a sandbox-credential blob can't be fed
    // to a different-purpose unseal path) wouldn't hold.
    let sealed = dpapi::seal(b"secret", b"entropy-A").expect("seal");
    assert!(dpapi::unseal(&sealed, b"entropy-B").is_err());
}

#[test]
fn pseudo_console_create_and_drop_succeeds() {
    // Spec: `CreatePseudoConsole` + Drop teardown work end-to-end.
    // The PTY allocator is a Windows 10 1809+ API; a failure here on
    // a `windows-latest` runner would mean the runner image is older
    // than expected (unlikely but useful to catch loudly).
    let pty = conpty::OwnedPseudoConsole::new().expect("ConPTY create");
    assert!(pty.hpcon() != 0);
    drop(pty);
}

#[test]
fn end_to_end_trampoline_argv_shape_includes_pipe_handle() {
    // Spec: `wrap_command` produces argv whose head is `[self_exe,
    // __windows-sandbox-spawn, --request-pipe, <decimal>]`. A
    // regression that drops the flag or its value would route
    // sandboxed calls to a clap-error in the main binary. Verifies
    // the full argv shape under real `current_exe()` resolution
    // (the unit tests pass in a fake path).
    use sandbox::policy::SandboxPolicy;
    let policy = SandboxPolicy::default();
    let args = vec!["script.py".to_string()];
    let cmd = policy.wrap_command("python3", &args, std::path::Path::new(r"C:\tools\test"));
    assert_eq!(cmd.args[0], sandbox::windows::SPAWN_SUBCOMMAND);
    assert_eq!(cmd.args[1], sandbox::windows::REQUEST_PIPE_ARG);
    // Third arg should parse as a HANDLE decimal — non-empty and a
    // base-10 integer.
    let handle_str = &cmd.args[2];
    assert!(!handle_str.is_empty(), "handle arg is empty: {cmd:?}");
    assert!(
        handle_str.parse::<usize>().is_ok(),
        "handle arg is not a decimal integer: {handle_str}",
    );
    // Round-trip via the trampoline parser. This actually reads the
    // pipe — we still own `cmd._resources` so the parent's read-end
    // is alive.
    let parsed = sandbox::windows::parse_trampoline_args(&cmd.args[1..])
        .expect("trampoline parser must succeed");
    assert_eq!(parsed.program, "python3");
    assert_eq!(parsed.args, args);
    assert_eq!(parsed.tool_dir, std::path::Path::new(r"C:\tools\test"));
}
