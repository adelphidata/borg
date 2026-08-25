//! ngrok tunnel via the `ngrok` binary.

use anyhow::{anyhow, bail, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration, Instant};

use crate::tunnel::{
    kill_shared, new_shared_process, new_shared_url, SharedProcess, SharedUrl, Tunnel, TunnelError,
    TunnelProcess,
};

const NGROK_INSTALL_URL: &str = "https://ngrok.com/download";
/// ngrok surfaces its URL over the local agent API almost immediately, so the
/// deadline is half cloudflared's; the bounded error tail keeps a crash loop
/// from flooding the log.
const URL_DEADLINE: Duration = Duration::from_secs(15);
const PER_LINE_TIMEOUT: Duration = Duration::from_secs(3);
const ERROR_TAIL_TIMEOUT: Duration = Duration::from_secs(1);
const ERROR_TAIL_MAX_LINES: usize = 10;

/// Wraps `ngrok` with optional custom-domain support (paid plan).
pub struct NgrokTunnel {
    auth_token: String,
    domain: Option<String>,
    proc: SharedProcess,
    url: SharedUrl,
}

impl NgrokTunnel {
    pub fn new(auth_token: String, domain: Option<String>) -> Self {
        Self {
            auth_token,
            domain,
            proc: new_shared_process(),
            url: new_shared_url(),
        }
    }
}

/// Build the ngrok argv list. **The auth token MUST NOT appear in argv** —
/// it is passed via the `NGROK_AUTHTOKEN` env var by `start()`. The argv
/// goes through `tasklist /v` / `ps -ef` and any same-session local
/// process can read it; the env var is per-process and inherits cleanly.
pub fn build_ngrok_args(local_host: &str, local_port: u16, domain: Option<&str>) -> Vec<String> {
    let mut args = vec!["http".into(), format!("{local_host}:{local_port}")];
    if let Some(domain) = domain {
        args.push("--domain".into());
        args.push(domain.into());
    }
    // logfmt + stdout makes URL extraction trivial and deterministic;
    // the alternative ANSI-coloured `term` format breaks the parser when
    // ngrok detects a non-TTY child.
    args.push("--log".into());
    args.push("stdout".into());
    args.push("--log-format".into());
    args.push("logfmt".into());
    args
}

/// Parse a single logfmt line for an `url=https://...` token. Returns
/// `None` when the line has no `url=` field (every ngrok log line gets
/// a chance; only one carries the URL).
pub fn parse_ngrok_url_from_line(line: &str) -> Option<String> {
    let idx = line.find("url=https://")?;
    // Skip past `url=` so the slice starts at `https://`.
    let url_part = &line[idx + 4..];
    let end = url_part
        .find(|c: char| c.is_whitespace())
        .unwrap_or(url_part.len());
    Some(url_part[..end].to_string())
}

#[async_trait::async_trait]
impl Tunnel for NgrokTunnel {
    fn name(&self) -> &str {
        "ngrok"
    }

    async fn start(&self, local_host: &str, local_port: u16) -> Result<String> {
        if which::which("ngrok").is_err() {
            bail!(TunnelError::BinaryMissing {
                binary: "ngrok",
                install_url: NGROK_INSTALL_URL,
            });
        }

        let args = build_ngrok_args(local_host, local_port, self.domain.as_deref());

        let mut child = Command::new("ngrok")
            .args(&args)
            .env("NGROK_AUTHTOKEN", &self.auth_token)
            // Long-lived daemon child; see the cloudflare backend.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture ngrok stdout"))?;
        let stderr = child.stderr.take();
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut public_url = String::new();

        let deadline = Instant::now() + URL_DEADLINE;
        while Instant::now() < deadline {
            match timeout(PER_LINE_TIMEOUT, reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    tracing::debug!("ngrok: {line}");
                    if let Some(url) = parse_ngrok_url_from_line(&line) {
                        public_url = url;
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading ngrok output: {e}"),
                Err(_) => {}
            }
        }

        if public_url.is_empty() {
            let detail = drain_error_tail(stderr).await;
            if let Err(e) = child.kill().await {
                tracing::debug!("ngrok kill after URL timeout: {e}");
            }
            if detail.is_empty() {
                bail!("ngrok did not produce a public URL within 15s");
            }
            bail!("ngrok failed to start: {detail}");
        }

        if let Ok(mut guard) = self.url.write() {
            *guard = Some(public_url.clone());
        }

        let drain_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::trace!("ngrok: {line}");
            }
        });

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut err_reader = tokio::io::BufReader::new(stderr).lines();
                // ngrok emits operational info on stderr (rate-limit
                // warnings, edge reconnects). Surface at trace so a
                // RUST_LOG=trace session has the data without spamming
                // operators by default.
                while let Ok(Some(line)) = err_reader.next_line().await {
                    tracing::trace!("ngrok[stderr]: {line}");
                }
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

async fn drain_error_tail(stderr: Option<tokio::process::ChildStderr>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };
    let mut reader = tokio::io::BufReader::new(stderr).lines();
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
    fn build_args_omits_domain_when_none_set() {
        let args = build_ngrok_args("127.0.0.1", 8080, None);
        assert!(
            !args.iter().any(|a| a == "--domain"),
            "free-plan tunnel must not pass --domain (ngrok errors otherwise): {args:?}"
        );
        // The forward address must be in `host:port` form, not a URL —
        // ngrok rejects `http://host:port` here.
        assert!(
            args.iter().any(|a| a == "127.0.0.1:8080"),
            "forward target missing: {args:?}"
        );
        assert!(!args.iter().any(|a| a.starts_with("http://")));
    }

    #[test]
    fn build_args_includes_domain_when_set() {
        let args = build_ngrok_args("127.0.0.1", 8080, Some("my-domain.ngrok.app"));
        let dom_idx = args.iter().position(|a| a == "--domain").expect("--domain");
        assert_eq!(
            args.get(dom_idx + 1).map(String::as_str),
            Some("my-domain.ngrok.app")
        );
    }

    #[test]
    fn build_args_force_logfmt_so_parser_is_deterministic() {
        // Real failure mode: without --log-format=logfmt, ngrok's `term`
        // output uses ANSI colour codes and the URL parser misses the
        // line, then the deadline fires.
        let args = build_ngrok_args("127.0.0.1", 8080, None);
        let fmt_idx = args
            .iter()
            .position(|a| a == "--log-format")
            .expect("--log-format");
        assert_eq!(args.get(fmt_idx + 1).map(String::as_str), Some("logfmt"));
        let log_idx = args.iter().position(|a| a == "--log").expect("--log");
        assert_eq!(args.get(log_idx + 1).map(String::as_str), Some("stdout"));
    }

    /// SECURITY-CRITICAL regression guard. The ngrok auth token MUST NOT
    /// appear in argv — same-session local processes can read argv via
    /// `ps -ef` / `tasklist /v` and would harvest the token. The token
    /// is passed via `NGROK_AUTHTOKEN` env var instead, which only the
    /// child process sees.
    #[test]
    fn build_args_never_contains_an_auth_token() {
        let args = build_ngrok_args("127.0.0.1", 8080, Some("dom.example.com"));
        for a in &args {
            assert!(
                !a.contains("authtoken"),
                "argv must not contain the literal 'authtoken' substring: {a}"
            );
            assert!(
                !a.starts_with("2"),
                "argv contains what looks like an ngrok token (starts with digit): {a}"
            );
        }
    }

    #[test]
    fn parse_url_extracts_url_from_logfmt() {
        let line = r#"t=2026-05-17T01:23:45+0000 lvl=info msg="started tunnel" obj=tunnels name=command_line addr=http://127.0.0.1:8080 url=https://abcd.ngrok-free.app"#;
        let url = parse_ngrok_url_from_line(line).expect("URL should be extracted");
        assert_eq!(url, "https://abcd.ngrok-free.app");
    }

    #[test]
    fn parse_url_extracts_paid_custom_domain() {
        let line = r#"t=... lvl=info msg=connected url=https://my-tunnel.example.com region=us"#;
        let url = parse_ngrok_url_from_line(line).expect("URL should be extracted");
        assert_eq!(url, "https://my-tunnel.example.com");
    }

    #[test]
    fn parse_url_skips_url_field_when_value_is_http_not_https() {
        // ngrok logs both addr=http://... and url=https://... — the parser
        // must only pick the secure tunnel URL. Picking the addr value
        // would publish a 127.0.0.1 URL to the operator's gateway, which
        // is useless externally.
        let line = "addr=http://127.0.0.1:8080 url=https://abc.ngrok-free.app";
        let url = parse_ngrok_url_from_line(line).expect("URL");
        assert!(url.starts_with("https://"), "{url}");
        assert!(!url.contains("127.0.0.1"), "{url}");
    }

    #[test]
    fn parse_url_returns_none_for_lines_without_url_field() {
        assert!(parse_ngrok_url_from_line("").is_none());
        assert!(parse_ngrok_url_from_line("msg=ready").is_none());
        // A bare https:// (no `url=` prefix) is NOT a URL line for ngrok.
        // Picking it up would catch heartbeat messages that mention the
        // dashboard URL.
        assert!(parse_ngrok_url_from_line("dashboard at https://dashboard.ngrok.com").is_none());
    }

    #[test]
    fn constructor_stores_auth_token_and_domain() {
        let tunnel = NgrokTunnel::new("tok".into(), Some("my.ngrok.app".into()));
        assert_eq!(tunnel.auth_token, "tok");
        assert_eq!(tunnel.domain.as_deref(), Some("my.ngrok.app"));
    }

    #[tokio::test]
    async fn stop_without_start_is_idempotent() {
        let tunnel = NgrokTunnel::new("tok".into(), None);
        assert!(tunnel.stop().await.is_ok());
        assert!(tunnel.stop().await.is_ok());
    }

    #[tokio::test]
    async fn health_false_before_start() {
        assert!(!NgrokTunnel::new("tok".into(), None).health_check().await);
    }

    /// Full lifecycle against a fake `ngrok` on PATH: `start()` must return
    /// the URL scraped from the logfmt stdout, publish it via
    /// `public_url()`, report healthy while the child lives, and `stop()`
    /// must clear both the URL and the process slot. The provider name is
    /// asserted too — it is the stable identifier the supervisor logs and
    /// `borg tunnel status` prints.
    #[tokio::test]
    async fn start_reports_url_health_and_stop_clears_state() {
        let _guard = crate::test_env::borg_home_guard();
        let _shim = crate::tunnel::test_support::BinShim::install(
            "ngrok",
            "echo 'lvl=info msg=\"started tunnel\" url=https://fake.ngrok-free.app'\nexec sleep 30",
        );
        let tunnel = NgrokTunnel::new("tok".into(), None);
        assert_eq!(tunnel.name(), "ngrok");
        assert!(
            tunnel.public_url().is_none(),
            "no URL may be published before start"
        );

        let url = tunnel
            .start("127.0.0.1", 18082)
            .await
            .expect("fake ngrok surfaces a URL");
        assert_eq!(url, "https://fake.ngrok-free.app");
        assert_eq!(
            tunnel.public_url().as_deref(),
            Some("https://fake.ngrok-free.app"),
            "public_url must reflect the started tunnel"
        );
        assert!(
            tunnel.health_check().await,
            "live child must report healthy"
        );

        tunnel.stop().await.expect("stop succeeds");
        assert!(
            tunnel.public_url().is_none(),
            "stop must clear the published URL"
        );
        assert!(
            !tunnel.health_check().await,
            "stopped tunnel must report unhealthy"
        );
    }

    /// When ngrok exits without printing a URL, `start()` must fail with
    /// the child's stderr tail in the message — bounded at 10 lines so a
    /// crash loop can't be dumped wholesale into the log.
    #[tokio::test]
    async fn start_failure_surfaces_bounded_stderr_tail() {
        let _guard = crate::test_env::borg_home_guard();
        let _shim = crate::tunnel::test_support::BinShim::install(
            "ngrok",
            "i=1\nwhile [ $i -le 12 ]; do echo \"tail-line-$i\" >&2; i=$((i+1)); done\nexit 1",
        );
        let tunnel = NgrokTunnel::new("tok".into(), None);
        let err = tunnel
            .start("127.0.0.1", 18083)
            .await
            .expect_err("no URL must fail start");
        let msg = format!("{err}");
        assert!(msg.contains("tail-line-2"), "tail must carry detail: {msg}");
        assert!(msg.contains("tail-line-10"), "tail keeps 10 lines: {msg}");
        assert!(
            !msg.contains("tail-line-11"),
            "tail must be capped at {ERROR_TAIL_MAX_LINES} lines: {msg}"
        );
    }
}
