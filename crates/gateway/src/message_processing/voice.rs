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
/// drop the message itself, so errors are warned and swallowed.
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
            None
        }
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

    #[tokio::test]
    async fn transcribe_or_warn_swallows_failure_as_none() {
        // STT is best-effort on the inbound path: a failing provider must
        // yield None (message continues without a transcript), never Some.
        let transcriber = transcriber_with_command(": {audio_file} && exit 3");
        let bytes = vec![0u8; 2048];
        let out = transcribe_or_warn(&transcriber, &bytes, "audio/ogg", "note.ogg", "test").await;
        assert_eq!(out, None);
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
