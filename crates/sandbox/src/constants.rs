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
///
/// The list is split across `cfg(unix)` and `cfg(windows)` because the
/// minimal set a child *actually needs to function* differs sharply:
/// a Windows process with no `USERPROFILE` / `SYSTEMROOT` / `WINDIR`
/// won't find user state or load core DLLs, while a Unix process with
/// those vars but no `PATH` / `HOME` is equally broken. Forwarding the
/// union on every OS would needlessly widen the trust surface — a Unix
/// child has no business reading `SYSTEMROOT`, etc.
#[cfg(unix)]
pub const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
    "TEMP", "TMP", "TERM",
];

/// Same allowlist as the Unix variant but with Windows-specific entries
/// (USERPROFILE, SYSTEMROOT, etc). See module docs for the rationale on
/// why these can't be a single shared list.
#[cfg(windows)]
pub const SAFE_ENV_VARS: &[&str] = &[
    // SECURITY: PATH on Windows is part of the DLL search path. A
    // sandboxed child that loads a DLL by name (most do) inherits
    // whatever planting surface the parent's PATH has. The sandbox
    // limits filesystem reach but does NOT clean the DLL search path —
    // any Tier-2 elevation should also normalize PATH to a known-good
    // value (System32 + the runtime's bin dir) before spawning the child.
    "PATH",
    // Process essentials — DLL loader bootstrap + working dir + temp.
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "COMSPEC",
    "TEMP",
    "TMP",
    "TZ",
    // User identity / profile roots (USERPROFILE is HOME's equivalent;
    // many Windows tools refuse to start without it).
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    // Program locations — needed by anything that resolves install paths
    // via env rather than HKLM (which most tools do).
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    // Architecture — build/test tools key off these for cross-compilation
    // and parallelism. Not identity-sensitive.
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_ARCHITEW6432",
    "NUMBER_OF_PROCESSORS",
    "OS",
    // Locale — same role as LANG/LC_* on Unix.
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    // NOTE: COMPUTERNAME intentionally NOT forwarded — leaks machine
    // identity to sandboxed scripts and isn't needed for normal tool
    // operation. The sandbox is meant to prevent fingerprinting.
];

/// Fallback list used when neither `cfg(unix)` nor `cfg(windows)` is set
/// (exotic targets — wasi, redox, etc.). Conservative — only PATH/TEMP.
#[cfg(not(any(unix, windows)))]
pub const SAFE_ENV_VARS: &[&str] = &["PATH", "TEMP", "TMP"];

/// Standard system paths whose binaries a sandboxed script may invoke
/// (`cat`, `grep`, `curl`, etc.). Used to widen Seatbelt's `process-exec`
/// rule beyond just the runtime binary.
pub const STANDARD_EXEC_PATHS: &[&str] =
    &["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];

/// Sensitive subpaths under `$HOME` that the Seatbelt profile denies reads
/// to as a defense-in-depth backstop, even though `file-read*` is broadly
/// allowed. Mirrors the manifest-level `blocked_paths` filter.
pub const SENSITIVE_HOME_PATHS: &[&str] = &[".ssh", ".aws", ".gnupg", ".config/gh"];
