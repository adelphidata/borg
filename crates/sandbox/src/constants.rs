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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn safe_env_vars_contain_minimum_viable_set() {
        // PATH is the load-bearing one — a sandboxed child without PATH
        // can't resolve `python3`, `bash`, etc. and dies on spawn. Lock
        // the minimum so a "cleanup" PR can't accidentally drop it.
        let set: HashSet<&&str> = SAFE_ENV_VARS.iter().collect();
        assert!(set.contains(&"PATH"), "PATH must be forwarded");

        #[cfg(unix)]
        for required in ["HOME", "TERM"] {
            assert!(
                set.contains(&required),
                "{required} must be in Unix SAFE_ENV_VARS"
            );
        }
        #[cfg(windows)]
        for required in ["SYSTEMROOT", "USERPROFILE", "WINDIR"] {
            assert!(
                set.contains(&required),
                "{required} must be in Windows SAFE_ENV_VARS"
            );
        }
    }

    #[test]
    fn safe_env_vars_has_no_duplicates() {
        // Duplicates indicate a merge mistake and would hint at a wider
        // copy-paste error in the allowlist. Cheap invariant; encode it.
        let mut seen: HashSet<&&str> = HashSet::new();
        for v in SAFE_ENV_VARS {
            assert!(seen.insert(v), "duplicate env var in SAFE_ENV_VARS: {v}");
        }
    }

    #[test]
    fn safe_env_vars_omits_credential_carriers() {
        // SECURITY: the allowlist must NEVER include common secret carriers.
        // Forwarding any of these into a sandboxed child would defeat the
        // env-stripping defense. Regression check: a contributor adds e.g.
        // `OPENROUTER_API_KEY` "to fix this one script" and breaks the
        // whole trust model.
        const FORBIDDEN: &[&str] = &[
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "BORG_KEK_PASSPHRASE",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "SLACK_BOT_TOKEN",
            "TELEGRAM_BOT_TOKEN",
        ];
        for f in FORBIDDEN {
            assert!(
                !SAFE_ENV_VARS.contains(f),
                "SAFE_ENV_VARS must NEVER forward {f}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_safe_env_vars_omits_computername() {
        // The module doc explicitly calls out COMPUTERNAME as intentionally
        // dropped to prevent host-fingerprinting. Encode that intent.
        assert!(!SAFE_ENV_VARS.contains(&"COMPUTERNAME"));
    }

    #[test]
    fn standard_exec_paths_includes_homebrew_and_system_bins() {
        // Seatbelt widens `process-exec` to these paths — missing
        // `/usr/bin` would mean every `cat`/`grep`/`curl` invocation
        // from a sandboxed script fails. Lock the union.
        let set: HashSet<&&str> = STANDARD_EXEC_PATHS.iter().collect();
        for required in ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            assert!(
                set.contains(&required),
                "STANDARD_EXEC_PATHS missing {required}"
            );
        }
        // All entries must be absolute — a relative path would let a
        // sandboxed child execute binaries in a cwd it controls.
        for p in STANDARD_EXEC_PATHS {
            assert!(p.starts_with('/'), "STANDARD_EXEC_PATHS entry not absolute: {p}");
        }
    }

    #[test]
    fn sensitive_home_paths_covers_known_credential_dirs() {
        // The manifest-level `blocked_paths` filter mirrors this list;
        // dropping `.ssh` or `.aws` here would silently disable the
        // defense-in-depth backstop without breaking any test that doesn't
        // exist. Encode the spec.
        let set: HashSet<&&str> = SENSITIVE_HOME_PATHS.iter().collect();
        for required in [".ssh", ".aws", ".gnupg", ".config/gh"] {
            assert!(
                set.contains(&required),
                "SENSITIVE_HOME_PATHS missing {required}"
            );
        }
    }

    #[test]
    fn sensitive_home_paths_are_dotfiles_not_root_paths() {
        // Each entry MUST be a relative path under $HOME — a leading `/`
        // here would mis-construct the Seatbelt rule and produce a
        // never-matching pattern (silent security regression).
        for p in SENSITIVE_HOME_PATHS {
            assert!(!p.starts_with('/'), "{p} must be relative to $HOME");
            assert!(!p.is_empty(), "empty SENSITIVE_HOME_PATHS entry");
        }
    }
}
