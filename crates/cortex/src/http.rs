//! Shared outbound HTTP clients.
//!
//! Before this module every call site built its own `reqwest::Client`. That
//! had two costs. Timeouts, proxy handling and TLS could not be changed in one
//! place — and many sites set none at all, so a server that accepted the
//! connection and then went quiet wedged the caller forever. And each client
//! owns a private connection pool, so repeated calls to the same host from
//! different subsystems re-handshaked every time.
//!
//! Two named profiles cover almost everything:
//!
//! * [`shared`] — ordinary request/response JSON traffic.
//! * [`downloads`] — large transfers that legitimately take minutes.
//!
//! **A call site with genuinely different needs still builds its own**, and
//! several do: the web-fetch path installs an SSRF-guarding DNS resolver, the
//! memory providers disable redirects entirely, and the LLM transport carries
//! a cross-host-safe redirect policy. Those settings are the security boundary
//! for those paths, not a default anyone should inherit. Each such site says
//! so in a comment — do not "consolidate" them back in.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Client;

/// Identifies Borg to the servers it talks to. A request with no user-agent is
/// the first thing an API operator blocks, and a shared string means one place
/// to change when that happens.
pub const USER_AGENT: &str = concat!("borg/", env!("CARGO_PKG_VERSION"));

/// Ceiling on an ordinary request. Long enough for a slow API, short enough
/// that a silently-dropped connection surfaces inside one agent turn instead
/// of hanging it.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on establishing the connection, applied to both profiles. A
/// download may legitimately run for minutes, but *reaching* the host should
/// never take more than a moment.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a large transfer. Release archives are tens of megabytes and a
/// slow link is not an error; an hour is not.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

static SHARED: OnceLock<Client> = OnceLock::new();
static DOWNLOADS: OnceLock<Client> = OnceLock::new();

/// The client for ordinary request/response traffic.
///
/// Cloning a `reqwest::Client` shares its connection pool, so callers should
/// clone this rather than keep a reference when they need an owned value.
pub fn shared() -> &'static Client {
    SHARED.get_or_init(|| build(DEFAULT_TIMEOUT))
}

/// The client for large transfers (release archives, media).
pub fn downloads() -> &'static Client {
    DOWNLOADS.get_or_init(|| build(DOWNLOAD_TIMEOUT))
}

/// Build a profile, falling back to reqwest's own defaults if the builder
/// fails.
///
/// The fallback exists because these are process-wide singletons reached from
/// paths that cannot return an error, and a builder failure here means the TLS
/// backend did not initialise — a condition under which every request will
/// fail anyway, with a real error the caller can report. Losing the timeout is
/// strictly better than panicking during startup. Unlike the memory provider's
/// client, no security policy is lost by the fallback: these profiles set only
/// timeouts and a user-agent.
fn build(timeout: Duration) -> Client {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            tracing::warn!("shared http client builder failed ({e}); using reqwest defaults");
            Client::new()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_client_is_built_once_and_reused() {
        // The pool is the point: two callers must land on the same client, or
        // consolidating the construction buys nothing.
        assert!(
            std::ptr::eq(shared(), shared()),
            "every caller must share one connection pool"
        );
        assert!(std::ptr::eq(downloads(), downloads()));
        assert!(
            !std::ptr::eq(shared(), downloads()),
            "the profiles must stay distinct — a 30s cap would break downloads"
        );
    }

    #[test]
    fn the_download_profile_allows_far_longer_than_an_ordinary_request() {
        assert!(
            DOWNLOAD_TIMEOUT > DEFAULT_TIMEOUT,
            "a release archive on a slow link is not an error"
        );
        assert!(
            CONNECT_TIMEOUT < DEFAULT_TIMEOUT,
            "reaching a host must fail faster than a whole request"
        );
    }

    #[test]
    fn the_user_agent_names_borg_and_its_version() {
        // An API operator blocks unidentified traffic first.
        assert!(USER_AGENT.starts_with("borg/"));
        assert!(USER_AGENT.len() > "borg/".len(), "version must be present");
    }

    /// Guard: a bare `reqwest::Client::new()` has no timeout, so a server that
    /// accepts the connection and then goes quiet wedges the caller forever.
    /// The type system cannot express "construct clients only here", and the
    /// mistake reappears every time someone adds an integration — so the
    /// invariant is checked against the source, matching the precedent of the
    /// redaction-chokepoint and TUI mouse guards.
    #[test]
    fn no_cortex_call_site_builds_a_bare_client() {
        // Files that legitimately build their own, each with the security
        // setting that forces it. Adding a name here needs a real reason.
        const EXEMPT: &[&str] = &[
            // Installs an SSRF-guarding DNS resolver.
            "src/web.rs",
            // Redirects disabled: the endpoint guard validates only the first URL.
            "src/memory/provider/mod.rs",
            // Cross-host-safe redirect policy for provider credentials.
            "src/llm/mod.rs",
            "src/llm/oauth_refresh.rs",
            // Operator-supplied webhook URLs, with their own timeout policy.
            "src/hooks/webhook.rs",
            // Media uploads carry their own long transfer budgets.
            "src/tool/vision_analyze.rs",
            "src/tool/video_analyze.rs",
            // This module — it is where the clients are built.
            "src/http.rs",
        ];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        let mut stack = vec![root.join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if EXEMPT.contains(&rel.as_str()) {
                    continue;
                }
                let Ok(source) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // Skip test modules: a test that wants a bare client is
                // building a fixture, not shipping an untimed request.
                let production = source.split("#[cfg(test)]").next().unwrap_or_default();
                if production.contains("reqwest::Client::new()")
                    || production.contains("reqwest::Client::builder()")
                {
                    offenders.push(rel);
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these files build their own HTTP client; use crate::http::shared() \
             (or downloads()), or add the file to EXEMPT with the reason: {offenders:?}"
        );
    }
}
