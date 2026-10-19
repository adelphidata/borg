//! A local HTTP server that answers LLM requests with canned SSE bodies.
//!
//! Each request is read to the end of its body before the reply goes out.
//! A server that closes a socket with request bytes still unread makes the
//! kernel send a reset, and the client can lose the reply.

/// One streamed reply of `text` that ends with `finish`, then a usage chunk
/// of 11 + 7 = 18 tokens.
pub(crate) fn reply_with_usage(text: &str, finish: &str) -> String {
    let content = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": finish}]
    });
    format!(
        "data: {content}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}}}\n\n\
         data: [DONE]\n\n"
    )
}

/// The requests that a fixture server received, in order.
pub(crate) type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Read one HTTP request to the end of its body.
fn read_request(sock: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut raw = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let text = String::from_utf8_lossy(&raw).to_string();
        if let Some(head_end) = text.find("\r\n\r\n") {
            let body_len = text[..head_end]
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= head_end + 4 + body_len {
                return text;
            }
        }
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => return String::from_utf8_lossy(&raw).to_string(),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
}

/// Serve the raw SSE bodies, one for each connection, in order. Returns
/// the URL for a client and the requests that arrived. A request is
/// recorded before its reply is sent, so a client that has its reply sees
/// its request counted.
pub(crate) fn serve_sse<B: AsRef<str> + Send + 'static>(bodies: Vec<B>) -> (String, Requests) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    let requests = Requests::default();
    let seen = requests.clone();
    std::thread::spawn(move || {
        for body in bodies {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let request = read_request(&mut sock);
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            // Test fixture: a failed write shows as a failed assertion
            // in the test that reads the reply.
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Connection: close\r\n\r\n{}",
                    body.as_ref()
                )
                .as_bytes(),
            );
            let _ = sock.flush();
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        requests,
    )
}
