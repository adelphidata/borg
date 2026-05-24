use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Result};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::{Config, TtsModelConfig};

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

/// Request data for TTS providers.
struct SynthesisRequest<'a> {
    text: &'a str,
    voice: String,
    format: AudioFormat,
    model: Option<String>,
    timeout: Duration,
}

/// Provider-agnostic TTS trait.
#[async_trait::async_trait]
trait TtsProvider: Send + Sync {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>>;
    fn name(&self) -> &str;
}

// ── OpenAI TTS Provider ──

/// OpenAI TTS provider (POST /v1/audio/speech).
struct OpenAiTtsProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
}

#[async_trait::async_trait]
impl TtsProvider for OpenAiTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let url = format!("{}/v1/audio/speech", self.base_url);

        let body = serde_json::json!({
            "model": model,
            "input": req.text,
            "voice": req.voice,
            "response_format": req.format.extension(),
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("OpenAI TTS returned {status}: {body}");
        }

        Ok(resp.bytes().await?.to_vec())
    }

    fn name(&self) -> &str {
        "openai"
    }
}

// ── ElevenLabs TTS Provider ──

/// Map AudioFormat to ElevenLabs output_format query parameter.
fn elevenlabs_format(fmt: AudioFormat) -> &'static str {
    match fmt {
        AudioFormat::Mp3 => "mp3_44100_128",
        AudioFormat::Opus => "opus_16000",
        AudioFormat::Aac => "aac_44100",
        AudioFormat::Flac => "flac_44100",
        AudioFormat::Wav => "pcm_44100",
    }
}

/// ElevenLabs TTS provider (POST /v1/text-to-speech/{voice_id}).
struct ElevenLabsTtsProvider {
    client: Client,
    api_key: String,
    default_model: String,
}

#[async_trait::async_trait]
impl TtsProvider for ElevenLabsTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        // Validate voice ID to prevent URL path traversal
        if !req
            .voice
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            bail!("Invalid ElevenLabs voice ID: {}", req.voice);
        }
        let url = format!("https://api.elevenlabs.io/v1/text-to-speech/{}", req.voice);

        let body = serde_json::json!({
            "text": req.text,
            "model_id": model,
            "voice_settings": {
                "stability": 0.5,
                "similarity_boost": 0.5,
            },
        });

        let resp = self
            .client
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .query(&[("output_format", elevenlabs_format(req.format))])
            .json(&body)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("ElevenLabs TTS returned {status}: {body}");
        }

        Ok(resp.bytes().await?.to_vec())
    }

    fn name(&self) -> &str {
        "elevenlabs"
    }
}

// ── TTS Synthesizer ──

/// A provider with its per-model configuration.
struct TtsProviderEntry {
    provider: Box<dyn TtsProvider>,
    model: Option<String>,
    voice: Option<String>,
    timeout: Option<Duration>,
}

/// Main TTS engine with multi-provider fallback.
pub struct TtsSynthesizer {
    /// Ordered list of provider entries for fallback.
    entries: Vec<TtsProviderEntry>,
    /// Default voice identifier when none is specified.
    default_voice: String,
    /// Default audio output format.
    default_format: AudioFormat,
    /// Maximum allowed input text length in bytes.
    max_text_length: usize,
    /// Default HTTP timeout for synthesis requests.
    default_timeout: Duration,
}

impl TtsSynthesizer {
    /// Build TTS engine from config. Returns None if disabled or no providers available.
    pub fn from_config(config: &Config) -> Option<Self> {
        if !config.tts.enabled {
            return None;
        }

        let tts = &config.tts;
        let client = Client::new();
        let mut entries: Vec<TtsProviderEntry> = Vec::new();

        for model_cfg in &tts.models {
            match Self::build_provider(&client, model_cfg, config) {
                Some(p) => entries.push(TtsProviderEntry {
                    provider: p,
                    model: model_cfg.model.clone(),
                    voice: model_cfg.voice.clone(),
                    timeout: model_cfg.timeout_ms.map(Duration::from_millis),
                }),
                None => {
                    debug!("Skipping TTS provider '{}': no API key", model_cfg.provider);
                }
            }
        }

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

    fn build_provider(
        client: &Client,
        model_cfg: &TtsModelConfig,
        config: &Config,
    ) -> Option<Box<dyn TtsProvider>> {
        let resolve_key = |env_var: &str| -> Option<String> {
            config
                .resolve_credential_or_env(env_var)
                .or_else(|| std::env::var(env_var).ok())
                .filter(|k| !k.is_empty())
        };

        match model_cfg.provider.as_str() {
            "openai" => {
                let env_var = model_cfg.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(OpenAiTtsProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.openai.com".to_string(),
                    default_model: "tts-1".to_string(),
                }))
            }
            "elevenlabs" => {
                let env_var = model_cfg
                    .api_key_env
                    .as_deref()
                    .unwrap_or("ELEVENLABS_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(ElevenLabsTtsProvider {
                    client: client.clone(),
                    api_key,
                    default_model: "eleven_multilingual_v2".to_string(),
                }))
            }
            other => {
                warn!("Unknown TTS provider: {other}");
                None
            }
        }
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

        if text.len() > self.max_text_length {
            bail!(
                "Text too long: {} characters (maximum: {})",
                text.len(),
                self.max_text_length
            );
        }

        let out_format = format.unwrap_or(self.default_format);
        let mut attempts = Vec::new();

        for entry in &self.entries {
            let voice_id = voice
                .map(String::from)
                .or_else(|| entry.voice.clone())
                .unwrap_or_else(|| self.default_voice.clone());
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

// ─── Markdown stripping for TTS ─────────────────────────────────────────────
//
// Spoken markdown noise ("asterisk asterisk bold asterisk asterisk") destroys
// the listening experience. Mirrors `_strip_markdown_for_tts` in
// `reference/hermes-agent/tools/tts_tool.py:2079-2091` with three additions
// for Rust-side completeness: `__bold__`, `_italic_`, and `~~strikethrough~~`.
//
// Application order matters:
// 1. Fenced code blocks first — their bodies may contain other markdown.
// 2. Links before bare URLs — `[label](url)` becomes `label`; orphan URLs go.
// 3. Bold (`**` / `__`) before italic (`*` / `_`) so `**x**` doesn't degrade
//    into `*x*` and then match the italic pattern asymmetrically.
// 4. Inline code after emphasis.
// 5. Block-level markers (headers, bullets, rules) last.

struct MdRegexes {
    code_block: Regex,
    link: Regex,
    url: Regex,
    bold_star: Regex,
    bold_under: Regex,
    italic_star: Regex,
    italic_under: Regex,
    strike: Regex,
    inline_code: Regex,
    header: Regex,
    list_item: Regex,
    hr: Regex,
    excess_nl: Regex,
}

// Regex literals are compile-time-validated; a compile failure here is a
// programmer bug at static-init time, not a runtime risk. Lint override
// follows the codebase convention (see `memory/injection.rs:38`).
#[allow(clippy::expect_used)]
fn md_regexes() -> &'static MdRegexes {
    static REGEXES: OnceLock<MdRegexes> = OnceLock::new();
    REGEXES.get_or_init(|| MdRegexes {
        // `(?s)` enables dot-matches-newline for code-block bodies.
        code_block: Regex::new(r"(?s)```.*?```").expect("code_block regex"),
        link: Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("link regex"),
        url: Regex::new(r"https?://\S+").expect("url regex"),
        bold_star: Regex::new(r"\*\*(.+?)\*\*").expect("bold_star regex"),
        bold_under: Regex::new(r"__(.+?)__").expect("bold_under regex"),
        italic_star: Regex::new(r"\*(.+?)\*").expect("italic_star regex"),
        // Word-bounded so `snake_case_var` doesn't get mangled.
        italic_under: Regex::new(r"\b_([^_\n]+?)_\b").expect("italic_under regex"),
        strike: Regex::new(r"~~(.+?)~~").expect("strike regex"),
        inline_code: Regex::new(r"`([^`]+)`").expect("inline_code regex"),
        header: Regex::new(r"(?m)^#+\s*").expect("header regex"),
        list_item: Regex::new(r"(?m)^\s*[-*]\s+").expect("list_item regex"),
        hr: Regex::new(r"-{3,}").expect("hr regex"),
        excess_nl: Regex::new(r"\n{3,}").expect("excess_nl regex"),
    })
}

/// Strip markdown formatting that shouldn't be spoken aloud.
///
/// Returns the cleaned, trimmed string. See module-level comment for the
/// Hermes-parity reference and ordering rationale.
pub fn strip_markdown_for_tts(text: &str) -> String {
    let r = md_regexes();
    let mut out = r.code_block.replace_all(text, " ").into_owned();
    out = r.link.replace_all(&out, "$1").into_owned();
    out = r.url.replace_all(&out, "").into_owned();
    out = r.bold_star.replace_all(&out, "$1").into_owned();
    out = r.bold_under.replace_all(&out, "$1").into_owned();
    out = r.italic_star.replace_all(&out, "$1").into_owned();
    out = r.italic_under.replace_all(&out, "$1").into_owned();
    out = r.strike.replace_all(&out, "$1").into_owned();
    out = r.inline_code.replace_all(&out, "$1").into_owned();
    out = r.header.replace_all(&out, "").into_owned();
    out = r.list_item.replace_all(&out, "").into_owned();
    out = r.hr.replace_all(&out, "").into_owned();
    out = r.excess_nl.replace_all(&out, "\n\n").into_owned();
    out.trim().to_string()
}

/// Prepare text for TTS: strip markdown, then truncate at a sentence boundary
/// to fit within `max_bytes`. Single entry point that callers should use over
/// raw `truncate_for_tts` — it avoids the agent's reply being read with
/// "asterisk asterisk" noise around bold runs.
pub fn prepare_for_tts(text: &str, max_bytes: usize) -> String {
    truncate_for_tts(&strip_markdown_for_tts(text), max_bytes)
}

/// Truncate text for TTS, preferring sentence boundaries.
/// Uses byte-level slicing with char-boundary safety to avoid panics on multi-byte UTF-8.
pub fn truncate_for_tts(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Find the last valid char boundary at or before max_bytes
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &text[..end];
    if let Some(pos) = truncated.rfind(['.', '!', '?']) {
        truncated[..=pos].to_string()
    } else {
        let ellipsis_end = max_bytes.saturating_sub(3).min(end);
        let mut e = ellipsis_end;
        while e > 0 && !text.is_char_boundary(e) {
            e -= 1;
        }
        format!("{}...", &text[..e])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn elevenlabs_format_mapping() {
        assert_eq!(elevenlabs_format(AudioFormat::Mp3), "mp3_44100_128");
        assert_eq!(elevenlabs_format(AudioFormat::Opus), "opus_16000");
        assert_eq!(elevenlabs_format(AudioFormat::Aac), "aac_44100");
        assert_eq!(elevenlabs_format(AudioFormat::Flac), "flac_44100");
        assert_eq!(elevenlabs_format(AudioFormat::Wav), "pcm_44100");
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

    #[test]
    fn truncate_for_tts_short_text() {
        assert_eq!(truncate_for_tts("Hello.", 100), "Hello.");
    }

    #[test]
    fn truncate_for_tts_at_sentence_boundary() {
        let text = "First sentence. Second sentence. Third sentence is very long.";
        let result = truncate_for_tts(text, 35);
        assert_eq!(result, "First sentence. Second sentence.");
    }

    #[test]
    fn truncate_for_tts_no_sentence_boundary() {
        let text = "This is one very long word without any sentence ending punctuation";
        let result = truncate_for_tts(text, 20);
        assert_eq!(result, "This is one very ...");
    }

    #[test]
    fn truncate_for_tts_exact_limit() {
        let text = "Hello.";
        assert_eq!(truncate_for_tts(text, 6), "Hello.");
    }

    #[test]
    fn truncate_for_tts_unicode_safe() {
        // Emoji is 4 bytes — slicing at byte 8 would split the emoji
        let text = "Hello \u{1F600} world!";
        let result = truncate_for_tts(text, 8);
        // Should not panic, should truncate before the emoji
        assert!(!result.is_empty());
        assert!(result.len() <= 11); // 8 + "..."
    }

    #[test]
    fn strip_markdown_table_driven() {
        // Each row: (description, input, expected_output).
        // Locks Hermes parity at
        // reference/hermes-agent/tools/tts_tool.py:2079-2091 plus the three
        // Rust-side additions (__bold__, _italic_, ~~strike~~).
        let cases: &[(&str, &str, &str)] = &[
            (
                "fenced code block stripped to space",
                "Before ```rust\nfn x() {}\n``` after",
                "Before   after",
            ),
            (
                "link keeps label drops url",
                "See [the docs](https://example.com/x) please",
                "See the docs please",
            ),
            (
                "bare url removed",
                "Visit https://example.com now",
                "Visit  now",
            ),
            (
                "bold-star unwrapped",
                "This is **important** text",
                "This is important text",
            ),
            (
                "bold-underscore unwrapped",
                "This is __important__ text",
                "This is important text",
            ),
            (
                "italic-star unwrapped",
                "This is *emphasized* text",
                "This is emphasized text",
            ),
            (
                "italic-underscore unwrapped on word boundary",
                "This is _emphasized_ text",
                "This is emphasized text",
            ),
            (
                "snake_case identifier preserved (no italic match)",
                "Call snake_case_var to fix",
                "Call snake_case_var to fix",
            ),
            (
                "strikethrough unwrapped",
                "Use ~~old api~~ instead",
                "Use old api instead",
            ),
            (
                "inline code unwrapped",
                "Run `cargo test` now",
                "Run cargo test now",
            ),
            (
                "atx headers stripped",
                "# Title\n## Subtitle\nbody",
                "Title\nSubtitle\nbody",
            ),
            (
                "list items stripped",
                "- one\n- two\n* three",
                "one\ntwo\nthree",
            ),
            (
                "horizontal rule removed",
                "above\n---\nbelow",
                "above\n\nbelow",
            ),
            ("excess newlines collapsed", "a\n\n\n\nb", "a\n\nb"),
            (
                "nested bold+italic+inline",
                "**Bold _italic_ and `code`** here",
                "Bold italic and code here",
            ),
            (
                "multiple links in one line",
                "[A](http://a) and [B](http://b)",
                "A and B",
            ),
            (
                "link before bare url leaves nothing dangling",
                "[GitHub](https://github.com/x) vs https://gitlab.com",
                "GitHub vs",
            ),
            ("empty input", "", ""),
            (
                "plain text untouched",
                "The quick brown fox.",
                "The quick brown fox.",
            ),
        ];

        for (desc, input, expected) in cases {
            let got = strip_markdown_for_tts(input);
            assert_eq!(&got, expected, "case `{desc}`: input={input:?}");
        }
    }

    #[test]
    fn prepare_for_tts_strips_then_truncates() {
        // Markdown is stripped first, then truncation applies to the cleaned
        // text — so the byte budget covers spoken content, not syntax noise.
        let input = "**First sentence.** *Second sentence.* `Third one.`";
        let out = prepare_for_tts(input, 35);
        // After strip: "First sentence. Second sentence. Third one."
        // Truncated at last sentence boundary <= 35: "First sentence. Second sentence."
        assert_eq!(out, "First sentence. Second sentence.");
    }

    #[test]
    fn prepare_for_tts_handles_code_block_in_reply() {
        // Models drop fenced code blocks into prose replies. The spoken
        // version must not announce backticks or read the code body.
        let input = "Here's the fix:\n```rust\nlet x = 1;\n```\nThat should work.";
        let out = prepare_for_tts(input, 1000);
        assert!(
            !out.contains("```"),
            "fenced markers leaked into TTS text: {out:?}"
        );
        assert!(!out.contains("let x = 1;"), "code body leaked: {out:?}");
        assert!(out.contains("Here's the fix:"));
        assert!(out.contains("That should work."));
    }

    #[test]
    fn truncate_for_tts_cjk_safe() {
        // Each CJK char is 3 bytes; slicing at byte 7 would split the
        // third codepoint mid-byte. The truncator must back off to a char
        // boundary and produce a valid UTF-8 prefix (not panic, not return
        // arbitrary bytes).
        let text = "\u{4F60}\u{597D}\u{4E16}\u{754C}"; // 你好世界
        let result = truncate_for_tts(text, 7);
        // At least the first codepoint must survive — the truncator is
        // not allowed to back off all the way to an empty string.
        assert!(
            result.starts_with("\u{4F60}"),
            "expected output to start with first CJK codepoint, got {result:?}"
        );
        // Output respects max_bytes + "..." ellipsis suffix budget.
        assert!(
            result.len() <= 7 + "...".len(),
            "result {result:?} ({} bytes) exceeded 7 + ellipsis budget",
            result.len()
        );
        // Valid UTF-8 boundary at the end — if the truncator sliced
        // mid-codepoint, the surrounding `String::from_str` would have
        // panicked. This is the explicit sanity guard.
        assert!(result.is_char_boundary(result.len()));
    }
}
