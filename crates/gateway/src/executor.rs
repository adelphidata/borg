use anyhow::{bail, Result};
use std::path::Path;

use crate::manifest::ChannelManifest;
use sandbox::runner::{run_sandboxed_script, validate_script_path};

pub struct ChannelExecutor<'a> {
    manifest: &'a ChannelManifest,
    channel_dir: &'a Path,
}

impl<'a> ChannelExecutor<'a> {
    pub fn new(manifest: &'a ChannelManifest, channel_dir: &'a Path) -> Self {
        Self {
            manifest,
            channel_dir,
        }
    }

    /// Run the verify script with {headers, body, secret} JSON on stdin.
    /// Returns Ok(true) if verified, Ok(false) if rejected.
    pub async fn verify(&self, input_json: &str, blocked_paths: &[String]) -> Result<bool> {
        let script_name = match &self.manifest.scripts.verify {
            Some(v) => v,
            None => return Ok(true), // No verify script = pass-through
        };

        let output = self
            .run_script(script_name, input_json, blocked_paths)
            .await?;
        let trimmed = output.trim();
        // Try JSON first: {"valid": true/false}
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(valid) = parsed.get("valid").and_then(serde_json::Value::as_bool) {
                return Ok(valid);
            }
        }
        // Fall back to bare string matching
        Ok(trimmed == "true" || trimmed == "ok" || trimmed == "1")
    }

    /// Run the inbound script with {headers, body} JSON on stdin.
    /// Returns the normalized message JSON from stdout.
    pub async fn parse_inbound(
        &self,
        input_json: &str,
        blocked_paths: &[String],
    ) -> Result<String> {
        self.run_script(&self.manifest.scripts.inbound, input_json, blocked_paths)
            .await
    }

    /// Run the poll script with input JSON on stdin.
    /// Returns the script's stdout (expected to be a JSON array of messages).
    pub async fn poll(&self, input_json: &str, blocked_paths: &[String]) -> Result<String> {
        let script_name = match &self.manifest.scripts.poll {
            Some(p) => p.clone(),
            None => bail!(
                "Channel '{}' has no poll script configured",
                self.manifest.name
            ),
        };
        self.run_script(&script_name, input_json, blocked_paths)
            .await
    }

    /// Run the outbound script with {text, sender_id, channel_id, token} JSON on stdin.
    /// Returns the script's stdout.
    pub async fn send_outbound(
        &self,
        input_json: &str,
        blocked_paths: &[String],
    ) -> Result<String> {
        self.run_script(&self.manifest.scripts.outbound, input_json, blocked_paths)
            .await
    }

    /// Validate that a script path stays within the channel directory.
    fn validated_script_path(&self, script_name: &str) -> Result<std::path::PathBuf> {
        validate_script_path(self.channel_dir, script_name)
    }

    async fn run_script(
        &self,
        script_name: &str,
        input_json: &str,
        blocked_paths: &[String],
    ) -> Result<String> {
        let script_path = self.validated_script_path(script_name)?;
        let sandbox_policy = self.manifest.sandbox_policy().configured(blocked_paths);

        // Collect auth env vars
        let mut extra_env = Vec::new();
        if let Some(ref secret_env) = self.manifest.auth.secret_env {
            if let Ok(val) = std::env::var(secret_env) {
                extra_env.push((secret_env.clone(), val));
            }
        }
        if let Some(ref token_env) = self.manifest.auth.token_env {
            if let Ok(val) = std::env::var(token_env) {
                extra_env.push((token_env.clone(), val));
            }
        }

        let name = format!("channel:{}/{script_name}", self.manifest.name);
        let (ok, text) = run_sandboxed_script(
            &self.manifest.runtime,
            &script_path,
            self.channel_dir,
            sandbox_policy,
            self.manifest.settings.timeout_ms,
            &extra_env,
            &name,
            input_json,
        )
        .await?;

        if !ok {
            bail!("Channel script '{script_name}': {text}");
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ChannelManifest;

    fn bash_channel_manifest(name: &str, inbound: &str) -> ChannelManifest {
        toml::from_str(&format!(
            "name = \"{name}\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"{inbound}\"\noutbound = \"out.sh\"\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn run_bash_script_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo.sh");
        std::fs::write(&script, "#!/bin/bash\ncat\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let manifest = bash_channel_manifest("test-chan", "echo.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor
            .parse_inbound(r#"{"text":"hi"}"#, &[])
            .await
            .unwrap();
        assert_eq!(result, r#"{"text":"hi"}"#);
    }

    #[tokio::test]
    async fn run_nonexistent_script_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = bash_channel_manifest("test-chan", "missing.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.parse_inbound("{}", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn verify_no_script_returns_true() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = bash_channel_manifest("test-chan", "echo.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());
        // No verify script configured — should return Ok(true)
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn verify_script_returns_json_valid() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho '{\"valid\": true}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn verify_script_returns_json_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho '{\"valid\": false}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn verify_bare_true_string() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho 'true'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn verify_bare_ok_string() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho 'ok'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn poll_without_script_errors() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = bash_channel_manifest("test-chan", "echo.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.poll("{}", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no poll script"));
    }

    #[tokio::test]
    async fn send_outbound_runs_script() {
        let dir = tempfile::tempdir().unwrap();
        let inbound_script = dir.path().join("echo.sh");
        std::fs::write(&inbound_script, "#!/bin/bash\ncat\n").unwrap();
        let outbound_script = dir.path().join("out.sh");
        std::fs::write(&outbound_script, "#!/bin/bash\necho 'sent'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&inbound_script, std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::fs::set_permissions(&outbound_script, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let manifest = bash_channel_manifest("test-chan", "echo.sh");
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor
            .send_outbound(r#"{"text":"hello"}"#, &[])
            .await
            .unwrap();
        assert_eq!(result.trim(), "sent");
    }

    #[tokio::test]
    async fn verify_malformed_json_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho '{\"not_valid_key\": 123}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(
            !result,
            "malformed JSON without 'valid' key should return false"
        );
    }

    #[tokio::test]
    async fn verify_unrecognized_string_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho 'maybe'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(!result, "'maybe' is not a recognized affirmative string");
    }

    #[tokio::test]
    async fn verify_bare_1_returns_true() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        std::fs::write(&script, "#!/bin/bash\necho '1'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest: ChannelManifest = toml::from_str(
            "name = \"test\"\ndescription = \"test\"\nruntime = \"bash\"\n\n[scripts]\ninbound = \"echo.sh\"\noutbound = \"out.sh\"\nverify = \"verify.sh\"\n"
        ).unwrap();
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.verify("{}", &[]).await.unwrap();
        assert!(result, "'1' should be recognized as truthy");
    }

    #[tokio::test]
    async fn path_traversal_in_script_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = bash_channel_manifest("test-chan", "../../../etc/passwd");
        let executor = ChannelExecutor::new(&manifest, dir.path());
        let result = executor.parse_inbound("{}", &[]).await;
        assert!(result.is_err());
    }
}
