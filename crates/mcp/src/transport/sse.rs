//! SSE flavour of MCP HTTP transport.
//!
//! The server holds open a long-lived SSE stream on which it pushes
//! responses and notifications; the client POSTs requests to a sibling
//! endpoint. The SSE `endpoint` event carries the POST URL (per MCP spec).
//!
//! Same URL policy as [`super::http`] applies.

use super::Transport;
use crate::errors::{McpError, Result};
use crate::protocol::Message;
use crate::transport::http::{
    build_client, resolve_endpoint, HttpPolicy, DEFAULT_MAX_RESPONSE_BYTES,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;

/// Config for an SSE MCP endpoint.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// Server name (for diagnostics).
    pub server_name: String,
    /// SSE GET URL.
    pub url: String,
    /// Headers to send on both GET (SSE) and POST (requests).
    pub headers: HashMap<String, String>,
    /// Per-call timeout for the POST side. The SSE side is long-lived.
    pub timeout: Duration,
    /// Policy gates.
    pub policy: HttpPolicy,
    /// Optional client certificate for mutual TLS. Loaded at connect time.
    pub client_identity: Option<crate::manifest::ClientIdentity>,
    /// Optional PEM bundle of extra certificate authorities to trust, added to
    /// the built-in roots. Loaded at connect time.
    pub ca_cert_path: Option<String>,
    /// Hard cap on the bytes of a single SSE event (one blank-line-delimited
    /// frame). The stream is long-lived, so the whole body cannot be capped;
    /// the per-frame bound is what stops a server from growing one event
    /// without end. 0 means "unset" and falls back to
    /// [`DEFAULT_MAX_RESPONSE_BYTES`].
    pub max_response_bytes: usize,
}

/// Connected SSE transport.
///
/// One background task pumps inbound events into a channel; `recv` drains
/// that channel. `send` POSTs to the per-session endpoint advertised by the
/// server's first `endpoint` event.
pub struct SseTransport {
    server_name: String,
    client: Client,
    post_url: Url,
    headers: HashMap<String, String>,
    inbox: mpsc::Receiver<Message>,
    /// Set by the pump when it exits due to an error. `recv` checks this so
    /// pump errors surface as `TransportClosed` rather than the silent
    /// `Ok(None)` you'd get from a clean `mpsc::Receiver::recv` after the
    /// sender drops.
    pump_error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    pump: Option<JoinHandle<()>>,
}

impl SseTransport {
    /// Server name, for diagnostics.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// Open an SSE transport. Waits up to `connect_timeout` for the server's
/// `endpoint` event (which carries the POST URL).
///
/// DNS is resolved once for the SSE URL up-front and pinned into the GET
/// client; the POST URL advertised by the server is resolved separately
/// (full async policy re-check) and pinned into the POST client so
/// rebinding is closed on both sides. The advertised POST URL must share the
/// SSE URL's origin — see the check in the body for why.
pub async fn open(config: SseConfig, connect_timeout: Duration) -> Result<SseTransport> {
    let sse_resolved = resolve_endpoint(&config.url, &config.policy).await?;
    let sse_url = sse_resolved.url.clone();
    let client = build_client(
        config.timeout,
        &sse_resolved.pins,
        config.client_identity.as_ref(),
        config.ca_cert_path.as_deref(),
    )?;

    let mut req = client
        .get(sse_url.clone())
        .header(reqwest::header::ACCEPT, "text/event-stream");
    for (k, v) in &config.headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| {
        McpError::Connect(format!("{}: sse connect failed: {e}", config.server_name))
    })?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(McpError::Auth(format!(
            "{}: SSE HTTP {}",
            config.server_name,
            status.as_u16()
        )));
    }
    if !status.is_success() {
        return Err(McpError::Connect(format!(
            "{}: SSE HTTP {}",
            config.server_name,
            status.as_u16()
        )));
    }

    let frame_cap = if config.max_response_bytes == 0 {
        DEFAULT_MAX_RESPONSE_BYTES
    } else {
        config.max_response_bytes
    };
    let mut stream = Box::pin(cap_sse_frames(resp.bytes_stream(), frame_cap).eventsource());
    let (tx, rx) = mpsc::channel::<Message>(64);

    // Wait for the first `endpoint` event so we know where to POST.
    let endpoint_fut = async {
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) if ev.event == "endpoint" => {
                    let s: String = ev.data;
                    return Ok(s);
                }
                Ok(_) => continue,
                Err(e) => {
                    return Err(McpError::Connect(format!("sse stream error: {e}")));
                }
            }
        }
        Err(McpError::Connect(
            "sse closed before endpoint event".to_string(),
        ))
    };
    let endpoint: String = match tokio::time::timeout(connect_timeout, endpoint_fut).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(McpError::Timeout {
                ms: connect_timeout.as_millis() as u64,
            })
        }
    };

    // The endpoint event may be either an absolute URL or a path relative to
    // the SSE URL; resolve relative against the SSE URL.
    let post_url_parsed = sse_url
        .join(&endpoint)
        .map_err(|e| McpError::Connect(format!("invalid endpoint url '{endpoint}': {e}")))?;
    // Pin the POST target to the SSE origin. The `endpoint` event is server
    // controlled and may be an absolute URL, so without this check a hostile or
    // compromised server names any third-party host and every subsequent POST
    // carries `config.headers` — the operator's `Authorization: Bearer …` and
    // any `${SECRET:NAME}` header — straight to it. The policy re-check below
    // only closes SSRF (private and cloud-metadata addresses); a collector on a
    // public host is exactly what it lets through. Comparing the full origin
    // (scheme + host + port) mirrors the LLM side's
    // `cross_host_safe_redirect_policy`, and like it we fail closed rather than
    // strip headers: the credentialed request is simply never sent. A
    // non-special scheme yields an opaque origin that compares unequal to
    // everything, including itself, so it fails closed too.
    if post_url_parsed.origin() != sse_url.origin() {
        return Err(McpError::Connect(format!(
            "{}: the server's sse endpoint event points at {}, but the server \
             itself is {} — borg will not send credentialed requests to a \
             second origin, so the server must advertise its post endpoint on \
             its own (scheme, host and port must all match)",
            config.server_name,
            post_url_parsed.origin().ascii_serialization(),
            sse_url.origin().ascii_serialization(),
        )));
    }
    // Full async re-check against policy — the origin check above fixes the
    // host, but the path is still server-chosen and resolution is recorded so
    // we can pin the POST host into the POST client too.
    let post_resolved = resolve_endpoint(post_url_parsed.as_str(), &config.policy).await?;
    let post_url = post_resolved.url;

    // Build a second client that's pinned to both the SSE host (so any
    // ambient code path that reuses this client for the SSE host still
    // can't be rebound) and the POST host. reqwest's `resolve_to_addrs`
    // is per-domain and accumulates across calls, so combining the two
    // pin lists is safe.
    let mut combined_pins = sse_resolved.pins.clone();
    combined_pins.extend(post_resolved.pins);
    let client = build_client(
        config.timeout,
        &combined_pins,
        config.client_identity.as_ref(),
        config.ca_cert_path.as_deref(),
    )?;

    // Spawn the pump for the remaining lifetime of the stream.
    let server_name_pump = config.server_name.clone();
    let pump_error = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let pump_error_inner = pump_error.clone();
    let pump = tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) if ev.event == "message" || ev.event.is_empty() => {
                    match crate::protocol::decode_line(ev.data.as_bytes()) {
                        Ok(msg) => {
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                server = %server_name_pump,
                                error = %e,
                                "mcp sse decode failed"
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        server = %server_name_pump,
                        error = %e,
                        "mcp sse stream error"
                    );
                    if let Ok(mut slot) = pump_error_inner.lock() {
                        *slot = Some(format!("{e}"));
                    }
                    break;
                }
            }
        }
    });

    Ok(SseTransport {
        server_name: config.server_name,
        client,
        post_url,
        headers: config.headers,
        inbox: rx,
        pump_error,
        pump: Some(pump),
    })
}

/// Error from the capped SSE byte stream.
#[derive(Debug)]
enum SseBodyError<E> {
    /// The underlying body stream failed.
    Transport(E),
    /// One event grew past the cap before its terminating blank line.
    FrameTooLarge { cap: usize },
}

impl<E: std::fmt::Display> std::fmt::Display for SseBodyError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::FrameTooLarge { cap } => {
                write!(f, "sse event exceeds max_response_bytes ({cap} bytes)")
            }
        }
    }
}

/// Byte counter for the SSE event currently in flight.
///
/// An event ends at a blank line. Lines end in LF, CR, or CRLF, and a CRLF can
/// be split across two chunks, so the state carries over between
/// [`FrameLimit::feed`] calls.
struct FrameLimit {
    cap: usize,
    /// Bytes of the current event, line terminators excluded.
    frame_len: usize,
    /// True when no byte has arrived on the current line yet.
    at_line_start: bool,
    /// The previous byte was a CR, so an LF right after it closes the same
    /// line rather than adding a blank one.
    after_cr: bool,
}

impl FrameLimit {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            frame_len: 0,
            at_line_start: true,
            after_cr: false,
        }
    }

    /// Account for `chunk`. Returns `Err(())` once the current event is over
    /// the cap.
    fn feed(&mut self, chunk: &[u8]) -> std::result::Result<(), ()> {
        for &b in chunk {
            let after_cr = std::mem::replace(&mut self.after_cr, false);
            match b {
                b'\n' if after_cr => {}
                b'\r' | b'\n' => {
                    if self.at_line_start {
                        // Blank line: the event is complete.
                        self.frame_len = 0;
                    }
                    self.at_line_start = true;
                    self.after_cr = b == b'\r';
                }
                _ => {
                    self.at_line_start = false;
                    self.frame_len += 1;
                    if self.frame_len > self.cap {
                        return Err(());
                    }
                }
            }
        }
        Ok(())
    }
}

/// Pass `inner` through unchanged while bounding every SSE event to `cap`
/// bytes. When an event goes over, yield one [`SseBodyError::FrameTooLarge`]
/// and end the stream, so the parser downstream never buffers past the cap.
fn cap_sse_frames<S, B, E>(
    inner: S,
    cap: usize,
) -> impl Stream<Item = std::result::Result<B, SseBodyError<E>>>
where
    S: Stream<Item = std::result::Result<B, E>>,
    B: AsRef<[u8]>,
{
    let state = (Box::pin(inner), FrameLimit::new(cap), false);
    futures::stream::unfold(state, move |(mut inner, mut limit, over)| async move {
        if over {
            return None;
        }
        let item = match inner.next().await? {
            Ok(chunk) => match limit.feed(chunk.as_ref()) {
                Ok(()) => Ok(chunk),
                Err(()) => {
                    return Some((
                        Err(SseBodyError::FrameTooLarge { cap }),
                        (inner, limit, true),
                    ))
                }
            },
            Err(e) => Err(SseBodyError::Transport(e)),
        };
        Some((item, (inner, limit, false)))
    })
}

#[async_trait]
impl Transport for SseTransport {
    async fn send(&mut self, msg: Value) -> Result<()> {
        let mut req = self
            .client
            .post(self.post_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.json(&msg).send().await.map_err(|e| {
            McpError::TransportClosed(format!("{}: sse post failed: {e}", self.server_name))
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(McpError::Auth(format!(
                "{}: SSE POST HTTP {}",
                self.server_name,
                status.as_u16()
            )));
        }
        if !status.is_success() && status != reqwest::StatusCode::ACCEPTED {
            return Err(McpError::TransportClosed(format!(
                "{}: SSE POST HTTP {}",
                self.server_name,
                status.as_u16()
            )));
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        match self.inbox.recv().await {
            Some(m) => Ok(Some(m)),
            None => {
                // mpsc::Receiver::recv returns None when the sender is
                // dropped. That happens (1) on clean shutdown or (2) when
                // the pump exited due to an error. Distinguish via the
                // shared error slot.
                if let Some(reason) = self.pump_error.lock().ok().and_then(|g| g.clone()) {
                    return Err(McpError::TransportClosed(format!(
                        "{}: sse stream error: {reason}",
                        self.server_name
                    )));
                }
                Ok(None)
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(p) = self.pump.take() {
            p.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `chunks` through the capped stream and the real SSE parser and
    /// collect what comes out: event data on success, the error text on
    /// failure.
    async fn parse(
        chunks: Vec<&'static [u8]>,
        cap: usize,
    ) -> Vec<std::result::Result<String, String>> {
        let inner = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        cap_sse_frames(inner, cap)
            .eventsource()
            .map(|r| r.map(|ev| ev.data).map_err(|e| e.to_string()))
            .collect()
            .await
    }

    #[tokio::test]
    async fn an_event_over_the_cap_errors_and_ends_the_stream() {
        // 40 bytes of data in one event against a 32-byte cap. The event is
        // split over several chunks so the count must carry across them. The
        // event after it is never reached: the stream ends at the error.
        let out = parse(
            vec![
                b"data: 0123456789",
                b"0123456789",
                b"0123456789\n\n",
                b"data: next\n\n",
            ],
            32,
        )
        .await;
        assert_eq!(out.len(), 1, "stream must end at the error: {out:?}");
        let err = out[0].as_ref().expect_err("oversized event must error");
        assert!(
            err.contains("exceeds max_response_bytes (32 bytes)"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn many_small_events_pass_even_when_their_total_exceeds_the_cap() {
        // 50 events of 10 bytes each is 500 bytes against a 32-byte cap. The
        // bound is per event, so a long-lived stream must never trip it.
        let chunks: Vec<&'static [u8]> = vec![b"data: abcd\n\n"; 50];
        let out = parse(chunks, 32).await;
        assert_eq!(out.len(), 50);
        assert!(out.iter().all(|r| r.as_deref() == Ok("abcd")), "{out:?}");
    }

    #[test]
    fn each_line_ending_style_closes_an_event() {
        // Every terminator style from the SSE grammar must reset the count.
        // Each event alone is under the cap; two together are over it.
        for (style, event) in [
            ("LF", &b"data: 0123456789\n\n"[..]),
            ("CR", &b"data: 0123456789\r\r"[..]),
            ("CRLF", &b"data: 0123456789\r\n\r\n"[..]),
        ] {
            let mut limit = FrameLimit::new(20);
            assert_eq!(limit.feed(event), Ok(()), "{style}: first event");
            assert_eq!(limit.feed(event), Ok(()), "{style}: count was not reset");
        }
    }

    #[test]
    fn a_crlf_split_across_chunks_is_one_line_ending() {
        // A CRLF whose LF starts the next chunk ends one line, not two. Read
        // as two, the LF would look like a blank line, reset the count, and
        // let an event with many short lines grow past the cap.
        let mut limit = FrameLimit::new(25);
        assert_eq!(limit.feed(b"data: 01234\r"), Ok(()));
        assert_eq!(limit.feed(b"\ndata: 56789\r"), Ok(()));
        assert_eq!(limit.feed(b"\ndata: overflow"), Err(()));

        // And a blank line whose final LF arrives alone still closes the
        // event, so the next event starts from zero.
        let mut limit = FrameLimit::new(25);
        assert_eq!(limit.feed(b"data: 0123456789\r\n\r"), Ok(()));
        assert_eq!(limit.feed(b"\n"), Ok(()));
        assert_eq!(limit.feed(b"data: 0123456789\r\n\r\n"), Ok(()));
    }

    #[tokio::test]
    async fn transport_errors_pass_through_without_ending_the_stream_early() {
        // A body error is the underlying stream's own; the cap must forward
        // it verbatim rather than mask it as a size error.
        let inner = futures::stream::iter(vec![
            Ok(&b"data: ok\n\n"[..]),
            Err(std::io::Error::other("connection reset")),
        ]);
        let out: Vec<_> = cap_sse_frames(inner, 32).collect().await;
        assert_eq!(out.len(), 2);
        assert!(out[0].is_ok());
        match &out[1] {
            Err(SseBodyError::Transport(e)) => assert_eq!(e.to_string(), "connection reset"),
            other => panic!("expected the transport error, got {other:?}"),
        }
    }
}
