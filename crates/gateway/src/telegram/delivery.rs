use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::health::ChannelHealthRegistry;
use crate::voice::{should_send_voice_reply, VoiceReplyMode};

use super::api::TelegramClient;

/// Telegram's voice-message bubble expects OGG/Opus. Synth providers that
/// don't natively emit Opus (Edge always returns MP3, Piper returns WAV,
/// Gemini returns WAV) leave us with bytes Telegram rejects with an
/// inscrutable "voice not supported" error. Detect non-OGG output and
/// try a host `ffmpeg` for a one-shot transcode; if ffmpeg isn't
/// installed, fall back to sending the original bytes and log so the
/// operator can install ffmpeg if voice bubbles look broken.
///
/// Returns `(bytes, true)` if conversion succeeded, `(bytes, false)` if
/// the original bytes were OGG/Opus already, or `(original, false)` on
/// any failure (caller still attempts the send — Telegram's error is
/// more actionable than silently dropping the reply).
pub(crate) async fn ensure_opus_for_telegram(audio: Vec<u8>) -> (Vec<u8>, bool) {
    if is_ogg(&audio) {
        return (audio, false);
    }
    match transcode_to_opus(audio.clone()).await {
        Ok(opus) => (opus, true),
        Err(e) => {
            warn!(
                "Telegram voice transcode to OGG/Opus failed ({e}); sending original bytes — \
                 install ffmpeg if voice bubbles are rejected"
            );
            (audio, false)
        }
    }
}

/// OGG container files start with the literal magic `OggS`. The shortest
/// possible OGG page is well over four bytes, so this is a safe sniff.
fn is_ogg(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS")
}

/// Defense-in-depth cap on the input handed to ffmpeg. Real TTS replies
/// are < 1 MiB; anything past this is either a runaway provider or an
/// adversarial payload — fail fast rather than spawn ffmpeg on it.
const MAX_TRANSCODE_INPUT_BYTES: usize = 10 * 1024 * 1024;
/// ffmpeg should finish a sub-megabyte transcode in well under a second.
/// 15 s is a generous backstop against a hung or stuck process — the
/// timeout prevents a stuck ffmpeg from holding the Telegram delivery
/// task indefinitely.
const TRANSCODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Shell out to ffmpeg, piping audio in on stdin and reading OGG/Opus on
/// stdout. Tempfiles would force a redundant disk round-trip for a
/// payload that's already in memory.
///
/// Concurrency model: write stdin and drain stdout/stderr in parallel
/// (`tokio::try_join!`) so a payload larger than the OS pipe buffer
/// (~16 KiB on macOS, ~64 KiB on Linux) doesn't deadlock — without
/// this, ffmpeg blocks waiting for someone to drain its stdout while
/// we block waiting to flush more stdin. Wrapped in a 15 s timeout so
/// a stuck ffmpeg can't hold the Telegram delivery task forever.
async fn transcode_to_opus(audio: Vec<u8>) -> Result<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::Command;

    if audio.len() > MAX_TRANSCODE_INPUT_BYTES {
        anyhow::bail!(
            "audio payload {} bytes exceeds {} byte transcode cap",
            audio.len(),
            MAX_TRANSCODE_INPUT_BYTES
        );
    }

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-c:a",
            "libopus",
            "-b:a",
            "32k",
            "-vbr",
            "on",
            "-application",
            "voip",
            "-f",
            "ogg",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let audio_for_write = audio.clone();

    // Writer task: flush stdin then close. Concurrent with the readers
    // so a large payload doesn't deadlock against ffmpeg's pipe buffer.
    let writer = async move {
        if let Some(mut s) = stdin {
            s.write_all(&audio_for_write).await?;
            s.shutdown().await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let read_stdout = async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            s.read_to_end(&mut buf).await?;
        }
        Ok::<_, std::io::Error>(buf)
    };
    let read_stderr = async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            s.read_to_end(&mut buf).await?;
        }
        Ok::<_, std::io::Error>(buf)
    };

    let joined = tokio::time::timeout(TRANSCODE_TIMEOUT, async {
        let (_, stdout_buf, stderr_buf) = tokio::try_join!(writer, read_stdout, read_stderr)?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, stdout_buf, stderr_buf))
    })
    .await;

    let (status, stdout_buf, stderr_buf) = match joined {
        Ok(Ok(tup)) => tup,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            // Timeout: kill ffmpeg so we don't leak a zombie. `start_kill`
            // sends SIGKILL; the child handle is dropped here so the OS
            // reaps it on the next poll.
            let _ = child.start_kill();
            anyhow::bail!("ffmpeg transcode timed out after {TRANSCODE_TIMEOUT:?}");
        }
    };

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr_buf);
        anyhow::bail!("ffmpeg exited with {status}: {}", stderr_text.trim());
    }
    if !stderr_buf.is_empty() {
        // ffmpeg may emit informational stderr even on success (sample
        // rate mismatches, codec choice notes). Surface at debug so a
        // future quality regression is visible in logs.
        debug!(
            "ffmpeg transcode stderr (success): {}",
            String::from_utf8_lossy(&stderr_buf).trim()
        );
    }
    debug!(
        "ffmpeg transcode: {} bytes in → {} bytes out (OGG/Opus)",
        audio.len(),
        stdout_buf.len()
    );
    Ok(stdout_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec: the OGG sniff must accept the canonical `OggS` magic and
    /// reject everything else (MP3 `ID3`, WAV `RIFF`, FLAC `fLaC`,
    /// random bytes). Sniff drift would either skip needed conversion
    /// (Telegram rejects the bubble) or transcode already-OK files
    /// (CPU-burn + ffmpeg dependency for nothing).
    #[test]
    fn is_ogg_recognizes_magic_only() {
        assert!(is_ogg(b"OggS\x00\x02\x00\x00"));
        assert!(!is_ogg(b"ID3\x03\x00")); // MP3 with ID3v2
        assert!(!is_ogg(b"RIFF\x00\x00\x00\x00WAVE")); // WAV
        assert!(!is_ogg(b"fLaC")); // FLAC
        assert!(!is_ogg(b"")); // empty
        assert!(!is_ogg(b"ogg")); // case-sensitive: lower-case is not the magic
    }

    /// Spec: when the source is already OGG, `ensure_opus_for_telegram`
    /// returns the bytes untouched and reports `converted = false`.
    /// Re-running ffmpeg on already-Opus output would waste CPU and
    /// hide a class of latent bugs around partial double-encoding.
    #[tokio::test]
    async fn ensure_opus_passes_through_ogg() {
        let mut payload = b"OggS\x00\x02".to_vec();
        payload.extend_from_slice(&[0u8; 64]); // padding so it looks like a real page
        let original_len = payload.len();
        let (out, converted) = ensure_opus_for_telegram(payload).await;
        assert!(!converted, "OGG input must not trigger transcode");
        assert_eq!(
            out.len(),
            original_len,
            "OGG input must be returned byte-for-byte"
        );
    }

    /// Spec: if ffmpeg is unavailable (or any transcode error happens),
    /// `ensure_opus_for_telegram` falls back to the original bytes
    /// rather than dropping the reply. This test asserts that contract
    /// by forcing ffmpeg to fail on malformed input — Telegram's "voice
    /// not supported" surface is more actionable than silent failure.
    #[tokio::test]
    async fn ensure_opus_falls_back_on_transcode_failure() {
        // 32 bytes of garbage — definitely not a valid audio file. If
        // ffmpeg is on PATH it'll reject; if it isn't, the spawn fails.
        // Either way the function must return the original bytes.
        let garbage = b"this is not audio at all".to_vec();
        let original = garbage.clone();
        let (out, converted) = ensure_opus_for_telegram(garbage).await;
        assert!(!converted, "transcode of garbage must report failure");
        assert_eq!(out, original, "fallback must preserve original bytes");
    }

    /// Deadlock regression: a payload larger than the OS pipe buffer
    /// (~16 KiB on macOS, ~64 KiB on Linux) used to hang transcode_to_opus
    /// forever because we wrote stdin sequentially before draining
    /// stdout. ffmpeg blocked on full stdout, we blocked on `write_all`.
    /// With concurrent drain + 15 s timeout, this must complete in
    /// bounded time even when ffmpeg can't make progress on garbage.
    ///
    /// 512 KiB chosen to dwarf any reasonable pipe buffer while staying
    /// under the 10 MiB cap and keeping the test fast.
    #[tokio::test]
    async fn ensure_opus_does_not_deadlock_on_large_payload() {
        let large = vec![0xABu8; 512 * 1024];
        let original_len = large.len();
        // Whether ffmpeg is on PATH or not, this MUST return within the
        // 15 s timeout — pre-fix it deadlocked indefinitely.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ensure_opus_for_telegram(large),
        )
        .await;
        let (out, _converted) =
            result.expect("ensure_opus_for_telegram deadlocked on large payload");
        // Fallback (or success) — either way bytes are non-empty.
        assert!(
            !out.is_empty(),
            "expected either transcoded or pass-through bytes, got empty"
        );
        // Pass-through path returns the original 512 KiB. Successful
        // transcode would be smaller. Either is acceptable; the point of
        // this test is that it terminates.
        assert!(
            out.len() == original_len || out.len() < original_len,
            "unexpected expansion: {} → {} bytes",
            original_len,
            out.len()
        );
    }

    /// Defense-in-depth: a multi-gigabyte payload (more than the cap)
    /// must be rejected before ffmpeg is spawned. We pass exactly the
    /// boundary +1 so the cap arithmetic is exercised. Allocation here
    /// is bounded by the cap so the test stays under typical CI memory.
    #[tokio::test]
    async fn ensure_opus_rejects_oversized_input() {
        let oversize = vec![0u8; MAX_TRANSCODE_INPUT_BYTES + 1];
        let original = oversize.clone();
        let (out, converted) = ensure_opus_for_telegram(oversize).await;
        assert!(!converted, "oversize input must not report conversion");
        // Caller still gets bytes back (the fallback path); Telegram
        // will reject them, surfacing the issue to the operator rather
        // than silently dropping the reply.
        assert_eq!(
            out, original,
            "oversize input must round-trip via the fallback path"
        );
    }

    /// Spec: the transcode input cap is exactly 10 MiB, and the guard is
    /// strict `>` — an at-cap payload must still be handed to ffmpeg, only
    /// a payload past the cap is rejected. Asserted through observable
    /// error text with hardcoded byte counts (not the constant) so cap
    /// arithmetic drift is caught: shrinking the cap would reject the
    /// at-cap payload; widening the comparison (`>=`) would too; inverting
    /// it (`<`/`==`) misclassifies the small and over-cap payloads.
    /// Hermetic either way: with ffmpeg on PATH the zero-byte payload is
    /// rejected as invalid input, without it the spawn fails — neither
    /// error mentions the cap.
    #[tokio::test]
    async fn transcode_cap_is_exactly_ten_mib_and_exclusive() {
        // Small input: must never trip the cap error.
        let err = transcode_to_opus(b"tiny garbage".to_vec())
            .await
            .expect_err("garbage input cannot transcode successfully");
        assert!(
            !err.to_string().contains("transcode cap"),
            "small payload wrongly hit the size cap: {err}"
        );

        // Exactly at the cap (10 * 1024 * 1024 bytes): accepted by the size
        // guard, so whatever error surfaces is ffmpeg/spawn-shaped.
        let err = transcode_to_opus(vec![0u8; 10_485_760])
            .await
            .expect_err("zero-filled payload cannot transcode successfully");
        assert!(
            !err.to_string().contains("transcode cap"),
            "at-cap payload must not be rejected by the size guard: {err}"
        );

        // One byte past the cap: rejected before ffmpeg is spawned.
        let err = transcode_to_opus(vec![0u8; 10_485_761])
            .await
            .expect_err("over-cap payload must be rejected");
        assert!(
            err.to_string().contains("transcode cap"),
            "over-cap payload must be rejected by the size guard, got: {err}"
        );
    }

    /// Spec: image extensions route through `sendPhoto`; anything else must
    /// go via `sendDocument` so the original bytes survive (Telegram
    /// re-encodes photos — a PDF or GIF sent as photo is rejected or
    /// flattened). Case-insensitive on the extension.
    #[test]
    fn photo_vs_document_routing_by_extension() {
        let cases = [
            ("/tmp/chart.png", true),
            ("/tmp/pic.JPG", true),
            ("/tmp/pic.jpeg", true),
            ("/tmp/sticker.webp", true),
            ("/tmp/report.pdf", false),
            ("/tmp/anim.gif", false),
            ("/tmp/data.csv", false),
            ("/tmp/noext", false),
        ];
        for (path, want_photo) in cases {
            assert_eq!(
                is_photo_path(std::path::Path::new(path)),
                want_photo,
                "routing for {path}"
            );
        }
    }

    /// Spec: voice send happens only when every prerequisite holds —
    /// synthesizer configured, per-chat voice mode set, and the policy
    /// rule (`should_send_voice_reply`) accepts the response text +
    /// inbound modality. Any missing prerequisite must short-circuit so
    /// the delivery path falls back to plain text.
    #[test]
    fn should_attempt_voice_send_requires_every_prerequisite() {
        // Happy path: everything aligned → true.
        assert!(should_attempt_voice_send(
            true,
            Some(VoiceReplyMode::All),
            "hello there",
            false
        ));

        // Synthesizer unconfigured → false even with mode + text + voice
        // input. This is the "user enabled /voice mode but no TTS provider"
        // shape; we must not crash on synthesize() of a None.
        assert!(!should_attempt_voice_send(
            false,
            Some(VoiceReplyMode::All),
            "hello",
            true
        ));

        // Per-chat mode unset → false. The chat hasn't opted in; we
        // should not infer intent from auto_mode here (auto_mode flows
        // into the default at the resolver, not at this decision).
        assert!(!should_attempt_voice_send(true, None, "hello", true));

        // Mode is VoiceOnly but the inbound was text → false. The
        // policy rule must propagate this — otherwise users get
        // unwanted TTS replies for typed messages.
        assert!(!should_attempt_voice_send(
            true,
            Some(VoiceReplyMode::VoiceOnly),
            "hello",
            false
        ));

        // Mode is VoiceOnly and inbound was voice → true.
        assert!(should_attempt_voice_send(
            true,
            Some(VoiceReplyMode::VoiceOnly),
            "hello",
            true
        ));

        // Mode is Off → always false regardless of inbound modality.
        assert!(!should_attempt_voice_send(
            true,
            Some(VoiceReplyMode::Off),
            "hello",
            true
        ));

        // Error-shaped response → false. The policy rule (in
        // `voice::should_send_voice_reply`) suppresses TTS for
        // operator-facing error strings so the user doesn't hear the
        // bot say "the AI provider is temporarily rate-limited".
        assert!(!should_attempt_voice_send(
            true,
            Some(VoiceReplyMode::All),
            "The AI provider is temporarily rate-limited. Please try again in a moment.",
            true
        ));

        // Empty response → false. The send_response function returns
        // before reaching the voice path on empty inputs, but the
        // decision helper still needs to handle the empty case
        // gracefully in case it's called from a future call site.
        assert!(!should_attempt_voice_send(
            true,
            Some(VoiceReplyMode::All),
            "",
            false
        ));
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn send_response(
    client: &TelegramClient,
    chat_id: i64,
    response_text: &str,
    thread_id: Option<i64>,
    reply_to: Option<i64>,
    input_was_voice: bool,
    voice_reply_mode: Option<VoiceReplyMode>,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
    tts_synthesizer: Option<&cortex::media::tts::TtsSynthesizer>,
) -> Result<()> {
    if response_text.trim().is_empty() {
        tracing::debug!("Telegram response empty after trim, skipping send for chat {chat_id}");
        return Ok(());
    }

    // Chunk the markdown SOURCE at fence-aware boundaries, then render each
    // chunk to HTML independently. Chunking the rendered HTML (the old
    // order) could cut inside a <pre>/<a> tag — Telegram rejects the chunk
    // with "can't parse entities" — and the recovery then re-sent the whole
    // message as plain text, duplicating every chunk already delivered.
    // Fallback is now per-chunk: only the failing chunk degrades to plain
    // text; delivered chunks are never re-sent.
    for (i, (chunk, html)) in plan_render_chunks(response_text).into_iter().enumerate() {
        // Only reply to the original message on the first chunk.
        let reply_id = if i == 0 { reply_to } else { None };
        if let Err(e) = client
            .send_message(chat_id, &html, Some("HTML"), thread_id, reply_id)
            .await
        {
            warn!("HTML send failed for chunk {i}, retrying that chunk as plain text: {e}");
            if let Err(e2) = client
                .send_message(chat_id, &chunk, None, thread_id, reply_id)
                .await
            {
                health
                    .write()
                    .await
                    .record_error("telegram", &e2.to_string());
                return Err(e2);
            }
        }
    }
    health.write().await.record_outbound("telegram");

    // Two-step bind so should_attempt_voice_send's decision is a
    // single pure check while the synthesizer borrow stays inside its
    // own `if let` arm — no unwrap/expect, no clippy lint.
    if let Some(synth) = tts_synthesizer {
        if should_attempt_voice_send(true, voice_reply_mode, response_text, input_was_voice) {
            let tts_text = cortex::media::tts::prepare_for_tts(response_text, 4096);
            match synth
                .synthesize(&tts_text, None, Some(cortex::media::tts::AudioFormat::Opus))
                .await
            {
                Ok((audio_bytes, _, _)) => {
                    // Edge/Gemini/Piper/MiniMax may return non-OGG bytes
                    // even when we ask for Opus — Telegram's voice-message
                    // bubble only accepts OGG/Opus, so run an ffmpeg
                    // conversion when the sniff says we have something
                    // else.
                    let (final_bytes, _converted) = ensure_opus_for_telegram(audio_bytes).await;
                    if let Err(e) = client
                        .send_voice(chat_id, &final_bytes, None, thread_id, None)
                        .await
                    {
                        warn!("Failed to send TTS voice message: {e}");
                    }
                }
                Err(e) => warn!("TTS synthesis failed: {e}"),
            }
        }
    }

    Ok(())
}

/// Pure decision: should the Telegram delivery path attempt a TTS voice
/// reply for this response? Returns `true` iff every one of these is
/// true: a synthesizer is configured, a per-chat voice mode is set, and
/// the policy rule (`should_send_voice_reply`) accepts the response.
///
/// Kept as a pure function so the four-way truth table can be tested
/// without spinning up a Telegram HTTP mock. The `send_response` path
/// is the only caller in production.
pub(crate) fn should_attempt_voice_send(
    synthesizer_configured: bool,
    voice_reply_mode: Option<VoiceReplyMode>,
    response_text: &str,
    input_was_voice: bool,
) -> bool {
    if !synthesizer_configured {
        return false;
    }
    let Some(mode) = voice_reply_mode else {
        return false;
    };
    should_send_voice_reply(mode, response_text, input_was_voice)
}

/// File extensions Telegram accepts through `sendPhoto`. Everything else
/// (PDFs, CSVs, audio, GIFs — Telegram re-encodes photos and would flatten a
/// GIF) goes through `sendDocument`, which preserves the original bytes.
pub(crate) fn is_photo_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp")
    )
}

/// Plan the outbound chunks for a markdown response: fence-aware markdown
/// chunks paired with their rendered HTML.
///
/// The markdown budget leaves headroom for HTML expansion, but entity
/// escaping is up to 5× (`&` → `&amp;`), so escape-heavy content (code
/// fences full of `<`/`&`) can still render past Telegram's 4096-char cap —
/// and `send_message`'s safety-net splitter is markup-blind, which would
/// reintroduce the mid-tag split this path exists to eliminate. Any chunk
/// whose rendering exceeds the wire cap is re-split at half its size and
/// re-rendered; halving terminates because the budget shrinks every round.
fn plan_render_chunks(response_text: &str) -> Vec<(String, String)> {
    use crate::constants::{DEFAULT_MESSAGE_CHUNK_SIZE, TELEGRAM_MARKDOWN_CHUNK_SIZE};

    let mut queue: std::collections::VecDeque<String> =
        crate::message_processing::chunker::chunk_markdown_fenced(
            response_text,
            TELEGRAM_MARKDOWN_CHUNK_SIZE,
        )
        .into();
    let mut out = Vec::new();
    while let Some(chunk) = queue.pop_front() {
        let html = crate::telegram::format::markdown_to_telegram_html(&chunk);
        // Telegram's cap is UTF-16 code units: emoji-heavy content overflows
        // the wire while under the char budget. Measure both renderings — the
        // per-chunk plain-text fallback re-sends the markdown source, so it
        // has to fit too.
        let units = crate::message_processing::chunker::utf16_len(&html)
            .max(crate::message_processing::chunker::utf16_len(&chunk));
        if units > DEFAULT_MESSAGE_CHUNK_SIZE {
            let smaller = chunk.chars().count() / 2;
            if smaller >= 64 {
                let sub =
                    crate::message_processing::chunker::chunk_markdown_fenced(&chunk, smaller);
                if sub.len() > 1 {
                    for s in sub.into_iter().rev() {
                        queue.push_front(s);
                    }
                    continue;
                }
            }
            // Unsplittable below the cap (pathological single token):
            // send_message's own splitter bounds it as a last resort.
        }
        out.push((chunk, html));
    }
    out
}

#[cfg(test)]
mod chunked_send_tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Escape-heavy markdown (dense `&`/`<`) can render to >4096 chars from a
    /// within-budget markdown chunk. The planner must re-split so every
    /// rendered chunk fits the wire cap — otherwise the markup-blind
    /// safety-net splitter cuts inside tags/entities and formatting is lost.
    #[test]
    fn escape_heavy_chunks_are_resplit_until_rendered_html_fits() {
        // ~3300 markdown chars, nearly all requiring entity escaping.
        let mut text = String::from("```xml\n");
        for _ in 0..100 {
            text.push_str("<a>&<b>&<c>&<d>&<e>&<f>&<g>&\n");
        }
        text.push_str("```");
        let planned = plan_render_chunks(&text);
        assert!(!planned.is_empty());
        for (chunk, html) in &planned {
            assert!(
                html.chars().count() <= crate::constants::DEFAULT_MESSAGE_CHUNK_SIZE,
                "rendered chunk exceeds wire cap ({} chars) for markdown chunk of {} chars",
                html.chars().count(),
                chunk.chars().count()
            );
        }
        // Content preserved across the re-splits.
        let total_amps: usize = planned.iter().map(|(c, _)| c.matches('&').count()).sum();
        assert_eq!(total_amps, 700, "escaped content lost in re-split");
    }

    /// Telegram's 4096 cap is UTF-16 code units, and every emoji counts as
    /// two. A chunk within the 3500-char markdown budget can therefore
    /// overflow the wire and get 400-rejected — and the plain-text fallback
    /// re-sent the same oversized chunk. The planner must bound every chunk
    /// (markdown source and rendered HTML alike) in UTF-16 units.
    #[test]
    fn emoji_heavy_chunks_are_resplit_to_fit_the_utf16_wire_cap() {
        // 3300 chars but 5400 UTF-16 units: under the char budget, over the cap.
        let mut text = String::new();
        for _ in 0..300 {
            text.push_str("\u{1F600}\u{1F680}\u{1F389}\u{1F914}\u{1F4A1}\u{1F525}\u{1F44D} ok\n");
        }
        let planned = plan_render_chunks(&text);
        assert!(planned.len() > 1, "expected a re-split");
        for (chunk, html) in &planned {
            for (label, s) in [("markdown", chunk), ("html", html)] {
                assert!(
                    crate::message_processing::chunker::utf16_len(s)
                        <= crate::constants::DEFAULT_MESSAGE_CHUNK_SIZE,
                    "{label} rendering exceeds the UTF-16 wire cap: {} units",
                    crate::message_processing::chunker::utf16_len(s)
                );
            }
        }
        let total: usize = planned
            .iter()
            .map(|(c, _)| c.matches('\u{1F600}').count())
            .sum();
        assert_eq!(total, 300, "emoji content lost in re-split");
    }

    /// Boundary spec: the wire cap is inclusive. A chunk whose rendering
    /// lands at exactly 4000 UTF-16 units fits Telegram's limit and must be
    /// sent as one message — re-splitting it would double the message count
    /// for content that was already legal on the wire.
    #[test]
    fn chunk_rendering_at_exactly_the_wire_cap_is_not_resplit() {
        // 200 ampersands render to "&amp;" (5 units each) + 3000 plain chars:
        // exactly 200 * 5 + 3000 = 4000 rendered units from a 3200-char
        // markdown chunk that fits the 3500-char markdown budget unsplit.
        let text = format!("{}{}", "&".repeat(200), "a".repeat(3000));
        let planned = plan_render_chunks(&text);
        assert_eq!(
            planned.len(),
            1,
            "an exactly-at-cap rendering must ship as a single chunk"
        );
        assert_eq!(
            crate::message_processing::chunker::utf16_len(&planned[0].1),
            4000,
            "test premise: rendering must land exactly on the cap"
        );
        assert_eq!(planned[0].0, text, "chunk content must be untouched");
    }

    /// Spec: when an over-cap chunk cannot be split any further (the
    /// re-chunker hands back a single piece), the planner must emit the
    /// ORIGINAL chunk unchanged and let `send_message`'s splitter bound it
    /// as a last resort. Accepting a single-piece "split" instead would
    /// swap in the re-chunker's trimmed output and silently drop content.
    #[test]
    fn unsplittable_over_cap_chunk_is_emitted_verbatim() {
        // 667 ampersands + newline + 667 trailing spaces: 1335 chars, so the
        // first-pass chunker returns it whole (early return, untrimmed).
        // Rendering is 667 * 5 + 1 + 667 = 4003 units — over the cap — but
        // the halving re-chunker trims the whitespace-only tail away and
        // hands back exactly one piece, so no real split exists.
        let text = format!("{}\n{}", "&".repeat(667), " ".repeat(667));
        let planned = plan_render_chunks(&text);
        assert_eq!(planned.len(), 1, "no-progress re-split must not loop");
        assert_eq!(
            planned[0].0, text,
            "unsplittable chunk must pass through verbatim, not swap in the \
             re-chunker's lossy single piece"
        );
    }

    /// Spec: only the FIRST chunk of a multi-chunk response replies to the
    /// inbound message; follow-up chunks are plain sends. Inverting this
    /// would spam the user with N reply notifications and detach the reply
    /// arrow from the message that prompted the response.
    #[tokio::test]
    async fn only_first_chunk_replies_to_inbound_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .mount(&server)
            .await;

        // Enough lines to force at least two chunks.
        let mut text = String::new();
        for i in 0..250 {
            text.push_str(&format!("HEADMARK filler line {i:03}\n"));
        }
        text.push_str("\nTAILMARK end");

        let client =
            super::super::api::TelegramClient::with_base("123:ABC", &server.uri()).expect("client");
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));

        send_response(
            &client,
            7,
            &text,
            None,
            Some(4242),
            false,
            None,
            &health,
            None,
        )
        .await
        .expect("send must succeed");

        let requests = server.received_requests().await.expect("recorded");
        assert!(requests.len() >= 2, "expected a multi-chunk send");
        for r in &requests {
            let body = String::from_utf8_lossy(&r.body);
            let is_first_chunk = body.contains("HEADMARK filler line 000");
            assert_eq!(
                body.contains("\"reply_to_message_id\":4242"),
                is_first_chunk,
                "reply id must ride on the first chunk and only the first chunk: {body}"
            );
        }
    }

    /// Regression: recovery from a rejected chunk used to re-send the ENTIRE
    /// message as plain text even though earlier chunks were already
    /// delivered. Fallback must be per-chunk — the first chunk goes out
    /// exactly once, only the failing chunk degrades to plain text.
    #[tokio::test]
    async fn failed_chunk_does_not_resend_delivered_chunks() {
        let server = MockServer::start().await;

        // The HTML attempt of the chunk carrying the marker is rejected the
        // way Telegram rejects malformed markup.
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .and(body_string_contains("SECONDCHUNKMARKER"))
            .and(body_string_contains("\"parse_mode\":\"HTML\""))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: can't parse entities"
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Everything else (first chunk HTML, second chunk plain-text retry)
        // succeeds.
        Mock::given(method("POST"))
            .and(path("/bot123:ABC/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .mount(&server)
            .await;

        // Enough distinct lines to force a split ahead of the marker chunk.
        let mut text = String::new();
        for i in 0..250 {
            text.push_str(&format!("FIRSTBLOCK filler line {i:03}\n"));
        }
        text.push_str("\nSECONDCHUNKMARKER tail");

        let client =
            super::super::api::TelegramClient::with_base("123:ABC", &server.uri()).expect("client");
        let health = Arc::new(RwLock::new(ChannelHealthRegistry::new()));

        send_response(&client, 7, &text, None, None, false, None, &health, None)
            .await
            .expect("send must succeed via per-chunk plain-text fallback");

        let requests = server.received_requests().await.expect("recorded");
        let first_chunk_sends = requests
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("FIRSTBLOCK filler line 000"))
            .count();
        assert_eq!(
            first_chunk_sends, 1,
            "already-delivered first chunk must not be re-sent on a later chunk's failure"
        );
        let marker_plain_sends = requests
            .iter()
            .filter(|r| {
                let body = String::from_utf8_lossy(&r.body);
                body.contains("SECONDCHUNKMARKER") && !body.contains("\"parse_mode\":\"HTML\"")
            })
            .count();
        assert_eq!(
            marker_plain_sends, 1,
            "failing chunk must be retried as plain text exactly once"
        );
    }
}
