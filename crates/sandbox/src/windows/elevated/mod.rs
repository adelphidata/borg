//! Tier 2 elevated runner — sandbox-account spawn brokering.
//!
//! The Tier 2 backend runs the wrapped command under a dedicated
//! low-privilege local user account (`BorgSandboxOnline` /
//! `BorgSandboxOffline`) rather than as the operator. Two motivations:
//!
//! 1. **Network isolation** — Windows Filtering Platform filters
//!    target user SIDs. A dedicated offline account whose outbound
//!    network is blocked at the WFP layer gives us network=false
//!    semantics that the operator's own account cannot have.
//! 2. **Filesystem isolation** — the sandbox account's default
//!    profile has no inherited access to the operator's home,
//!    SSH keys, browser cookies, etc. The cap-SID DACLs become
//!    *additive* (granting workspace access) rather than the only
//!    line of defense against a same-user breakout.
//!
//! The runner runs as the operator (no elevation needed at spawn
//! time), but the *setup* step that creates the accounts and WFP
//! rules requires admin (`borg service install --windows-sandbox`).
//!
//! # Auto-promote / fall-back
//!
//! `mode::detect()` reads `~/.borg/sandbox-windows/setup.json`. If
//! present and version-current, the trampoline routes spawns via the
//! elevated runner (Tier 2). Otherwise it falls back to Tier 1
//! (restricted-token + cap-SID DACLs, current behavior). The pattern
//! mirrors `security.kek_backend = auto` — no operator-visible
//! breakage if the elevated setup is incomplete or rolled back.
//!
//! # Privilege model
//!
//! The runner is launched *as the sandbox account* via
//! `CreateProcessWithLogonW` (in [`runner_client`]), which a non-admin
//! operator can do with the account password (unsealed from DPAPI at
//! spawn time). The runner — already running as the sandbox account —
//! then spawns the wrapped command with the Tier 1 hardening stack
//! (restricted token + cap-SID DACLs + job + mitigations) layered on
//! top. This avoids needing `SeAssignPrimaryTokenPrivilege` /
//! `SeIncreaseQuotaPrivilege`, which `CreateProcessAsUserW` would
//! require of the operator and a non-admin operator lacks.
//!
//! # Fail-safe fallback
//!
//! `runtime::run_inner` falls back to Tier 1 only when a Tier 2 session
//! fails *before* the child is launched ([`runner_client::SessionError`]
//! distinguishes pre- from post-spawn). A post-spawn failure propagates
//! rather than re-running the command, so a non-idempotent command is
//! never double-executed.
//!
//! # Status — implemented; awaiting real-host validation
//!
//! All operational paths are implemented (account provisioning, WFP
//! filters, the elevated runner + named-pipe IPC, ConPTY). What remains
//! is hands-on enforcement validation on an admin Windows VM — see the
//! "Windows sandbox — real-host validation pass" item in `docs/ROADMAP.md`.

pub mod account;
/// `borg service install --windows-sandbox` orchestration (accounts +
/// sealed creds + WFP + setup.json). Cross-platform shell; Windows FFI.
pub mod install;
/// Framed message protocol for the operator ⇄ elevated-runner conversation.
/// Cross-platform (pure data + framing); driven by the Windows-only
/// `runner_pipe` / `runner_client` transports.
pub mod ipc_runner;
pub mod mode;
pub mod named_pipe;
pub mod setup;

/// Operator-side client that launches the runner and streams the session.
#[cfg(target_os = "windows")]
pub mod runner_client;
/// Runner-side named-pipe server + child broker.
#[cfg(target_os = "windows")]
pub mod runner_pipe;

/// Sandbox-account provisioning. The strong-password generator is
/// cross-platform (unit-tested everywhere); the `NetUserAdd` / LSA FFI
/// that creates accounts is gated to Windows inside the module.
pub mod provision;
