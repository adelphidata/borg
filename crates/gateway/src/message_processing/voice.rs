//! Shared inbound voice-transcription helpers.
//!
//! Channels differ in how audio bytes are fetched (auth, attachment shape),
//! so downloading stays per-channel; the transcribe-and-warn step and the
//! transcript-append convention are identical everywhere and live here.

use cortex::media::understanding::AudioTranscriber;

/// Prefix marking an STT transcript injected into inbound message text.
pub(crate) const VOICE_TRANSCRIPT_PREFIX: &str = "[Voice transcript]: ";

/// Transcribe one audio attachment, logging (not propagating) failures.
///
/// STT is best-effort on the inbound path: a failed transcript must never
/// drop the message itself. A failure is warned and replaced by a
/// [`transcription_failure_placeholder`] so the agent still learns that a
/// voice note arrived and why it has no text — a silent `None` made it
/// answer as if the sender had said nothing.
pub(crate) async fn transcribe_or_warn(
    transcriber: &AudioTranscriber,
    bytes: &[u8],
    mime: &str,
    filename: &str,
    channel: &str,
) -> Option<String> {
    match transcriber.transcribe(bytes, mime, filename, None).await {
        Ok((transcript, _)) => Some(transcript),
        Err(e) => {
            tracing::warn!(error = %e, "{channel}: audio transcription failed");
            Some(transcription_failure_placeholder(&e))
        }
    }
}

/// `[audio could not be transcribed: <reason>]` for a failed STT attempt.
///
/// The reason is a short, fixed-vocabulary summary of the engine's error:
/// provider bodies and URLs (which can carry keys or hostnames) never
/// reach the model context.
pub(crate) fn transcription_failure_placeholder(err: &anyhow::Error) -> String {
    format!(
        "[audio could not be transcribed: {}]",
        transcription_failure_reason(err)
    )
}

fn transcription_failure_reason(err: &anyhow::Error) -> String {
    let msg = format!("{err:#}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("too large") {
        return match size_limit_from_message(&msg) {
            Some((size, max)) => format!(
                "file too large ({} > {} limit)",
                human_bytes(size),
                human_bytes(max)
            ),
            None => "file too large".to_string(),
        };
    }
    if lower.contains("too small") {
        return "file too short".to_string();
    }
    if lower.contains("no audio transcription provider") {
        return "no transcription provider configured".to_string();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "provider timed out".to_string();
    }
    "provider error".to_string()
}

/// Pull `(size, max)` out of the engine's
/// `Audio file too large: N bytes (maximum: M)` message.
fn size_limit_from_message(msg: &str) -> Option<(u64, u64)> {
    let mut numbers = msg
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok());
    Some((numbers.next()?, numbers.next()?))
}

fn human_bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} KB", n / KB)
    } else {
        format!("{n} B")
    }
}

/// Append joined voice transcripts to inbound text, replacing it when blank.
pub(crate) fn append_voice_transcripts(text: &mut String, transcripts: &[String]) {
    if transcripts.is_empty() {
        return;
    }
    let joined = transcripts.join(" ");
    *text = if text.trim().is_empty() {
        format!("{VOICE_TRANSCRIPT_PREFIX}{joined}")
    } else {
        format!("{text}\n{VOICE_TRANSCRIPT_PREFIX}{joined}")
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::{AudioModelConfig, Config};

    /// Transcriber backed by the command-template STT provider — hermetic
    /// (no network, no keys): the transcript is whatever the shell command
    /// prints.
    fn transcriber_with_command(cmd: &str) -> AudioTranscriber {
        let mut config = Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![AudioModelConfig {
            provider: "custom".into(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: Some(10_000),
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some(cmd.into()),
        }];
        AudioTranscriber::from_config(&config).expect("custom STT provider configured")
    }

    #[tokio::test]
    async fn transcribe_or_warn_returns_provider_transcript() {
        let transcriber = transcriber_with_command(": {audio_file} && printf 'voice says hi'");
        let bytes = vec![0u8; 2048]; // above the 1 KB minimum size gate
        let out = transcribe_or_warn(&transcriber, &bytes, "audio/ogg", "note.ogg", "test").await;
        assert_eq!(out.as_deref(), Some("voice says hi"));
    }

    /// A failing provider must not vanish: the message continues, but
    /// with a placeholder that tells the agent the note had no transcript.
    #[tokio::test]
    async fn transcribe_or_warn_replaces_provider_failure_with_placeholder() {
        let transcriber = transcriber_with_command(": {audio_file} && exit 3");
        let bytes = vec![0u8; 2048];
        let out = transcribe_or_warn(&transcriber, &bytes, "audio/ogg", "note.ogg", "test").await;
        assert_eq!(
            out.as_deref(),
            Some("[audio could not be transcribed: provider error]")
        );
    }

    /// An oversized note names the limit so the user can be told what to
    /// change; the engine's raw byte counts are rendered human-readable.
    #[tokio::test]
    async fn transcribe_or_warn_names_size_limit_for_oversized_audio() {
        let transcriber = transcriber_with_command(": {audio_file} && printf never");
        // Default `audio.max_file_size` is 20 MiB; 25 MiB is over.
        let bytes = vec![0u8; 25 * 1024 * 1024];
        let out = transcribe_or_warn(&transcriber, &bytes, "audio/ogg", "big.ogg", "test").await;
        assert_eq!(
            out.as_deref(),
            Some("[audio could not be transcribed: file too large (25.0 MB > 20.0 MB limit)]")
        );
    }

    /// The reason vocabulary is fixed: provider error bodies (status
    /// lines, hostnames, anything that could carry a key) never leak into
    /// the model-visible placeholder.
    #[test]
    fn placeholder_reason_never_echoes_provider_error_body() {
        let err = anyhow::anyhow!("All transcription providers failed (1 attempted)")
            .context("openai returned 401 Unauthorized: sk-live-secret at https://api.example");
        let out = transcription_failure_placeholder(&err);
        assert_eq!(out, "[audio could not be transcribed: provider error]");

        let timeout = anyhow::anyhow!("custom stt timed out after 30s");
        assert_eq!(
            transcription_failure_placeholder(&timeout),
            "[audio could not be transcribed: provider timed out]"
        );
    }

    #[test]
    fn append_replaces_blank_text_and_appends_to_existing() {
        let mut blank = "  ".to_string();
        append_voice_transcripts(&mut blank, &["hello".to_string()]);
        assert_eq!(blank, "[Voice transcript]: hello");

        let mut existing = "caption".to_string();
        append_voice_transcripts(&mut existing, &["one".to_string(), "two".to_string()]);
        assert_eq!(existing, "caption\n[Voice transcript]: one two");

        let mut untouched = "caption".to_string();
        append_voice_transcripts(&mut untouched, &[]);
        assert_eq!(untouched, "caption");
    }
}
