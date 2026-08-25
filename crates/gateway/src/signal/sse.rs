use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::api::SignalClient;
use super::dedup::MessageDeduplicator;
use super::parse::{enrich_attachments, parse_envelope};
use super::types::{Attachment, SignalEnvelope};
use crate::handler::InboundMessage;
use crate::message_processing::voice;

use cortex::config::Config;
use cortex::constants;

const MIN_BACKOFF: Duration = constants::SIGNAL_SSE_MIN_BACKOFF;
const MAX_BACKOFF: Duration = constants::SIGNAL_SSE_MAX_BACKOFF;
const BACKOFF_FACTOR: f64 = constants::SIGNAL_SSE_BACKOFF_FACTOR;
const JITTER_FRACTION: f64 = constants::SIGNAL_SSE_JITTER_FRACTION;
const STALL_TIMEOUT: Duration = constants::SIGNAL_SSE_STALL_TIMEOUT;
const MAX_ATTACHMENT_BYTES: u64 = constants::SIGNAL_MAX_ATTACHMENT_BYTES;

/// Maximum SSE buffer size before we drop and reconnect (1 MB).
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

/// Callback invoked for each parsed inbound message from the SSE stream.
/// Arguments: (inbound message, recipient string for replies, optional group_id).
pub type SseCallback = Arc<
    dyn Fn(InboundMessage, String, Option<String>) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Run a persistent SSE connection to the signal-cli daemon's event stream.
///
/// Automatically handles:
/// - SSE frame parsing (data: lines)
/// - Exponential backoff on connection errors with jitter
/// - Own-message filtering
/// - Stall watchdog: reconnects if no data within STALL_TIMEOUT
/// - Buffer size limit to prevent memory exhaustion
pub async fn run_sse_loop(
    client: Arc<SignalClient>,
    callback: SseCallback,
    shutdown: CancellationToken,
    config: Config,
) {
    let mut consecutive_errors: u32 = 0;
    let mut dedup = MessageDeduplicator::new();

    loop {
        if shutdown.is_cancelled() {
            info!("Signal SSE loop shutting down");
            break;
        }

        // Backoff happens at the top of the loop, not at the error site, so
        // every `continue` path above is guaranteed to be throttled.
        if consecutive_errors > 0 {
            let backoff = calculate_backoff(consecutive_errors);
            info!("Signal SSE reconnecting in {backoff:?} (attempt {consecutive_errors})");
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
        }

        // URL-encode the account (phone numbers contain '+' which is space in query strings)
        let encoded_account = url_encode(client.account());
        let url = format!(
            "{}/api/v1/events?account={}",
            client.base_url(),
            encoded_account
        );

        let response = tokio::select! {
            _ = shutdown.cancelled() => break,
            // Bounds connect through response headers only — the stream body
            // is long-lived and is policed by the STALL_TIMEOUT watchdog below.
            result = tokio::time::timeout(
                Duration::from_secs(30),
                client.get_sse(&url)
            ) => {
                match result {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(e)) => {
                        consecutive_errors += 1;
                        warn!("Signal SSE connection error (attempt {consecutive_errors}): {e}");
                        continue;
                    }
                    Err(_) => {
                        consecutive_errors += 1;
                        warn!("Signal SSE connection timed out (attempt {consecutive_errors})");
                        continue;
                    }
                }
            }
        };

        info!("Signal SSE stream connected");
        consecutive_errors = 0;

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut data_lines: Vec<String> = Vec::new();

        loop {
            let chunk = tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("Signal SSE loop shutting down (stream active)");
                    return;
                }
                result = tokio::time::timeout(STALL_TIMEOUT, stream.next()) => {
                    match result {
                        Ok(Some(Ok(bytes))) => bytes,
                        Ok(Some(Err(e))) => {
                            warn!("Signal SSE stream error: {e}");
                            consecutive_errors += 1;
                            break;
                        }
                        Ok(None) => {
                            info!("Signal SSE stream ended");
                            consecutive_errors += 1;
                            break;
                        }
                        Err(_) => {
                            warn!("Signal SSE stalled (no data in {STALL_TIMEOUT:?}), reconnecting");
                            consecutive_errors += 1;
                            break;
                        }
                    }
                }
            };

            // SSE frame parsing
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Guard against unbounded buffer growth
            if buffer_over_cap(buffer.len()) {
                warn!(
                    "Signal SSE buffer exceeded {} bytes, dropping and reconnecting",
                    MAX_BUFFER_SIZE
                );
                consecutive_errors += 1;
                break;
            }

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim_end_matches('\r').to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() {
                    // Empty line = dispatch event
                    if !data_lines.is_empty() {
                        let event_data = data_lines.join("\n");
                        data_lines.clear();

                        if let Some((mut inbound, recipient, group_id, raw_attachments)) =
                            parse_sse_event(&event_data, client.account())
                        {
                            // Dedup: skip if we've seen this (sender, timestamp) before
                            let ts = inbound
                                .message_id
                                .as_deref()
                                .and_then(|id| id.parse::<i64>().ok())
                                .unwrap_or(0);
                            if dedup.seen(&inbound.sender_id, ts) {
                                continue;
                            }
                            // Download attachments via signal-cli; if any are
                            // audio AND STT is configured, transcribe and
                            // prepend `[Voice transcript]: …` to the message
                            // text — parity with Telegram inbound voice.
                            if !raw_attachments.is_empty() {
                                enrich_attachments(
                                    &mut inbound,
                                    &raw_attachments,
                                    &client,
                                    MAX_ATTACHMENT_BYTES,
                                )
                                .await;
                                if let Some(transcriber) =
                                    cortex::media::understanding::AudioTranscriber::from_config(
                                        &config,
                                    )
                                {
                                    let mut transcripts: Vec<String> = Vec::new();
                                    for att in &inbound.attachments {
                                        if !att.mime_type.starts_with("audio/") {
                                            continue;
                                        }
                                        use base64::Engine;
                                        let Ok(bytes) = base64::engine::general_purpose::STANDARD
                                            .decode(&att.data)
                                        else {
                                            continue;
                                        };
                                        let fname = att
                                            .filename
                                            .clone()
                                            .unwrap_or_else(|| "signal-voice.audio".to_string());
                                        transcripts.extend(
                                            voice::transcribe_or_warn(
                                                &transcriber,
                                                &bytes,
                                                &att.mime_type,
                                                &fname,
                                                "signal",
                                            )
                                            .await,
                                        );
                                    }
                                    voice::append_voice_transcripts(
                                        &mut inbound.text,
                                        &transcripts,
                                    );
                                }
                            }
                            callback(inbound, recipient, group_id).await;
                        }
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim_start().to_string());
                }
                // Ignore other SSE fields (event:, id:, retry:, comments)
            }
        }
    }
}

/// Parse an SSE event data payload into an InboundMessage + reply target +
/// the raw attachment refs from the envelope's `data_message`. Callers run
/// `enrich_attachments` against those refs to materialize the bytes (and
/// optionally feed audio through STT) before dispatching to the handler.
fn parse_sse_event(
    data: &str,
    own_account: &str,
) -> Option<(InboundMessage, String, Option<String>, Vec<Attachment>)> {
    let envelope: SignalEnvelope = serde_json::from_str(data).ok()?;
    let inbound = parse_envelope(&envelope, own_account)?;
    let raw_attachments = envelope
        .envelope
        .data_message
        .as_ref()
        .and_then(|dm| dm.attachments.clone())
        .unwrap_or_default();

    let group_id = inbound.channel_id.clone();
    let recipient = inbound.sender_id.clone();

    Some((inbound, recipient, group_id, raw_attachments))
}

/// True when the accumulated SSE buffer must be dropped and the stream
/// reconnected. No legitimate event payload approaches [`MAX_BUFFER_SIZE`],
/// so an over-cap buffer means a runaway or malicious stream; anything at
/// or under the cap keeps parsing.
fn buffer_over_cap(buffer_len: usize) -> bool {
    buffer_len > MAX_BUFFER_SIZE
}

/// Minimal percent-encoding for query string values.
/// Encodes characters that are not unreserved per RFC 3986.
fn url_encode(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    encoded
}

fn calculate_backoff(consecutive_errors: u32) -> Duration {
    crate::resilience::backoff::calculate_backoff(
        consecutive_errors,
        MIN_BACKOFF,
        MAX_BACKOFF,
        BACKOFF_FACTOR,
        JITTER_FRACTION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// signal/sse.rs is a thin adapter over `resilience::backoff` —
    /// the only signal-specific invariants are (a) the first attempt
    /// floors at `MIN_BACKOFF` and (b) any number of attempts caps at
    /// `MAX_BACKOFF + jitter`. Exhaustive backoff curve testing lives in
    /// `resilience::backoff`.
    #[test]
    fn backoff_floors_at_min_and_caps_at_max_plus_jitter() {
        let max_with_jitter = MAX_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION);

        let first = calculate_backoff(1);
        assert!(
            first.as_secs_f64() >= MIN_BACKOFF.as_secs_f64(),
            "first attempt must not be below MIN_BACKOFF, got {first:?}"
        );
        assert!(
            first.as_secs_f64() <= MIN_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION) + 0.01,
            "first attempt must not exceed MIN_BACKOFF + jitter, got {first:?}"
        );

        let saturated = calculate_backoff(100);
        assert!(
            saturated.as_secs_f64() <= max_with_jitter,
            "any attempt count must cap at MAX_BACKOFF + jitter, got {saturated:?}"
        );
    }

    /// `url_encode` builds the SSE subscription URL — `+` MUST become
    /// `%2B` (Signal phone numbers contain `+`, and `+` is a space in
    /// query strings) and reserved chars MUST percent-encode. Pure
    /// passthrough chars (alnum) stay unchanged.
    #[test]
    fn url_encode_percent_encodes_reserved_chars_and_preserves_alnum() {
        let cases = [
            ("+15559876543", "%2B15559876543"),
            ("hello", "hello"),
            ("a b+c&d=e", "a%20b%2Bc%26d%3De"),
        ];
        for (input, expected) in cases {
            assert_eq!(url_encode(input), expected, "url_encode({input:?})");
        }
    }

    #[test]
    fn parse_sse_dm_event() {
        let data = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "message": "Hello"
                }
            },
            "account": "+15559876543"
        }"#;
        let (msg, recipient, group_id, _atts) = parse_sse_event(data, "+15559876543").unwrap();
        assert_eq!(msg.sender_id, "+15551234567");
        assert_eq!(msg.text, "Hello");
        assert_eq!(recipient, "+15551234567");
        assert!(group_id.is_none());
    }

    #[test]
    fn parse_sse_group_event() {
        let data = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "message": "Group msg",
                    "groupInfo": { "groupId": "grp-abc" }
                }
            }
        }"#;
        let (msg, recipient, group_id, _atts) = parse_sse_event(data, "+15559876543").unwrap();
        assert_eq!(msg.text, "Group msg");
        assert_eq!(msg.channel_id.as_deref(), Some("grp-abc"));
        assert_eq!(recipient, "+15551234567");
        assert_eq!(group_id.as_deref(), Some("grp-abc"));
    }

    #[test]
    fn parse_sse_own_message_filtered() {
        let data = r#"{
            "envelope": {
                "source": "+15559876543",
                "timestamp": 1700000000000,
                "dataMessage": {
                    "timestamp": 1700000000000,
                    "message": "My own"
                }
            }
        }"#;
        assert!(parse_sse_event(data, "+15559876543").is_none());
    }

    #[test]
    fn parse_sse_invalid_json() {
        assert!(parse_sse_event("not json", "+15559876543").is_none());
    }

    #[test]
    fn parse_sse_receipt_only() {
        let data = r#"{
            "envelope": {
                "source": "+15551234567",
                "timestamp": 1700000000000,
                "receiptMessage": { "when": 1700000000000 }
            }
        }"#;
        assert!(parse_sse_event(data, "+15559876543").is_none());
    }

    /// The 1 MB cap exists to bound memory on a runaway stream while
    /// leaving generous room for legitimate envelopes: a buffer in the
    /// tens-of-KB range (large group metadata, many attachment refs) must
    /// keep parsing, and the cap is inclusive — dropping the stream at or
    /// under it would lose a valid in-flight event.
    #[test]
    fn buffer_cap_allows_large_events_up_to_and_including_the_cap() {
        assert!(
            !buffer_over_cap(64 * 1024),
            "a 64 KB event buffer is legitimate and must not force a reconnect"
        );
        assert!(!buffer_over_cap(MAX_BUFFER_SIZE), "the cap is inclusive");
        assert!(buffer_over_cap(MAX_BUFFER_SIZE + 1));
    }

    /// Loop-level reconnect discipline, exercised against a raw loopback
    /// TCP stub:
    /// - the FIRST connection must open immediately (no backoff before
    ///   any error has happened);
    /// - after a buffer-overflow drop, the reconnect must be throttled by
    ///   at least the minimum backoff (a hot reconnect loop would hammer
    ///   the daemon), but must actually happen.
    #[tokio::test]
    async fn sse_overflow_reconnects_after_backoff_never_immediately() {
        use std::time::Instant;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel::<Instant>();

        tokio::spawn(async move {
            // Connection 1: headers, then an over-cap body with no newline
            // so nothing can be drained — the loop must drop the stream.
            let (mut sock1, _) = listener.accept().await.unwrap();
            let _ = evt_tx.send(Instant::now());
            let _ = sock1
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .await;
            let junk = vec![b'x'; MAX_BUFFER_SIZE + 64 * 1024];
            let _ = sock1.write_all(&junk).await;
            let _ = sock1.flush().await;
            let _ = evt_tx.send(Instant::now());

            // Connection 2: the reconnect after backoff.
            let (mut sock2, _) = listener.accept().await.unwrap();
            let _ = evt_tx.send(Instant::now());
            let _ = sock2
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .await;
            // Hold both sockets open until the test tears down.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(sock1);
        });

        let client =
            Arc::new(SignalClient::new(&format!("http://{addr}"), "+15550001111").unwrap());
        let callback: SseCallback = Arc::new(|_msg, _recipient, _group| Box::pin(async {}));
        let shutdown = CancellationToken::new();
        let spawned_at = Instant::now();
        let loop_task = tokio::spawn(run_sse_loop(
            client,
            callback,
            shutdown.clone(),
            Config::default(),
        ));

        async fn recv(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Instant>) -> Option<Instant> {
            tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .ok()
                .flatten()
        }
        let conn1_at = recv(&mut evt_rx).await.unwrap();
        assert!(
            conn1_at.duration_since(spawned_at) < Duration::from_millis(700),
            "the first connection must not wait for a backoff, took {:?}",
            conn1_at.duration_since(spawned_at)
        );

        let junk_written_at = recv(&mut evt_rx).await.unwrap();
        let conn2_at = recv(&mut evt_rx)
            .await
            .expect("the loop must reconnect after a buffer overflow");
        let gap = conn2_at.duration_since(junk_written_at);
        assert!(
            gap >= Duration::from_millis(700),
            "reconnect after an error must be throttled by backoff, got {gap:?}"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), loop_task).await;
    }

    /// Full inbound pipeline against a loopback signal-cli stub: an event
    /// carrying an audio attachment must have its bytes downloaded into
    /// `inbound.attachments`, and — with STT configured — the audio must
    /// be transcribed and appended to the message text. Uses the hermetic
    /// command-template STT provider (no network, no keys).
    #[tokio::test]
    async fn sse_audio_attachment_is_downloaded_and_transcribed() {
        use axum::routing::get;
        use base64::Engine as _;

        let audio_bytes = vec![7u8; 2048];

        let events = || async {
            let envelope = serde_json::json!({
                "envelope": {
                    "source": "+15557778888",
                    "timestamp": 1_723_000_000_000_i64,
                    "dataMessage": {
                        "timestamp": 1_723_000_000_000_i64,
                        "message": "voice note",
                        "attachments": [{
                            "id": "att-1",
                            "contentType": "audio/ogg",
                            "filename": "note.ogg"
                        }]
                    }
                }
            });
            let frame = format!("data: {envelope}\n\n");
            let stream = futures::stream::once(async move {
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(frame))
            })
            .chain(futures::stream::pending());
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
        };
        let served_audio = audio_bytes.clone();
        let app = axum::Router::new()
            .route("/api/v1/events", get(events))
            .route(
                "/api/v1/attachments/att-1",
                get(move || {
                    let bytes = served_audio.clone();
                    async move { bytes }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut config = Config::default();
        config.audio.enabled = true;
        config.audio.min_file_size = 1;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "custom".into(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: Some(10_000),
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some(": {audio_file} && printf 'stub transcript'".into()),
        }];

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();
        let callback: SseCallback = Arc::new(move |msg, _recipient, _group| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(msg);
            })
        });

        let client =
            Arc::new(SignalClient::new(&format!("http://{addr}"), "+15550001111").unwrap());
        let shutdown = CancellationToken::new();
        let loop_task = tokio::spawn(run_sse_loop(
            client,
            callback,
            shutdown.clone(),
            config,
        ));

        let inbound = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("the SSE event must reach the callback")
            .unwrap();
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), loop_task).await;

        assert_eq!(
            inbound.attachments.len(),
            1,
            "the attachment ref must be downloaded into the inbound message"
        );
        assert_eq!(inbound.attachments[0].mime_type, "audio/ogg");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&inbound.attachments[0].data)
                .unwrap(),
            audio_bytes,
            "attachment data must be the daemon's bytes, base64-encoded"
        );
        assert!(
            inbound.text.contains("voice note"),
            "original text must survive: {}",
            inbound.text
        );
        assert!(
            inbound.text.contains("stub transcript"),
            "the audio attachment must be transcribed into the text: {}",
            inbound.text
        );
    }
}
