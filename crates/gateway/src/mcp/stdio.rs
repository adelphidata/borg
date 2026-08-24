//! Stdio transport — JSON-RPC over stdin/stdout with LSP-style framing.
//!
//! Each message is preceded by a `Content-Length: N\r\n\r\n` header
//! block, then the N-byte JSON body. This is what Claude Desktop's MCP
//! client speaks to subprocess servers; matching the framing exactly
//! is required for it to work.
//!
//! Bare line-delimited JSON is also accepted as a fallback so MCP
//! Inspector and ad-hoc shell pipelines keep working — Inspector sends
//! one JSON object per line by default.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::server::McpServer;

/// Maximum framed message size accepted on stdio. Matches the HTTP
/// transport's body cap so the two surfaces have a single
/// abuse-budget answer. A forged `Content-Length: 999999999999`
/// would otherwise allocate that much before the read fails.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Buffer size for in-memory `tokio::io::duplex` channels used by the
/// stdio test harness. 64 KB matches typical pipe buffers and keeps a
/// single response (well under `MAX_FRAME_BYTES`) from blocking the
/// writer.
#[cfg(test)]
const DUPLEX_BUFFER_BYTES: usize = 64 * 1024;

/// Drive the MCP server from arbitrary async byte streams.
///
/// Reads framed JSON-RPC messages from `input`, dispatches each to
/// `server.handle`, and writes responses to `output` with the same
/// framing convention. Returns when `input` reaches EOF.
pub async fn run<R, W>(server: Arc<McpServer>, input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(input);

    loop {
        let frame = match read_frame(&mut reader).await? {
            Some(f) => f,
            None => return Ok(()),
        };
        let request: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                // Malformed JSON: emit a JSON-RPC parse error
                // response and continue. JSON-RPC says to use code
                // -32700 with id=null when id can't be parsed.
                let resp = serde_json::json!({
                    "jsonrpc":"2.0","id":null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                });
                write_frame(&mut output, &resp).await?;
                continue;
            }
        };

        if let Some(resp) = server.handle(&request).await {
            write_frame(&mut output, &resp).await?;
        }
    }
}

/// Read one framed message. Returns `Ok(None)` on clean EOF.
async fn read_frame<R>(reader: &mut BufReader<R>) -> Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Read headers line-by-line until blank line. If the first line
    // is a JSON object, treat the whole line as the message (line-
    // delimited fallback for MCP Inspector / ad-hoc pipelines).
    let mut content_length: Option<usize> = None;
    let mut first_line: Option<String> = None;

    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read header line")?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if first_line.is_none() && trimmed.starts_with('{') {
            // Line-delimited JSON fallback. Push the whole line
            // back as a complete message.
            return Ok(Some(trimmed.as_bytes().to_vec()));
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let parsed: usize = rest
                .trim()
                .parse()
                .context("Content-Length header is not a number")?;
            content_length = Some(parsed);
        }
        first_line.get_or_insert_with(|| trimmed.to_string());
    }

    let len = content_length.context("missing Content-Length header")?;
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("Content-Length {len} exceeds {MAX_FRAME_BYTES}-byte cap");
    }
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .context("read framed body")?;
    Ok(Some(buf))
}

async fn write_frame<W>(output: &mut W, value: &serde_json::Value) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value).context("serialize response")?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    output.write_all(header.as_bytes()).await?;
    output.write_all(&body).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::server::{McpServer, McpToolDescriptor, ReadOnlyToolHandler};
    use super::*;
    use async_trait::async_trait;
    use tokio::io::AsyncReadExt;

    struct EchoTool;
    #[async_trait]
    impl ReadOnlyToolHandler for EchoTool {
        fn descriptor(&self) -> McpToolDescriptor {
            McpToolDescriptor {
                name: "memory_search".into(),
                description: "stub".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, super::super::server::McpError> {
            Ok(args)
        }
    }

    fn server() -> Arc<McpServer> {
        Arc::new(McpServer::new(vec![Box::new(EchoTool)], "0.1.0-test"))
    }

    #[tokio::test]
    async fn lsp_framed_round_trip() {
        // Claude Desktop speaks Content-Length framing. The server
        // must accept it AND emit the same framing on responses, or
        // Desktop's reader can't decode the reply.
        let request = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
        });
        let body = serde_json::to_vec(&request).expect("serialize");
        let mut input = Vec::new();
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        input.extend_from_slice(&body);

        let (mut out_reader, mut out_writer) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
        let server = server();
        let task = tokio::spawn(async move {
            let cursor = std::io::Cursor::new(input);
            run(server, cursor, &mut out_writer).await
        });

        // Read header line.
        let mut buf = vec![0u8; 1024];
        let n = out_reader.read(&mut buf).await.expect("read response");
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            response.starts_with("Content-Length:"),
            "response must use LSP framing, got: {response}"
        );
        let (header, json_part) = response
            .split_once("\r\n\r\n")
            .expect("framing has blank-line separator");
        let len: usize = header
            .strip_prefix("Content-Length:")
            .unwrap()
            .trim()
            .parse()
            .expect("length parses");
        let parsed: serde_json::Value =
            serde_json::from_str(&json_part[..len]).expect("body parses");
        assert_eq!(parsed["id"], 1);
        assert!(parsed["result"]["tools"].is_array());

        // Drop reader so the input cursor returns EOF and `run` returns.
        drop(out_reader);
        let _ = task.await;
    }

    #[tokio::test]
    async fn line_delimited_json_fallback_works() {
        // MCP Inspector and ad-hoc shell pipelines often send JSON
        // line-by-line without the LSP header. The fallback path
        // accepts that for development convenience while the framed
        // path remains the canonical one for production clients.
        let mut input = Vec::new();
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#);
        input.extend_from_slice(b"\n");

        let (mut out_reader, mut out_writer) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
        let server = server();
        let task = tokio::spawn(async move {
            let cursor = std::io::Cursor::new(input);
            run(server, cursor, &mut out_writer).await
        });

        let mut buf = vec![0u8; 1024];
        let n = out_reader.read(&mut buf).await.expect("read response");
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        // Server always replies with framed JSON regardless of what
        // the client sent. That's intentional — most clients can
        // parse framed responses; line-delimited responses confuse
        // some by leaving partial reads.
        assert!(response.starts_with("Content-Length:"));
        assert!(response.contains("\"id\":7"));
        drop(out_reader);
        let _ = task.await;
    }

    /// The stdio cap is 1 MiB — the same abuse budget as the HTTP body
    /// limit — and inclusive: a frame of exactly the cap is legitimate.
    /// The size is hardcoded (not derived from `MAX_FRAME_BYTES`) so a
    /// drifted constant fails here instead of the test drifting with it.
    #[tokio::test]
    async fn frame_at_exactly_one_mib_is_accepted() {
        const ONE_MIB: usize = 1_048_576;
        let probe = serde_json::json!({
            "jsonrpc":"2.0","id":9,"method":"tools/list","params":{"pad":""}
        });
        let base_len = serde_json::to_vec(&probe).expect("serialize").len();
        let request = serde_json::json!({
            "jsonrpc":"2.0","id":9,"method":"tools/list",
            "params":{"pad": "x".repeat(ONE_MIB - base_len)}
        });
        let body = serde_json::to_vec(&request).expect("serialize");
        assert_eq!(body.len(), ONE_MIB, "test must sit exactly on the cap");

        let mut input = Vec::new();
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        input.extend_from_slice(&body);

        let (mut out_reader, mut out_writer) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
        let server = server();
        let task = tokio::spawn(async move {
            let cursor = std::io::Cursor::new(input);
            run(server, cursor, &mut out_writer).await
        });

        let mut buf = vec![0u8; 4096];
        let n = out_reader.read(&mut buf).await.expect("read response");
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            response.contains("\"id\":9") && !response.contains("exceeds"),
            "an at-cap frame must be processed, got: {response}"
        );
        drop(out_reader);
        let _ = task.await;
    }

    /// One byte over the cap must be refused up front by the declared
    /// length — not by attempting the oversized allocation and read.
    #[tokio::test]
    async fn frame_over_the_cap_is_rejected_by_the_header() {
        let mut input = Vec::new();
        input.extend_from_slice(b"Content-Length: 1048577\r\n\r\n");
        input.extend_from_slice(b"{}");

        let err = run(server(), std::io::Cursor::new(input), tokio::io::sink())
            .await
            .expect_err("over-cap frame must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("1048577") && msg.contains("exceeds"),
            "must reject via the cap check, got: {msg}"
        );
    }

    /// The harness duplex buffer must absorb a whole multi-KB framed
    /// response while nothing is draining the read side, or `run`
    /// deadlocks mid-`write_all` — the invariant the buffer size exists
    /// to uphold.
    #[tokio::test]
    async fn large_response_completes_without_a_draining_reader() {
        let pad = "y".repeat(8 * 1024);
        let request = serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"memory_search","arguments":{"pad": pad}}
        });
        let mut input = serde_json::to_vec(&request).expect("serialize");
        input.push(b'\n');

        let (mut out_reader, mut out_writer) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
        let server = server();
        let task = tokio::spawn(async move {
            let cursor = std::io::Cursor::new(input);
            run(server, cursor, &mut out_writer).await
        });

        // Await completion BEFORE reading: the full ~8 KB response must
        // fit the buffer without backpressure wedging the writer.
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("run must finish while the reader is idle")
            .expect("join")
            .expect("run ok");

        let mut all = String::new();
        out_reader
            .read_to_string(&mut all)
            .await
            .expect("read response");
        assert!(
            all.contains(&"y".repeat(8 * 1024)),
            "echoed pad must arrive intact"
        );
    }

    #[tokio::test]
    async fn malformed_json_emits_parse_error_and_continues() {
        // A garbled message in the middle of a session must not
        // wedge the loop. The server should reply with -32700 and
        // process subsequent messages normally.
        let bad_body = b"{not json";
        let good = serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/list","params":{}
        });
        let good_body = serde_json::to_vec(&good).expect("serialize");

        let mut input = Vec::new();
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", bad_body.len()).as_bytes());
        input.extend_from_slice(bad_body);
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", good_body.len()).as_bytes());
        input.extend_from_slice(&good_body);

        let (mut out_reader, mut out_writer) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
        let server = server();
        let task = tokio::spawn(async move {
            let cursor = std::io::Cursor::new(input);
            run(server, cursor, &mut out_writer).await
        });

        let mut all = Vec::new();
        // Read until task completes.
        loop {
            let mut buf = vec![0u8; 1024];
            match out_reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
            }
            if all.windows(7).any(|w| w == b"\"id\":2,") || all.contains(&b'2') && all.len() > 200 {
                break;
            }
        }
        drop(out_reader);
        let _ = task.await;

        let text = String::from_utf8_lossy(&all);
        assert!(
            text.contains("-32700"),
            "first reply must be parse-error, got: {text}"
        );
        assert!(
            text.contains("\"id\":2"),
            "second reply must process normally, got: {text}"
        );
    }
}
