use anyhow::Result;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use tracing::debug;

use crate::config::Config;

/// Per-path cache of the last-logged mtime so `load_identity` only emits a
/// log line on first load or when the file actually changes — it was firing
/// every turn from `build_system_prompt`.
static LOAD_LOG_CACHE: std::sync::LazyLock<
    Mutex<std::collections::HashMap<PathBuf, Option<SystemTime>>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn should_log_load(path: &std::path::Path, mtime: Option<SystemTime>) -> bool {
    // Recover from a poisoned mutex — the only state inside is a HashMap of
    // mtimes; nothing about a panicked insert leaves it inconsistent. A
    // poisoned-and-discarded lock would silently disable identity-load logs
    // for the rest of the process, which is a surprising failure mode.
    let mut cache = LOAD_LOG_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match cache.get(path) {
        Some(prev) if *prev == mtime => false,
        _ => {
            cache.insert(path.to_path_buf(), mtime);
            true
        }
    }
}

#[cfg(test)]
mod log_cache_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn should_log_load_dedupes_on_same_path_and_mtime() {
        let path = std::path::PathBuf::from("/tmp/borg-identity-test-fixture-1");
        let t = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert!(should_log_load(&path, t), "first call must log");
        assert!(!should_log_load(&path, t), "same mtime must not re-log");
    }

    #[test]
    fn should_log_load_relogs_on_mtime_change() {
        let path = std::path::PathBuf::from("/tmp/borg-identity-test-fixture-2");
        let t1 = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let t2 = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001));
        assert!(should_log_load(&path, t1), "first call must log");
        assert!(should_log_load(&path, t2), "mtime change must log");
        assert!(!should_log_load(&path, t2), "stable mtime suppresses again");
    }
}

const DEFAULT_IDENTITY: &str = "You are Borg, a personal AI assistant.\n";

/// Tokens that indicate IDENTITY.md is still on the unfilled scaffold —
/// either the raw template placeholders or any of its instructional
/// HTML comments.
const TEMPLATE_MARKERS: &[&str] = &[
    "{agent_name}",
    "{owner_name}",
    "<!-- Develop this during your first conversation",
    "<!-- Define this based on what works for",
    "<!-- What does",
];

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
        let mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        let content = std::fs::read_to_string(&path)?;
        if should_log_load(&path, mtime) {
            debug!(
                "Loaded identity from {} ({} bytes)",
                path.display(),
                content.len()
            );
        }
        Ok(content)
    } else {
        if should_log_load(&path, None) {
            debug!(
                "No identity file found at {}, using default",
                path.display()
            );
        }
        Ok(DEFAULT_IDENTITY.to_string())
    }
}

/// [`load_identity`], with the no-file default answering to `agent_name`
/// (the `user.agent_name` setting). The web onboarding names the agent
/// without seeding `IDENTITY.md`, and the name must still reach the prompt.
pub fn load_identity_named(agent_name: Option<&str>) -> Result<String> {
    let identity = load_identity()?;
    Ok(match agent_name.map(str::trim) {
        Some(name) if !name.is_empty() && identity == DEFAULT_IDENTITY => {
            default_identity_for(name)
        }
        _ => identity,
    })
}

fn default_identity_for(name: &str) -> String {
    format!("You are {name}, a personal AI assistant built on Borg.\n")
}

/// True when IDENTITY.md exists, isn't the bare default, and has no
/// remaining template scaffold markers. Used by the heartbeat
/// `identity_unpersonalized` augmenter to decide whether to nudge the
/// user to personalize.
pub fn is_personalized() -> bool {
    let path = match identity_path() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    is_personalized_content(&content)
}

/// Pure form of [`is_personalized`] — exposed so the augmenter and unit
/// tests can check arbitrary content without touching the filesystem.
pub fn is_personalized_content(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed == DEFAULT_IDENTITY.trim() {
        return false;
    }
    !TEMPLATE_MARKERS.iter().any(|m| content.contains(m))
}

/// Persist the agent's persona to `IDENTITY.md`.
///
/// Crash-safe: a truncating write that dies mid-way leaves a short or empty
/// file, and [`load_identity`] treats unreadable/empty content as "not
/// personalized" and silently falls back to [`DEFAULT_IDENTITY`] — so the
/// failure surfaces as "my persona was never set" rather than as an error.
/// The atomic write makes the file either the old persona or the new one.
pub fn save_identity(content: &str) -> Result<()> {
    let path = identity_path()?;
    // Serialized with renames, which read-modify-write the same file.
    let _guard = crate::agent_name::MEMORY_FILE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    coding::fs_atomic::atomic_write(&path, content.as_bytes())?;
    debug!("Saved IDENTITY.md ({} bytes)", content.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_identity_for_names_the_agent() {
        assert_eq!(
            default_identity_for("Borgzilla"),
            "You are Borgzilla, a personal AI assistant built on Borg.\n"
        );
    }

    #[test]
    fn load_identity_returns_non_empty_content() {
        // load_identity is the system-prompt source; whether it falls back to
        // DEFAULT_IDENTITY or reads the on-disk file, the result must be
        // non-empty so the system prompt is never blank. The actual
        // fallback-vs-disk branching is covered by the override-based tests
        // below; here we just pin the never-blank contract on the default
        // entry point.
        let loaded = load_identity().expect("load_identity must not error");
        assert!(
            !loaded.trim().is_empty(),
            "loaded identity must be non-empty so system prompt is never blank"
        );
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
    fn is_personalized_content_rejects_empty() {
        assert!(!is_personalized_content(""));
        assert!(!is_personalized_content("   \n\t  "));
    }

    #[test]
    fn is_personalized_content_rejects_default_identity() {
        assert!(!is_personalized_content(DEFAULT_IDENTITY));
        assert!(!is_personalized_content(
            "  You are Borg, a personal AI assistant.  \n"
        ));
    }

    #[test]
    fn is_personalized_content_rejects_raw_template() {
        let template = include_str!("../../interface/templates/IDENTITY.md");
        assert!(
            !is_personalized_content(template),
            "raw template should not count as personalized"
        );
    }

    #[test]
    fn is_personalized_content_rejects_partially_filled_template() {
        // Names substituted but scaffold comments still present — common
        // half-finished state where the file was scaffolded but the user
        // never edited the sections.
        let half = "# Borg\n\nYou are Borg, an AI personal assistant belonging to Mike.\n\n## Personality\n<!-- Develop this during your first conversation. Be specific about who you are. -->\n";
        assert!(!is_personalized_content(half));
    }

    #[test]
    fn is_personalized_content_accepts_filled_identity() {
        let filled = "# Borg\n\nYou are Borg, an AI personal assistant belonging to Mike.\n\n## Personality\nWry, direct, and a little impatient with hand-waving.\n\n## Communication Style\nShort sentences. No emoji unless asked.\n\n## What Matters\nMike ships fast and hates churn — bias toward small reversible changes.\n";
        assert!(
            is_personalized_content(filled),
            "fully filled identity should be personalized"
        );
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
