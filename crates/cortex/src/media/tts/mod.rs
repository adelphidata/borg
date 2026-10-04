//! Text-to-speech: the engine that walks an operator's provider chain.
//!
//! Providers and their defaults live in [`providers`]; turning a reply into
//! speakable text lives in [`text`].

use std::time::Duration;

use anyhow::{bail, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::{Config, TtsModelConfig};
use providers::{SynthesisRequest, TtsProvider};

// ── Audio Format ──

/// Supported TTS output formats.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    /// MP3 format (default).
    #[default]
    Mp3,
    /// Opus format (Ogg container).
    Opus,
    /// AAC format.
    Aac,
    /// FLAC lossless format.
    Flac,
    /// WAV uncompressed format.
    Wav,
}

impl AudioFormat {
    /// Returns the MIME type string for this audio format.
    pub fn mime_type(&self) -> &str {
        match self {
            AudioFormat::Mp3 => "audio/mpeg",
            AudioFormat::Opus => "audio/ogg",
            AudioFormat::Aac => "audio/aac",
            AudioFormat::Flac => "audio/flac",
            AudioFormat::Wav => "audio/wav",
        }
    }

    /// Returns the file extension for this audio format.
    pub fn extension(&self) -> &str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Opus => "opus",
            AudioFormat::Aac => "aac",
            AudioFormat::Flac => "flac",
            AudioFormat::Wav => "wav",
        }
    }

    /// Parse from string (e.g. config value).
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "mp3" => Some(AudioFormat::Mp3),
            "opus" | "ogg" => Some(AudioFormat::Opus),
            "aac" => Some(AudioFormat::Aac),
            "flac" => Some(AudioFormat::Flac),
            "wav" => Some(AudioFormat::Wav),
            _ => None,
        }
    }
}

// ── Synthesis Tracking ──

/// Outcome of a single provider synthesis attempt.
#[derive(Debug, Clone)]
pub enum SynthesisOutcome {
    /// Synthesis completed successfully.
    Success,
    /// Synthesis failed with the given error message.
    Failed(String),
}

/// Tracks each provider attempt in the fallback chain.
#[derive(Debug, Clone)]
pub struct SynthesisAttempt {
    /// Name of the TTS provider attempted.
    pub provider: String,
    /// Model identifier used for synthesis, if any.
    pub model: Option<String>,
    /// Whether the attempt succeeded or failed.
    pub outcome: SynthesisOutcome,
}

// ── Provider Trait ──

mod providers;
mod text;

pub(crate) use providers::validate_voice_id;
pub use providers::{apply_xai_auto_speech_tags, providers};
pub use text::{
    chunk_for_streaming_tts, prepare_for_tts, strip_markdown_for_tts, truncate_for_tts,
};

// ── TTS Synthesizer ──

/// A provider with its per-model configuration.
struct TtsProviderEntry {
    provider: Box<dyn TtsProvider>,
    model: Option<String>,
    voice: Option<String>,
    /// The provider's own default, used when nothing more specific is set.
    catalog_voice: &'static str,
    timeout: Option<Duration>,
    max_text_length: usize,
}

/// Main TTS engine with multi-provider fallback.
pub struct TtsSynthesizer {
    /// Ordered list of provider entries for fallback.
    entries: Vec<TtsProviderEntry>,
    /// `tts.default_voice`: the operator's cross-provider voice, empty by
    /// default (see [`TtsSynthesizer::voice_for`]).
    default_voice: String,
    /// Default audio output format.
    default_format: AudioFormat,
    /// Maximum allowed input text length in bytes.
    max_text_length: usize,
    /// Default HTTP timeout for synthesis requests.
    default_timeout: Duration,
}

impl TtsSynthesizer {
    /// Test-only constructor producing a synthesizer with no providers. Lets
    /// downstream handler tests exercise the "all providers failed" branch
    /// without spinning up a mock HTTP server.
    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            entries: Vec::new(),
            default_voice: "alloy".to_string(),
            default_format: AudioFormat::Mp3,
            max_text_length: 4096,
            default_timeout: Duration::from_secs(30),
        }
    }

    /// Test-only constructor that wires a single OpenAI-compatible provider
    /// at an operator-supplied `base_url` — typically a `wiremock::MockServer`
    /// uri. Lets handler tests exercise the happy path (Multimodal audio
    /// response, byte length, MIME) without depending on a real cloud API
    /// key. Production code MUST go through `from_config`.
    #[cfg(test)]
    pub(crate) fn single_openai_for_test(base_url: String, api_key: String) -> Self {
        Self {
            entries: vec![TtsProviderEntry {
                provider: Box::new(providers::OpenAiTtsProvider {
                    client: Client::new(),
                    api_key,
                    base_url,
                    default_model: "tts-1".to_string(),
                }),
                model: None,
                voice: Some("alloy".to_string()),
                catalog_voice: "alloy",
                timeout: Some(Duration::from_secs(5)),
                max_text_length: 4096,
            }],
            default_voice: "alloy".to_string(),
            default_format: AudioFormat::Mp3,
            max_text_length: 4096,
            default_timeout: Duration::from_secs(5),
        }
    }

    /// Build TTS engine from config. Returns None if disabled or no providers available.
    pub fn from_config(config: &Config) -> Option<Self> {
        if !config.tts.enabled {
            return None;
        }

        let tts = &config.tts;
        // Cloned, not built: every provider entry below shares the one
        // connection pool, and per-request budgets are applied on the request.
        let client = crate::http::shared().clone();
        let entries: Vec<TtsProviderEntry> = tts
            .models
            .iter()
            .filter_map(|row| Self::build_entry(&client, row, config))
            .collect();

        if entries.is_empty() {
            debug!("No TTS providers available");
            return None;
        }

        let default_format = AudioFormat::from_str_lossy(&tts.default_format).unwrap_or_default();

        Some(Self {
            entries,
            default_voice: tts.default_voice.clone(),
            default_format,
            max_text_length: tts.max_text_length,
            default_timeout: Duration::from_millis(tts.timeout_ms),
        })
    }

    fn build_entry(
        client: &Client,
        row: &TtsModelConfig,
        config: &Config,
    ) -> Option<TtsProviderEntry> {
        let Some(def) = providers::find(&row.provider) else {
            warn!("Unknown TTS provider: {}", row.provider);
            return None;
        };
        let Some(provider) = def.build(client, row, config) else {
            debug!(
                "Skipping TTS provider '{}': no API key or command",
                row.provider
            );
            return None;
        };
        Some(TtsProviderEntry {
            provider,
            model: row.model.clone(),
            voice: row.voice.clone().filter(|v| !v.is_empty()),
            catalog_voice: def.info.default_voice,
            timeout: row.timeout_ms.map(Duration::from_millis),
            max_text_length: row
                .max_text_length
                .unwrap_or_else(|| (def.max_text_length)(row.model.as_deref())),
        })
    }

    /// The voice a provider is asked for: the caller's, else the row's, else
    /// `tts.default_voice`, else the provider's own catalog default. The
    /// global default comes before the catalog one so an operator's explicit
    /// choice is honored, but it ships empty: a voice id belongs to one
    /// provider, and sending OpenAI's "alloy" to ElevenLabs fails the call.
    fn voice_for(&self, entry: &TtsProviderEntry, requested: Option<&str>) -> String {
        requested
            .filter(|v| !v.is_empty())
            .or(entry.voice.as_deref())
            .or(Some(self.default_voice.as_str()).filter(|v| !v.is_empty()))
            .unwrap_or(entry.catalog_voice)
            .to_string()
    }

    /// Synthesize text to audio bytes.
    /// Returns (audio_bytes, format, attempts_log).
    pub async fn synthesize(
        &self,
        text: &str,
        voice: Option<&str>,
        format: Option<AudioFormat>,
    ) -> Result<(Vec<u8>, AudioFormat, Vec<SynthesisAttempt>)> {
        if text.is_empty() {
            bail!("Text is empty");
        }
        // Largest per-provider cap, floored at the configured global limit so
        // an empty/short chain still rejects obviously oversized input. Each
        // provider re-checks its own cap below; this is just the fast reject.
        let max_allowed = self
            .entries
            .iter()
            .map(|entry| entry.max_text_length)
            .max()
            .unwrap_or(0)
            .max(self.max_text_length);
        if text.len() > max_allowed {
            bail!(
                "Text too long: {} characters (maximum: {})",
                text.len(),
                max_allowed
            );
        }

        let out_format = format.unwrap_or(self.default_format);
        let mut attempts = Vec::new();

        for entry in &self.entries {
            if text.len() > entry.max_text_length {
                attempts.push(SynthesisAttempt {
                    provider: entry.provider.name().to_string(),
                    model: entry.model.clone(),
                    outcome: SynthesisOutcome::Failed(format!(
                        "text too long: {} characters (maximum for {}: {})",
                        text.len(),
                        entry.provider.name(),
                        entry.max_text_length
                    )),
                });
                continue;
            }
            let voice_id = self.voice_for(entry, voice);
            let timeout = entry.timeout.unwrap_or(self.default_timeout);

            let req = SynthesisRequest {
                text,
                voice: voice_id,
                format: out_format,
                model: entry.model.clone(),
                timeout,
            };

            match entry.provider.synthesize(&req).await {
                Ok(bytes) => {
                    attempts.push(SynthesisAttempt {
                        provider: entry.provider.name().to_string(),
                        model: entry.model.clone(),
                        outcome: SynthesisOutcome::Success,
                    });
                    return Ok((bytes, out_format, attempts));
                }
                Err(e) => {
                    let reason = format!("{e}");
                    warn!(
                        "TTS synthesis failed with {}: {reason}",
                        entry.provider.name()
                    );
                    attempts.push(SynthesisAttempt {
                        provider: entry.provider.name().to_string(),
                        model: entry.model.clone(),
                        outcome: SynthesisOutcome::Failed(reason),
                    });
                }
            }
        }

        bail!("All TTS providers failed ({} attempted)", attempts.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{ElevenLabsTtsProvider, OpenAiTtsProvider};
    use reqwest::Client;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    #[test]
    fn from_config_returns_none_when_disabled() {
        let config = Config::default();
        assert!(!config.tts.enabled);
        assert!(TtsSynthesizer::from_config(&config).is_none());
    }

    #[test]
    fn from_config_returns_none_when_no_providers() {
        let mut config = Config::default();
        config.tts.enabled = true;
        // No models configured, no API keys -> None
        assert!(TtsSynthesizer::from_config(&config).is_none());
    }

    #[test]
    fn audio_format_mime_types() {
        assert_eq!(AudioFormat::Mp3.mime_type(), "audio/mpeg");
        assert_eq!(AudioFormat::Opus.mime_type(), "audio/ogg");
        assert_eq!(AudioFormat::Aac.mime_type(), "audio/aac");
        assert_eq!(AudioFormat::Flac.mime_type(), "audio/flac");
        assert_eq!(AudioFormat::Wav.mime_type(), "audio/wav");
    }

    #[test]
    fn audio_format_extensions() {
        assert_eq!(AudioFormat::Mp3.extension(), "mp3");
        assert_eq!(AudioFormat::Opus.extension(), "opus");
        assert_eq!(AudioFormat::Aac.extension(), "aac");
        assert_eq!(AudioFormat::Flac.extension(), "flac");
        assert_eq!(AudioFormat::Wav.extension(), "wav");
    }

    #[test]
    fn audio_format_from_str_lossy() {
        assert_eq!(AudioFormat::from_str_lossy("mp3"), Some(AudioFormat::Mp3));
        assert_eq!(AudioFormat::from_str_lossy("opus"), Some(AudioFormat::Opus));
        assert_eq!(AudioFormat::from_str_lossy("ogg"), Some(AudioFormat::Opus));
        assert_eq!(AudioFormat::from_str_lossy("MP3"), Some(AudioFormat::Mp3));
        assert_eq!(AudioFormat::from_str_lossy("invalid"), None);
    }

    #[tokio::test]
    async fn synthesize_rejects_empty_text() {
        let synth = TtsSynthesizer {
            entries: Vec::new(),
            default_voice: "alloy".into(),
            default_format: AudioFormat::Mp3,
            max_text_length: 4096,
            default_timeout: Duration::from_secs(30),
        };
        let result = synth.synthesize("", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn synthesize_rejects_oversized_text() {
        let synth = TtsSynthesizer {
            entries: Vec::new(),
            default_voice: "alloy".into(),
            default_format: AudioFormat::Mp3,
            max_text_length: 100,
            default_timeout: Duration::from_secs(30),
        };
        let long_text = "a".repeat(200);
        let result = synth.synthesize(&long_text, None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too long"));
    }

    #[tokio::test]
    async fn synthesize_fails_with_no_providers() {
        let synth = TtsSynthesizer {
            entries: Vec::new(),
            default_voice: "alloy".into(),
            default_format: AudioFormat::Mp3,
            max_text_length: 4096,
            default_timeout: Duration::from_secs(30),
        };
        let result = synth.synthesize("Hello world", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0 attempted"));
    }

    /// Spec: every registry row must build when its credentials are present.
    /// A drop means a config row naming that provider silently disappears
    /// from the fallback chain. Driven by the registry itself, so a new row
    /// is covered without editing this test. Key variables are unique to this
    /// test so it never races tests that touch the conventional names.
    #[tokio::test]
    async fn from_config_builds_every_registry_row() {
        use crate::config::TtsModelConfig;
        let var = |id: &str| format!("BORG_TTS_ROW_{}", id.to_ascii_uppercase());
        for info in providers() {
            unsafe { std::env::set_var(var(info.id), "test-key") };
        }

        let mut config = Config::default();
        config.tts.enabled = true;
        config.tts.models = providers()
            .map(|info| TtsModelConfig {
                provider: info.id.to_string(),
                api_key_env: Some(var(info.id)),
                // Only command rows read it; harmless for the rest.
                command: Some("cp {text_file} {out_path}".to_string()),
                ..Default::default()
            })
            .collect();

        let synth = TtsSynthesizer::from_config(&config).expect("synthesizer must build");
        let names: Vec<&str> = synth.entries.iter().map(|e| e.provider.name()).collect();
        let ids: Vec<&str> = providers().map(|i| i.id).collect();
        assert_eq!(
            names, ids,
            "every registry row must produce an entry, in order"
        );

        for info in providers() {
            unsafe { std::env::remove_var(var(info.id)) };
        }
    }

    /// A key saved through Settings (`credentials.{provider}` row) must reach
    /// the speech provider with no env var or credential-store entry.
    #[test]
    fn key_saved_in_settings_enables_the_provider() {
        let rows = [
            ("tts.enabled".to_string(), "true".to_string()),
            (
                "tts.models".to_string(),
                r#"[{"provider":"elevenlabs","api_key_env":"BORG_TTS_SAVED_KEY_UNSET"}]"#
                    .to_string(),
            ),
            (
                "credentials.elevenlabs".to_string(),
                r#"{"type":"literal","value":"xi-saved"}"#.to_string(),
            ),
        ];
        let config = Config::from_setting_rows(rows.clone());
        let synth = TtsSynthesizer::from_config(&config).expect("saved key must enable the row");
        assert_eq!(synth.entries[0].provider.name(), "elevenlabs");

        let without_key = Config::from_setting_rows(
            rows.into_iter()
                .filter(|(k, _)| !k.starts_with("credentials.")),
        );
        assert!(TtsSynthesizer::from_config(&without_key).is_none());
    }

    /// Regression: a row with no voice used to receive the global default
    /// ("alloy", an OpenAI voice), so ElevenLabs was asked for
    /// `/v1/text-to-speech/alloy` and failed. Precedence is call → row →
    /// operator's `tts.default_voice` → the provider's catalog default.
    #[test]
    fn voice_precedence_ends_at_the_providers_own_default() {
        let entry = |voice: Option<&str>| TtsProviderEntry {
            provider: Box::new(providers::OpenAiTtsProvider {
                client: Client::new(),
                api_key: String::new(),
                base_url: String::new(),
                default_model: String::new(),
            }),
            model: None,
            voice: voice.map(String::from),
            catalog_voice: "catalog",
            timeout: None,
            max_text_length: 4096,
        };
        let synth = |default_voice: &str| TtsSynthesizer {
            default_voice: default_voice.to_string(),
            ..TtsSynthesizer::empty_for_test()
        };
        let cases: &[(&str, Option<&str>, Option<&str>, &str)] = &[
            // (tts.default_voice, row voice, requested, expected)
            ("", None, None, "catalog"),
            ("operator", None, None, "operator"),
            ("operator", Some("row"), None, "row"),
            ("operator", Some("row"), Some("call"), "call"),
            ("", None, Some(""), "catalog"),
        ];
        for (global, row, requested, expected) in cases {
            assert_eq!(
                synth(global).voice_for(&entry(*row), *requested),
                *expected,
                "global={global:?} row={row:?} requested={requested:?}"
            );
        }
    }

    /// Spec: the custom arm requires its `command` template; a row without
    /// one must be skipped silently (no panic, no spurious entry). Same
    /// shape as the negative test for missing API keys on cloud providers.
    #[test]
    fn from_config_custom_arm_requires_command_template() {
        use crate::config::TtsModelConfig;
        let mut config = Config::default();
        config.tts.enabled = true;
        config.tts.models = vec![TtsModelConfig {
            provider: "custom".to_string(),
            model: None,
            voice: Some("test".to_string()),
            api_key_env: None,
            timeout_ms: None,
            base_url: None,
            language: None,
            sample_rate_hz: None,
            bit_rate: None,
            max_text_length: None,
            name: Some("missing-command".to_string()),
            command: None,
            output_extension: None,
            xai_auto_speech_tags: None,
        }];
        // No usable providers → from_config returns None.
        assert!(TtsSynthesizer::from_config(&config).is_none());
    }

    /// Spec: the TTS fallback chain must move on to the next provider
    /// when the first one fails, and the `SynthesisAttempt` log must
    /// record both attempts in order so observability can show what
    /// was tried.
    #[tokio::test]
    async fn tts_fallback_chain_advances_past_failing_provider() {
        let server_a = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/speech"))
            .respond_with(ResponseTemplate::new(500).set_body_string("primary down"))
            .expect(1)
            .mount(&server_a)
            .await;

        // The `name()` returned by each provider must differ so the
        // fallback log discriminates them. Use OpenAI as primary,
        // ElevenLabs as secondary — different paths, different provider
        // names in the attempt log.
        let server_b = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/text-to-speech/alloy"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secondary-audio".to_vec()))
            .expect(1)
            .mount(&server_b)
            .await;

        let synthesizer = TtsSynthesizer {
            entries: vec![
                TtsProviderEntry {
                    provider: Box::new(OpenAiTtsProvider {
                        client: Client::new(),
                        api_key: "k1".to_string(),
                        base_url: server_a.uri(),
                        default_model: "tts-1".to_string(),
                    }),
                    model: None,
                    voice: Some("alloy".to_string()),
                    catalog_voice: "",
                    timeout: Some(Duration::from_secs(5)),
                    max_text_length: 4096,
                },
                TtsProviderEntry {
                    provider: Box::new(ElevenLabsTtsProvider {
                        client: Client::new(),
                        api_key: "k2".to_string(),
                        base_url: server_b.uri(),
                        default_model: "eleven_multilingual_v2".to_string(),
                    }),
                    model: None,
                    voice: Some("alloy".to_string()),
                    catalog_voice: "",
                    timeout: Some(Duration::from_secs(5)),
                    max_text_length: 10_000,
                },
            ],
            default_voice: "alloy".to_string(),
            default_format: AudioFormat::Mp3,
            max_text_length: 4096,
            default_timeout: Duration::from_secs(5),
        };
        let (bytes, _fmt, attempts) = synthesizer
            .synthesize("hello", None, None)
            .await
            .expect("fallback must succeed");
        assert_eq!(bytes, b"secondary-audio");
        assert_eq!(attempts.len(), 2, "both attempts must be recorded");
        assert_eq!(attempts[0].provider, "openai");
        assert!(
            matches!(attempts[0].outcome, SynthesisOutcome::Failed(_)),
            "primary attempt must be marked failed"
        );
        assert_eq!(attempts[1].provider, "elevenlabs");
        assert!(matches!(attempts[1].outcome, SynthesisOutcome::Success));
    }
}
