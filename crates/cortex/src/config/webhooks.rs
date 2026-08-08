//! Outbound signed lifecycle webhooks.
//!
//! Endpoints live in the DB-backed settings table under `webhooks.endpoints`
//! (a JSON array), consistent with other structured settings like
//! `llm.fallback`. Each endpoint subscribes to lifecycle events by their
//! hooks.json-compatible names (`SessionStart`, `SessionEnd`,
//! `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`); an empty list
//! subscribes to all of them.
//!
//! URL policy mirrors the MCP client's HTTPS-only stance: `https://` is
//! required unless the host is loopback (`localhost`, `127.0.0.0/8`, `::1`).
//! Violations are rejected at deserialization time so a bad endpoint fails
//! `borg settings set` with a clear error instead of silently posting secrets
//! over cleartext.

use serde::{Deserialize, Serialize};

/// Configuration for outbound lifecycle webhooks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhooksConfig {
    /// HTTP endpoints that receive signed lifecycle event deliveries.
    pub endpoints: Vec<WebhookEndpoint>,
}

/// A single outbound webhook endpoint.
///
/// Validated on deserialization via [`RawWebhookEndpoint`]: the URL must be
/// `https://`, or `http://` to a loopback host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawWebhookEndpoint")]
pub struct WebhookEndpoint {
    /// Target URL for POST deliveries.
    pub url: String,
    /// HMAC-SHA256 signing secret. When absent, deliveries are sent unsigned
    /// (a startup warning is emitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Subscribed event names (hooks.json-compatible). Empty = all events.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

/// Unvalidated wire form of [`WebhookEndpoint`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWebhookEndpoint {
    url: String,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    events: Vec<String>,
}

impl TryFrom<RawWebhookEndpoint> for WebhookEndpoint {
    type Error = String;

    fn try_from(raw: RawWebhookEndpoint) -> Result<Self, Self::Error> {
        validate_webhook_url(&raw.url)?;
        Ok(WebhookEndpoint {
            url: raw.url,
            secret: raw.secret,
            events: raw.events,
        })
    }
}

/// Enforce the HTTPS-only-unless-loopback policy for a webhook URL.
pub fn validate_webhook_url(raw: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw.trim())
        .map_err(|e| format!("invalid webhook endpoint URL {raw:?}: {e}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = match parsed.host() {
                Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if loopback {
                Ok(())
            } else {
                Err(format!(
                    "webhook endpoint {raw:?} must use https:// — \
                     http:// is only allowed for localhost/loopback hosts"
                ))
            }
        }
        other => Err(format!(
            "webhook endpoint {raw:?} has unsupported scheme {other:?} (expected https)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn http_non_localhost_endpoint_refused_at_config_load() {
        // Cleartext posts to a remote host would leak signed payloads (which
        // can contain tool metadata) over the network. Must fail loudly at
        // `borg settings set` time, not degrade silently.
        let mut config = Config::default();
        let err = config
            .apply_setting(
                "webhooks.endpoints",
                r#"[{"url":"http://example.com/hook","secret":"s"}]"#,
            )
            .expect_err("http:// to a non-loopback host must be refused");
        assert!(
            err.to_string().contains("https"),
            "error must explain the https requirement, got: {err}"
        );
        assert!(
            config.webhooks.endpoints.is_empty(),
            "refused endpoint must not be applied"
        );
    }

    #[test]
    fn loopback_http_and_remote_https_endpoints_accepted() {
        let mut config = Config::default();
        let msg = config
            .apply_setting(
                "webhooks.endpoints",
                r#"[
                    {"url":"http://localhost:8080/hook","secret":"a"},
                    {"url":"http://127.0.0.1/hook"},
                    {"url":"https://hooks.example.com/borg","events":["SessionEnd","Stop"]}
                ]"#,
            )
            .expect("loopback http and remote https must both load");
        assert!(msg.contains("3 endpoints"), "got: {msg}");
        assert_eq!(config.webhooks.endpoints.len(), 3);
        assert_eq!(
            config.webhooks.endpoints[2].events,
            vec!["SessionEnd".to_string(), "Stop".to_string()]
        );
        assert!(config.webhooks.endpoints[1].secret.is_none());
    }

    #[test]
    fn non_http_scheme_refused() {
        let err = serde_json::from_str::<WebhookEndpoint>(r#"{"url":"ftp://example.com/x"}"#)
            .expect_err("non-http(s) scheme must be refused");
        assert!(err.to_string().contains("unsupported scheme"), "{err}");
    }
}
