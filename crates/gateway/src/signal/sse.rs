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

use cortex::config::Config;
use cortex::constants;

const MIN_BACKOFF: Duration = constants::SIGNAL_SSE_MIN_BACKOFF;
const MAX_BACKOFF: Duration = constants::SIGNAL_SSE_MAX_BACKOFF;
const BACKOFF_FACTOR: f64 = constants::SIGNAL_SSE_BACKOFF_FACTOR;
const JITTER_FRACTION: f64 = constants::SIGNAL_SSE_JITTER_FRACTION;
const STALL_TIMEOUT: Duration = constants::SIGNAL_SSE_STALL_TIMEOUT;

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

        // Apply backoff on errors
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

        // Connect to SSE stream using the client's configured HTTP client
        let response = tokio::select! {
            _ = shutdown.cancelled() => break,
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

        // Process the byte stream as SSE frames
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
            if buffer.len() > MAX_BUFFER_SIZE {
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
                                enrich_attachments(&mut inbound, &raw_attachments, &client, 10_000)
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
                                        if let Ok((transcript, _)) = transcriber
                                            .transcribe(&bytes, &att.mime_type, &fname, None)
                                            .await
                                        {
                                            transcripts.push(transcript);
                                        }
                                    }
                                    if !transcripts.is_empty() {
                                        let joined = transcripts.join(" ");
                                        inbound.text = if inbound.text.trim().is_empty() {
                                            format!("[Voice transcript]: {joined}")
                                        } else {
                                            format!(
                                                "{}\n[Voice transcript]: {joined}",
                                                inbound.text
                                            )
                                        };
                                    }
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
}
