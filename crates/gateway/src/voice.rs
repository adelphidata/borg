use std::str::FromStr;

use anyhow::Result;
use tracing::warn;

use cortex::config::Config;
use cortex::db::Database;

/// Per-chat voice reply mode for messaging channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VoiceReplyMode {
    /// Never send voice replies automatically.
    Off,
    /// Send voice replies only when the inbound message was a voice message.
    VoiceOnly,
    /// Send voice replies for all replies.
    All,
}

impl VoiceReplyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::VoiceOnly => "voice_only",
            Self::All => "all",
        }
    }

    pub fn status_label(self) -> &'static str {
        match self {
            Self::Off => "Off (text only)",
            Self::VoiceOnly => "On (voice reply to voice messages)",
            Self::All => "TTS (voice reply to all messages)",
        }
    }
}

impl FromStr for VoiceReplyMode {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "voice_only" => Ok(Self::VoiceOnly),
            "all" => Ok(Self::All),
            _ => Err(()),
        }
    }
}

pub fn voice_mode_setting_key(channel_name: &str, chat_id: &str) -> String {
    format!("gw:voice_mode:{channel_name}:{chat_id}")
}

pub fn default_voice_reply_mode(config: &Config) -> VoiceReplyMode {
    if config.tts.auto_mode {
        VoiceReplyMode::All
    } else {
        VoiceReplyMode::Off
    }
}

pub fn configured_tts_synthesizer(config: &Config) -> Option<cortex::media::tts::TtsSynthesizer> {
    cortex::media::tts::TtsSynthesizer::from_config(config)
}

pub fn voice_reply_unavailable_reason(config: &Config) -> Option<&'static str> {
    if !config.tts.enabled {
        Some("Voice replies require `tts.enabled = true`.")
    } else if configured_tts_synthesizer(config).is_none() {
        Some("Voice replies require at least one configured TTS provider.")
    } else {
        None
    }
}

pub async fn resolve_voice_reply_mode(
    db: &Database,
    config: &Config,
    channel_name: &str,
    chat_id: &str,
) -> Result<VoiceReplyMode> {
    let key = voice_mode_setting_key(channel_name, chat_id);
    match db.get_setting(&key).await? {
        Some(value) => {
            if let Ok(mode) = VoiceReplyMode::from_str(&value) {
                Ok(mode)
            } else {
                warn!("Invalid voice mode setting for {key}: {value}");
                Ok(default_voice_reply_mode(config))
            }
        }
        None => Ok(default_voice_reply_mode(config)),
    }
}

pub fn should_send_voice_reply(
    mode: VoiceReplyMode,
    response_text: &str,
    is_voice_input: bool,
) -> bool {
    let trimmed = response_text.trim();
    let lower = trimmed.to_ascii_lowercase();
    if trimmed.is_empty() || trimmed.starts_with('(') {
        return false;
    }

    if cortex::error_format::classify_error(trimmed) != cortex::error_format::ErrorCategory::Unknown
    {
        return false;
    }

    if lower.contains("the ai provider")
        || lower.contains("rate-limited")
        || lower.contains("authentication failed")
        || lower.contains("timed out")
    {
        return false;
    }

    match mode {
        VoiceReplyMode::Off => false,
        VoiceReplyMode::VoiceOnly => is_voice_input,
        VoiceReplyMode::All => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_uses_tts_auto_mode() {
        let mut config = Config::default();
        config.tts.auto_mode = false;
        assert_eq!(default_voice_reply_mode(&config), VoiceReplyMode::Off);

        config.tts.auto_mode = true;
        assert_eq!(default_voice_reply_mode(&config), VoiceReplyMode::All);
    }

    #[test]
    fn should_send_voice_reply_obeys_mode_and_input_kind() {
        assert!(!should_send_voice_reply(VoiceReplyMode::Off, "hello", true));
        assert!(should_send_voice_reply(
            VoiceReplyMode::VoiceOnly,
            "hello",
            true
        ));
        assert!(!should_send_voice_reply(
            VoiceReplyMode::VoiceOnly,
            "hello",
            false
        ));
        assert!(should_send_voice_reply(VoiceReplyMode::All, "hello", false));
    }

    #[test]
    fn should_not_send_voice_for_empty_or_errorish_responses() {
        assert!(!should_send_voice_reply(VoiceReplyMode::All, "", true));
        assert!(!should_send_voice_reply(
            VoiceReplyMode::All,
            "(request timed out — agent was idle too long)",
            true
        ));
        assert!(!should_send_voice_reply(
            VoiceReplyMode::All,
            "The AI provider is temporarily rate-limited. Please try again in a moment.",
            true
        ));
    }

    #[tokio::test]
    async fn resolve_voice_reply_mode_reads_override_from_db() {
        let db = Database::test_db().await;
        let mut config = Config::default();
        config.tts.auto_mode = true;
        let key = voice_mode_setting_key("telegram", "42");
        db.set_setting(&key, VoiceReplyMode::VoiceOnly.as_str())
            .await
            .unwrap();

        let mode = resolve_voice_reply_mode(&db, &config, "telegram", "42")
            .await
            .unwrap();
        assert_eq!(mode, VoiceReplyMode::VoiceOnly);
    }

    #[tokio::test]
    async fn resolve_voice_reply_mode_propagates_setting_lookup_errors() {
        let db = Database::test_db().await;
        // Drop the settings table to force a lookup error.
        db.conn()
            .execute_batch("DROP TABLE settings")
            .await
            .unwrap();

        let err = resolve_voice_reply_mode(&db, &Config::default(), "telegram", "42")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }

    #[test]
    fn voice_reply_unavailable_reason_reports_disabled_tts() {
        let config = Config::default();
        assert_eq!(
            voice_reply_unavailable_reason(&config),
            Some("Voice replies require `tts.enabled = true`.")
        );
    }

    #[test]
    fn configured_tts_synthesizer_ignores_auto_mode_flag() {
        let mut config = Config::default();
        config.tts.enabled = true;
        config.tts.auto_mode = false;
        // Unique per-test env var name so this doesn't race the
        // commands.rs `handle_voice_can_enable_all_and_voice_only_modes`
        // test, which uses the same `BORG_TEST_TTS_KEY` name with its
        // own set/remove pair. Tests run in parallel by default.
        config.tts.models = vec![cortex::config::TtsModelConfig {
            provider: "openai".to_string(),
            model: None,
            voice: None,
            timeout_ms: None,
            api_key_env: Some("BORG_VOICE_RS_TEST_TTS_KEY".to_string()),
            name: None,
            command: None,
            output_extension: None,
        }];
        unsafe {
            std::env::set_var("BORG_VOICE_RS_TEST_TTS_KEY", "test-key");
        }

        assert!(configured_tts_synthesizer(&config).is_some());

        unsafe {
            std::env::remove_var("BORG_VOICE_RS_TEST_TTS_KEY");
        }
    }
}
