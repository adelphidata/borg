use std::time::Duration;

use anyhow::Context;
use anyhow::{bail, Result};
use reqwest::Client;
use tracing::{debug, warn};

use crate::config::{AudioModelConfig, Config};
use crate::media::core::ensure_success;

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

        let resp = ensure_success(resp, &self.provider_name).await?;

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
    base_url: String,
    default_model: String,
}

#[async_trait::async_trait]
impl TranscriptionProvider for DeepgramProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut url = format!("{}/v1/listen?model={model}", self.base_url);
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

        let resp = ensure_success(resp, "Deepgram").await?;

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
        if self.format {
            let lang = req.language.clone().unwrap_or_else(|| "en".to_string());
            form = form.text("language", lang);
            form = form.text("format", "true");
        } else if let Some(ref lang) = req.language {
            form = form.text("language", lang.clone());
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

        let resp = ensure_success(resp, "xAI STT").await?;

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

// ── Custom STT Provider (local CLI via shell template) ──

/// Custom STT — shells out to an operator-defined command template.
/// Template tokens include `{audio_file}`/`{input_path}`, `{output_path}`,
/// `{language}`, and `{model}`. The command can write the transcript to stdout
/// or `{output_path}`. Mirrors `CustomTtsProvider` so any local CLI inference
/// tool (Parakeet, faster-whisper, MLX-whisper, `whisper.cpp`'s `main` binary,
/// etc.) becomes a one-config-line install without needing whisper-rs's heavy
/// C++ build path.
struct CustomSttProvider {
    name: String,
    /// Command template with custom STT placeholders.
    command_template: String,
    /// Optional local model identifier/path exposed as `{model}`.
    model: String,
    /// Extension to give the temp audio file; some CLIs sniff by extension.
    audio_extension: String,
}

#[async_trait::async_trait]
impl TranscriptionProvider for CustomSttProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let template = self.command_template.clone();
        let ext = self.audio_extension.clone();
        let model = self.model.clone();
        let audio_bytes = req.audio_bytes.to_vec();
        let language = req.language.clone().unwrap_or_default();
        let timeout = req.timeout;

        tokio::task::spawn_blocking(move || {
            custom_transcribe(&template, &audio_bytes, &ext, &language, &model, timeout)
        })
        .await
        .context("custom stt task panicked")?
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn custom_transcribe(
    template: &str,
    audio_bytes: &[u8],
    extension: &str,
    language: &str,
    model: &str,
    timeout: Duration,
) -> Result<String> {
    use std::io::{Read as _, Write as _};
    use std::process::{Command, Stdio};

    // Same trust boundary as `CustomTtsProvider`'s voice id — reuse the
    // validator rather than letting model-supplied language hints become
    // arbitrary shell flags or paths.
    if !language.is_empty() {
        crate::media::tts::validate_voice_id(language).context("custom STT language hint")?;
    }

    let mut audio_file = tempfile::Builder::new()
        .suffix(&format!(".{extension}"))
        .tempfile()?;
    audio_file.write_all(audio_bytes)?;
    audio_file.flush()?;
    let audio_path = audio_file.path().to_string_lossy().into_owned();
    let out_file = tempfile::Builder::new().suffix(".txt").tempfile()?;
    let out_path = out_file.path().to_string_lossy().into_owned();
    let out_dir = out_file
        .path()
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let cmd_str = crate::media::shell_template::render(
        template,
        &[
            ("audio_file", &audio_path),
            ("input_path", &audio_path),
            ("output_path", &out_path),
            ("output_dir", &out_dir),
            ("format", "txt"),
            ("language", language),
            ("model", model),
        ],
    );

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn custom stt")?;

    let started = std::time::Instant::now();
    let status = loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("custom stt timed out after {timeout:?}");
        }
        match child.try_wait()? {
            Some(status) => break status,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    let mut stdout = String::new();
    if let Some(mut s) = child.stdout.take() {
        s.read_to_string(&mut stdout)?;
    }
    let mut stderr = String::new();
    if let Some(mut s) = child.stderr.take() {
        s.read_to_string(&mut stderr)?;
    }

    if !status.success() {
        bail!(
            "custom stt exited with {status}: {}",
            stderr.trim().chars().take(400).collect::<String>()
        );
    }

    let transcript_from_file = std::fs::read_to_string(out_file.path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let stdout_trimmed = stdout.trim();
    let transcript = transcript_from_file.as_deref().unwrap_or(stdout_trimmed);
    if transcript.is_empty() {
        bail!("custom stt produced empty transcript");
    }
    Ok(transcript.to_string())
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
                    base_url: model_cfg
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.openai.com".to_string()),
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
                    base_url: model_cfg
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.groq.com/openai".to_string()),
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
                    base_url: model_cfg
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.deepgram.com".to_string()),
                    default_model: "nova-3".to_string(),
                }))
            }
            // Mistral's `/v1/audio/transcriptions` is OpenAI-compatible
            // (multipart `file`+`model`), so the existing WhisperProvider
            // works as-is. Default model voxtral-mini-latest matches
            // (DEFAULT_MISTRAL_STT_MODEL in
            "mistral" => {
                let env_var = model_cfg
                    .api_key_env
                    .as_deref()
                    .unwrap_or("MISTRAL_API_KEY");
                let api_key = resolve_key(env_var)?;
                Some(Box::new(WhisperProvider {
                    client: client.clone(),
                    api_key,
                    base_url: model_cfg
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.mistral.ai".to_string()),
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
                    base_url: model_cfg
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.x.ai/v1".to_string()),
                    // `None` preserves the historical default (format on,
                    // diarize off). The Option fields exist so operators
                    // can flip either behavior per-model row.
                    format: model_cfg.xai_stt_format.unwrap_or(true),
                    diarize: model_cfg.xai_stt_diarize.unwrap_or(false),
                }))
            }
            // Local CLI shell-template. Unblocks Parakeet, faster-whisper,
            // MLX-whisper, and any other ASR binary without forcing the
            // operator to wrap it in an HTTP server first. No API key —
            // skip only when the template is missing (operator error).
            "custom" => {
                let template = model_cfg
                    .command
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())?;
                if !template.contains("{audio_file}") && !template.contains("{input_path}") {
                    warn!(
                        "STT provider 'custom' is missing `{{audio_file}}` or `{{input_path}}` in its command \
                         template; skipping"
                    );
                    return None;
                }
                let ext = match model_cfg
                    .model
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("mp3") => "mp3".to_string(),
                    Some("flac") => "flac".to_string(),
                    Some("ogg") => "ogg".to_string(),
                    _ => "wav".to_string(),
                };
                Some(Box::new(CustomSttProvider {
                    name: "custom".to_string(),
                    command_template: template.to_string(),
                    model: model_cfg.model.clone().unwrap_or_default(),
                    audio_extension: ext,
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

    /// Spec: every documented STT provider arm in
    /// `AudioTranscriber::build_provider` must successfully construct when
    /// its credentials are present. Mirrors `tts.rs::from_config_builds_every_provider_arm`.
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

    /// Spec: `custom` STT provider executes the operator's shell template
    /// and returns stdout as the transcript. Uses `cat` so the test stays
    /// hermetic (no Parakeet / faster-whisper / Python deps to install).
    #[tokio::test]
    async fn custom_stt_runs_shell_template_and_returns_stdout() {
        use crate::config::AudioModelConfig;
        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        // Test payloads are short; drop the floor so size validation
        // doesn't short-circuit before the provider runs.
        config.audio.min_file_size = 1;
        // `cat {audio_file}` echoes the audio bytes back as the transcript.
        // We feed UTF-8 in, so the round-trip works for assertion purposes.
        config.audio.models = vec![AudioModelConfig {
            provider: "custom".to_string(),
            model: Some("wav".to_string()),
            api_key_env: None,
            language: None,
            timeout_ms: Some(5_000),
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some("test {model} = wav && cat '{audio_file}'".to_string()),
        }];

        let transcriber = AudioTranscriber::from_config(&config).expect("custom STT must build");
        let payload = b"parakeet would put real text here";
        let (text, attempts) = transcriber
            .transcribe(payload, "audio/wav", "test.wav", None)
            .await
            .expect("custom STT transcribe");
        assert_eq!(text, "parakeet would put real text here");
        assert_eq!(attempts.len(), 1);
        assert!(matches!(attempts[0].outcome, AttemptOutcome::Success));
        assert_eq!(attempts[0].provider, "custom");
    }

    /// Spec: a hostile `language` hint containing shell metacharacters
    /// must be rejected before substitution. Otherwise a model-supplied
    /// `language = "en$(touch /tmp/borg-pwn)"` would let the LLM run
    /// arbitrary shell. Mirrors `tts.rs::custom_synthesize_rejects_shell_metachars_in_voice`.
    #[tokio::test]
    async fn custom_stt_rejects_shell_metachars_in_language() {
        use crate::config::AudioModelConfig;
        // Marker file the injection would touch if validation failed.
        let marker = std::env::temp_dir().join("borg-stt-injection-check");
        let _ = std::fs::remove_file(&marker);

        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        config.audio.min_file_size = 1;
        config.audio.models = vec![AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            // Vector tries to break out of the template into a real shell
            // command. Validation must reject before `sh -c` runs.
            language: Some(format!("en$(touch {})", marker.display())),
            timeout_ms: Some(5_000),
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            // Template references `{audio_file}` so build_provider accepts
            // it; the language-validation check fires at transcribe time.
            command: Some("cat {audio_file}".to_string()),
        }];
        let transcriber = AudioTranscriber::from_config(&config).expect("custom STT must build");

        let result = transcriber
            .transcribe(b"ignored", "audio/wav", "x.wav", None)
            .await;
        assert!(result.is_err(), "injection vector must error");
        assert!(
            !marker.exists(),
            "shell command ran despite language validation — security regression"
        );
    }

    /// Spec: an empty-transcript stdout must surface as an error so the
    /// fallback chain can try the next provider rather than silently
    /// committing a blank transcription.
    #[tokio::test]
    async fn custom_stt_empty_stdout_errors() {
        use crate::config::AudioModelConfig;
        let mut config = crate::config::Config::default();
        config.audio.enabled = true;
        config.audio.min_file_size = 1;
        config.audio.models = vec![AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: Some(5_000),
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            // Command that exits 0 with no stdout — silent ASR failure.
            command: Some("true # consume {audio_file}".to_string()),
        }];
        let transcriber = AudioTranscriber::from_config(&config).expect("custom STT must build");
        let err = transcriber
            .transcribe(b"some-audio", "audio/wav", "x.wav", None)
            .await
            .expect_err("empty transcript must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("All transcription providers failed"),
            "expected fallback-chain failure summary, got: {msg}"
        );
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

    // ── Wiremock-backed provider HTTP tests ──
    //
    // These exercise the actual `reqwest` request shape (method, path,
    // auth header, multipart form fields) against a `MockServer`. Until
    // they existed, every cloud STT provider could regress its wire
    // protocol without a single test failing. We bypass `from_config`
    // and instantiate providers directly so we can pin `base_url` at
    // the mock URL — production callers still go through `from_config`.

    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_request<'a>(audio: &'a [u8]) -> TranscriptionRequest<'a> {
        TranscriptionRequest {
            audio_bytes: audio,
            mime_type: "audio/wav".to_string(),
            filename: "clip.wav".to_string(),
            model: None,
            language: None,
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn whisper_provider_returns_transcript_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .and(header("authorization", "Bearer sk-mock-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text": "hello from mock"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = WhisperProvider {
            client: Client::new(),
            api_key: "sk-mock-key".to_string(),
            base_url: server.uri(),
            default_model: "whisper-1".to_string(),
            provider_name: "openai".to_string(),
        };
        let transcript = provider
            .transcribe(&make_request(b"audio-bytes"))
            .await
            .expect("mocked success");
        assert_eq!(transcript, "hello from mock");
    }

    #[tokio::test]
    async fn whisper_provider_surfaces_5xx_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream busy"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = WhisperProvider {
            client: Client::new(),
            api_key: "sk".to_string(),
            base_url: server.uri(),
            default_model: "whisper-1".to_string(),
            provider_name: "openai".to_string(),
        };
        let err = provider
            .transcribe(&make_request(b"audio"))
            .await
            .expect_err("503 must surface");
        let msg = format!("{err}");
        // The fallback chain reads the provider's error string to decide
        // whether to retry the next provider, so the status + provider
        // name need to actually be in the message.
        assert!(msg.contains("openai"), "missing provider name: {msg}");
        assert!(msg.contains("503"), "missing status: {msg}");
        assert!(msg.contains("upstream busy"), "missing body: {msg}");
    }

    #[tokio::test]
    async fn whisper_provider_errors_on_missing_text_field() {
        // OpenAI/Groq sometimes return 200 with an unexpected payload
        // shape on partial outages. Treat that as a failed attempt so
        // the chain falls over to the next provider rather than
        // returning an empty transcript to the user.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"unrelated": 1})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = WhisperProvider {
            client: Client::new(),
            api_key: "sk".to_string(),
            base_url: server.uri(),
            default_model: "whisper-1".to_string(),
            provider_name: "openai".to_string(),
        };
        let err = provider
            .transcribe(&make_request(b"audio"))
            .await
            .expect_err("missing text must error");
        assert!(format!("{err}").contains("Whisper"));
    }

    #[tokio::test]
    async fn deepgram_provider_returns_transcript_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/listen"))
            .and(header("authorization", "Token dg-mock-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": {"channels": [{"alternatives": [{"transcript": "from deepgram"}]}]}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = DeepgramProvider {
            client: Client::new(),
            api_key: "dg-mock-token".to_string(),
            base_url: server.uri(),
            default_model: "nova-3".to_string(),
        };
        let transcript = provider
            .transcribe(&make_request(b"audio"))
            .await
            .expect("deepgram success");
        assert_eq!(transcript, "from deepgram");
    }

    #[tokio::test]
    async fn deepgram_provider_surfaces_500_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/listen"))
            .respond_with(ResponseTemplate::new(500).set_body_string("deepgram-down"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = DeepgramProvider {
            client: Client::new(),
            api_key: "k".to_string(),
            base_url: server.uri(),
            default_model: "nova-3".to_string(),
        };
        let err = provider
            .transcribe(&make_request(b"audio"))
            .await
            .expect_err("500 must surface");
        let msg = format!("{err}");
        assert!(msg.contains("Deepgram"), "missing provider: {msg}");
        assert!(msg.contains("500"), "missing status: {msg}");
        assert!(msg.contains("deepgram-down"), "missing body: {msg}");
    }

    #[tokio::test]
    async fn xai_stt_provider_returns_transcript_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/stt"))
            .and(header("authorization", "Bearer xai-mock-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "hi xai"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = XaiSttProvider {
            client: Client::new(),
            api_key: "xai-mock-key".to_string(),
            // xAI's base is `…/v1`, the provider appends `/stt`.
            base_url: format!("{}/v1", server.uri()),
            format: true,
            diarize: false,
        };
        let transcript = provider
            .transcribe(&make_request(b"audio"))
            .await
            .expect("xai success");
        assert_eq!(transcript, "hi xai");
    }

    #[tokio::test]
    async fn xai_stt_provider_surfaces_4xx_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/stt"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = XaiSttProvider {
            client: Client::new(),
            api_key: "xai-mock-key".to_string(),
            // xAI's base is `…/v1`, the provider appends `/stt`.
            base_url: format!("{}/v1", server.uri()),
            format: true,
            diarize: false,
        };
        let err = provider
            .transcribe(&make_request(b"audio"))
            .await
            .expect_err("401 must surface");
        let msg = format!("{err}");
        assert!(msg.contains("401"), "missing status: {msg}");
    }

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
}
