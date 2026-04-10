use anyhow::Result;
use std::path::PathBuf;
use tracing::debug;

use crate::config::Config;

const DEFAULT_IDENTITY: &str = "You are Borg, a personal AI assistant.\n";

pub fn identity_path() -> Result<PathBuf> {
    Config::identity_path()
}

pub fn load_identity() -> Result<String> {
    load_identity_with_override(None)
}

/// Load identity, optionally from an override path.
/// Used by gateway routing to support per-binding identity files.
pub fn load_identity_with_override(override_path: Option<&std::path::Path>) -> Result<String> {
    let path = if let Some(p) = override_path {
        p.to_path_buf()
    } else {
        identity_path()?
    };

    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        debug!(
            "Loaded identity from {} ({} bytes)",
            path.display(),
            content.len()
        );
        Ok(content)
    } else {
        debug!(
            "No identity file found at {}, using default",
            path.display()
        );
        Ok(DEFAULT_IDENTITY.to_string())
    }
}

pub fn save_identity(content: &str) -> Result<()> {
    let path = identity_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    debug!("Saved IDENTITY.md ({} bytes)", content.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_identity_is_non_empty() {
        assert!(!DEFAULT_IDENTITY.is_empty());
    }

    #[test]
    fn default_identity_contains_identity() {
        assert!(DEFAULT_IDENTITY.contains("Borg"));
        assert!(DEFAULT_IDENTITY.contains("assistant"));
    }

    #[test]
    fn identity_path_ends_with_identity_md() {
        let path = identity_path().unwrap();
        assert!(path.to_string_lossy().ends_with("IDENTITY.md"));
        assert!(path.to_string_lossy().contains(".borg"));
    }

    #[test]
    fn load_identity_returns_content() {
        // Should return either the file content or the default — never an error
        let identity = load_identity().unwrap();
        // Other tests may mutate the file, so we only check it's non-error
        let _ = identity;
    }

    #[test]
    fn save_and_load_identity_round_trip() {
        // Save original, modify, restore
        let original = load_identity().unwrap();

        let test_content = "# Test Identity\nThis is a test personality.";
        save_identity(test_content).unwrap();

        let loaded = load_identity().unwrap();
        assert_eq!(loaded, test_content);

        // Restore original
        save_identity(&original).unwrap();
    }

    #[test]
    fn load_identity_with_override_from_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# Custom Identity\nOverride content.").unwrap();
        let loaded = load_identity_with_override(Some(tmp.path())).unwrap();
        assert_eq!(loaded, "# Custom Identity\nOverride content.");
    }

    #[test]
    fn load_identity_with_override_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nonexistent.md");
        let loaded = load_identity_with_override(Some(&missing)).unwrap();
        assert_eq!(loaded, DEFAULT_IDENTITY);
    }

    #[test]
    fn load_identity_with_override_none_uses_default_path() {
        let loaded = load_identity_with_override(None).unwrap();
        assert!(!loaded.is_empty());
    }

    #[test]
    fn default_identity_is_single_line() {
        // Minimal identity line — personality is built during onboarding into ~/.borg/IDENTITY.md
        assert!(
            !DEFAULT_IDENTITY.contains("## "),
            "default identity should be minimal, not contain markdown sections"
        );
    }

    #[test]
    fn load_identity_with_override_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "").unwrap();
        let loaded = load_identity_with_override(Some(tmp.path())).unwrap();
        assert_eq!(loaded, "");
    }

    #[test]
    fn load_identity_with_override_unicode() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let unicode = "# 🤖 Bot\n你好世界\nEmoji: 🎉🔥";
        std::fs::write(tmp.path(), unicode).unwrap();
        let loaded = load_identity_with_override(Some(tmp.path())).unwrap();
        assert_eq!(loaded, unicode);
    }
}
