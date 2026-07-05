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
