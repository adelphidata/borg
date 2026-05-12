use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::Client;
use tracing::warn;

/// Build a `reqwest::Client` for a native channel with optional connect/request
/// timeouts. Centralizes the construction so error context strings stay
/// uniform ("Failed to build {channel} HTTP client").
pub fn build_channel_client(
    channel: &str,
    timeout: Duration,
    connect_timeout: Option<Duration>,
) -> Result<Client> {
    let mut builder = Client::builder().timeout(timeout);
    if let Some(ct) = connect_timeout {
        builder = builder.connect_timeout(ct);
    }
    builder
        .build()
        .with_context(|| format!("Failed to build {channel} HTTP client"))
}

/// Configuration for HTTP 429 rate-limit retry behavior.
pub struct RateLimitPolicy {
    pub max_retries: u32,
    pub max_retry_after_secs: u64,
    pub service_name: &'static str,
}

impl Default for RateLimitPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            max_retry_after_secs: 300,
            service_name: "API",
        }
    }
}

impl RateLimitPolicy {
    /// Default policy named after a specific service. Replaces the
    /// `RateLimitPolicy { service_name: "Foo", ..Default::default() }`
    /// struct-update boilerplate that channel API clients otherwise repeat.
    pub fn for_service(service_name: &'static str) -> Self {
        Self {
            service_name,
            ..Self::default()
        }
    }
}

/// Send an HTTP request with automatic 429 rate-limit retry.
///
/// `make_request` is called on each attempt and must return a fresh `reqwest::Response`.
/// On 429, parses the `Retry-After` header, sleeps, and retries up to `policy.max_retries`.
/// Returns the first non-429 response, or errors after exhausting retries.
pub async fn send_with_rate_limit_retry<F, Fut>(
    policy: &RateLimitPolicy,
    mut make_request: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response>>,
{
    let mut attempts = 0u32;

    loop {
        let resp = make_request().await?;

        if resp.status().as_u16() != 429 {
            return Ok(resp);
        }

        attempts += 1;
        if attempts > policy.max_retries {
            bail!(
                "{} rate limited after {} retries",
                policy.service_name,
                policy.max_retries
            );
        }

        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let capped = retry_after.min(policy.max_retry_after_secs);
        warn!(
            "{} rate limited, retry after {capped}s (attempt {attempts}/{})",
            policy.service_name, policy.max_retries
        );
        tokio::time::sleep(Duration::from_secs(capped)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_response(status: u16) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(status)
                .body("")
                .unwrap(),
        )
    }

    #[test]
    fn default_policy() {
        let policy = RateLimitPolicy::default();
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.max_retry_after_secs, 300);
        assert_eq!(policy.service_name, "API");
    }

    #[tokio::test]
    async fn non_429_returns_immediately() {
        let policy = RateLimitPolicy {
            max_retries: 3,
            max_retry_after_secs: 60,
            service_name: "Test",
        };
        let resp = send_with_rate_limit_retry(&policy, || async { Ok(mock_response(200)) })
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn returns_non_429_error_status() {
        let policy = RateLimitPolicy::default();
        let resp = send_with_rate_limit_retry(&policy, || async { Ok(mock_response(500)) })
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 500);
    }

    #[tokio::test]
    async fn exhausts_retries_on_429() {
        let policy = RateLimitPolicy {
            max_retries: 0,
            max_retry_after_secs: 1,
            service_name: "Test",
        };
        let result = send_with_rate_limit_retry(&policy, || async { Ok(mock_response(429)) }).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rate limited"));
    }

    #[tokio::test]
    async fn request_error_propagated() {
        let policy = RateLimitPolicy::default();
        let result = send_with_rate_limit_retry(&policy, || async {
            Err(anyhow::anyhow!("connection refused"))
        })
        .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("connection refused"));
    }
}
