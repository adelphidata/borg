//! Plugin marketplace — catalog, installation, and verification of integrations.
//!
//! Manages one-click installation of messaging, email, and productivity plugins
//! with credential storage and file template extraction.
#![warn(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::approx_constant,
        clippy::assertions_on_constants,
        clippy::const_is_empty,
        clippy::expect_used,
        clippy::field_reassign_with_default,
        clippy::identity_op,
        clippy::items_after_test_module,
        clippy::len_zero,
        clippy::manual_range_contains,
        clippy::needless_borrow,
        clippy::needless_collect,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls,
        clippy::uninlined_format_args,
        clippy::unnecessary_cast,
        clippy::unnecessary_map_or,
        clippy::unwrap_used,
        clippy::useless_format,
        clippy::useless_vec
    )
)]

/// Plugin catalog with built-in integration definitions.
pub mod catalog;
/// Credential storage: keychain primary, on-disk JSON fallback.
pub mod credential_store;
/// Plugin installer: credential prompts, template extraction, keychain storage.
pub mod installer;
/// OS keychain operations for storing plugin credentials.
pub mod keychain;
/// Installation integrity verification via file hashes.
pub mod verifier;
/// Capability-sandboxed WebAssembly plugin packages.
pub mod wasm;

use serde::{Deserialize, Serialize};

/// The kind of plugin — determines where files are installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginKind {
    /// A messaging channel integration.
    Channel,
    /// A standalone tool integration.
    Tool,
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Channel => write!(f, "channel"),
            Self::Tool => write!(f, "tool"),
        }
    }
}

/// Integration category for grouping in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    /// Communication channels — how users talk to Borg (Telegram, Slack, Discord, etc.).
    Channels,
    /// Core skills — always-on essentials (browser, search, email, calendar).
    Core,
    /// Email clients and providers.
    Email,
    /// Developer tools (git, docker, databases, etc.).
    Developer,
    /// Productivity tools (Calendar, Notion, Linear).
    Productivity,
    /// General utilities (search, browser, weather, etc.).
    Utilities,
    /// Payments and wallets (Stripe Link, etc.).
    Payments,
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Channels => write!(f, "CHANNELS"),
            Self::Core => write!(f, "CORE"),
            Self::Email => write!(f, "EMAIL"),
            Self::Developer => write!(f, "DEVELOPER"),
            Self::Productivity => write!(f, "PRODUCTIVITY"),
            Self::Utilities => write!(f, "UTILITIES"),
            Self::Payments => write!(f, "PAYMENTS"),
        }
    }
}

/// Target platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Available on all platforms.
    All,
    /// macOS only (e.g., iMessage).
    MacOS,
}

impl Platform {
    /// Returns true if this platform matches the current OS.
    pub fn is_available(&self) -> bool {
        match self {
            Self::All => true,
            Self::MacOS => cfg!(target_os = "macos"),
        }
    }

    /// Human-readable platform label for the UI (e.g., "macOS only").
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::MacOS => Some("macOS only"),
        }
    }
}

/// Specification for a credential required by a plugin in the marketplace
/// catalog.
///
/// Distinct from `interface::plugins::CredentialSpec` (the legacy
/// `borg add` CLI flow's prompt-data type), which carries a free-text
/// `help` instead of `help_url` and lacks `is_optional`. Both intentionally
/// coexist; see the doc comment on the interface-side struct for the
/// rationale and the consolidation note.
#[derive(Debug, Clone)]
pub struct CredentialSpec {
    /// Environment variable or keychain key name.
    pub key: &'static str,
    /// Human-readable label shown during prompts.
    pub label: &'static str,
    /// URL to documentation for obtaining this credential.
    pub help_url: &'static str,
    /// Whether the credential can be skipped during setup.
    pub is_optional: bool,
}

/// A file template to be extracted during installation.
#[derive(Debug, Clone)]
pub struct TemplateFile {
    /// Path relative to the target base directory.
    pub relative_path: &'static str,
    /// Embedded file content to write.
    pub content: &'static str,
    /// Where this file should be installed.
    pub target: TemplateTarget,
}

/// Where template files are installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateTarget {
    /// Install to `~/.borg/channels/`.
    Channels,
    /// Install to `~/.borg/tools/`.
    Tools,
}

impl TemplateTarget {
    /// Base directory for this target type under the Borg data directory.
    pub fn base_dir(&self, data_dir: &std::path::Path) -> std::path::PathBuf {
        match self {
            TemplateTarget::Channels => data_dir.join("channels"),
            TemplateTarget::Tools => data_dir.join("tools"),
        }
    }
}

/// Metadata about a credential stored in the OS keychain during installation.
#[derive(Debug, Clone)]
pub struct CredentialEntry {
    /// Credential key name.
    pub key: String,
    /// Keychain service identifier.
    pub service: String,
    /// Keychain account name.
    pub account: String,
}

/// Result returned from a successful installation with user-facing notes.
#[derive(Debug, Clone, Default)]
pub struct InstallResult {
    /// User-facing notes to display after installation.
    pub notes: Vec<String>,
    /// Credentials that were stored in the keychain.
    pub credential_entries: Vec<CredentialEntry>,
    /// File hashes for integrity verification (path, sha256).
    pub file_hashes: Vec<(String, String)>,
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared helpers for tests that mutate process-global state.
    //!
    //! Any test that changes an env var, or whose outcome depends on which
    //! keychain helper binary `PATH` resolves, must hold [`env_lock`] for its
    //! whole body so parallel tests cannot observe each other's overrides.
    //! [`KeychainShim`] keeps such tests hermetic: keychain helpers resolve to
    //! a fake script and `BORG_HOME` points at a temp dir, so nothing ever
    //! touches the user's real login keychain or `~/.borg`.

    use std::ffi::{OsStr, OsString};
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

    /// RAII handle for the env-serialization lock.
    ///
    /// Wraps the `MutexGuard` so async tests can hold it across `.await`
    /// without tripping `clippy::await_holding_lock`: every `#[tokio::test]`
    /// here runs on its own per-test runtime, so a contended lock only parks
    /// that test's OS thread — it can never deadlock a shared executor. The
    /// serialization guarantee is unchanged: the guard (and the lock) lives
    /// until this handle drops.
    pub(crate) struct EnvLockGuard {
        _guard: MutexGuard<'static, ()>,
    }

    /// Global lock serializing env-mutating tests. Poisoning is ignored — the
    /// lock guards exclusivity, not data invariants.
    pub(crate) fn env_lock() -> EnvLockGuard {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        EnvLockGuard {
            _guard: LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// Sets (or unsets) one env var for the guard's lifetime, restoring the
    /// prior value on drop. Callers must hold [`env_lock`] first.
    pub(crate) struct EnvVarGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl EnvVarGuard {
        /// Override `key` with `value` until the guard drops.
        pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }

        /// Remove `key` from the environment until the guard drops.
        pub(crate) fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Service name the [`KeychainShim::find_succeeds`] script answers for.
    /// Scoping success to one service keeps concurrently running tests that
    /// probe other (nonexistent) services from seeing phantom credentials.
    pub(crate) const SHIM_SERVICE: &str = "borg-shim-svc";
    /// Secret value the [`KeychainShim::find_succeeds`] script prints.
    pub(crate) const SHIM_VALUE: &str = "kc-value";

    /// Redirects keychain helper invocations (`security` on macOS,
    /// `secret-tool` on Linux) to a fake shell script and points `BORG_HOME`
    /// at a fresh temp dir. Callers must hold [`env_lock`] first.
    #[cfg(unix)]
    pub(crate) struct KeychainShim {
        tmp: tempfile::TempDir,
        _guards: Vec<EnvVarGuard>,
    }

    #[cfg(unix)]
    impl KeychainShim {
        /// Install a fake keychain helper with the given `sh` body, prepended
        /// to `PATH` so it shadows the real binaries. `{{LOG}}` in the body is
        /// replaced with [`Self::log_path`]. The body should drain stdin
        /// (`cat >/dev/null`) if it must succeed on the Linux `secret-tool
        /// store` path, which pipes the secret in.
        pub(crate) fn fake_helper(body: &str) -> Self {
            let tmp = tempfile::TempDir::new().expect("shim tempdir");
            let bin = tmp.path().join("bin");
            std::fs::create_dir_all(&bin).expect("shim bin dir");
            let log = tmp.path().join("helper.log");
            let script = format!(
                "#!/bin/sh\n{}\n",
                body.replace("{{LOG}}", &log.display().to_string())
            );
            for name in ["security", "secret-tool"] {
                write_executable(&bin.join(name), &script);
            }
            let home = tmp.path().join("borg-home");
            std::fs::create_dir_all(&home).expect("shim home dir");
            let mut path = bin.into_os_string();
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            let guards = vec![
                EnvVarGuard::set("PATH", &path),
                EnvVarGuard::set("BORG_HOME", &home),
            ];
            Self {
                tmp,
                _guards: guards,
            }
        }

        /// Helper that always fails — forces the file fallback everywhere and
        /// guarantees `check`/`read` keychain misses.
        pub(crate) fn failing() -> Self {
            Self::fake_helper("exit 1")
        }

        /// Helper where credential lookups succeed — but only for
        /// [`SHIM_SERVICE`], printing [`SHIM_VALUE`]. Everything else fails.
        pub(crate) fn find_succeeds() -> Self {
            Self::fake_helper(concat!(
                "case \"$1\" in find-generic-password|lookup) ;; *) exit 1 ;; esac\n",
                "for a in \"$@\"; do\n",
                "  [ \"$a\" = \"borg-shim-svc\" ] && { printf 'kc-value\\n'; exit 0; }\n",
                "done\n",
                "exit 1",
            ))
        }

        /// `PATH` with no keychain helper at all. Keeps `sh`, `cat`, and
        /// `python3` resolvable via symlinks so unrelated concurrent tests
        /// that spawn subprocesses keep working.
        pub(crate) fn without_helpers() -> Self {
            let tmp = tempfile::TempDir::new().expect("shim tempdir");
            let bin = tmp.path().join("bin");
            std::fs::create_dir_all(&bin).expect("shim bin dir");
            for (name, target) in [
                ("sh", "/bin/sh"),
                ("cat", "/bin/cat"),
                ("python3", "/usr/bin/python3"),
            ] {
                if std::path::Path::new(target).exists() {
                    std::os::unix::fs::symlink(target, bin.join(name)).expect("shim symlink");
                }
            }
            let home = tmp.path().join("borg-home");
            std::fs::create_dir_all(&home).expect("shim home dir");
            let guards = vec![
                EnvVarGuard::set("PATH", bin.as_os_str()),
                EnvVarGuard::set("BORG_HOME", &home),
            ];
            Self {
                tmp,
                _guards: guards,
            }
        }

        /// Path the `{{LOG}}` placeholder in [`Self::fake_helper`] expands to.
        pub(crate) fn log_path(&self) -> std::path::PathBuf {
            self.tmp.path().join("helper.log")
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, content: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, content).expect("write shim script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim script");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_category_display_match_ui_vocabulary() {
        // These strings are UI/config vocabulary — lowercase kind nouns and
        // uppercase section headers. A Display that writes nothing (or the
        // wrong casing) breaks marketplace rendering and settings matching.
        let kinds: &[(PluginKind, &str)] =
            &[(PluginKind::Channel, "channel"), (PluginKind::Tool, "tool")];
        for (kind, expected) in kinds {
            assert_eq!(kind.to_string(), *expected);
        }

        let categories: &[(Category, &str)] = &[
            (Category::Channels, "CHANNELS"),
            (Category::Core, "CORE"),
            (Category::Email, "EMAIL"),
            (Category::Developer, "DEVELOPER"),
            (Category::Productivity, "PRODUCTIVITY"),
            (Category::Utilities, "UTILITIES"),
            (Category::Payments, "PAYMENTS"),
        ];
        for (category, expected) in categories {
            assert_eq!(category.to_string(), *expected);
        }
    }

    #[test]
    fn platform_label_flags_only_restricted_platforms() {
        // The marketplace badges restricted plugins; unrestricted ones must
        // show no badge at all, and the restricted label must name the OS.
        assert_eq!(Platform::All.label(), None);
        assert_eq!(Platform::MacOS.label(), Some("macOS only"));
    }

    #[test]
    fn platform_availability_tracks_host_os() {
        // `All` installs everywhere; `MacOS` only where the OS matches. On a
        // non-macOS host a `MacOS` platform reporting available would let the
        // installer write templates for a channel that can never run.
        assert!(Platform::All.is_available());
        assert_eq!(Platform::MacOS.is_available(), cfg!(target_os = "macos"));
    }
}

/// Events emitted during installation for progress display.
#[derive(Debug, Clone)]
pub enum InstallEvent {
    /// Installation has begun.
    Starting {
        /// Plugin identifier.
        id: String,
        /// Human-readable plugin name.
        name: String,
    },
    /// Writing template files to disk.
    WritingFiles {
        /// Plugin identifier.
        id: String,
    },
    /// Prompting user for a credential.
    CredentialPrompt {
        /// Plugin identifier.
        id: String,
        /// Credential label being prompted.
        label: String,
    },
    /// A credential was stored in the keychain.
    CredentialStored {
        /// Plugin identifier.
        id: String,
        /// Credential key that was stored.
        key: String,
    },
    /// Installation completed successfully.
    Complete {
        /// Plugin identifier.
        id: String,
    },
    /// Installation failed.
    Error {
        /// Plugin identifier.
        id: String,
        /// Error description.
        message: String,
    },
}
