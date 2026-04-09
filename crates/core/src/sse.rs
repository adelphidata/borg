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

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = tx.send(StreamEvent::Done).await;
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
                        return Err(LlmError::Retryable {
                            source: anyhow::anyhow!("Stream read error: {e}"),
                            retry_after: None,
                            reason: FailoverReason::Timeout,
                        });
                    }
                    Ok(None) => {
                        let _ = tx.send(StreamEvent::Done).await;
                        return Ok(());
                    }
                    Err(_) => {
                        return Err(LlmError::Retryable {
                            source: anyhow::anyhow!("No data received for {chunk_timeout_secs}s"),
                            retry_after: None,
                            reason: FailoverReason::Timeout,
                        });
                    }
                }
            }
        };

        buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));

        if buffer.len() > max_buffer {
            return Err(LlmError::Retryable {
                source: anyhow::anyhow!("SSE buffer exceeded {max_buffer} bytes"),
                retry_after: None,
                reason: FailoverReason::Overloaded,
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
                            tracing::debug!("SSE: stream receiver closed");
                            return Ok(());
                        }
                    }
                }
                SseAction::Done(events) => {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            tracing::debug!("SSE: stream receiver closed");
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
    async fn cancellation_stops_stream() {
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();

        // Create a stream that yields one chunk then "blocks" (empty)
        let stream = make_stream(vec![]);
        cancel.cancel();

        let result = process_sse_stream(stream, &tx, &cancel, 0, 1024 * 1024, |_| {
            SseAction::Continue
        })
        .await;

        assert!(result.is_ok());
        let e = rx.recv().await.unwrap();
        assert!(matches!(e, StreamEvent::Done));
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
            })
        })
        .await;

        assert!(matches!(result, Err(LlmError::Retryable { .. })));
    }
}
