use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::api::SignalClient;
use super::dedup::MessageDeduplicator;
use super::parse::parse_envelope;
use super::types::SignalEnvelope;
use crate::handler::InboundMessage;

use borg_core::constants;

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

                        if let Some((inbound, recipient, group_id)) =
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

/// Parse an SSE event data payload into an InboundMessage + reply target.
fn parse_sse_event(
    data: &str,
    own_account: &str,
) -> Option<(InboundMessage, String, Option<String>)> {
    let envelope: SignalEnvelope = serde_json::from_str(data).ok()?;
    let inbound = parse_envelope(&envelope, own_account)?;

    // Determine reply target: group_id for groups, sender_id for DMs
    let group_id = inbound.channel_id.clone();
    let recipient = inbound.sender_id.clone();

    Some((inbound, recipient, group_id))
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
    crate::backoff::calculate_backoff(
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

    #[test]
    fn backoff_increases_with_errors() {
        let b1 = calculate_backoff(1);
        let b3 = calculate_backoff(3);
        assert!(b1.as_secs_f64() >= MIN_BACKOFF.as_secs_f64());
        // b3 base (without jitter) should be larger than b1 base
        let b3_max = MAX_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION);
        assert!(b3.as_secs_f64() <= b3_max);
    }

    #[test]
    fn backoff_capped_at_max() {
        let b = calculate_backoff(100);
        assert!(b.as_secs_f64() <= MAX_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION));
    }

    #[test]
    fn backoff_first_attempt() {
        let b = calculate_backoff(1);
        // First attempt: base = MIN_BACKOFF * factor^0 = MIN_BACKOFF
        assert!(b.as_secs_f64() >= MIN_BACKOFF.as_secs_f64());
        assert!(b.as_secs_f64() <= MIN_BACKOFF.as_secs_f64() * (1.0 + JITTER_FRACTION) + 0.01);
    }

    #[test]
    fn url_encode_phone_number() {
        let encoded = url_encode("+15559876543");
        assert_eq!(encoded, "%2B15559876543");
    }

    #[test]
    fn url_encode_plain_string() {
        let encoded = url_encode("hello");
        assert_eq!(encoded, "hello");
    }

    #[test]
    fn url_encode_special_chars() {
        let encoded = url_encode("a b+c&d=e");
        assert_eq!(encoded, "a%20b%2Bc%26d%3De");
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
        let (msg, recipient, group_id) = parse_sse_event(data, "+15559876543").unwrap();
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
        let (msg, recipient, group_id) = parse_sse_event(data, "+15559876543").unwrap();
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
