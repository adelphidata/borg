//! Built-in transcription providers and the registry that names them.
//!
//! Same shape as the speech registry: a [`TranscriptionProvider`] impl plus
//! one [`SttProviderDef`] row in [`PROVIDERS`] holding the defaults, so the
//! engine and the Settings UI read one table. Any OpenAI-compatible server
//! plugs in through the `openai` row's `base_url`, any local engine through
//! `custom`.

use std::time::Duration;

use anyhow::Context;
use anyhow::{bail, Result};
use reqwest::Client;
use tracing::{debug, warn};

use crate::config::{AudioModelConfig, Config};
use crate::media::core::ensure_success;
use crate::media::provider_info::ProviderInfo;

/// Request data for transcription providers.
pub(super) struct TranscriptionRequest<'a> {
    pub audio_bytes: &'a [u8],
    pub mime_type: String,
    pub filename: String,
    pub model: Option<String>,
    pub language: Option<String>,
    /// Names and phrases the recording is likely to contain. Whisper-shaped
    /// providers take it as `prompt`; the rest ignore it.
    pub prompt: Option<String>,
    /// Decoding temperature; 0.0 is the provider default and is not sent.
    pub temperature: f64,
    /// Beam width for the on-device decoder; cloud providers ignore it.
    #[cfg_attr(not(feature = "local-stt"), allow(dead_code))]
    pub beam_size: u32,
    /// Silence confidence the on-device decoder needs before dropping a stretch.
    #[cfg_attr(not(feature = "local-stt"), allow(dead_code))]
    pub no_speech_threshold: f64,
    /// One line per segment with its time span; the on-device decoder only.
    #[cfg_attr(not(feature = "local-stt"), allow(dead_code))]
    pub timestamps: bool,
    pub timeout: Duration,
}

/// Provider-agnostic transcription trait.
#[async_trait::async_trait]
pub(super) trait TranscriptionProvider: Send + Sync {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String>;
    fn name(&self) -> &str;
}

/// OpenAI-compatible Whisper provider (works for OpenAI and Groq).
pub(super) struct WhisperProvider {
    pub(super) client: Client,
    pub(super) api_key: String,
    pub(super) base_url: String,
    pub(super) default_model: String,
    pub(super) provider_name: String,
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
        if let Some(ref prompt) = req.prompt {
            form = form.text("prompt", prompt.clone());
        }
        // Zero is every provider's own default; sending it would only trip
        // the endpoints that reject the field.
        if req.temperature > 0.0 {
            form = form.text("temperature", req.temperature.to_string());
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
pub(super) struct DeepgramProvider {
    pub(super) client: Client,
    pub(super) api_key: String,
    pub(super) base_url: String,
    pub(super) default_model: String,
}

#[async_trait::async_trait]
impl TranscriptionProvider for DeepgramProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        // Query values go through reqwest's encoder, never string
        // concatenation: a language hint can only ever be one parameter.
        let mut query: Vec<(&str, &str)> = vec![("model", model)];
        if let Some(ref lang) = req.language {
            query.push(("language", lang));
        }

        let resp = self
            .client
            .post(format!("{}/v1/listen", self.base_url))
            .query(&query)
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

/// ElevenLabs Scribe — `POST {base}/v1/speech-to-text` (multipart
/// `file` + `model_id`, optional `language_code`); answers `{ "text": … }`.
struct ElevenLabsSttProvider {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: String,
}

#[async_trait::async_trait]
impl TranscriptionProvider for ElevenLabsSttProvider {
    async fn transcribe(&self, req: &TranscriptionRequest<'_>) -> Result<String> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let file_part = reqwest::multipart::Part::bytes(req.audio_bytes.to_vec())
            .file_name(req.filename.clone())
            .mime_str(&req.mime_type)?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model_id", model.to_string());
        if let Some(ref lang) = req.language {
            form = form.text("language_code", lang.clone());
        }

        let resp = self
            .client
            .post(format!("{}/v1/speech-to-text", self.base_url))
            .header("xi-api-key", &self.api_key)
            .multipart(form)
            .timeout(req.timeout)
            .send()
            .await?;
        let resp = ensure_success(resp, "ElevenLabs STT").await?;

        let json: serde_json::Value = resp.json().await?;
        json["text"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("Missing 'text' in ElevenLabs STT response"))
    }

    fn name(&self) -> &str {
        "elevenlabs"
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
        let prompt = req.prompt.clone();
        let decode = LocalDecode {
            temperature: req.temperature as f32,
            beam_size: req.beam_size,
            no_speech_threshold: req.no_speech_threshold as f32,
            timestamps: req.timestamps,
        };

        // whisper-rs is purely synchronous + heavy — push it to spawn_blocking
        // so a 5-second transcription doesn't stall the tokio dispatcher.
        tokio::task::spawn_blocking(move || {
            local_whisper_transcribe(
                &model_path,
                &audio,
                language.as_deref(),
                prompt.as_deref(),
                decode,
                threads,
            )
        })
        .await
        .context("local whisper task panicked")?
    }

    fn name(&self) -> &str {
        "local"
    }
}

/// How the on-device decoder searches: `audio.temperature` and `audio.beam_size`.
#[cfg(feature = "local-stt")]
#[derive(Clone, Copy)]
struct LocalDecode {
    temperature: f32,
    beam_size: u32,
    no_speech_threshold: f32,
    timestamps: bool,
}

/// Centiseconds from whisper.cpp as `m:ss.cc`.
#[cfg_attr(not(feature = "local-stt"), allow(dead_code))]
fn segment_clock(centiseconds: i64) -> String {
    let cs = centiseconds.max(0);
    let total_secs = cs / 100;
    format!("{}:{:02}.{:02}", total_secs / 60, total_secs % 60, cs % 100)
}

#[cfg(feature = "local-stt")]
fn local_whisper_transcribe(
    model_path: &std::path::Path,
    audio_bytes: &[u8],
    language: Option<&str>,
    prompt: Option<&str>,
    decode: LocalDecode,
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
    // Beam width 1 is greedy, which is also the cheapest path on a laptop.
    let strategy = if decode.beam_size > 1 {
        SamplingStrategy::BeamSearch {
            beam_size: decode.beam_size as i32,
            patience: -1.0,
        }
    } else {
        SamplingStrategy::Greedy { best_of: 1 }
    };
    let mut params = FullParams::new(strategy);
    params.set_n_threads(threads.max(1));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_temperature(decode.temperature.clamp(0.0, 1.0));
    params.set_no_speech_thold(decode.no_speech_threshold.clamp(0.0, 1.0));
    if let Some(lang) = language {
        params.set_language(Some(lang));
    }
    if let Some(prompt) = prompt {
        params.set_initial_prompt(prompt);
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
        let text = seg.to_str_lossy().context("whisper: decode segment text")?;
        if decode.timestamps {
            out.push_str(&format!(
                "[{} → {}] {}\n",
                segment_clock(seg.start_timestamp()),
                segment_clock(seg.end_timestamp()),
                text.trim()
            ));
        } else {
            out.push_str(&text);
        }
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
    /// Sent on every request: the server-side default has moved between
    /// model generations, so leaving it out makes the wire nondeterministic.
    default_model: String,
    /// Inverse text normalization — emits punctuation/casing/digits. On
    /// by default, but only sent alongside a language: the endpoint
    /// answers HTTP 400 to `format=true` without one.
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

        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", model.to_string());
        // No language means auto-detect; forcing `en` here would silently
        // mistranscribe every other language.
        if let Some(ref lang) = req.language {
            form = form.text("language", lang.clone());
            if self.format {
                form = form.text("format", "true");
            }
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
        // Non-interactive: the audio path is passed as an argument.
        .stdin(Stdio::null())
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

// ── Registry ──

/// What a registry row's constructor receives: the config row plus the
/// key and endpoint already resolved from the provider's [`ProviderInfo`].
struct BuildContext<'a> {
    client: &'a Client,
    row: &'a AudioModelConfig,
    info: &'static ProviderInfo,
    /// Empty for keyless providers.
    api_key: String,
    base_url: String,
}

/// OpenAI-shaped multipart transcription, shared by every provider that
/// speaks `/v1/audio/transcriptions`.
fn build_whisper(cx: BuildContext<'_>) -> Option<Box<dyn TranscriptionProvider>> {
    Some(Box::new(WhisperProvider {
        client: cx.client.clone(),
        api_key: cx.api_key,
        base_url: cx.base_url,
        default_model: cx.info.default_model.to_string(),
        provider_name: cx.info.id.to_string(),
    }))
}

/// One built-in transcription provider.
pub(super) struct SttProviderDef {
    pub(super) info: ProviderInfo,
    /// `None` when the row is unusable; the engine logs and skips it.
    build: fn(BuildContext<'_>) -> Option<Box<dyn TranscriptionProvider>>,
}

impl SttProviderDef {
    /// Build this provider for `row`, or `None` when its key is missing or
    /// the row is incomplete.
    pub(super) fn build(
        &'static self,
        client: &Client,
        row: &AudioModelConfig,
        config: &Config,
    ) -> Option<Box<dyn TranscriptionProvider>> {
        let api_key = self.info.api_key(config, row.api_key_env.as_deref())?;
        (self.build)(BuildContext {
            client,
            row,
            info: &self.info,
            api_key,
            base_url: self.info.base_url_or(row.base_url.as_deref()),
        })
    }
}

/// whisper.cpp in-process, behind the `local-stt` cargo feature. The model
/// path is the row's `model`; `api_key_env` is still read as a path for
/// rows written before `model` carried it.
#[cfg(feature = "local-stt")]
fn build_local(cx: BuildContext<'_>) -> Option<Box<dyn TranscriptionProvider>> {
    let path_str = cx
        .row
        .model
        .clone()
        .or_else(|| cx.row.api_key_env.clone())
        .unwrap_or_else(|| "~/.borg/models/whisper-base.en.bin".to_string());
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2);
    Some(Box::new(LocalWhisperProvider {
        model_path: std::path::PathBuf::from(shellexpand_home(&path_str)),
        threads: (cpus / 2).max(1) as i32,
    }))
}

#[cfg(not(feature = "local-stt"))]
fn build_local(_: BuildContext<'_>) -> Option<Box<dyn TranscriptionProvider>> {
    debug!("STT provider 'local' needs a build with --features local-stt; skipping");
    None
}

/// Local CLI shell template: Parakeet, faster-whisper, MLX-whisper, or any
/// other ASR binary, without wrapping it in an HTTP server first.
fn build_custom(cx: BuildContext<'_>) -> Option<Box<dyn TranscriptionProvider>> {
    let template = cx.row.command.as_deref().filter(|s| !s.trim().is_empty())?;
    if !template.contains("{audio_file}") && !template.contains("{input_path}") {
        warn!(
            "STT provider 'custom' is missing `{{audio_file}}` or `{{input_path}}` in its command \
             template; skipping"
        );
        return None;
    }
    let ext = match cx
        .row
        .model
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") => "mp3",
        Some("flac") => "flac",
        Some("ogg") => "ogg",
        _ => "wav",
    };
    Some(Box::new(CustomSttProvider {
        name: "custom".to_string(),
        command_template: template.to_string(),
        model: cx.row.model.clone().unwrap_or_default(),
        audio_extension: ext.to_string(),
    }))
}

/// Every built-in transcription provider, in picker order.
pub(super) static PROVIDERS: &[SttProviderDef] = &[
    SttProviderDef {
        info: ProviderInfo {
            key_env: &["OPENAI_API_KEY"],
            key_url: "https://platform.openai.com/api-keys",
            base_url: "https://api.openai.com",
            default_model: "whisper-1",
            ..ProviderInfo::new("openai", "OpenAI")
        },
        build: build_whisper,
    },
    SttProviderDef {
        info: ProviderInfo {
            key_env: &["ELEVENLABS_API_KEY"],
            key_url: "https://elevenlabs.io/app/settings/api-keys",
            base_url: "https://api.elevenlabs.io",
            default_model: "scribe_v1",
            ..ProviderInfo::new("elevenlabs", "ElevenLabs Scribe")
        },
        build: |cx| {
            Some(Box::new(ElevenLabsSttProvider {
                client: cx.client.clone(),
                default_model: cx.info.default_model.to_string(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    SttProviderDef {
        info: ProviderInfo {
            key_env: &["GROQ_API_KEY"],
            key_url: "https://console.groq.com/keys",
            base_url: "https://api.groq.com/openai",
            default_model: "whisper-large-v3-turbo",
            ..ProviderInfo::new("groq", "Groq")
        },
        build: build_whisper,
    },
    SttProviderDef {
        info: ProviderInfo {
            key_env: &["DEEPGRAM_API_KEY"],
            key_url: "https://console.deepgram.com",
            base_url: "https://api.deepgram.com",
            default_model: "nova-3",
            ..ProviderInfo::new("deepgram", "Deepgram")
        },
        build: |cx| {
            Some(Box::new(DeepgramProvider {
                client: cx.client.clone(),
                default_model: cx.info.default_model.to_string(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    // Mistral's transcription endpoint is OpenAI-compatible.
    SttProviderDef {
        info: ProviderInfo {
            key_env: &["MISTRAL_API_KEY"],
            key_url: "https://console.mistral.ai/api-keys",
            base_url: "https://api.mistral.ai",
            default_model: "voxtral-mini-latest",
            ..ProviderInfo::new("mistral", "Mistral Voxtral")
        },
        build: build_whisper,
    },
    // `/v1/stt` multipart, not OpenAI-compatible.
    SttProviderDef {
        info: ProviderInfo {
            key_env: &["XAI_API_KEY"],
            key_url: "https://console.x.ai",
            base_url: "https://api.x.ai/v1",
            default_model: "grok-voice-transcribe-2.0",
            ..ProviderInfo::new("xai", "xAI Grok")
        },
        build: |cx| {
            Some(Box::new(XaiSttProvider {
                client: cx.client.clone(),
                default_model: cx.info.default_model.to_string(),
                // `None` keeps the historical default: format on, diarize off.
                format: cx.row.xai_stt_format.unwrap_or(true),
                diarize: cx.row.xai_stt_diarize.unwrap_or(false),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    SttProviderDef {
        info: ProviderInfo::new("local", "Whisper (on this device)"),
        build: build_local,
    },
    SttProviderDef {
        info: ProviderInfo {
            requires_command: true,
            ..ProviderInfo::new("custom", "Custom command")
        },
        build: build_custom,
    },
];

/// Look up a built-in transcription provider by id.
pub(super) fn find(id: &str) -> Option<&'static SttProviderDef> {
    crate::media::provider_info::find(PROVIDERS, id, |d| &d.info)
}

/// Every built-in transcription provider, for pickers.
pub fn providers() -> impl Iterator<Item = &'static ProviderInfo> {
    PROVIDERS.iter().map(|d| &d.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::understanding::{AttemptOutcome, AudioTranscriber};
    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
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
    fn registry_is_well_formed() {
        crate::media::provider_info::assert_registry_well_formed(providers());
    }

    #[test]
    fn parse_deepgram_response_missing_transcript() {
        let json = serde_json::json!({"results": {"channels": []}});
        assert!(parse_deepgram_response(&json).is_err());
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

    // ── Wiremock-backed provider HTTP tests ──
    //
    // These exercise the actual `reqwest` request shape (method, path,
    // auth header, multipart form fields) against a `MockServer`. Until
    // they existed, every cloud STT provider could regress its wire
    // protocol without a single test failing. Providers are built
    // directly so we can pin `base_url` at
    // the mock URL — production callers still go through `from_config`.

    fn make_request<'a>(audio: &'a [u8]) -> TranscriptionRequest<'a> {
        TranscriptionRequest {
            audio_bytes: audio,
            mime_type: "audio/wav".to_string(),
            filename: "clip.wav".to_string(),
            model: None,
            language: None,
            prompt: None,
            temperature: 0.0,
            beam_size: 5,
            no_speech_threshold: 0.6,
            timestamps: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn elevenlabs_stt_provider_sends_scribe_form_and_reads_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/speech-to-text"))
            .and(header("xi-api-key", "xi-mock"))
            .and(wiremock::matchers::body_string_contains("scribe_v1"))
            .and(wiremock::matchers::body_string_contains("language_code"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text": "hello from scribe"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = ElevenLabsSttProvider {
            client: Client::new(),
            api_key: "xi-mock".into(),
            base_url: server.uri(),
            default_model: "scribe_v1".into(),
        };
        let mut req = make_request(b"RIFF....WAVEfmt ");
        req.language = Some("en".into());
        assert_eq!(
            provider.transcribe(&req).await.unwrap(),
            "hello from scribe"
        );
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
    async fn whisper_provider_sends_the_vocabulary_hint_as_prompt() {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .and(body_string_contains("name=\"prompt\""))
            .and(body_string_contains("Borg, Adelphi"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ok"})),
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
        let mut req = make_request(b"audio-bytes");
        req.prompt = Some("Borg, Adelphi".to_string());
        provider.transcribe(&req).await.expect("mocked success");
    }

    #[test]
    fn segment_clock_reads_as_minutes_seconds_and_hundredths() {
        assert_eq!(segment_clock(0), "0:00.00");
        assert_eq!(segment_clock(1_234), "0:12.34");
        assert_eq!(segment_clock(60_00 + 5_07), "1:05.07");
        assert_eq!(segment_clock(-5), "0:00.00");
    }

    #[tokio::test]
    async fn whisper_provider_sends_temperature_only_when_set() {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .and(body_string_contains("name=\"temperature\""))
            .and(body_string_contains("0.2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "warm"})),
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
        let mut req = make_request(b"audio-bytes");
        req.temperature = 0.2;
        assert_eq!(
            provider.transcribe(&req).await.expect("mocked success"),
            "warm"
        );

        // The default stays off the wire: a provider that rejects the field
        // must still work out of the box.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .and(body_string_contains("name=\"temperature\""))
            .respond_with(ResponseTemplate::new(400).set_body_string("no temperature here"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "cold"})),
            )
            .mount(&server)
            .await;
        let provider = WhisperProvider {
            base_url: server.uri(),
            ..provider
        };
        let req = make_request(b"audio-bytes");
        assert_eq!(
            provider.transcribe(&req).await.expect("mocked success"),
            "cold"
        );
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
            default_model: "grok-voice-transcribe-2.0".to_string(),
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
            default_model: "grok-voice-transcribe-2.0".to_string(),
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

    /// Matcher asserting a substring is absent from the request body —
    /// wiremock ships only the positive `body_string_contains`.
    struct BodyLacks(&'static str);

    impl wiremock::Match for BodyLacks {
        fn matches(&self, request: &wiremock::Request) -> bool {
            !String::from_utf8_lossy(&request.body).contains(self.0)
        }
    }

    /// Spec: the language hint is one encoded query value. A hint that
    /// carries `&callback=…` must arrive as a single `language` parameter,
    /// never as a second parameter the model controls.
    #[tokio::test]
    async fn deepgram_provider_encodes_language_as_single_query_param() {
        use wiremock::matchers::{query_param, query_param_is_missing};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/listen"))
            .and(query_param("model", "nova-3"))
            .and(query_param("language", "en&callback=https://evil/x"))
            .and(query_param_is_missing("callback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": {"channels": [{"alternatives": [{"transcript": "safe"}]}]}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = DeepgramProvider {
            client: Client::new(),
            api_key: "k".to_string(),
            base_url: server.uri(),
            default_model: "nova-3".to_string(),
        };
        let mut req = make_request(b"audio");
        req.language = Some("en&callback=https://evil/x".to_string());
        assert_eq!(provider.transcribe(&req).await.unwrap(), "safe");
    }

    /// Spec: without a language the request is auto-detect — no forced
    /// `en`, and no `format` flag (the endpoint rejects it without a
    /// language). The model is always named.
    #[tokio::test]
    async fn xai_stt_without_language_sends_model_and_no_forced_english() {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/stt"))
            .and(body_string_contains("name=\"model\""))
            .and(body_string_contains("grok-voice-transcribe-2.0"))
            .and(BodyLacks("name=\"language\""))
            .and(BodyLacks("name=\"format\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "auto"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = XaiSttProvider {
            client: Client::new(),
            api_key: "xai".to_string(),
            base_url: format!("{}/v1", server.uri()),
            default_model: "grok-voice-transcribe-2.0".to_string(),
            format: true,
            diarize: false,
        };
        assert_eq!(
            provider.transcribe(&make_request(b"audio")).await.unwrap(),
            "auto"
        );
    }

    /// Spec: with a language resolved, `language` and `format=true` are
    /// sent, and the row's configured model wins over the default.
    #[tokio::test]
    async fn xai_stt_with_language_sends_format_and_configured_model() {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/v1/stt"))
            .and(body_string_contains("name=\"language\""))
            .and(body_string_contains("name=\"format\""))
            .and(body_string_contains("grok-voice-transcribe-1.0"))
            .and(BodyLacks("grok-voice-transcribe-2.0"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "pinned"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = XaiSttProvider {
            client: Client::new(),
            api_key: "xai".to_string(),
            base_url: format!("{}/v1", server.uri()),
            default_model: "grok-voice-transcribe-2.0".to_string(),
            format: true,
            diarize: false,
        };
        let mut req = make_request(b"audio");
        req.model = Some("grok-voice-transcribe-1.0".to_string());
        req.language = Some("de".to_string());
        assert_eq!(provider.transcribe(&req).await.unwrap(), "pinned");
    }
}
