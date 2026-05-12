//! Security-critical constants shared across sandbox profiles and the runner.
//!
//! Centralized so a security audit can scan a single file. Each list is
//! intentionally minimal — additions widen the trust surface and should come
//! with a rationale in the commit message.
//!
//! Note: [`SENSITIVE_HOME_PATHS`] overlaps semantically with
//! `cortex::config::security::blocked_paths`, but the sandbox copy is
//! defense-in-depth and intentionally redundant. Do not cross-link them.

/// Parent environment variables forwarded into sandboxed subprocesses.
/// Everything not in this list is stripped. Order is not significant.
pub const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
    "TEMP", "TMP", "TERM",
];

/// Standard system paths whose binaries a sandboxed script may invoke
/// (`cat`, `grep`, `curl`, etc.). Used to widen Seatbelt's `process-exec`
/// rule beyond just the runtime binary.
pub const STANDARD_EXEC_PATHS: &[&str] =
    &["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];

/// Sensitive subpaths under `$HOME` that the Seatbelt profile denies reads
/// to as a defense-in-depth backstop, even though `file-read*` is broadly
/// allowed. Mirrors the manifest-level `blocked_paths` filter.
pub const SENSITIVE_HOME_PATHS: &[&str] = &[".ssh", ".aws", ".gnupg", ".config/gh"];
