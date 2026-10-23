//! Speech-to-text: the engine that walks an operator's transcription chain.
//! Providers and their defaults live in [`providers`].

use std::time::Duration;

use anyhow::{bail, Result};
use reqwest::Client;
use tracing::{debug, warn};

use crate::config::{AudioModelConfig, Config};
use providers::{TranscriptionProvider, TranscriptionRequest};

mod providers;

pub use providers::{parse_deepgram_response, providers};
// ── Audio Transcription ──

/// Outcome of a single provider attempt.
#[derive(Debug, Clone)]
pub enum AttemptOutcome {
    Success,
    Skipped(String),
    Failed(String),
}

/// Tracks each provider attempt in the fallback chain.
#[derive(Debug, Clone)]
pub struct TranscriptionAttempt {
    pub provider: String,
    pub model: Option<String>,
    pub outcome: AttemptOutcome,
}

/// A provider with its per-model configuration.
struct ProviderEntry {
    provider: Box<dyn TranscriptionProvider>,
    model: Option<String>,
    language: Option<String>,
    timeout: Option<Duration>,
}

/// Main transcription engine with multi-provider fallback.
pub struct AudioTranscriber {
    entries: Vec<ProviderEntry>,
    max_file_size: u64,
    min_file_size: u64,
    default_language: Option<String>,
    /// `audio.prompt`: the vocabulary hint every request carries.
    prompt: Option<String>,
    /// `audio.temperature` and `audio.beam_size`: how the decoder searches.
    temperature: f64,
    beam_size: u32,
    no_speech_threshold: f64,
    /// `audio.timestamps`: segment spans from the on-device decoder.
    timestamps: bool,
    /// `audio.cjk_spacing`: the mixed-script spacing pass on every transcript.
    cjk_spacing: bool,
    default_timeout: Duration,
}

impl AudioTranscriber {
    /// Build transcription engine from config. Returns None if disabled or no providers available.
    pub fn from_config(config: &Config) -> Option<Self> {
        if !config.audio.enabled {
            return None;
        }

        let audio = &config.audio;
        // Cloned, not built: every provider entry below shares the one
        // connection pool, and per-request budgets are applied on the request.
        let client = crate::http::shared().clone();
        let entries: Vec<ProviderEntry> = audio
            .models
            .iter()
            .filter_map(|row| Self::build_entry(&client, row, config))
            .collect();

        if entries.is_empty() {
            debug!("No audio transcription providers available");
            return None;
        }

        Some(Self {
            entries,
            max_file_size: audio.max_file_size,
            min_file_size: audio.min_file_size,
            default_language: audio.language.clone(),
            prompt: audio.prompt.clone().filter(|p| !p.trim().is_empty()),
            temperature: audio.temperature.clamp(0.0, 1.0),
            beam_size: audio.beam_size.max(1),
            no_speech_threshold: audio.no_speech_threshold.clamp(0.0, 1.0),
            timestamps: audio.timestamps,
            cjk_spacing: audio.cjk_spacing,
            default_timeout: Duration::from_millis(audio.timeout_ms),
        })
    }

    fn build_entry(
        client: &Client,
        row: &AudioModelConfig,
        config: &Config,
    ) -> Option<ProviderEntry> {
        let Some(def) = providers::find(&row.provider) else {
            warn!("Unknown audio transcription provider: {}", row.provider);
            return None;
        };
        let Some(provider) = def.build(client, row, config) else {
            debug!(
                "Skipping audio provider '{}': no API key or command",
                row.provider
            );
            return None;
        };
        Some(ProviderEntry {
            provider,
            model: row.model.clone(),
            language: row.language.clone(),
            timeout: row.timeout_ms.map(Duration::from_millis),
        })
    }

    /// Transcribe audio bytes using the multi-provider fallback chain.
    /// Returns the transcript text and a log of all attempts.
    pub async fn transcribe(
        &self,
        bytes: &[u8],
        mime_type: &str,
        filename: &str,
        language: Option<&str>,
    ) -> Result<(String, Vec<TranscriptionAttempt>)> {
        let size = bytes.len() as u64;

        if size < self.min_file_size {
            bail!(
                "Audio file too small: {size} bytes (minimum: {})",
                self.min_file_size
            );
        }
        if size > self.max_file_size {
            bail!(
                "Audio file too large: {size} bytes (maximum: {})",
                self.max_file_size
            );
        }

        let mut attempts = Vec::new();
        // The caller's hint is model-controlled (the `transcribe_audio` tool
        // passes it straight from the LLM), so it is normalised once here
        // before any provider splices it into a URL or form field.
        let language = language.and_then(normalize_language_hint);

        for entry in &self.entries {
            // Per-model overrides: language, timeout, model
            let lang = language
                .clone()
                .or_else(|| entry.language.clone())
                .or_else(|| self.default_language.clone());
            let timeout = entry.timeout.unwrap_or(self.default_timeout);

            let req = TranscriptionRequest {
                audio_bytes: bytes,
                mime_type: mime_type.to_string(),
                filename: filename.to_string(),
                model: entry.model.clone(),
                language: lang,
                prompt: self.prompt.clone(),
                temperature: self.temperature,
                beam_size: self.beam_size,
                no_speech_threshold: self.no_speech_threshold,
                timestamps: self.timestamps,
                timeout,
            };

            match entry.provider.transcribe(&req).await {
                Ok(text) => {
                    attempts.push(TranscriptionAttempt {
                        provider: entry.provider.name().to_string(),
                        model: entry.model.clone(),
                        outcome: AttemptOutcome::Success,
                    });
                    let text = if self.cjk_spacing {
                        cjk_spacing(&text)
                    } else {
                        text
                    };
                    return Ok((text, attempts));
                }
                Err(e) => {
                    let reason = format!("{e}");
                    warn!(
                        "Audio transcription failed with {}: {reason}",
                        entry.provider.name()
                    );
                    attempts.push(TranscriptionAttempt {
                        provider: entry.provider.name().to_string(),
                        model: entry.model.clone(),
                        outcome: AttemptOutcome::Failed(reason),
                    });
                }
            }
        }

        bail!(
            "All transcription providers failed ({} attempted)",
            attempts.len()
        )
    }
}

/// Whether a character is Chinese, Japanese or Korean writing.
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF   // hiragana, katakana
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0xAC00..=0xD7AF // hangul syllables
        | 0x1100..=0x11FF // hangul jamo
        | 0xFF66..=0xFF9F // half-width katakana
    )
}

/// One space between CJK text and the Latin letters or digits beside it,
/// which is how mixed-script writing is usually set. Spaces already there
/// are kept as they are; punctuation and other scripts are left alone.
pub fn cjk_spacing(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if let Some(p) = prev {
            let boundary = (is_cjk(p) && c.is_ascii_alphanumeric())
                || (p.is_ascii_alphanumeric() && is_cjk(c));
            if boundary {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// Reduce a language hint to a BCP-47-shaped tag or drop it.
///
/// Accepts a 2-3 letter ASCII base (lowercased) optionally followed by
/// `-`/`_` and a 2-4 character alphanumeric subtag (`en`, `en-US`,
/// `zh-Hant`). Anything else — including a hint carrying `&`, `=`, `/`
/// or whitespace — is rejected with a warning so a hostile value can
/// never add query parameters or form fields on a provider request.
pub fn normalize_language_hint(hint: &str) -> Option<String> {
    let hint = hint.trim();
    let (base, region) = match hint.find(['-', '_']) {
        Some(idx) => (
            &hint[..idx],
            Some((hint.as_bytes()[idx] as char, &hint[idx + 1..])),
        ),
        None => (hint, None),
    };
    let base_ok = (2..=3).contains(&base.len()) && base.bytes().all(|b| b.is_ascii_alphabetic());
    let region_ok = match region {
        None => true,
        Some((_, sub)) => {
            (2..=4).contains(&sub.len()) && sub.bytes().all(|b| b.is_ascii_alphanumeric())
        }
    };
    if !base_ok || !region_ok {
        warn!("Ignoring malformed audio language hint {hint:?}");
        return None;
    }
    let mut out = base.to_ascii_lowercase();
    if let Some((sep, sub)) = region {
        out.push(sep);
        out.push_str(sub);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::WhisperProvider;

    /// Spec: the hint reaching a provider must be a plain language tag.
    /// A value that smuggles query syntax is dropped, not forwarded.
    #[test]
    fn normalize_language_hint_accepts_tags_and_drops_injection() {
        let cases: &[(&str, Option<&str>)] = &[
            ("en", Some("en")),
            ("EN", Some("en")),
            (" en-US ", Some("en-US")),
            ("zh-Hant", Some("zh-Hant")),
            ("pt_BR", Some("pt_BR")),
            ("ast", Some("ast")),
            ("en&callback=https://evil/x", None),
            ("en-US&model=x", None),
            ("e", None),
            ("english", None),
            ("en-", None),
            ("en-U", None),
            ("en-USABC", None),
            ("en US", None),
            ("", None),
            ("en/..", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_language_hint(input).as_deref(),
                *expected,
                "hint {input:?}"
            );
        }
    }
    #[test]
    fn from_config_returns_none_when_disabled() {
        let config = Config::default();
        assert!(!config.audio.enabled);
        assert!(AudioTranscriber::from_config(&config).is_none());
    }

    #[test]
    fn from_config_returns_none_when_no_providers() {
        let mut config = Config::default();
        config.audio.enabled = true;
        // No models configured, no API keys → None
        assert!(AudioTranscriber::from_config(&config).is_none());
    }

    /// Spec: every documented STT provider arm in
    /// registry row must successfully construct when its credentials are
    /// present. Mirrors the speech engine's `from_config_builds_every_provider_arm`.
    /// `local` is gated behind the `local-stt` cargo feature and exercised
    /// in a separate test below.
    #[tokio::test]
    async fn from_config_builds_every_cloud_provider_arm() {
        use crate::config::AudioModelConfig;
        let arms: &[(&str, &str)] = &[
            ("openai", "BORG_STT_ARM_OPENAI"),
            ("groq", "BORG_STT_ARM_GROQ"),
            ("deepgram", "BORG_STT_ARM_DEEPGRAM"),
            ("mistral", "BORG_STT_ARM_MISTRAL"),
            ("xai", "BORG_STT_ARM_XAI"),
            ("elevenlabs", "BORG_STT_ARM_ELEVEN"),
        ];
        for (_, var) in arms {
            unsafe { std::env::set_var(var, "test-key") };
        }

        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = arms
            .iter()
            .map(|(provider, var)| AudioModelConfig {
                provider: (*provider).to_string(),
                model: None,
                api_key_env: Some((*var).to_string()),
                language: None,
                timeout_ms: None,
                base_url: None,
                xai_stt_format: None,
                xai_stt_diarize: None,
                command: None,
            })
            .collect();

        let transcriber = AudioTranscriber::from_config(&config)
            .expect("transcriber must build with every cloud arm");
        assert_eq!(
            transcriber.entries.len(),
            arms.len(),
            "every arm row must produce an entry"
        );
        let names: Vec<&str> = transcriber
            .entries
            .iter()
            .map(|e| e.provider.name())
            .collect();
        for (expected, _) in arms {
            assert!(
                names.contains(expected),
                "expected provider {expected} in entries, got {names:?}"
            );
        }

        for (_, var) in arms {
            unsafe { std::env::remove_var(var) };
        }
    }

    /// Spec: the xAI STT row's `xai_stt_format` and `xai_stt_diarize`
    /// options must round-trip through serde so an operator's choice
    /// survives DB persistence. Default-on (None) maps to format=true,
    /// diarize=false — the historical hardcoded values.
    #[test]
    fn audio_model_config_xai_options_serde_round_trip() {
        use crate::config::AudioModelConfig;
        // Explicit Some(true)/Some(false) must round-trip.
        let cfg = AudioModelConfig {
            provider: "xai".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: Some(false),
            xai_stt_diarize: Some(true),
            command: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: AudioModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.xai_stt_format, Some(false));
        assert_eq!(parsed.xai_stt_diarize, Some(true));

        // Default (None) round-trips as None — historical hardcoded
        // values apply only via `unwrap_or` at the provider build site,
        // never at the config layer.
        let none_cfg = AudioModelConfig {
            provider: "xai".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: None,
        };
        let json = serde_json::to_string(&none_cfg).unwrap();
        let parsed: AudioModelConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.xai_stt_format.is_none());
        assert!(parsed.xai_stt_diarize.is_none());
    }

    /// Spec: a config row for a provider whose API key isn't set must be
    /// skipped silently rather than poisoning the whole chain. Drops the
    /// matching env var to simulate a misconfigured operator setup.
    #[tokio::test]
    async fn from_config_skips_provider_without_api_key() {
        use crate::config::AudioModelConfig;
        // Pick a var name unused elsewhere so a stale value can't fake
        // success.
        let var = "BORG_STT_ARM_NEVER_SET";
        unsafe { std::env::remove_var(var) };
        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![AudioModelConfig {
            provider: "openai".to_string(),
            model: None,
            api_key_env: Some(var.to_string()),
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: None,
        }];
        // No usable providers → from_config returns None.
        assert!(AudioTranscriber::from_config(&config).is_none());
    }

    /// Spec: a config row with `provider = "custom"` but missing `command`
    /// must be skipped (operator misconfiguration) rather than panicking
    /// or substituting a default that runs arbitrary user state.
    #[test]
    fn custom_stt_without_command_template_is_skipped() {
        use crate::config::AudioModelConfig;
        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: None,
        }];
        // No other providers in the chain → from_config returns None.
        assert!(AudioTranscriber::from_config(&config).is_none());
    }

    /// Spec: a `command` template missing the `{audio_file}` placeholder
    /// is operator error — the CLI would have nothing to transcribe.
    /// Surface as "skip" so the fallback chain can recover, not as a
    /// hard panic at build time.
    #[test]
    fn custom_stt_command_without_audio_file_token_is_skipped() {
        use crate::config::AudioModelConfig;
        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some("echo hello".to_string()),
        }];
        assert!(AudioTranscriber::from_config(&config).is_none());
    }

    #[tokio::test]
    async fn transcribe_rejects_undersized_files() {
        let transcriber = AudioTranscriber {
            entries: Vec::new(),
            max_file_size: 20 * 1024 * 1024,
            min_file_size: 1024,
            default_language: None,
            prompt: None,
            temperature: 0.0,
            beam_size: 5,
            no_speech_threshold: 0.6,
            timestamps: false,
            cjk_spacing: false,
            default_timeout: Duration::from_secs(60),
        };
        let bytes = vec![0u8; 100]; // < 1024
        let result = transcriber
            .transcribe(&bytes, "audio/ogg", "test.ogg", None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too small"));
    }

    #[tokio::test]
    async fn transcribe_rejects_oversized_files() {
        let transcriber = AudioTranscriber {
            entries: Vec::new(),
            max_file_size: 1000,
            min_file_size: 10,
            default_language: None,
            prompt: None,
            temperature: 0.0,
            beam_size: 5,
            no_speech_threshold: 0.6,
            timestamps: false,
            cjk_spacing: false,
            default_timeout: Duration::from_secs(60),
        };
        let bytes = vec![0u8; 2000]; // > 1000
        let result = transcriber
            .transcribe(&bytes, "audio/ogg", "test.ogg", None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Spec: the fallback chain in `AudioTranscriber::transcribe` must move
    /// on to the next provider when the first one fails, and the
    /// `TranscriptionAttempt` log must record both attempts in order so
    /// downstream observability can show what was tried.
    #[tokio::test]
    async fn fallback_chain_advances_past_failing_provider() {
        // Server returns 503 on the first endpoint, 200 on the second.
        let server_a = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("primary down"))
            .expect(1)
            .mount(&server_a)
            .await;

        let server_b = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text": "secondary served it"})),
            )
            .expect(1)
            .mount(&server_b)
            .await;

        let transcriber = AudioTranscriber {
            entries: vec![
                ProviderEntry {
                    provider: Box::new(WhisperProvider {
                        client: Client::new(),
                        api_key: "k1".to_string(),
                        base_url: server_a.uri(),
                        default_model: "whisper-1".to_string(),
                        provider_name: "primary".to_string(),
                    }),
                    model: None,
                    language: None,
                    timeout: Some(Duration::from_secs(5)),
                },
                ProviderEntry {
                    provider: Box::new(WhisperProvider {
                        client: Client::new(),
                        api_key: "k2".to_string(),
                        base_url: server_b.uri(),
                        default_model: "whisper-1".to_string(),
                        provider_name: "secondary".to_string(),
                    }),
                    model: None,
                    language: None,
                    timeout: Some(Duration::from_secs(5)),
                },
            ],
            max_file_size: 20 * 1024 * 1024,
            min_file_size: 1,
            default_language: None,
            prompt: None,
            temperature: 0.0,
            beam_size: 5,
            no_speech_threshold: 0.6,
            timestamps: false,
            cjk_spacing: false,
            default_timeout: Duration::from_secs(5),
        };

        let (text, attempts) = transcriber
            .transcribe(b"audio-bytes", "audio/wav", "x.wav", None)
            .await
            .expect("fallback must succeed once secondary returns 200");
        assert_eq!(text, "secondary served it");
        assert_eq!(attempts.len(), 2, "both attempts must be recorded");
        assert_eq!(attempts[0].provider, "primary");
        assert!(
            matches!(attempts[0].outcome, AttemptOutcome::Failed(_)),
            "primary attempt must be marked failed"
        );
        assert_eq!(attempts[1].provider, "secondary");
        assert!(matches!(attempts[1].outcome, AttemptOutcome::Success));
    }

    /// Spec: the engine normalises the caller's hint before any provider
    /// sees it. An injected hint is dropped (no `language`, no `callback`
    /// on the wire); a well-formed one reaches the provider intact.
    #[tokio::test]
    async fn transcribe_drops_injected_language_hint_before_provider() {
        use providers::DeepgramProvider;
        use wiremock::matchers::{query_param, query_param_is_missing};

        let server = MockServer::start().await;
        let ok = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": {"channels": [{"alternatives": [{"transcript": "ok"}]}]}
        }));
        Mock::given(method("POST"))
            .and(wm_path("/v1/listen"))
            .and(query_param("model", "nova-3"))
            .and(query_param_is_missing("language"))
            .and(query_param_is_missing("callback"))
            .respond_with(ok.clone())
            .expect(1)
            .named("injected hint dropped")
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/listen"))
            .and(query_param("language", "en-US"))
            .respond_with(ok)
            .expect(1)
            .named("valid hint forwarded")
            .mount(&server)
            .await;

        let transcriber = AudioTranscriber {
            entries: vec![ProviderEntry {
                provider: Box::new(DeepgramProvider {
                    client: Client::new(),
                    api_key: "k".to_string(),
                    base_url: server.uri(),
                    default_model: "nova-3".to_string(),
                }),
                model: None,
                language: None,
                timeout: Some(Duration::from_secs(5)),
            }],
            max_file_size: 1024,
            min_file_size: 1,
            default_language: None,
            prompt: None,
            temperature: 0.0,
            beam_size: 5,
            no_speech_threshold: 0.6,
            timestamps: false,
            cjk_spacing: false,
            default_timeout: Duration::from_secs(5),
        };

        transcriber
            .transcribe(
                b"audio",
                "audio/wav",
                "x.wav",
                Some("en&callback=https://evil/x"),
            )
            .await
            .expect("injected hint must be dropped, not sent");
        transcriber
            .transcribe(b"audio", "audio/wav", "x.wav", Some(" EN-US"))
            .await
            .expect("valid hint must be forwarded");
        server.verify().await;
    }

    #[test]
    fn cjk_spacing_separates_scripts_and_keeps_existing_spaces() {
        assert_eq!(
            cjk_spacing("我用Borg写了3个skill"),
            "我用 Borg 写了 3 个 skill"
        );
        assert_eq!(cjk_spacing("已经 有 空格 Borg"), "已经 有 空格 Borg");
        assert_eq!(cjk_spacing("テストabc"), "テスト abc");
        assert_eq!(cjk_spacing("한글test"), "한글 test");
        // Punctuation and plain Latin text are untouched.
        assert_eq!(cjk_spacing("hello, world 2.0"), "hello, world 2.0");
        assert_eq!(cjk_spacing("你好，世界。"), "你好，世界。");
    }
}
