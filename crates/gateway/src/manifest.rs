use serde::{Deserialize, Serialize};
use std::path::Path;

/// Operating mode for a channel integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelMode {
    /// Channel receives messages via HTTP webhooks.
    Webhook,
    /// Channel actively polls for new messages.
    Poll,
}

impl Default for ChannelMode {
    fn default() -> Self {
        Self::Webhook
    }
}

/// Parsed representation of a `channel.toml` manifest file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelManifest {
    /// Unique channel name (used in webhook paths and registry keys).
    pub name: String,
    /// Human-readable description of the channel.
    pub description: String,
    /// Script runtime (e.g. "python", "node", "bash").
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Script paths for inbound/outbound/verify/poll operations.
    #[serde(default)]
    pub scripts: ScriptsSection,
    /// Sandbox permissions for script execution.
    #[serde(default)]
    pub sandbox: SandboxSection,
    /// Authentication environment variable names.
    #[serde(default)]
    pub auth: AuthSection,
    /// Channel behavior settings (webhook path, timeouts, retry, etc.).
    #[serde(default)]
    pub settings: SettingsSection,
}

/// Script file paths for channel message handling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptsSection {
    /// Script that parses raw webhook payloads into normalized messages.
    #[serde(default = "default_inbound")]
    pub inbound: String,
    /// Script that sends agent responses back to the channel.
    #[serde(default = "default_outbound")]
    pub outbound: String,
    /// Optional script for webhook signature verification.
    #[serde(default)]
    pub verify: Option<String>,
    /// Optional script for polling new messages (poll mode only).
    #[serde(default)]
    pub poll: Option<String>,
}

impl Default for ScriptsSection {
    fn default() -> Self {
        Self {
            inbound: default_inbound(),
            outbound: default_outbound(),
            verify: None,
            poll: None,
        }
    }
}

/// Sandbox permissions for channel script execution.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxSection {
    /// Whether the script is allowed network access.
    #[serde(default)]
    pub network: bool,
    /// Filesystem paths the script may read.
    #[serde(default)]
    pub fs_read: Vec<String>,
    /// Filesystem paths the script may write.
    #[serde(default)]
    pub fs_write: Vec<String>,
}

impl SandboxSection {
    /// Convert to a `SandboxPolicy` for script execution.
    pub fn to_policy(&self) -> borg_sandbox::policy::SandboxPolicy {
        borg_sandbox::policy::SandboxPolicy {
            network: self.network,
            fs_read: self.fs_read.clone(),
            fs_write: self.fs_write.clone(),
            ..Default::default()
        }
    }
}

/// Authentication configuration for a channel.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthSection {
    /// Env var name holding the webhook verification secret.
    pub secret_env: Option<String>,
    /// Env var name holding the API/bot token for outbound messages.
    pub token_env: Option<String>,
}

/// Behavioral settings for a channel integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsSection {
    /// Custom webhook URL path (defaults to `/webhook/<name>`).
    pub webhook_path: Option<String>,
    /// Script execution timeout in milliseconds.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Maximum concurrent script executions for this channel.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Whether the channel uses webhook or poll mode.
    #[serde(default)]
    pub mode: ChannelMode,
    /// Poll interval in milliseconds (poll mode only).
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    /// Maximum characters per outbound message chunk.
    #[serde(default)]
    pub max_message_chars: Option<usize>,
    /// Maximum number of outbound retry attempts.
    #[serde(default)]
    pub retry_max_attempts: Option<u32>,
    /// Initial delay in milliseconds before first retry.
    #[serde(default)]
    pub retry_initial_delay_ms: Option<u64>,
}

impl Default for SettingsSection {
    fn default() -> Self {
        Self {
            webhook_path: None,
            timeout_ms: default_timeout(),
            max_concurrent: default_max_concurrent(),
            mode: ChannelMode::default(),
            poll_interval_ms: None,
            max_message_chars: None,
            retry_max_attempts: None,
            retry_initial_delay_ms: None,
        }
    }
}

fn default_runtime() -> String {
    "python".to_string()
}
fn default_inbound() -> String {
    "parse_inbound.py".to_string()
}
fn default_outbound() -> String {
    "send_outbound.py".to_string()
}
fn default_timeout() -> u64 {
    borg_core::constants::CHANNEL_DEFAULT_TIMEOUT_MS
}
fn default_max_concurrent() -> usize {
    borg_core::constants::CHANNEL_DEFAULT_MAX_CONCURRENT
}

impl ChannelManifest {
    /// Load and validate a channel manifest from a `channel.toml` file.
    ///
    /// Synchronous. Called from the registry scanner during startup. For
    /// async callers (the iMessage monitor) use [`ChannelManifest::load_async`]
    /// so the reactor is not stalled on disk I/O.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::parse(&content)
    }

    /// Async variant of [`ChannelManifest::load`] for callers running on a
    /// tokio runtime (e.g. the iMessage monitor). Reads the manifest with
    /// `tokio::fs` so a slow disk can't block other reactor tasks.
    pub async fn load_async(path: &Path) -> anyhow::Result<Self> {
        let content = tokio::fs::read_to_string(path).await?;
        Self::parse(&content)
    }

    fn parse(content: &str) -> anyhow::Result<Self> {
        let manifest: Self = toml::from_str(content)?;
        // Validate channel name to prevent path traversal in webhook routes
        if manifest.name.contains('/')
            || manifest.name.contains('\\')
            || manifest.name.contains("..")
        {
            anyhow::bail!(
                "Channel name '{}' contains invalid characters (/, \\, or ..)",
                manifest.name
            );
        }
        Ok(manifest)
    }

    /// Returns `true` if the channel operates in poll mode.
    pub fn is_poll_mode(&self) -> bool {
        self.settings.mode == ChannelMode::Poll
    }

    /// Returns the webhook URL path, falling back to `/webhook/<name>`.
    pub fn webhook_path(&self) -> String {
        self.settings
            .webhook_path
            .clone()
            .unwrap_or_else(|| format!("/webhook/{}", self.name))
    }

    /// Build a sandbox policy from this manifest's sandbox section.
    pub fn sandbox_policy(&self) -> borg_sandbox::policy::SandboxPolicy {
        self.sandbox.to_policy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_TOML: &str = r#"
name = "my-slack"
description = "Slack workspace integration"
runtime = "python"

[scripts]
inbound = "parse_inbound.py"
outbound = "send_outbound.py"
verify = "verify.py"

[sandbox]
network = true
fs_read = ["/etc/ssl"]
fs_write = []

[auth]
secret_env = "SLACK_SIGNING_SECRET"
token_env = "SLACK_BOT_TOKEN"

[settings]
webhook_path = "/webhook/my-slack"
timeout_ms = 15000
max_concurrent = 5
"#;

    #[test]
    fn parse_full_manifest() {
        let manifest: ChannelManifest = toml::from_str(FULL_TOML).unwrap();
        assert_eq!(manifest.name, "my-slack");
        assert_eq!(manifest.description, "Slack workspace integration");
        assert_eq!(manifest.runtime, "python");
        assert_eq!(manifest.scripts.inbound, "parse_inbound.py");
        assert_eq!(manifest.scripts.outbound, "send_outbound.py");
        assert_eq!(manifest.scripts.verify.as_deref(), Some("verify.py"));
        assert!(manifest.sandbox.network);
        assert_eq!(
            manifest.auth.secret_env.as_deref(),
            Some("SLACK_SIGNING_SECRET")
        );
        assert_eq!(manifest.auth.token_env.as_deref(), Some("SLACK_BOT_TOKEN"));
        assert_eq!(manifest.settings.timeout_ms, 15000);
        assert_eq!(manifest.settings.max_concurrent, 5);
    }

    #[test]
    fn parse_minimal_manifest() {
        let toml_str = r#"
name = "test-channel"
description = "A test channel"
"#;
        let manifest: ChannelManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.name, "test-channel");
        assert_eq!(manifest.runtime, "python");
        assert_eq!(manifest.scripts.inbound, "parse_inbound.py");
        assert_eq!(manifest.scripts.outbound, "send_outbound.py");
        assert!(manifest.scripts.verify.is_none());
        assert!(!manifest.sandbox.network);
        assert_eq!(manifest.settings.timeout_ms, 15000);
        assert_eq!(manifest.settings.max_concurrent, 5);
    }

    #[test]
    fn webhook_path_default() {
        let toml_str = r#"
name = "discord"
description = "Discord bot"
"#;
        let manifest: ChannelManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.webhook_path(), "/webhook/discord");
    }

    #[test]
    fn webhook_path_custom() {
        let manifest: ChannelManifest = toml::from_str(FULL_TOML).unwrap();
        assert_eq!(manifest.webhook_path(), "/webhook/my-slack");
    }

    #[test]
    fn sandbox_policy_conversion() {
        let manifest: ChannelManifest = toml::from_str(FULL_TOML).unwrap();
        let policy = manifest.sandbox_policy();
        assert!(policy.network);
        assert_eq!(policy.fs_read, vec!["/etc/ssl"]);
        assert!(policy.fs_write.is_empty());
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channel.toml");
        std::fs::write(&path, FULL_TOML).unwrap();
        let manifest = ChannelManifest::load(&path).unwrap();
        assert_eq!(manifest.name, "my-slack");
    }

    #[test]
    fn load_nonexistent_file_errors() {
        let result = ChannelManifest::load(std::path::Path::new("/tmp/nonexistent_channel.toml"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn load_async_reads_same_file_as_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channel.toml");
        std::fs::write(&path, FULL_TOML).unwrap();
        let sync_manifest = ChannelManifest::load(&path).unwrap();
        let async_manifest = ChannelManifest::load_async(&path).await.unwrap();
        assert_eq!(sync_manifest.name, async_manifest.name);
        assert_eq!(sync_manifest.runtime, async_manifest.runtime);
        assert_eq!(
            sync_manifest.settings.timeout_ms,
            async_manifest.settings.timeout_ms
        );
    }

    #[tokio::test]
    async fn load_async_rejects_path_traversal_in_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("channel.toml");
        std::fs::write(
            &path,
            r#"
name = "../escape"
description = "x"
runtime = "python"
"#,
        )
        .unwrap();
        let err = ChannelManifest::load_async(&path).await.unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn parse_poll_mode_manifest() {
        let toml_str = r#"
name = "imessage"
description = "Bidirectional iMessage via macOS Messages"
runtime = "python"

[scripts]
poll = "poll_messages.py"
outbound = "send_outbound.sh"

[sandbox]
network = false
fs_read = ["~/Library/Messages"]
fs_write = ["~/.borg/channels/imessage"]

[settings]
mode = "poll"
poll_interval_ms = 5000
timeout_ms = 15000
max_concurrent = 3
"#;
        let manifest: ChannelManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.name, "imessage");
        assert!(manifest.is_poll_mode());
        assert_eq!(manifest.settings.poll_interval_ms, Some(5000));
        assert_eq!(manifest.scripts.poll.as_deref(), Some("poll_messages.py"));
        assert_eq!(manifest.scripts.outbound, "send_outbound.sh");
        assert!(manifest.scripts.verify.is_none());
    }

    #[test]
    fn default_mode_is_webhook() {
        let toml_str = r#"
name = "test"
description = "Test"
"#;
        let manifest: ChannelManifest = toml::from_str(toml_str).unwrap();
        assert!(!manifest.is_poll_mode());
        assert_eq!(manifest.settings.mode, ChannelMode::Webhook);
    }
}
