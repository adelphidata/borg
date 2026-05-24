//! Helpers for attaching the agent's browser tool to an externally-running
//! Chromium-family browser over Chrome DevTools Protocol (CDP). Parse a CDP
//! URL, probe whether it's reachable, optionally auto-launch a
//! Chromium-family browser with `--remote-debugging-port` in a dedicated
//! user-data-dir.
//!
//! Pure-Rust HTTP probe via `reqwest::blocking` (already a workspace dep);
//! TCP fallback for `ws://…/devtools/browser/<id>` style URLs that the
//! HTTP JSON probe can't reach.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use url::Url;

use crate::constants::BROWSER_DEFAULT_CDP_PORT;

/// Default URL `/browser connect` falls back to when no URL is supplied.
pub const DEFAULT_CDP_URL: &str = "http://127.0.0.1:9222";

/// Parsed + normalized CDP URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpUrl {
    /// Canonical URL string to hand to `chromiumoxide::Browser::connect`.
    pub url: String,
    /// Host component (for probing TCP connectivity).
    pub host: String,
    /// Port component (defaults from scheme when omitted).
    pub port: u16,
    /// Whether the URL points at a specific browser endpoint
    /// (`ws://…/devtools/browser/<id>`). When `true` the HTTP `/json/version`
    /// probe is skipped — the path can't be reached by the discovery API.
    pub is_browser_path: bool,
}

/// Parse and normalize a user-provided CDP URL. Empty input → default.
///
/// Accepts `http`/`https`/`ws`/`wss`, requires a host, computes the port
/// from the scheme when not provided, and strips path/query/fragment unless
/// the path is a specific `/devtools/browser/<id>` endpoint.
pub fn parse_cdp_url(input: &str) -> Result<CdpUrl> {
    let trimmed = input.trim();
    let raw = if trimmed.is_empty() {
        DEFAULT_CDP_URL.to_string()
    } else if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };

    let parsed = Url::parse(&raw).map_err(|e| anyhow!("Invalid browser url '{raw}': {e}"))?;

    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return Err(anyhow!(
            "Unsupported browser url scheme: {scheme} (expected one of: http, https, ws, wss)"
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("Missing host in browser url: {raw}"))?
        .to_string();

    let port = match parsed.port() {
        Some(p) => p,
        None => match scheme {
            "https" | "wss" => 443,
            _ => 80,
        },
    };

    let is_browser_path = parsed.path().starts_with("/devtools/browser/");

    let url = if is_browser_path {
        parsed.to_string()
    } else {
        // Strip path/query/fragment so chromiumoxide hits `/json/version` itself.
        let mut bare = parsed.clone();
        bare.set_path("");
        bare.set_query(None);
        bare.set_fragment(None);
        // url crate keeps the trailing slash; strip for cleaner display.
        bare.to_string().trim_end_matches('/').to_string()
    };

    Ok(CdpUrl {
        url,
        host,
        port,
        is_browser_path,
    })
}

/// Return `true` when the URL exposes a reachable Chrome DevTools endpoint.
///
/// **Blocking.** Uses `reqwest::blocking` + `std::net::TcpStream`. Call sites
/// in async code MUST wrap this in `tokio::task::spawn_blocking` to avoid
/// stalling the runtime — every existing caller does so. If you're tempted
/// to call this directly from an async fn, don't; the name's `_blocking`
/// suffix exists to keep this footgun visible.
///
/// For `http`/`https`: GETs `/json/version` (the canonical DevTools discovery
/// endpoint) and accepts any 2xx response. For `ws`/`wss` URLs that point
/// directly at a browser endpoint, falls back to a raw TCP connect.
pub fn is_cdp_ready_blocking(parsed: &CdpUrl, timeout: Duration) -> bool {
    if parsed.is_browser_path {
        return tcp_probe(&parsed.host, parsed.port, timeout);
    }

    // Map ws→http for the discovery probe.
    let probe_root = parsed
        .url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);

    let client = match reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(_) => return tcp_probe(&parsed.host, parsed.port, timeout),
    };

    for path in ["/json/version", "/json"] {
        let probe = format!("{probe_root}{path}");
        if let Ok(resp) = client.get(&probe).send() {
            if resp.status().is_success() {
                return true;
            }
        }
    }

    tcp_probe(&parsed.host, parsed.port, timeout)
}

fn tcp_probe(host: &str, port: u16, timeout: Duration) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok())
}

/// Path to the dedicated Chromium user-data-dir used for borg's auto-launched
/// debug browser. Kept separate from the user's normal profile so the debug
/// instance can coexist with an already-running Chrome.
pub fn chrome_debug_data_dir() -> Result<PathBuf> {
    Ok(crate::config::Config::data_dir()?.join("chrome-debug"))
}

fn chrome_debug_args(port: u16, data_dir: &std::path::Path) -> Vec<String> {
    vec![
        format!("--remote-debugging-port={port}"),
        // Bind the DevTools listener to loopback only. Modern Chrome defaults
        // to this since ~M111 but earlier builds + some forks (Brave, older
        // Edge) historically did not — pass it explicitly so this isn't
        // load-bearing on Chrome's defaults.
        "--remote-debugging-address=127.0.0.1".to_string(),
        // Allow the DevTools-over-WebSocket upgrade. Modern Chrome rejects WS
        // upgrades whose Origin isn't in this list; chromiumoxide's
        // `Browser::connect` would otherwise fail the handshake silently. The
        // loopback bind above is the actual access boundary.
        "--remote-allow-origins=*".to_string(),
        format!("--user-data-dir={}", data_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ]
}

/// Cross-platform list of Chromium-family executables to try when auto-launching.
/// Reuses `detection::find_chrome` so the candidate set stays consistent with
/// the headless launch path.
pub fn chrome_debug_candidates() -> Vec<PathBuf> {
    super::detection::find_chrome(None).all_found
}

/// Try to launch a Chromium-family browser with remote debugging enabled.
/// Returns `Ok(())` if a process was spawned (no guarantee it became reachable).
pub fn try_launch_chrome_debug(port: u16) -> Result<()> {
    let candidates = chrome_debug_candidates();
    if candidates.is_empty() {
        return Err(anyhow!(
            "No Chrome/Chromium executable found. Install Chrome, Chromium, Brave, or Edge."
        ));
    }

    let data_dir = chrome_debug_data_dir()?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| anyhow!("Failed to create chrome-debug data dir: {e}"))?;

    let args = chrome_debug_args(port, &data_dir);

    let mut last_err: Option<std::io::Error> = None;
    for candidate in &candidates {
        match spawn_detached(candidate, &args) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }

    Err(anyhow!(
        "Failed to launch any Chromium-family browser: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no spawn attempted".to_string())
    ))
}

#[cfg(unix)]
fn spawn_detached(exe: &std::path::Path, args: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe and detaches the child from our
    // controlling terminal/process group so a TUI Ctrl-C doesn't kill it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

#[cfg(windows)]
fn spawn_detached(exe: &std::path::Path, args: &[String]) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    std::process::Command::new(exe)
        .args(args)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Shell-quoted command the user can paste to launch a debug Chromium
/// themselves, used as a fallback hint when auto-launch fails.
pub fn manual_chrome_debug_command(port: u16) -> Option<String> {
    let candidates = chrome_debug_candidates();
    let exe = candidates.first()?;
    let data_dir = chrome_debug_data_dir().ok()?;
    let args = chrome_debug_args(port, &data_dir);
    let mut parts = vec![shell_quote(&exe.display().to_string())];
    parts.extend(args.iter().map(|a| shell_quote(a)));
    Some(parts.join(" "))
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '=' | ':'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Pure helper: should `handle_browser` close the active session and
/// reopen it because `config.browser.cdp_url` no longer matches the URL
/// the session was opened against?
///
/// - `(None, None)` — both sides are headless-local; keep session.
/// - `(Some(a), Some(b))` — both are CDP; reconnect iff they differ.
/// - `(Some(_), None)` / `(None, Some(_))` — mode change; reconnect.
pub fn should_reconnect(session_url: Option<&str>, config_url: Option<&str>) -> bool {
    match (session_url, config_url) {
        (None, None) => false,
        (Some(a), Some(b)) => a != b,
        _ => true,
    }
}

/// Default CDP port for borg's debug Chrome (9222, the conventional Chromium
/// DevTools port).
pub const fn default_cdp_port() -> u16 {
    BROWSER_DEFAULT_CDP_PORT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_when_empty() {
        let p = parse_cdp_url("").unwrap();
        assert_eq!(p.url, "http://127.0.0.1:9222");
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 9222);
        assert!(!p.is_browser_path);
    }

    #[test]
    fn parse_bare_host_port_gets_http_scheme() {
        let p = parse_cdp_url("127.0.0.1:9222").unwrap();
        assert_eq!(p.url, "http://127.0.0.1:9222");
        assert_eq!(p.port, 9222);
    }

    #[test]
    fn parse_browser_path_preserved() {
        let p = parse_cdp_url("ws://127.0.0.1:9222/devtools/browser/abc-123").unwrap();
        assert_eq!(p.url, "ws://127.0.0.1:9222/devtools/browser/abc-123");
        assert!(p.is_browser_path);
        assert_eq!(p.port, 9222);
    }

    #[test]
    fn parse_strips_extra_path() {
        let p = parse_cdp_url("http://localhost:9222/some/other/path").unwrap();
        assert_eq!(p.url, "http://localhost:9222");
        assert!(!p.is_browser_path);
    }

    #[test]
    fn parse_rejects_bad_scheme() {
        let e = parse_cdp_url("ftp://x:9222").unwrap_err().to_string();
        assert!(e.contains("Unsupported browser url scheme"), "got: {e}");
    }

    #[test]
    fn parse_rejects_missing_host() {
        // `http://` parses but has no host.
        let e = parse_cdp_url("http://").unwrap_err().to_string();
        assert!(
            e.to_lowercase().contains("host") || e.to_lowercase().contains("invalid"),
            "got: {e}"
        );
    }

    #[test]
    fn parse_rejects_invalid_port() {
        let e = parse_cdp_url("http://x:notaport").unwrap_err().to_string();
        assert!(
            e.to_lowercase().contains("invalid") || e.to_lowercase().contains("port"),
            "got: {e}"
        );
    }

    #[test]
    fn parse_default_port_from_scheme() {
        let p = parse_cdp_url("https://example.test").unwrap();
        assert_eq!(p.port, 443);
        let p = parse_cdp_url("http://example.test").unwrap();
        assert_eq!(p.port, 80);
        let p = parse_cdp_url("wss://example.test/devtools/browser/x").unwrap();
        assert_eq!(p.port, 443);
    }

    #[test]
    fn is_cdp_ready_returns_false_for_unbound_port() {
        // Use an ephemeral port we just closed to guarantee it's unbound.
        // Theoretically racy — the kernel could reassign the port before
        // the probe — but ephemeral-port reuse delay makes this safe in
        // practice on Linux/macOS. If this ever flakes, switch to an
        // accept-then-close listener instead of bind-and-drop.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let parsed = parse_cdp_url(&format!("http://127.0.0.1:{port}")).unwrap();
        assert!(!is_cdp_ready_blocking(&parsed, Duration::from_millis(200)));
    }

    #[test]
    fn is_cdp_ready_succeeds_against_minimal_http_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            // Serve a single /json/version response, then drop.
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = br#"{"Browser":"Chrome/121.0.0.0"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let parsed = parse_cdp_url(&format!("http://127.0.0.1:{port}")).unwrap();
        let ready = is_cdp_ready_blocking(&parsed, Duration::from_secs(1));
        let _ = handle.join();
        assert!(ready, "expected probe of mock /json/version to succeed");
    }

    #[test]
    fn should_reconnect_matrix() {
        assert!(!should_reconnect(None, None));
        assert!(!should_reconnect(
            Some("http://x:9222"),
            Some("http://x:9222")
        ));
        assert!(should_reconnect(Some("http://a"), Some("http://b")));
        assert!(should_reconnect(None, Some("http://x:9222")));
        assert!(should_reconnect(Some("http://x:9222"), None));
    }

    #[test]
    fn shell_quote_handles_spaces_and_apostrophes() {
        // Plain ASCII identifier path: pass through unchanged so commands
        // stay readable when the user copies them out of the diagnostic.
        assert_eq!(shell_quote("/usr/bin/chromium"), "/usr/bin/chromium");

        // Spaces force single-quoting; the user can paste the result into
        // their shell without word-splitting breaking the path.
        assert_eq!(
            shell_quote("/Applications/Google Chrome.app/MacOS/Chrome"),
            "'/Applications/Google Chrome.app/MacOS/Chrome'"
        );

        // Embedded apostrophes are the gotcha — POSIX single quotes can't
        // contain `'`, so we close-and-escape: `it's` → `'it'\''s'`.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
