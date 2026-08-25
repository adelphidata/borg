//! No-op tunnel used when the operator hasn't selected a provider.
//!
//! Returned by `create_tunnel` only when the caller explicitly opts in
//! via an `Option<NoneTunnel>` shape; the normal `Ok(None)` path in
//! [`super::create_tunnel`] is the typical no-tunnel signal.

use anyhow::Result;

use super::Tunnel;

/// Sentinel tunnel that never starts and reports no URL.
pub struct NoneTunnel;

#[async_trait::async_trait]
impl Tunnel for NoneTunnel {
    fn name(&self) -> &str {
        "none"
    }

    async fn start(&self, _local_host: &str, _local_port: u16) -> Result<String> {
        // Explicit error rather than returning an empty string — calling
        // start() on the none tunnel is always a programmer mistake; we
        // want a loud failure, not a silent no-op the caller has to
        // remember to special-case.
        anyhow::bail!("NoneTunnel::start called — selected provider is `none`")
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> bool {
        false
    }

    fn public_url(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_returns_actionable_error_so_callers_cant_silently_misuse() {
        let err = NoneTunnel.start("127.0.0.1", 1).await.unwrap_err();
        assert!(format!("{err}").contains("none"));
    }

    #[test]
    fn name_is_the_stable_provider_identifier() {
        // `name()` feeds supervisor logs and `borg tunnel status`; the
        // string is the provider's stable identifier, matching the
        // `tunnel.provider = none` config value operators set.
        assert_eq!(NoneTunnel.name(), "none");
    }

    #[tokio::test]
    async fn stop_and_health_check_match_no_op_semantics() {
        let t = NoneTunnel;
        assert!(t.stop().await.is_ok());
        assert!(!t.health_check().await);
        assert!(t.public_url().is_none());
    }
}
