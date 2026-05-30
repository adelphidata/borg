//! `transcribe_audio` tool.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TranscribeAudioArgs {
    /// Path to an existing local audio file.
    pub path: String,
    /// MIME type override. If omitted, inferred from the file extension.
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Language hint, e.g. `en`.
    #[serde(default)]
    pub language: Option<String>,
}

pub(crate) async fn handle(
    args: &TranscribeAudioArgs,
    config: &Config,
    ctx: &crate::execution_context::ExecutionContext,
) -> Result<String> {
    let path = match ctx.resolve_existing_path(&args.path) {
        Ok(path) => path,
        Err(_) => return Ok(format!("File not found: {}", args.path)),
    };
    if path.is_dir() {
        return Ok(format!(
            "Path is a directory, not an audio file: {}",
            args.path
        ));
    }
    if crate::tool_handlers::filesystem::is_blocked_path(
        &path,
        &config.security.blocked_paths,
        &config.security.allowed_paths,
    ) {
        return Ok(format!(
            "Access denied: {} is in a blocked path.",
            args.path
        ));
    }

    let mime_type = match args.mime_type.as_deref() {
        Some(mime) if is_supported_audio_mime(mime) => mime.to_string(),
        Some(mime) => return Ok(format!("Unsupported audio MIME type: {mime}")),
        None => match infer_audio_mime(&path) {
            Some(mime) => mime.to_string(),
            None => {
                return Ok(format!(
                    "Unsupported audio file extension for {}. Supported: mp3, mp4, mpeg, mpga, m4a, wav, webm, ogg, aac, flac",
                    args.path
                ));
            }
        },
    };

    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) => return Ok(format!("Error reading audio metadata: {e}")),
    };
    let size = metadata.len();
    if size < config.audio.min_file_size {
        return Ok(format!(
            "Audio file too small: {size} bytes (minimum: {})",
            config.audio.min_file_size
        ));
    }
    if size > config.audio.max_file_size {
        return Ok(format!(
            "Audio file too large: {size} bytes (maximum: {})",
            config.audio.max_file_size
        ));
    }

    let Some(transcriber) = crate::media::understanding::AudioTranscriber::from_config(config)
    else {
        return Ok(
            "No audio transcription provider configured. Set `audio.enabled true` and configure `audio.models`."
                .to_string(),
        );
    };

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return Ok(format!("Error reading audio file: {e}")),
    };
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio")
        .to_string();

    match transcriber
        .transcribe(&bytes, &mime_type, &filename, args.language.as_deref())
        .await
    {
        Ok((transcript, attempts)) => {
            let attempt_summary = attempts
                .iter()
                .map(|a| {
                    let model = a
                        .model
                        .as_ref()
                        .map(|m| format!(":{m}"))
                        .unwrap_or_default();
                    format!("{}{}={}", a.provider, model, attempt_status(&a.outcome))
                })
                .collect::<Vec<_>>()
                .join(", ");
            if attempt_summary.is_empty() {
                Ok(transcript)
            } else {
                Ok(format!(
                    "{transcript}\n\n[transcription attempts: {attempt_summary}]"
                ))
            }
        }
        Err(e) => Ok(format!("Transcription error: {e}")),
    }
}

fn attempt_status(outcome: &crate::media::understanding::AttemptOutcome) -> &'static str {
    match outcome {
        crate::media::understanding::AttemptOutcome::Success => "success",
        crate::media::understanding::AttemptOutcome::Skipped(_) => "skipped",
        crate::media::understanding::AttemptOutcome::Failed(_) => "failed",
    }
}

fn infer_audio_mime(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") | Some("mpeg") | Some("mpga") => Some("audio/mpeg"),
        Some("mp4") | Some("m4a") => Some("audio/mp4"),
        Some("wav") => Some("audio/wav"),
        Some("webm") => Some("audio/webm"),
        Some("ogg") | Some("opus") => Some("audio/ogg"),
        Some("aac") => Some("audio/aac"),
        Some("flac") => Some("audio/flac"),
        _ => None,
    }
}

fn is_supported_audio_mime(mime: &str) -> bool {
    matches!(
        mime.split(';').next().unwrap_or("").trim(),
        "audio/mpeg"
            | "audio/mp3"
            | "audio/mp4"
            | "audio/wav"
            | "audio/x-wav"
            | "audio/webm"
            | "audio/ogg"
            | "audio/aac"
            | "audio/flac"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_audio_mime_accepts_supported_extensions() {
        assert_eq!(
            infer_audio_mime(std::path::Path::new("clip.mp3")),
            Some("audio/mpeg")
        );
        assert_eq!(
            infer_audio_mime(std::path::Path::new("clip.wav")),
            Some("audio/wav")
        );
        assert_eq!(infer_audio_mime(std::path::Path::new("clip.txt")), None);
    }

    #[test]
    fn supported_mime_allows_parameters() {
        assert!(is_supported_audio_mime("audio/ogg; codecs=opus"));
        assert!(!is_supported_audio_mime("text/plain"));
    }

    #[tokio::test]
    async fn handle_rejects_oversized_file_before_provider_setup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("large.wav");
        std::fs::write(&path, b"larger than configured max").expect("write test audio");

        let mut config = Config::default();
        config.audio.enabled = true;
        config.audio.min_file_size = 1;
        config.audio.max_file_size = 3;
        config.audio.models.clear();

        let ctx =
            crate::execution_context::ExecutionContext::unrestricted(dir.path().to_path_buf());
        let result = handle(
            &TranscribeAudioArgs {
                path: "large.wav".to_string(),
                mime_type: None,
                language: None,
            },
            &config,
            &ctx,
        )
        .await
        .expect("handler should return text");

        assert!(
            result.contains("Audio file too large"),
            "expected size rejection before provider setup, got {result:?}"
        );
    }
}
