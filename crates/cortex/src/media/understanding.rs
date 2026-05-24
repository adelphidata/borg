use std::time::Duration;

use anyhow::{bail, Result};
#[cfg(feature = "local-stt")]
use anyhow::Context;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::{AudioModelConfig, Config};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaUnderstandingConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_true() -> bool {
    true
}

fn default_concurrency() -> usize {
    2
}

impl Default for MediaUnderstandingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            concurrency: 2,
        }
    }
}

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

/// Request data for transcription providers.
struct TranscriptionRequest<'a> {
    audio_bytes: &'a [u8],
    mime_type: String,
    filename: String,
    model: Option<String>,
    language: Option<String>,
    timeout: Duration,
}

/// Provider-agnostic transcription trait.
#[async_trait::async_trait]
trait TranscriptionProvider: Send + Sync {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String>;
    fn name(&self) -> &str;
}

/// OpenAI-compatible Whisper provider (works for OpenAI and Groq).
struct WhisperProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
    provider_name: String,
}

#[async_trait::async_trait]
impl TranscriptionProvider for WhisperProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let url = format!("{}/v1/audio/transcriptions", self.base_url);

        let file_part = reqwest::multipart::Part::bytes(req.audio_bytes.to_vec())
            .file_name(req.filename.clone())
            .mime_str(&req.mime_type)?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", model.to_string());

        if let Some(ref lang) = req.language {
            form = form.text("language", lang.clone());
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("{} returned {status}: {body}", self.provider_name);
        }

        let json: serde_json::Value = resp.json().await?;
        json["text"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("Missing 'text' in Whisper response"))
    }

    fn name(&self) -> &str {
        &self.provider_name
    }
}

/// Deepgram transcription provider.
struct DeepgramProvider {
    client: Client,
    api_key: String,
    default_model: String,
}

#[async_trait::async_trait]
impl TranscriptionProvider for DeepgramProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut url = format!("https://api.deepgram.com/v1/listen?model={model}");
        if let Some(ref lang) = req.language {
            url.push_str(&format!("&language={lang}"));
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Token {}", self.api_key))
            .header("Content-Type", &req.mime_type)
            .body(req.audio_bytes.to_vec())
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Deepgram returned {status}: {body}");
        }

        let json: serde_json::Value = resp.json().await?;
        parse_deepgram_response(&json)
    }

    fn name(&self) -> &str {
        "deepgram"
    }
}

/// Expand a leading `~/` to the user's home directory. Tiny inline
/// helper — adding `shellexpand` as a dep just for this would be
/// overkill, and the wider workspace doesn't use it.
#[cfg(feature = "local-stt")]
fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// Local whisper.cpp STT via the whisper-rs ggml bindings. Gated behind
/// the `local-stt` cargo feature so the C++ build cost stays opt-in.
///
/// The operator downloads a model to `~/.borg/models/whisper-{size}.bin`
/// and points `audio.models[*].api_key_env` (here repurposed via
/// `model.path`) at it.
#[cfg(feature = "local-stt")]
struct LocalWhisperProvider {
    /// Filesystem path to a ggml model (e.g. `~/.borg/models/whisper-base.en.bin`).
    /// Loaded lazily on first transcription so a /voice on with a misconfigured
    /// model surfaces the error inline rather than at app start.
    model_path: std::path::PathBuf,
    /// Thread count for the inference call. Defaults to `num_cpus / 2`,
    /// matching whisper.cpp's recommendation.
    threads: i32,
}

#[cfg(feature = "local-stt")]
#[async_trait::async_trait]
impl TranscriptionProvider for LocalWhisperProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let model_path = self.model_path.clone();
        let threads = self.threads;
        let audio = req.audio_bytes.to_vec();
        let language = req.language.clone();

        // whisper-rs is purely synchronous + heavy — push it to spawn_blocking
        // so a 5-second transcription doesn't stall the tokio dispatcher.
        tokio::task::spawn_blocking(move || {
            local_whisper_transcribe(&model_path, &audio, language.as_deref(), threads)
        })
        .await
        .context("local whisper task panicked")?
    }

    fn name(&self) -> &str {
        "local"
    }
}

#[cfg(feature = "local-stt")]
fn local_whisper_transcribe(
    model_path: &std::path::Path,
    audio_bytes: &[u8],
    language: Option<&str>,
    threads: i32,
) -> Result<String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    if !model_path.exists() {
        bail!(
            "Whisper model not found at {}. Download one (e.g. ggml-base.en.bin) from \
             https://huggingface.co/ggerganov/whisper.cpp and point voice.local_stt_model \
             at it.",
            model_path.display()
        );
    }

    // Decode the WAV bytes to mono f32 samples at 16 kHz — whisper.cpp's
    // native input shape. The voice recorder already emits 16 kHz mono int16
    // PCM, so this is a straight int16→f32 normalize for the production path.
    let reader = hound::WavReader::new(std::io::Cursor::new(audio_bytes))
        .context("whisper: decode WAV header")?;
    let spec = reader.spec();
    if spec.sample_rate != 16_000 || spec.channels != 1 {
        bail!(
            "whisper expects 16 kHz mono input; got {} Hz × {} channels",
            spec.sample_rate,
            spec.channels
        );
    }
    let samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32_768.0))
        .collect::<std::result::Result<_, _>>()
        .context("whisper: decode WAV samples")?;

    let ctx = WhisperContext::new_with_params(
        model_path.to_string_lossy().as_ref(),
        WhisperContextParameters::default(),
    )
    .context("whisper: load model")?;
    let mut state = ctx.create_state().context("whisper: create state")?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads.max(1));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    if let Some(lang) = language {
        params.set_language(Some(lang));
    }

    state
        .full(params, &samples)
        .context("whisper: full inference")?;

    let mut out = String::new();
    let num_segments = state.full_n_segments();
    for i in 0..num_segments {
        let seg = state
            .get_segment(i)
            .ok_or_else(|| anyhow::anyhow!("whisper: segment {i} missing"))?;
        out.push_str(&seg.to_str_lossy().context("whisper: decode segment text")?);
    }
    Ok(out.trim().to_string())
}

/// xAI Grok STT provider — `POST {base}/stt` (multipart). xAI's endpoint
/// shape differs from OpenAI's (`/stt` instead of `/audio/transcriptions`)
/// so it can't reuse [`WhisperProvider`].
struct XaiSttProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// Inverse text normalization — emits punctuation/casing/digits. On
    /// by default.
    format: bool,
    /// Speaker diarization. Off by default (paid feature).
    diarize: bool,
}

#[async_trait::async_trait]
impl TranscriptionProvider for XaiSttProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let url = format!("{}/stt", self.base_url.trim_end_matches('/'));

        let file_part = reqwest::multipart::Part::bytes(req.audio_bytes.to_vec())
            .file_name(req.filename.clone())
            .mime_str(&req.mime_type)?;

        let mut form = reqwest::multipart::Form::new().part("file", file_part);
        if let Some(ref lang) = req.language {
            form = form.text("language", lang.clone());
        }
        if self.format {
            form = form.text("format", "true");
        }
        if self.diarize {
            form = form.text("diarize", "true");
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .timeout(req.timeout)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("xAI STT returned {status}: {body}");
        }

        let json: serde_json::Value = resp.json().await?;
        let text = json["text"].as_str().map(str::trim).unwrap_or("");
        if text.is_empty() {
            bail!("xAI STT returned empty transcript");
        }
        Ok(text.to_string())
    }

    fn name(&self) -> &str {
        "xai"
    }
}

/// Parse Deepgram JSON response to extract transcript text.
pub fn parse_deepgram_response(json: &serde_json::Value) -> Result<String> {
    json["results"]["channels"]
        .as_array()
        .and_then(|channels| channels.first())
        .and_then(|ch| ch["alternatives"].as_array())
        .and_then(|alts| alts.first())
        .and_then(|alt| alt["transcript"].as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("Missing transcript in Deepgram response"))
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
    default_timeout: Duration,
}

impl AudioTranscriber {
    /// Build transcription engine from config. Returns None if disabled or no providers available.
    pub fn from_config(config: &Config) -> Option<Self> {
        if !config.audio.enabled {
            return None;
        }

        let audio = &config.audio;
        let client = Client::new();
        let mut entries: Vec<ProviderEntry> = Vec::new();

        for model_cfg in &audio.models {
            match Self::build_provider(&client, model_cfg, config) {
                Some(p) => entries.push(ProviderEntry {
                    provider: p,
                    model: model_cfg.model.clone(),
                    language: model_cfg.language.clone(),
                    timeout: model_cfg.timeout_ms.map(Duration::from_millis),
                }),
                None => {
                    debug!(
                        "Skipping audio provider '{}': no API key",
                        model_cfg.provider
                    );
                }
            }
        }

        if entries.is_empty() {
            debug!("No audio transcription providers available");
            return None;
        }

        Some(Self {
            entries,
            max_file_size: audio.max_file_size,
            min_file_size: audio.min_file_size,
            default_language: audio.language.clone(),
            default_timeout: Duration::from_millis(audio.timeout_ms),
        })
    }

    fn build_provider(
        client: &Client,
        model_cfg: &AudioModelConfig,
        config: &Config,
    ) -> Option<Box<dyn TranscriptionProvider>> {
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
                Some(Box::new(WhisperProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.openai.com".to_string(),
                    default_model: "whisper-1".to_string(),
                    provider_name: "openai".to_string(),
                }))
            }
            "groq" => {
                let env_var = model_cfg.api_key_env.as_deref().unwrap_or("GROQ_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(WhisperProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.groq.com/openai".to_string(),
                    default_model: "whisper-large-v3-turbo".to_string(),
                    provider_name: "groq".to_string(),
                }))
            }
            "deepgram" => {
                let env_var = model_cfg
                    .api_key_env
                    .as_deref()
                    .unwrap_or("DEEPGRAM_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(DeepgramProvider {
                    client: client.clone(),
                    api_key,
                    default_model: "nova-3".to_string(),
                }))
            }
            // Mistral's `/v1/audio/transcriptions` is OpenAI-compatible
            // (multipart `file`+`model`), so the existing WhisperProvider
            // works as-is. Default model voxtral-mini-latest matches
            // (DEFAULT_MISTRAL_STT_MODEL in
            "mistral" => {
                let env_var = model_cfg.api_key_env.as_deref().unwrap_or("MISTRAL_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(WhisperProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.mistral.ai".to_string(),
                    default_model: "voxtral-mini-latest".to_string(),
                    provider_name: "mistral".to_string(),
                }))
            }
            // Local whisper.cpp via the optional `local-stt` cargo feature.
            // When the feature is off, log + skip so a config row referencing
            // "local" doesn't break the fallback chain.
            #[cfg(feature = "local-stt")]
            "local" => {
                let path_str = model_cfg
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| "~/.borg/models/whisper-base.en.bin".to_string());
                let expanded = shellexpand_home(&path_str);
                let model_path = std::path::PathBuf::from(expanded);
                let cpus = std::thread::available_parallelism()
                    .map(std::num::NonZeroUsize::get)
                    .unwrap_or(2);
                let threads = (cpus / 2).max(1) as i32;
                Some(Box::new(LocalWhisperProvider {
                    model_path,
                    threads,
                }))
            }
            #[cfg(not(feature = "local-stt"))]
            "local" => {
                debug!(
                    "STT provider 'local' configured but cortex was built without \
                     --features local-stt; skipping"
                );
                None
            }
            // xAI Grok STT — `/v1/stt` multipart (not OpenAI-compatible).
            // Matches `_transcribe_xai` at
            "xai" => {
                let env_var = model_cfg.api_key_env.as_deref().unwrap_or("XAI_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(XaiSttProvider {
                    client: client.clone(),
                    api_key,
                    base_url: "https://api.x.ai/v1".to_string(),
                    format: true,
                    diarize: false,
                }))
            }
            other => {
                warn!("Unknown audio transcription provider: {other}");
                None
            }
        }
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

        for entry in &self.entries {
            // Per-model overrides: language, timeout, model
            let lang = language
                .map(String::from)
                .or_else(|| entry.language.clone())
                .or_else(|| self.default_language.clone());
            let timeout = entry.timeout.unwrap_or(self.default_timeout);

            let req = TranscriptionRequest {
                audio_bytes: bytes,
                mime_type: mime_type.to_string(),
                filename: filename.to_string(),
                model: entry.model.clone(),
                language: lang,
                timeout,
            };

            match entry.provider.transcribe(&req).await {
                Ok(text) => {
                    attempts.push(TranscriptionAttempt {
                        provider: entry.provider.name().to_string(),
                        model: entry.model.clone(),
                        outcome: AttemptOutcome::Success,
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parse_deepgram_response_valid() {
        let json = serde_json::json!({
            "results": {
                "channels": [{
                    "alternatives": [{
                        "transcript": "Hello world"
                    }]
                }]
            }
        });
        assert_eq!(parse_deepgram_response(&json).unwrap(), "Hello world");
    }

    #[test]
    fn parse_deepgram_response_missing_transcript() {
        let json = serde_json::json!({"results": {"channels": []}});
        assert!(parse_deepgram_response(&json).is_err());
    }

    #[tokio::test]
    async fn transcribe_rejects_undersized_files() {
        let transcriber = AudioTranscriber {
            entries: Vec::new(),
            max_file_size: 20 * 1024 * 1024,
            min_file_size: 1024,
            default_language: None,
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
            default_timeout: Duration::from_secs(60),
        };
        let bytes = vec![0u8; 2000]; // > 1000
        let result = transcriber
            .transcribe(&bytes, "audio/ogg", "test.ogg", None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }
}
