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
use crate::session_queue::SessionQueue;

use cortex::config::Config;
use cortex::constants;

const MIN_BACKOFF: Duration = constants::SIGNAL_SSE_MIN_BACKOFF;
const MAX_BACKOFF: Duration = constants::SIGNAL_SSE_MAX_BACKOFF;
const BACKOFF_FACTOR: f64 = constants::SIGNAL_SSE_BACKOFF_FACTOR;
const JITTER_FRACTION: f64 = constants::SIGNAL_SSE_JITTER_FRACTION;
const STALL_TIMEOUT: Duration = constants::SIGNAL_SSE_STALL_TIMEOUT;
const MAX_ATTACHMENT_BYTES: u64 = constants::SIGNAL_MAX_ATTACHMENT_BYTES;

/// How long a stream must stay up before its drop counts as a fresh failure.
///
/// A server that accepts the connection and then drops it at once is failing
/// just as much as one that refuses it. Without this bar, every accept reset
/// the backoff and such a server was retried at the minimum delay forever.
///
/// A server that accepts and then sends nothing is cut off by the longer
/// stall timeout and so counts as stable; each such cycle still takes the
/// full stall window, which bounds the reconnect rate.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// Maximum SSE buffer size before we drop and reconnect (1 MB).
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

/// Live conversation consumers one Signal sender may hold. The queue key is
/// per sender *per group*, so this must cover every group a member posts in
/// within the idle window — the webhook default (a few threads per sender)
/// would silently drop a chatty member of many groups. Still bounded so a
/// hostile sender cannot grow the map without limit.
const MAX_CONVERSATIONS_PER_SENDER: usize = 256;

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
    // Attachment download, STT and the agent turn run per conversation, in
    // order, off this loop: a long turn in one chat must not hold the
    // stream (and every other chat) until it ends.
    let queue = Arc::new(SessionQueue::with_sender_limit(
        config.gateway.max_concurrent.max(1),
        MAX_CONVERSATIONS_PER_SENDER,
    ));
    let config = Arc::new(config);

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
        let connected_at = std::time::Instant::now();

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
                            break;
                        }
                        Ok(None) => {
                            info!("Signal SSE stream ended");
                            break;
                        }
                        Err(_) => {
                            warn!("Signal SSE stalled (no data in {STALL_TIMEOUT:?}), reconnecting");
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
                            // `/stop` must not wait behind the turn it is
                            // meant to stop: it runs beside the conversation's
                            // queue, where the handler's cancel path reaches
                            // the running turn at once.
                            if crate::commands::is_cancel_command(&inbound.text) {
                                tokio::spawn(callback(inbound, recipient, group_id));
                                continue;
                            }
                            let key = conversation_key(&recipient, group_id.as_deref());
                            let client = client.clone();
                            let config = config.clone();
                            let callback = callback.clone();
                            let enqueued = queue
                                .enqueue(
                                    key.clone(),
                                    Box::pin(async move {
                                        if !raw_attachments.is_empty() {
                                            enrich_inbound(
                                                &mut inbound,
                                                &raw_attachments,
                                                &client,
                                                &config,
                                            )
                                            .await;
                                        }
                                        callback(inbound, recipient, group_id).await;
                                    }),
                                )
                                .await;
                            if !enqueued {
                                warn!(
                                    conversation = %key,
                                    "Signal inbound dropped: conversation queue at capacity"
                                );
                            }
                        }
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim_start().to_string());
                }
                // Ignore other SSE fields (event:, id:, retry:, comments)
            }
        }

        consecutive_errors = errors_after_disconnect(consecutive_errors, connected_at.elapsed());
    }
}

/// Per-conversation queue key: one consumer per sender per group (`recipient`
/// is the sender id, so two members of the same group do not share a
/// consumer), and one per sender for direct chats. Shaped
/// `channel:sender:sub` so the queue's per-sender cap can find the sender
/// segment.
fn conversation_key(recipient: &str, group_id: Option<&str>) -> String {
    format!("signal:{recipient}:{}", group_id.unwrap_or(""))
}

/// Download attachments via signal-cli; if any are audio AND STT is
/// configured, transcribe and append `[Voice transcript]: …` to the message
/// text — parity with Telegram inbound voice.
///
/// Runs nothing for a sender the queued turn will challenge or deny: their
/// audio never reaches a third party and no paid transcript is bought
/// before the pairing challenge. The plain text still goes through.
async fn enrich_inbound(
    inbound: &mut InboundMessage,
    raw_attachments: &[Attachment],
    client: &SignalClient,
    config: &Config,
) {
    if !crate::handler::sender_may_enrich(config, "signal", inbound).await {
        info!(
            sender = %inbound.sender_id,
            "signal: sender not approved; skipping attachment download and transcription"
        );
        return;
    }
    enrich_attachments(inbound, raw_attachments, client, MAX_ATTACHMENT_BYTES).await;
    let Some(transcriber) = cortex::media::understanding::AudioTranscriber::from_config(config)
    else {
        return;
    };
    let mut transcripts: Vec<String> = Vec::new();
    for att in &inbound.attachments {
        if !att.mime_type.starts_with("audio/") {
            continue;
        }
        use base64::Engine;
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&att.data) else {
            continue;
        };
        let fname = att
            .filename
            .clone()
            .unwrap_or_else(|| "signal-voice.audio".to_string());
        transcripts.extend(
            voice::transcribe_or_warn(&transcriber, &bytes, &att.mime_type, &fname, "signal").await,
        );
    }
    voice::append_voice_transcripts(&mut inbound.text, &transcripts);
}

/// Error count to carry into the next reconnect after a stream ends.
///
/// A stream that stayed up for [`STABLE_CONNECTION`] proved the server
/// healthy, so the count restarts at one (the minimum backoff). A shorter
/// stream is one more failure on top of the ones before it, so the backoff
/// keeps growing for a server that accepts and then drops at once.
fn errors_after_disconnect(previous: u32, uptime: Duration) -> u32 {
    if uptime >= STABLE_CONNECTION {
        1
    } else {
        previous.saturating_add(1)
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

    /// Spec: a server that accepts and then drops at once must back off like
    /// one that refuses outright, while a drop after a long healthy stream
    /// restarts at the minimum delay.
    #[test]
    fn short_lived_streams_escalate_backoff_and_stable_streams_reset_it() {
        let short = Duration::from_millis(50);
        let mut errors = 0;
        for expected in 1..=4 {
            errors = errors_after_disconnect(errors, short);
            assert_eq!(errors, expected, "short-lived stream must escalate");
        }
        assert!(
            calculate_backoff(errors) > calculate_backoff(1),
            "escalated count must give a longer delay"
        );

        let just_under = STABLE_CONNECTION - Duration::from_millis(1);
        assert_eq!(errors_after_disconnect(7, just_under), 8);
        assert_eq!(errors_after_disconnect(7, STABLE_CONNECTION), 1);
        assert_eq!(
            errors_after_disconnect(7, STABLE_CONNECTION * 10),
            1,
            "a long-lived stream resets the count"
        );
        assert_eq!(errors_after_disconnect(u32::MAX, short), u32::MAX);
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
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;
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
        let loop_task = tokio::spawn(run_sse_loop(client, callback, shutdown.clone(), config));

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

    /// Serve one SSE stream that emits every envelope in `frames` at once and
    /// then stays open, plus a fixed attachment body for any attachment id.
    /// Returns the daemon's base address and a counter of attachment fetches.
    async fn serve_sse(
        frames: Vec<serde_json::Value>,
    ) -> (String, Arc<std::sync::atomic::AtomicU32>) {
        use axum::routing::get;
        let fetches = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        let events = move || {
            let body = body.clone();
            async move {
                let stream = futures::stream::once(async move {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(body))
                })
                .chain(futures::stream::pending());
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
            }
        };
        let fetch_counter = fetches.clone();
        let app = axum::Router::new()
            .route("/api/v1/events", get(events))
            .route(
                "/api/v1/attachments/{id}",
                get(move || {
                    let fetch_counter = fetch_counter.clone();
                    async move {
                        fetch_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        vec![7u8; 2048]
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), fetches)
    }

    fn envelope(source: &str, ts: i64, message: &str, audio: bool) -> serde_json::Value {
        let mut data_message = serde_json::json!({ "timestamp": ts, "message": message });
        if audio {
            data_message["attachments"] = serde_json::json!([{
                "id": format!("att-{ts}"),
                "contentType": "audio/ogg",
                "filename": "note.ogg"
            }]);
        }
        serde_json::json!({
            "envelope": {
                "source": source,
                "timestamp": ts,
                "dataMessage": data_message,
            }
        })
    }

    /// Two senders on one stream: a slow turn for the first must not delay
    /// the second's message, and the stream itself must keep being read.
    #[tokio::test]
    async fn sse_conversations_run_independently_off_the_stream_loop() {
        let (base, _fetches) = serve_sse(vec![
            envelope("+15550000001", 1_723_000_000_001, "slow one", false),
            envelope("+15550000002", 1_723_000_000_002, "fast one", false),
        ])
        .await;

        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;

        let done: Arc<tokio::sync::Mutex<Vec<String>>> = Arc::new(tokio::sync::Mutex::new(vec![]));
        let done_c = done.clone();
        let callback: SseCallback = Arc::new(move |msg, _recipient, _group| {
            let done = done_c.clone();
            Box::pin(async move {
                if msg.text == "slow one" {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                done.lock().await.push(msg.text);
            })
        });

        let client = Arc::new(SignalClient::new(&base, "+15550001111").unwrap());
        let shutdown = CancellationToken::new();
        let loop_task = tokio::spawn(run_sse_loop(client, callback, shutdown.clone(), config));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while done.lock().await.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let first = done.lock().await.clone();
        assert_eq!(
            first,
            vec!["fast one".to_string()],
            "the second sender must be served while the first turn is still running"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while done.lock().await.len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            done.lock().await.len(),
            2,
            "the slow turn must still complete"
        );
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), loop_task).await;
    }

    /// `/stop` runs beside the conversation queue, not behind it: with a
    /// slow turn holding the sender's conversation, the cancel reaches the
    /// callback while that turn still runs.
    #[tokio::test]
    async fn sse_cancel_command_bypasses_the_conversation_queue() {
        let (base, _fetches) = serve_sse(vec![
            envelope("+15550000001", 1_723_000_000_001, "slow one", false),
            envelope("+15550000001", 1_723_000_000_002, "/stop", false),
        ])
        .await;

        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Open;

        let done: Arc<tokio::sync::Mutex<Vec<String>>> = Arc::new(tokio::sync::Mutex::new(vec![]));
        let done_c = done.clone();
        let callback: SseCallback = Arc::new(move |msg, _recipient, _group| {
            let done = done_c.clone();
            Box::pin(async move {
                if msg.text == "slow one" {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                done.lock().await.push(msg.text);
            })
        });

        let client = Arc::new(SignalClient::new(&base, "+15550001111").unwrap());
        let shutdown = CancellationToken::new();
        let loop_task = tokio::spawn(run_sse_loop(client, callback, shutdown.clone(), config));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while done.lock().await.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let first = done.lock().await.clone();
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), loop_task).await;
        assert_eq!(
            first,
            vec!["/stop".to_string()],
            "the cancel must be handled while the sender's turn is still running"
        );
    }

    /// A sender the turn will deny gets no attachment download and no STT:
    /// the daemon sees no attachment fetch, the transcriber never runs, and
    /// the plain text still reaches the callback so the denial is delivered.
    #[tokio::test]
    async fn sse_unapproved_sender_gets_no_download_or_transcription() {
        let (base, fetches) = serve_sse(vec![envelope(
            "+15557778888",
            1_723_000_000_000,
            "voice note",
            true,
        )])
        .await;

        let marker = tempfile::tempdir().unwrap();
        let marker_file = marker.path().join("stt-ran");
        let mut config = Config::default();
        config.gateway.dm_policy = cortex::pairing::DmPolicy::Disabled;
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
            command: Some(format!(
                ": {{audio_file}} && touch '{}' && printf 'stub transcript'",
                marker_file.display()
            )),
        }];

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();
        let callback: SseCallback = Arc::new(move |msg, _recipient, _group| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(msg);
            })
        });

        let client = Arc::new(SignalClient::new(&base, "+15550001111").unwrap());
        let shutdown = CancellationToken::new();
        let loop_task = tokio::spawn(run_sse_loop(client, callback, shutdown.clone(), config));

        let inbound = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("the plain message must still reach the callback")
            .unwrap();
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), loop_task).await;

        assert!(
            inbound.text.starts_with("voice note") && !inbound.text.contains("stub transcript"),
            "the text passes through without a transcript: {}",
            inbound.text
        );
        assert!(
            inbound.attachments.is_empty(),
            "no attachment may be downloaded for an unapproved sender"
        );
        assert_eq!(
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the daemon must see no attachment fetch"
        );
        assert!(!marker_file.exists(), "the STT command must not run");
    }
}
