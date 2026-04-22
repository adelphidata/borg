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

/// Specification for a credential required by a plugin.
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- PluginKind --

    #[test]
    fn plugin_kind_display() {
        assert_eq!(PluginKind::Channel.to_string(), "channel");
        assert_eq!(PluginKind::Tool.to_string(), "tool");
    }

    #[test]
    fn plugin_kind_equality() {
        assert_eq!(PluginKind::Channel, PluginKind::Channel);
        assert_ne!(PluginKind::Channel, PluginKind::Tool);
    }

    // -- Category --

    #[test]
    fn category_display() {
        assert_eq!(Category::Channels.to_string(), "CHANNELS");
        assert_eq!(Category::Core.to_string(), "CORE");
        assert_eq!(Category::Email.to_string(), "EMAIL");
        assert_eq!(Category::Developer.to_string(), "DEVELOPER");
        assert_eq!(Category::Productivity.to_string(), "PRODUCTIVITY");
        assert_eq!(Category::Utilities.to_string(), "UTILITIES");
    }

    // -- Platform --

    #[test]
    fn platform_all_is_always_available() {
        assert!(Platform::All.is_available());
    }

    #[test]
    fn platform_all_has_no_label() {
        assert!(Platform::All.label().is_none());
    }

    #[test]
    fn platform_macos_has_label() {
        assert_eq!(Platform::MacOS.label(), Some("macOS only"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn platform_macos_available_on_macos() {
        assert!(Platform::MacOS.is_available());
    }

    // -- TemplateTarget --

    #[test]
    fn template_target_base_dir_channels() {
        let dir = std::path::Path::new("/home/user/.borg");
        assert_eq!(
            TemplateTarget::Channels.base_dir(dir),
            std::path::PathBuf::from("/home/user/.borg/channels")
        );
    }

    #[test]
    fn template_target_base_dir_tools() {
        let dir = std::path::Path::new("/home/user/.borg");
        assert_eq!(
            TemplateTarget::Tools.base_dir(dir),
            std::path::PathBuf::from("/home/user/.borg/tools")
        );
    }

    // -- InstallResult --

    #[test]
    fn install_result_default_is_empty() {
        let result = InstallResult::default();
        assert!(result.notes.is_empty());
        assert!(result.credential_entries.is_empty());
        assert!(result.file_hashes.is_empty());
    }

    // -- CredentialSpec --

    #[test]
    fn credential_spec_fields() {
        let spec = CredentialSpec {
            key: "SLACK_TOKEN",
            label: "Slack Bot Token",
            help_url: "https://api.slack.com",
            is_optional: false,
        };
        assert_eq!(spec.key, "SLACK_TOKEN");
        assert!(!spec.is_optional);
    }

    // -- Serde roundtrip --

    #[test]
    fn plugin_kind_serde_roundtrip() {
        let json = serde_json::to_string(&PluginKind::Channel).unwrap();
        let deserialized: PluginKind = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, PluginKind::Channel);
    }

    #[test]
    fn category_serde_roundtrip() {
        let json = serde_json::to_string(&Category::Email).unwrap();
        let deserialized: Category = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, Category::Email);
    }
}
