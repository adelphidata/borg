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
//! # Status — scaffolding only
//!
//! The Phase 7 code in this directory lays out the module structure,
//! the on-disk `setup.json` schema, the named-pipe message protocol,
//! and the auto-promote detection logic. The operational paths
//! (`NetUserAdd` to create accounts, `LogonUserW` to obtain a sandbox-
//! account primary token, `CreateProcessAsUserW` with that token) are
//! marked TODO. Until they land, `mode::detect()` returns `Tier1`
//! unconditionally on every host — Tier 2 is unreachable, which is
//! the safe state.

pub mod account;
pub mod mode;
pub mod named_pipe;
pub mod setup;
