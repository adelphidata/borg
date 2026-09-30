//! Shared SSE (Server-Sent Events) stream processing.
//!
//! Extracts the common chunk-fetching, buffering, line-splitting, and timeout
//! logic used by both OpenAI and Anthropic streaming paths.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::llm::{FailoverReason, LlmError, StreamEvent};

/// Action returned by the per-line parse callback.
pub enum SseAction {
    /// Line processed, continue reading.
    Continue,
    /// Emit these events, then continue.
    Emit(Vec<StreamEvent>),
    /// Emit these events, then stop (stream complete).
    Done(Vec<StreamEvent>),
    /// Fatal error — abort the stream.
    Error(LlmError),
}

/// Replace any URL in a transport error with `<url>`.
///
/// Transport errors are the one error class that routinely carries the request
/// URL, and this text reaches the user, the log file, and any channel relay. A
/// provider endpoint can embed a deployment id, a project, or a key in its
/// query string, and none of that helps someone read "the connection dropped".
fn strip_urls(text: &str) -> String {
    // `split_inclusive` on whitespace keeps each separator attached to the
    // token before it, so newlines and runs of spaces survive verbatim. Only
    // the URL tokens change; the rest of the message reads as the transport
    // wrote it.
    text.split_inclusive(char::is_whitespace)
        .map(|piece| {
            let (token, sep) = match piece.find(char::is_whitespace) {
                Some(i) => piece.split_at(i),
                None => (piece, ""),
            };
            let trimmed = token.trim_start_matches(['(', '[', '"', '\'']);
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                format!("<url>{sep}")
            } else {
                piece.to_string()
            }
        })
        .collect()
}

/// Process an SSE byte stream, dispatching each non-empty data line to `parse_line`.
///
/// Handles: cancellation, per-chunk timeouts, buffer overflow, line splitting,
/// and async event sending. The `parse_line` callback is sync — it returns
/// `SseAction` variants and never needs to await.
pub async fn process_sse_stream<S, B, E, F>(
    mut stream: S,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    chunk_timeout_secs: u64,
    max_buffer: usize,
    mut parse_line: F,
) -> std::result::Result<(), LlmError>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
    F: FnMut(&str) -> SseAction,
{
    let mut buffer = String::new();
    // Network chunks split anywhere, including inside a multibyte character;
    // the decoder holds an incomplete trailing sequence until the next chunk
    // completes it instead of turning each half into U+FFFD.
    let mut decoder = crate::util::utf8_stream::Utf8StreamDecoder::new();

    loop {
        let chunk = tokio::select! {
            // `biased;` ensures cancellation always wins over a ready stream
            // chunk. Without it, an EOF + already-cancelled token race
            // non-deterministically picks the EOF arm and emits a spurious
            // `StreamEvent::Done`.
            biased;
            _ = cancel.cancelled() => {
                // Do NOT emit StreamEvent::Done here. The agent-loop consumer
                // treats `Done` as the authoritative end-of-turn signal; on a
                // revocation-driven cancel the retry loop must rotate to a
                // healthy credential and stream the real response on the same
                // channel.
                return Ok(());
            }
            maybe_chunk = async {
                if chunk_timeout_secs > 0 {
                    tokio::time::timeout(
                        Duration::from_secs(chunk_timeout_secs),
                        stream.next(),
                    ).await
                } else {
                    Ok(stream.next().await)
                }
            } => {
                match maybe_chunk {
                    Ok(Some(Ok(c))) => c,
                    Ok(Some(Err(e))) => {
                        // The connection broke mid-response. That is not the
                        // same failure as the chunk timeout below: this one
                        // says the provider is unreachable, and retrying it at
                        // ordinary speed spends the whole budget before the
                        // network has had a chance to come back.
                        return Err(LlmError::Retryable {
                            source: anyhow::anyhow!(
                                "Stream read error: {}",
                                strip_urls(&e.to_string())
                            ),
                            retry_after: None,
                            reason: FailoverReason::ConnectionLost,
                            status: None,
                        });
                    }
                    Ok(None) => {
                        // EOF: a provider may close the stream without a trailing
                        // newline on the final data line. Feed any remaining buffered
                        // content through parse_line once so we don't drop the last
                        // delta or the sentinel `[DONE]`.
                        buffer.push_str(&decoder.flush());
                        let tail = buffer.trim();
                        if !tail.is_empty() && !tail.starts_with(':') {
                            match parse_line(tail) {
                                SseAction::Continue => {}
                                SseAction::Emit(events) | SseAction::Done(events) => {
                                    for event in events {
                                        if tx.send(event).await.is_err() {
                                            tracing::debug!(
                                                "SSE: stream receiver closed during EOF flush"
                                            );
                                            return Ok(());
                                        }
                                    }
                                }
                                SseAction::Error(e) => return Err(e),
                            }
                        }
                        let _ = tx.send(StreamEvent::Done).await;
                        return Ok(());
                    }
                    Err(_) => {
                        return Err(LlmError::Retryable {
                            source: anyhow::anyhow!("No data received for {chunk_timeout_secs}s"),
                            retry_after: None,
                            reason: FailoverReason::Timeout,
                            status: None,
                        });
                    }
                }
            }
        };

        buffer.push_str(&decoder.decode(chunk.as_ref()));

        if buffer.len() > max_buffer {
            return Err(LlmError::Retryable {
                source: anyhow::anyhow!("SSE buffer exceeded {max_buffer} bytes"),
                retry_after: None,
                reason: FailoverReason::Overloaded,
                status: None,
            });
        }

        let mut cursor = 0;
        while let Some(rel_end) = buffer[cursor..].find('\n') {
            let line_end = cursor + rel_end;
            let line = buffer[cursor..line_end].trim();
            cursor = line_end + 1;

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            match parse_line(line) {
                SseAction::Continue => {}
                SseAction::Emit(events) => {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            tracing::trace!("SSE: stream receiver closed");
                            return Ok(());
                        }
                    }
                }
                SseAction::Done(events) => {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            tracing::trace!("SSE: stream receiver closed");
                            return Ok(());
                        }
                    }
                    return Ok(());
                }
                SseAction::Error(e) => return Err(e),
            }
        }
        buffer.drain(..cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn make_stream(
        chunks: Vec<&str>,
    ) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
        stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok(s.as_bytes().to_vec()))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn basic_line_splitting() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["data: hello\ndata: world\n"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        let e1 = rx.recv().await.unwrap();
        assert!(matches!(e1, StreamEvent::TextDelta(ref s) if s == "hello"));
        let e2 = rx.recv().await.unwrap();
        assert!(matches!(e2, StreamEvent::TextDelta(ref s) if s == "world"));
        // Stream ended → Done event
        let e3 = rx.recv().await.unwrap();
        assert!(matches!(e3, StreamEvent::Done));
    }

    #[tokio::test]
    async fn multi_chunk_buffering() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        // Data split across chunk boundaries
        let stream = make_stream(vec!["data: hel", "lo\n"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        let e = rx.recv().await.unwrap();
        assert!(matches!(e, StreamEvent::TextDelta(ref s) if s == "hello"));
    }

    /// Collect every `TextDelta` from a byte-chunked stream whose data lines
    /// are echoed back as text.
    async fn decode_chunks(chunks: Vec<Vec<u8>>) -> String {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            match line.strip_prefix("data: ") {
                Some(data) => SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())]),
                None => SseAction::Continue,
            }
        })
        .await
        .expect("stream must complete");
        drop(tx);
        let mut out = String::new();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::TextDelta(t) = event {
                out.push_str(&t);
            }
        }
        out
    }

    #[tokio::test]
    async fn multibyte_char_split_across_chunks_decodes_intact() {
        // Regression: each chunk was decoded on its own, so a character cut
        // by a chunk boundary became U+FFFD. Split every multibyte char at
        // every interior byte offset.
        for text in ["héllo", "日本語", "ok 🦀 done"] {
            let line = format!("data: {text}\n").into_bytes();
            for split in 1..line.len() {
                let (a, b) = line.split_at(split);
                let got = decode_chunks(vec![a.to_vec(), b.to_vec()]).await;
                assert_eq!(got, text, "split at byte {split}");
            }
        }
        // One byte per chunk: a 4-byte char spans four chunks.
        let bytes = "data: 🦀\n".as_bytes().iter().map(|b| vec![*b]).collect();
        assert_eq!(decode_chunks(bytes).await, "🦀");
    }

    #[tokio::test]
    async fn invalid_and_truncated_utf8_still_decode_lossily() {
        // Genuinely invalid bytes are replaced, not held forever.
        let got = decode_chunks(vec![b"data: a\xFFb\n".to_vec()]).await;
        assert_eq!(got, "a\u{FFFD}b");
        // A stream that ends mid-character surfaces one replacement char on
        // the EOF flush rather than dropping the tail.
        let got = decode_chunks(vec![b"data: x".to_vec(), vec![0xE6, 0x97]]).await;
        assert_eq!(got, "x\u{FFFD}");
    }

    #[tokio::test]
    async fn buffer_overflow() {
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["a]".repeat(100).as_str()]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 50, |_| SseAction::Continue).await;

        assert!(matches!(result, Err(LlmError::Retryable { .. })));
    }

    #[tokio::test]
    async fn empty_and_comment_lines_skipped() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec![": comment\n\ndata: real\n"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        let e = rx.recv().await.unwrap();
        assert!(matches!(e, StreamEvent::TextDelta(ref s) if s == "real"));
    }

    #[tokio::test]
    async fn done_action_stops_stream() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["data: first\ndata: [DONE]\ndata: ignored\n"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    SseAction::Done(vec![StreamEvent::Done])
                } else {
                    SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
                }
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        let e1 = rx.recv().await.unwrap();
        assert!(matches!(e1, StreamEvent::TextDelta(ref s) if s == "first"));
        let e2 = rx.recv().await.unwrap();
        assert!(matches!(e2, StreamEvent::Done));
    }

    #[tokio::test]
    async fn cancellation_stops_stream_without_emitting_done() {
        // Regression guard: when the cancel token fires, the SSE loop must
        // exit cleanly WITHOUT pushing `StreamEvent::Done`. The agent-loop
        // consumer treats `Done` as authoritative end-of-turn, so emitting
        // it on a revocation-driven cancel would commit a truncated turn
        // before the outer retry loop's rotation delivers the real response.
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec![]);
        cancel.cancel();

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |_| {
            SseAction::Continue
        })
        .await;
        assert!(
            result.is_ok(),
            "cancel arm returns Ok so the outer reclassifier can decide"
        );

        drop(tx);
        let event = rx.recv().await;
        assert!(
            event.is_none(),
            "cancel arm must not push any StreamEvent; channel should be empty (got {event:?})"
        );
    }

    #[tokio::test]
    async fn error_action_propagates() {
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["data: bad\n"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |_| {
            SseAction::Error(LlmError::Retryable {
                source: anyhow::anyhow!("parse error"),
                retry_after: None,
                reason: FailoverReason::Overloaded,
                status: None,
            })
        })
        .await;

        assert!(matches!(result, Err(LlmError::Retryable { .. })));
    }

    /// Provider closes the connection right after the final delta with no
    /// trailing newline. The previous behaviour silently dropped that line;
    /// the EOF-flush path must surface it before sending Done.
    #[tokio::test]
    async fn eof_flushes_trailing_line_without_newline() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        // Note: NO trailing \n on the final line.
        let stream = make_stream(vec!["data: first\n", "data: last"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(ref s)) if s == "first"));
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(ref s)) if s == "last"));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Done)));
    }

    /// Sentinel `[DONE]` arriving without a trailing newline must still
    /// terminate the stream cleanly. This used to be lost on EOF.
    #[tokio::test]
    async fn eof_flush_handles_done_sentinel_without_newline() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["data: hello\n", "data: [DONE]"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                if data.trim() == "[DONE]" {
                    SseAction::Done(vec![StreamEvent::Done])
                } else {
                    SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
                }
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(ref s)) if s == "hello"));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Done)));
    }

    /// An EOF-flushed line whose parser returns an `Error` must propagate that
    /// error rather than masking it with `Done`.
    #[tokio::test]
    async fn eof_flush_propagates_parse_error_on_trailing_line() {
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["data: oops"]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |_| {
            SseAction::Error(LlmError::Retryable {
                source: anyhow::anyhow!("bad final line"),
                retry_after: None,
                reason: FailoverReason::Overloaded,
                status: None,
            })
        })
        .await;

        assert!(matches!(result, Err(LlmError::Retryable { .. })));
    }

    /// Trailing whitespace/blank only — no parse_line call, no extra event
    /// before `Done`. Guards against double-emitting on benign EOF.
    #[tokio::test]
    async fn eof_with_only_blank_trailing_buffer_emits_done_once() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stream = make_stream(vec!["data: only\n", "   "]);

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |line| {
            if let Some(data) = line.strip_prefix("data: ") {
                SseAction::Emit(vec![StreamEvent::TextDelta(data.to_string())])
            } else {
                SseAction::Continue
            }
        })
        .await;

        assert!(result.is_ok());
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(ref s)) if s == "only"));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Done)));
        // Drop the sender so rx.recv() can return None instead of blocking on
        // the still-live Sender held by this scope.
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    /// A stream that yields one chunk and then fails, the way a connection
    /// reset mid-response looks to the reader.
    fn make_broken_stream(
        message: &str,
    ) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, message.to_string());
        stream::iter(vec![Ok(b"data: partial\n".to_vec()), Err(err)])
    }

    async fn read_error_from(message: &str) -> LlmError {
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        process_sse_stream(
            make_broken_stream(message),
            &tx,
            &cancel,
            0,
            1024 * 1024,
            |_| SseAction::Continue,
        )
        .await
        .expect_err("a failed read must surface an error")
    }

    #[tokio::test]
    async fn a_dropped_connection_is_not_reported_as_a_timeout() {
        // The distinction drives the retry schedule: a timeout means the
        // provider is slow, a dropped connection means it is unreachable.
        let err = read_error_from("connection reset by peer").await;
        assert_eq!(err.reason(), FailoverReason::ConnectionLost);
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_dropped_connection_does_not_leak_the_request_url() {
        // Transport errors routinely carry the URL, and this text reaches the
        // log file and any channel relay.
        let err = read_error_from(
            "error sending request for url (https://api.example.com/v1/chat?key=abc123)",
        )
        .await;

        let text = err.to_string();
        assert!(
            !text.contains("api.example.com"),
            "the endpoint must not survive into the error text: {text}"
        );
        assert!(!text.contains("abc123"));
        assert!(text.contains("<url>"));
    }

    #[tokio::test]
    async fn a_silent_provider_is_still_a_timeout() {
        // Counterpart: the chunk-timeout arm must keep its own class, or the
        // reconnect backoff would swallow slow-but-healthy responses too.
        let (tx, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        // A stream that never yields: `stream::pending` starves the 1s budget.
        let stream = stream::pending::<Result<Vec<u8>, std::io::Error>>();

        let err = process_sse_stream(stream, &tx, &cancel, 1, 1024 * 1024, |_| {
            SseAction::Continue
        })
        .await
        .expect_err("the chunk budget must expire");

        assert_eq!(err.reason(), FailoverReason::Timeout);
    }

    #[test]
    fn stripping_urls_leaves_ordinary_words_alone() {
        assert_eq!(
            strip_urls("connection reset by peer"),
            "connection reset by peer"
        );
        assert_eq!(
            strip_urls("failed to reach https://x.test/a?b=c now"),
            "failed to reach <url> now"
        );
    }

    /// Redaction must not reformat the message. A multi-line transport error
    /// stays readable; only the URL token changes.
    #[test]
    fn stripping_urls_preserves_the_original_layout() {
        assert_eq!(
            strip_urls("error sending request\n  url: https://x.test/v1\n  cause: reset"),
            "error sending request\n  url: <url>\n  cause: reset"
        );
    }
}
