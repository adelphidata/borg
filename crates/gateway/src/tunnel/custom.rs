//! Custom-command tunnel. Lets operators wrap any reverse-proxy /
//! tunnel binary that borg doesn't ship native support for, by templating
//! a shell command with `{host}` and `{port}` placeholders.

use anyhow::{anyhow, bail, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration, Instant};

use crate::tunnel::{
    kill_shared, new_shared_process, new_shared_url, SharedProcess, SharedUrl, Tunnel,
    TunnelProcess,
};

const URL_DEADLINE: Duration = Duration::from_secs(30);
const PER_LINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Operator-supplied tunnel command. Substitutes `{host}` and `{port}`
/// before splitting; the URL is scraped from the child's stdout via
/// `url_pattern` (substring match) or the operator must surface it
/// out-of-band when `url_pattern = None`.
pub struct CustomTunnel {
    argv: Vec<String>,
    url_pattern: Option<String>,
    health_url: Option<String>,
    proc: SharedProcess,
    url: SharedUrl,
}

impl CustomTunnel {
    pub fn new(
        start_command: String,
        health_url: Option<String>,
        url_pattern: Option<String>,
    ) -> Result<Self> {
        let argv = shell_split(&start_command)?;
        if argv.is_empty() {
            bail!("custom tunnel start_command parsed to no tokens");
        }
        Ok(Self {
            argv,
            url_pattern,
            health_url,
            proc: new_shared_process(),
            url: new_shared_url(),
        })
    }
}

/// Minimal shell-words split with support for `'…'` and `"…"` quoting.
/// Borg already pulls in shellexpand but not shell-words; rolling a
/// small splitter here avoids an extra workspace dep for a few lines of
/// quote handling.
fn shell_split(input: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
            }
            (Some(_), c) => current.push(c),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in custom tunnel start_command");
    }
    if !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

#[async_trait::async_trait]
impl Tunnel for CustomTunnel {
    fn name(&self) -> &str {
        "custom"
    }

    async fn start(&self, local_host: &str, local_port: u16) -> Result<String> {
        let port_s = local_port.to_string();
        let substituted: Vec<String> = self
            .argv
            .iter()
            .map(|arg| arg.replace("{host}", local_host).replace("{port}", &port_s))
            .collect();

        let (program, args) = substituted.split_first().ok_or_else(|| {
            anyhow!("custom tunnel argv unexpectedly empty after placeholder substitution")
        })?;

        let mut child = Command::new(program)
            .args(args)
            // Long-lived daemon child; see the cloudflare backend.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture custom-tunnel stdout"))?;
        let stderr = child.stderr.take();
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut public_url = String::new();

        if let Some(pattern) = &self.url_pattern {
            let deadline = Instant::now() + URL_DEADLINE;
            while Instant::now() < deadline {
                match timeout(PER_LINE_TIMEOUT, reader.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        tracing::debug!("custom-tunnel: {line}");
                        if line.contains(pattern.as_str()) {
                            if let Some(idx) = line.find("https://") {
                                let part = &line[idx..];
                                let end =
                                    part.find(|c: char| c.is_whitespace()).unwrap_or(part.len());
                                public_url = part[..end].to_string();
                                break;
                            }
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => bail!("Error reading custom-tunnel output: {e}"),
                    Err(_) => {}
                }
            }
            if public_url.is_empty() {
                if let Err(e) = child.kill().await {
                    tracing::debug!("custom-tunnel kill after URL timeout: {e}");
                }
                bail!("custom tunnel did not surface a URL matching `{pattern}` within 30s");
            }
        } else if let Some(h) = &self.health_url {
            // No URL pattern → assume the operator already knows the URL
            // (e.g. fixed cloudflared named tunnel). Surface the health
            // URL as a placeholder so `borg tunnel status` has something
            // useful to print.
            public_url = h.clone();
        }

        if let Ok(mut guard) = self.url.write() {
            *guard = if public_url.is_empty() {
                None
            } else {
                Some(public_url.clone())
            };
        }

        // Drain remaining stdout in the background to prevent pipe stall.
        let drain_handle = tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::trace!("custom-tunnel: {line}");
            }
        });
        // Drain stderr too — operator commands that write verbose progress
        // (caddy, tailscale debug, ngrok-style logs) will fill the ~64 KiB
        // pipe buffer and wedge the child after some uptime if we don't.
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut err_reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = err_reader.next_line().await {
                    tracing::trace!("custom-tunnel[stderr]: {line}");
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
        let alive = {
            let guard = self.proc.lock().await;
            guard.as_ref().is_some_and(|tp| tp.child.id().is_some())
        };
        if !alive {
            return false;
        }
        let Some(url) = self.health_url.as_ref() else {
            return alive;
        };
        match timeout(
            Duration::from_secs(5),
            reqwest::Client::new().get(url).send(),
        )
        .await
        {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        }
    }

    fn public_url(&self) -> Option<String> {
        self.url.read().ok().and_then(|g| g.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_handles_simple_tokens() {
        assert_eq!(
            shell_split("ngrok http 127.0.0.1:8080").unwrap(),
            vec!["ngrok", "http", "127.0.0.1:8080"]
        );
    }

    #[test]
    fn shell_split_preserves_quoted_whitespace() {
        // Operators write commands with quoted args (e.g. message text).
        // Naïve `split_whitespace` would corrupt them.
        let parts = shell_split(r#"echo "hello world" 'literal arg'"#).unwrap();
        assert_eq!(parts, vec!["echo", "hello world", "literal arg"]);
    }

    #[test]
    fn shell_split_supports_backslash_escape_outside_quotes() {
        let parts = shell_split(r"echo a\ b").unwrap();
        assert_eq!(parts, vec!["echo", "a b"]);
    }

    #[test]
    fn shell_split_rejects_unterminated_quote() {
        // Real failure mode: silently dropping the rest of the string
        // could produce a half-built command line and confuse the
        // operator with "command not found" instead of "fix your config".
        let err = shell_split(r#"echo "open quote"#).unwrap_err();
        assert!(
            format!("{err}").contains("unterminated quote"),
            "error must name the problem: {err}"
        );
    }

    #[test]
    fn constructor_rejects_empty_start_command() {
        let err = CustomTunnel::new(String::new(), None, None)
            .err()
            .expect("empty start_command must error");
        assert!(format!("{err}").contains("no tokens"));
    }

    #[test]
    fn constructor_succeeds_with_minimal_command() {
        let tunnel = CustomTunnel::new("true".into(), None, None).unwrap();
        assert_eq!(tunnel.name(), "custom");
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn stop_without_start_is_idempotent() {
        let tunnel = CustomTunnel::new("true".into(), None, None).unwrap();
        assert!(tunnel.stop().await.is_ok());
        assert!(tunnel.stop().await.is_ok());
    }
}
