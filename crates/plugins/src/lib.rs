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
