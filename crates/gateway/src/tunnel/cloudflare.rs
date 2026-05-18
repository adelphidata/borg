//! Cloudflare tunnel via the `cloudflared` binary.

use anyhow::{anyhow, bail, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration, Instant};

use crate::tunnel::{
    kill_shared, new_shared_process, new_shared_url, SharedProcess, SharedUrl, Tunnel, TunnelError,
    TunnelProcess,
};

const CLOUDFLARED_INSTALL_URL: &str =
    "https://github.com/cloudflare/cloudflared/releases (or `brew install cloudflared`)";
const URL_DEADLINE: Duration = Duration::from_secs(30);
const PER_LINE_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_TAIL_TIMEOUT: Duration = Duration::from_secs(1);
const ERROR_TAIL_MAX_LINES: usize = 10;

/// Wraps `cloudflared` with token-based auth from the Zero Trust dashboard.
pub struct CloudflareTunnel {
    token: String,
    proc: SharedProcess,
    url: SharedUrl,
}

impl CloudflareTunnel {
    pub fn new(token: String) -> Self {
        Self {
            token,
            proc: new_shared_process(),
            url: new_shared_url(),
        }
    }
}

/// Build the `cloudflared` argv. Extracted for unit testing: a regression
/// to omit `--no-autoupdate` would cause cloudflared to phone home on
/// every start, which on slow networks blocks long enough that the
/// URL-extraction deadline fires.
pub fn build_cloudflared_args(origin: &str, token: &str) -> Vec<String> {
    vec![
        "tunnel".into(),
        "--no-autoupdate".into(),
        "run".into(),
        "--token".into(),
        token.into(),
        "--url".into(),
        origin.into(),
    ]
}

/// Extract the public URL from a single cloudflared log line. Returns
/// `None` when no `https://` URL appears on the line.
///
/// Real-world line shape:
/// `2026-05-17T01:23:45Z INF +-----------------------------+ | https://abc-def.trycloudflare.com |`
pub fn parse_cloudflared_url_from_line(line: &str) -> Option<String> {
    let idx = line.find("https://")?;
    let url_part = &line[idx..];
    let end = url_part
        .find(|c: char| c.is_whitespace())
        .unwrap_or(url_part.len());
    Some(url_part[..end].to_string())
}

#[async_trait::async_trait]
impl Tunnel for CloudflareTunnel {
    fn name(&self) -> &str {
        "cloudflare"
    }

    async fn start(&self, local_host: &str, local_port: u16) -> Result<String> {
        if which::which("cloudflared").is_err() {
            bail!(TunnelError::BinaryMissing {
                binary: "cloudflared",
                install_url: CLOUDFLARED_INSTALL_URL,
            });
        }

        let origin = format!("http://{local_host}:{local_port}");
        let args = build_cloudflared_args(&origin, &self.token);

        let mut child = Command::new("cloudflared")
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout = child.stdout.take();
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture cloudflared stderr"))?;

        let mut reader = tokio::io::BufReader::new(stderr).lines();
        let mut public_url = String::new();

        let deadline = Instant::now() + URL_DEADLINE;
        while Instant::now() < deadline {
            match timeout(PER_LINE_TIMEOUT, reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    tracing::debug!("cloudflared: {line}");
                    if let Some(url) = parse_cloudflared_url_from_line(&line) {
                        public_url = url;
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading cloudflared output: {e}"),
                Err(_) => {} // per-line timeout, keep waiting until overall deadline
            }
        }

        if public_url.is_empty() {
            let detail = drain_error_tail(stdout).await;
            if let Err(e) = child.kill().await {
                tracing::debug!("cloudflared kill after URL timeout: {e}");
            }
            if detail.is_empty() {
                bail!("cloudflared did not produce a public URL within 30s");
            }
            bail!("cloudflared failed to start: {detail}");
        }

        if let Ok(mut guard) = self.url.write() {
            *guard = Some(public_url.clone());
        }

        // Pipe-drain invariant: see TunnelProcess docs in mod.rs. The
        // background drain stays alive for the child's lifetime; dropping
        // it would SIGPIPE cloudflared on its next log write.
        let drain_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::trace!("cloudflared: {line}");
            }
        });

        if let Some(stdout) = stdout {
            tokio::spawn(async move {
                let mut out_reader = tokio::io::BufReader::new(stdout).lines();
                while let Ok(Some(_)) = out_reader.next_line().await {}
            });
        }

        let mut guard = self.proc.lock().await;
        *guard = Some(TunnelProcess {
            child,
            _pipe_drain: Some(drain_handle),
        });

        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        if let Ok(mut guard) = self.url.write() {
            *guard = None;
        }
        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        let guard = self.proc.lock().await;
        guard.as_ref().is_some_and(|tp| tp.child.id().is_some())
    }

    fn public_url(&self) -> Option<String> {
        self.url.read().ok().and_then(|g| g.clone())
    }
}

async fn drain_error_tail(stdout: Option<tokio::process::ChildStdout>) -> String {
    let Some(stdout) = stdout else {
        return String::new();
    };
    let mut reader = tokio::io::BufReader::new(stdout).lines();
    let mut lines = Vec::new();
    while lines.len() < ERROR_TAIL_MAX_LINES {
        match timeout(ERROR_TAIL_TIMEOUT, reader.next_line()).await {
            Ok(Ok(Some(line))) => lines.push(line),
            _ => break,
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_includes_no_autoupdate_and_token_and_origin() {
        let args = build_cloudflared_args("http://127.0.0.1:8080", "secret-cf-token");
        // Real failure mode: dropping --no-autoupdate causes a phone-home
        // call on startup that often misses the 30s URL deadline on slow
        // networks. Token MUST come after `--token` and origin after
        // `--url` so cloudflared's flag parser binds them correctly.
        assert!(
            args.iter().any(|a| a == "--no-autoupdate"),
            "missing --no-autoupdate: {args:?}"
        );
        let token_idx = args
            .iter()
            .position(|a| a == "--token")
            .expect("--token flag present");
        assert_eq!(
            args.get(token_idx + 1).map(String::as_str),
            Some("secret-cf-token")
        );
        let url_idx = args
            .iter()
            .position(|a| a == "--url")
            .expect("--url flag present");
        assert_eq!(
            args.get(url_idx + 1).map(String::as_str),
            Some("http://127.0.0.1:8080")
        );
    }

    #[test]
    fn build_args_subcommand_is_tunnel_run() {
        let args = build_cloudflared_args("http://127.0.0.1:8080", "tok");
        // Cloudflared distinguishes `tunnel run` (token mode) from
        // `tunnel route` (named-tunnel mode). Drift to the wrong
        // subcommand would render every config silently inert.
        assert_eq!(args.first().map(String::as_str), Some("tunnel"));
        assert!(args.iter().any(|a| a == "run"));
    }

    #[test]
    fn parse_url_extracts_trycloudflare_url_from_log_line() {
        let line = "2026-05-17T01:23:45Z INF +-----------------------------+ | https://abc-def-ghi.trycloudflare.com |";
        let url = parse_cloudflared_url_from_line(line).expect("URL should be extracted");
        assert_eq!(url, "https://abc-def-ghi.trycloudflare.com");
    }

    #[test]
    fn parse_url_extracts_named_tunnel_url() {
        let line =
            "INF Registered tunnel connection origin=https://my-tunnel.example.com latency=14ms";
        let url = parse_cloudflared_url_from_line(line).expect("URL should be extracted");
        assert_eq!(url, "https://my-tunnel.example.com");
    }

    #[test]
    fn parse_url_returns_none_for_line_without_https() {
        // Negative case: most cloudflared lines are not URL lines.
        // Returning `Some("")` here (e.g. via `unwrap_or_default`)
        // would silently complete `start()` with an empty URL.
        assert!(parse_cloudflared_url_from_line("INF starting tunnel").is_none());
        assert!(parse_cloudflared_url_from_line("").is_none());
        assert!(parse_cloudflared_url_from_line("http://example.com no-tls").is_none());
    }

    #[test]
    fn parse_url_stops_at_first_whitespace_so_trailing_log_data_is_dropped() {
        let line = "INF tunnel started url=https://abc.trycloudflare.com handler=quic conn=4";
        let url = parse_cloudflared_url_from_line(line).expect("URL should be extracted");
        assert_eq!(url, "https://abc.trycloudflare.com");
    }

    #[test]
    fn parse_url_only_picks_https_not_http() {
        // Cloudflare tunnels are HTTPS-only. If parse picked up an http://
        // line we'd silently serve a plaintext URL to the agent, which the
        // operator would then publish.
        let line = "INF forwarding to http://internal-only:8080";
        assert!(parse_cloudflared_url_from_line(line).is_none());
    }

    #[test]
    fn constructor_stores_token() {
        let tunnel = CloudflareTunnel::new("cf-token".into());
        assert_eq!(tunnel.token, "cf-token");
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn stop_without_start_is_idempotent() {
        // Real failure mode: an early implementation panicked on
        // unwrap-of-None when the proc slot was empty. stop() must be
        // safe to call before start() (the supervisor's shutdown path
        // hits this when start failed) and after a previous stop().
        let tunnel = CloudflareTunnel::new("tok".into());
        assert!(tunnel.stop().await.is_ok());
        assert!(tunnel.stop().await.is_ok());
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn health_false_before_start() {
        assert!(!CloudflareTunnel::new("tok".into()).health_check().await);
    }
}
