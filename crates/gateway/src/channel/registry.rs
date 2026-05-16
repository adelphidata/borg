use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::manifest::ChannelManifest;

// --- Generic ManifestRegistry ---

/// Trait for manifest types that can be used with `ManifestRegistry`.
pub trait ManifestItem: Sized {
    /// Load and parse a manifest from the given file path.
    fn load(path: &Path) -> Result<Self>;
    /// Return the unique name of this item.
    fn item_name(&self) -> &str;
    /// Filename to look for inside each subdirectory (e.g. "channel.toml").
    const MANIFEST_FILENAME: &'static str;
    /// Subdirectory under the data dir where items live (e.g. "channels").
    const SUBDIR: &'static str;
    /// Human-readable type label for log messages (e.g. "channel").
    const ITEM_TYPE: &'static str;
}

/// A manifest item together with the directory it was loaded from.
#[derive(Clone)]
pub struct RegisteredItem<M: Clone> {
    /// The parsed manifest.
    pub manifest: M,
    /// Directory containing the manifest and associated scripts.
    pub dir: PathBuf,
}

/// Generic registry that scans a directory for manifest-based items.
pub struct ManifestRegistry<M: Clone> {
    items: HashMap<String, RegisteredItem<M>>,
    base_dir: PathBuf,
}

impl<M: ManifestItem + Clone> ManifestRegistry<M> {
    /// Create a registry using the default data directory (`~/.borg/<subdir>`).
    pub fn new() -> Result<Self> {
        let base_dir = cortex::config::Config::data_dir()?.join(M::SUBDIR);

        let mut registry = Self {
            items: HashMap::new(),
            base_dir,
        };

        registry.scan()?;
        Ok(registry)
    }

    /// Create a registry scanning the given directory.
    pub fn with_dir(dir: PathBuf) -> Result<Self> {
        let mut registry = Self {
            items: HashMap::new(),
            base_dir: dir,
        };

        registry.scan()?;
        Ok(registry)
    }

    /// Re-scan the base directory, replacing all registered items.
    pub fn scan(&mut self) -> Result<()> {
        self.items.clear();

        let scanned = scan_manifest_dir(
            &self.base_dir,
            M::MANIFEST_FILENAME,
            M::load,
            |m| m.item_name().to_string(),
            M::ITEM_TYPE,
        )?;

        for (name, (manifest, dir)) in scanned {
            self.items.insert(name, RegisteredItem { manifest, dir });
        }

        Ok(())
    }

    /// Look up an item by name.
    pub fn get(&self, name: &str) -> Option<&RegisteredItem<M>> {
        self.items.get(name)
    }

    /// Iterate over all registered items.
    pub fn items(&self) -> impl Iterator<Item = &RegisteredItem<M>> {
        self.items.values()
    }
}

// --- ManifestItem impl for ChannelManifest ---

impl ManifestItem for ChannelManifest {
    fn load(path: &Path) -> Result<Self> {
        ChannelManifest::load(path)
    }
    fn item_name(&self) -> &str {
        &self.name
    }
    const MANIFEST_FILENAME: &'static str = "channel.toml";
    const SUBDIR: &'static str = "channels";
    const ITEM_TYPE: &'static str = "channel";
}

/// Legacy type alias for backward compatibility.
pub type RegisteredChannel = RegisteredItem<ChannelManifest>;

/// Registry of script-based channel integrations loaded from `~/.borg/channels/`.
pub struct ChannelRegistry {
    inner: ManifestRegistry<ChannelManifest>,
}

impl ChannelRegistry {
    /// Create a channel registry using the default channels directory.
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: ManifestRegistry::new()?,
        })
    }

    /// Create a channel registry scanning the given directory.
    pub fn with_dir(dir: PathBuf) -> Result<Self> {
        Ok(Self {
            inner: ManifestRegistry::with_dir(dir)?,
        })
    }

    /// Re-scan the channels directory for manifest changes.
    pub fn scan(&mut self) -> Result<()> {
        self.inner.scan()
    }

    /// Look up a channel by name.
    pub fn get(&self, name: &str) -> Option<&RegisteredChannel> {
        self.inner.get(name)
    }

    /// Return human-readable summaries of all registered channels.
    pub fn list_channels(&self) -> Vec<String> {
        self.inner
            .items()
            .map(|c| {
                format!(
                    "{}: {} (webhook: {})",
                    c.manifest.name,
                    c.manifest.description,
                    c.manifest.webhook_path()
                )
            })
            .collect()
    }

    /// Iterate over all registered channels.
    pub fn all_channels(&self) -> impl Iterator<Item = &RegisteredChannel> {
        self.inner.items()
    }
}

/// Generic directory scanner for manifest-based registries.
fn scan_manifest_dir<M, F, N>(
    dir: &Path,
    manifest_filename: &str,
    load_fn: F,
    name_fn: N,
    item_type: &str,
) -> Result<HashMap<String, (M, PathBuf)>>
where
    F: Fn(&Path) -> Result<M>,
    N: Fn(&M) -> String,
{
    let mut items = HashMap::new();

    if !dir.exists() {
        debug!("{item_type} directory does not exist: {}", dir.display());
        return Ok(items);
    }

    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Skip symlinks to prevent registering items from unexpected locations
        if path.is_symlink() {
            warn!(
                "Skipping symlinked {item_type} directory: {}",
                path.display()
            );
            continue;
        }

        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join(manifest_filename);
        if !manifest_path.exists() {
            continue;
        }

        match load_fn(&manifest_path) {
            Ok(manifest) => {
                let name = name_fn(&manifest);
                debug!("Registered {item_type}: {name} from {}", path.display());
                items.insert(name, (manifest, path));
            }
            Err(e) => {
                warn!(
                    "Failed to load {item_type} manifest {}: {e}",
                    manifest_path.display()
                );
            }
        }
    }

    debug!("Loaded {} {item_type}(s)", items.len());
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_lists_nothing() {
        let registry =
            ChannelRegistry::with_dir(PathBuf::from("/tmp/nonexistent_channels_dir")).unwrap();
        assert!(registry.list_channels().is_empty());
        assert!(registry.get("anything").is_none());
    }

    #[test]
    fn scan_nonexistent_dir_succeeds() {
        let mut registry =
            ChannelRegistry::with_dir(PathBuf::from("/tmp/nonexistent_channels_dir_xyz")).unwrap();
        registry
            .scan()
            .expect("scan must succeed even when the channels dir is missing");
        assert!(registry.list_channels().is_empty());
    }

    #[test]
    fn scan_dir_with_valid_channel() {
        let dir = tempfile::tempdir().unwrap();
        let channel_dir = dir.path().join("test-channel");
        std::fs::create_dir_all(&channel_dir).unwrap();
        std::fs::write(
            channel_dir.join("channel.toml"),
            r#"
name = "test-channel"
description = "A test channel"
"#,
        )
        .unwrap();

        let mut registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        registry.scan().unwrap();
        assert_eq!(registry.list_channels().len(), 1);
        assert!(registry.get("test-channel").is_some());
    }

    #[test]
    fn scan_valid_channel_via_with_dir() {
        let dir = tempfile::tempdir().unwrap();
        let channel_dir = dir.path().join("my-discord");
        std::fs::create_dir_all(&channel_dir).unwrap();
        std::fs::write(
            channel_dir.join("channel.toml"),
            "name = \"my-discord\"\ndescription = \"Discord integration\"\n",
        )
        .unwrap();

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert_eq!(registry.list_channels().len(), 1);
        assert!(registry.get("my-discord").is_some());
    }

    #[test]
    fn scan_skips_invalid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let channel_dir = dir.path().join("bad-channel");
        std::fs::create_dir_all(&channel_dir).unwrap();
        std::fs::write(channel_dir.join("channel.toml"), "not valid {{{{").unwrap();

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert!(registry.list_channels().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinked_subdirectory() {
        // Two tempdirs: real channel lives in `external`, `dir` only contains a symlink to it.
        // Security invariant: the registry must not register items reachable via symlink,
        // because that would let an attacker plant a channel in any readable location.
        let dir = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let real_channel = external.path().join("real");
        std::fs::create_dir_all(&real_channel).unwrap();
        std::fs::write(
            real_channel.join("channel.toml"),
            "name = \"real\"\ndescription = \"x\"\n",
        )
        .unwrap();

        std::os::unix::fs::symlink(&real_channel, dir.path().join("via-symlink")).unwrap();

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert!(
            registry.list_channels().is_empty(),
            "symlinked dirs must not register"
        );
        assert!(registry.get("real").is_none());
        assert!(registry.get("via-symlink").is_none());
    }

    #[test]
    fn scan_skips_plain_files() {
        // A loose file at the top level (matching the manifest filename or otherwise)
        // is not a channel and must be ignored without error.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("channel.toml"), "name = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("README"), "ignore me").unwrap();

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert!(registry.list_channels().is_empty());
    }

    #[test]
    fn scan_skips_dirs_without_manifest_filename() {
        let dir = tempfile::tempdir().unwrap();
        let stray = dir.path().join("not-a-channel");
        std::fs::create_dir_all(&stray).unwrap();
        // No channel.toml inside.
        std::fs::write(stray.join("notes.txt"), "scratch").unwrap();

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert!(registry.list_channels().is_empty());
        assert!(registry.get("not-a-channel").is_none());
    }

    #[test]
    fn rescan_replaces_previous_items() {
        let dir = tempfile::tempdir().unwrap();
        let chan_a = dir.path().join("a");
        std::fs::create_dir_all(&chan_a).unwrap();
        std::fs::write(
            chan_a.join("channel.toml"),
            "name = \"a\"\ndescription = \"x\"\n",
        )
        .unwrap();

        let mut registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert_eq!(
            registry.list_channels().len(),
            1,
            "with_dir scan should register exactly one channel"
        );
        assert!(registry.get("a").is_some());

        // Replace channel `a` with channel `b` on disk.
        std::fs::remove_dir_all(&chan_a).unwrap();
        let chan_b = dir.path().join("b");
        std::fs::create_dir_all(&chan_b).unwrap();
        std::fs::write(
            chan_b.join("channel.toml"),
            "name = \"b\"\ndescription = \"y\"\n",
        )
        .unwrap();

        registry.scan().unwrap();
        assert!(registry.get("a").is_none(), "prior items must be cleared");
        assert!(registry.get("b").is_some());
        assert_eq!(registry.list_channels().len(), 1);
    }

    #[test]
    fn multiple_channels_all_listed() {
        let dir = tempfile::tempdir().unwrap();
        for name in &["alpha", "beta", "gamma"] {
            let cd = dir.path().join(name);
            std::fs::create_dir_all(&cd).unwrap();
            std::fs::write(
                cd.join("channel.toml"),
                format!("name = \"{name}\"\ndescription = \"d\"\n"),
            )
            .unwrap();
        }

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        assert_eq!(registry.list_channels().len(), 3);
        for name in &["alpha", "beta", "gamma"] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }

        let names: std::collections::HashSet<_> = registry
            .all_channels()
            .map(|c| c.manifest.name.clone())
            .collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        assert!(names.contains("gamma"));
    }

    #[test]
    fn channel_definitions_format() {
        let dir = tempfile::tempdir().unwrap();
        let channel_dir = dir.path().join("slack");
        std::fs::create_dir_all(&channel_dir).unwrap();
        std::fs::write(
            channel_dir.join("channel.toml"),
            "name = \"slack\"\ndescription = \"Slack bot\"\n",
        )
        .unwrap();

        let registry = ChannelRegistry::with_dir(dir.path().to_path_buf()).unwrap();
        let list = registry.list_channels();
        assert_eq!(list.len(), 1);
        assert!(list[0].contains("slack"));
        assert!(list[0].contains("Slack bot"));
        assert!(list[0].contains("/webhook/slack"));
    }
}
