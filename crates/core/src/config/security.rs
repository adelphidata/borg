use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolPolicyConfig {
    /// Tool profile: minimal, coding, messaging, full (default: full)
    pub profile: String,
    /// Explicit allow list (supports `group:xxx` references). Empty = all allowed.
    pub allow: Vec<String>,
    /// Explicit deny list (supports `group:xxx` references). Deny always wins.
    pub deny: Vec<String>,
    /// Tools denied to subagents.
    pub subagent_deny: Vec<String>,
}

impl Default for ToolPolicyConfig {
    fn default() -> Self {
        Self {
            profile: "full".to_string(),
            allow: Vec::new(),
            deny: Vec::new(),
            subagent_deny: vec!["schedule".to_string(), "browser".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub mode: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "strict".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub secret_detection: bool,
    /// Path component names that, when present anywhere in a canonicalized
    /// file path, cause file-read/list operations to be denied.
    pub blocked_paths: Vec<String>,
    /// Explicit allow list that overrides [`blocked_paths`]. Each entry is a
    /// tilde-expandable prefix; any file whose canonical path starts with one
    /// of these is allowed regardless of blocklist matches.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    pub host_audit: bool,
    #[serde(default)]
    pub action_limits: crate::rate_guard::ActionLimits,
    #[serde(default = "default_gateway_action_limits")]
    pub gateway_action_limits: crate::rate_guard::ActionLimits,
}

fn default_gateway_action_limits() -> crate::rate_guard::ActionLimits {
    crate::rate_guard::ActionLimits::gateway_defaults()
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            secret_detection: true,
            blocked_paths: vec![
                ".ssh".into(),
                ".aws".into(),
                ".gnupg".into(),
                ".config/gh".into(),
                ".env".into(),
                "credentials".into(),
                "private_key".into(),
                ".db_key".into(),
            ],
            allowed_paths: Vec::new(),
            host_audit: true,
            action_limits: crate::rate_guard::ActionLimits::default(),
            gateway_action_limits: crate::rate_guard::ActionLimits::gateway_defaults(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_config_defaults() {
        let cfg = SecurityConfig::default();
        assert!(cfg.secret_detection);
        assert!(cfg.host_audit);
        assert_eq!(cfg.blocked_paths.len(), 8);
        assert!(cfg.blocked_paths.contains(&".ssh".to_string()));
        assert!(cfg.blocked_paths.contains(&".aws".to_string()));
        assert!(cfg.blocked_paths.contains(&".gnupg".to_string()));
        assert!(cfg.blocked_paths.contains(&".config/gh".to_string()));
        assert!(cfg.blocked_paths.contains(&".env".to_string()));
        assert!(cfg.blocked_paths.contains(&"credentials".to_string()));
        assert!(cfg.blocked_paths.contains(&"private_key".to_string()));
        assert!(cfg.blocked_paths.contains(&".db_key".to_string()));
        assert!(cfg.allowed_paths.is_empty());
    }

    #[test]
    fn security_config_from_toml() {
        let toml_str = r#"
            secret_detection = false
            blocked_paths = [".ssh", ".env"]
            host_audit = false
        "#;
        let cfg: SecurityConfig = toml::from_str(toml_str).expect("parse");
        assert!(!cfg.secret_detection);
        assert!(!cfg.host_audit);
        assert_eq!(cfg.blocked_paths, vec![".ssh", ".env"]);
    }

    #[test]
    fn security_config_empty_blocked_paths() {
        let toml_str = r#"
            blocked_paths = []
        "#;
        let cfg: SecurityConfig = toml::from_str(toml_str).expect("parse");
        assert!(cfg.blocked_paths.is_empty());
        // other defaults still apply
        assert!(cfg.secret_detection);
    }

    #[test]
    fn security_config_allowed_paths() {
        let toml_str = r#"
            allowed_paths = ["~/work/.env", "/opt/secrets"]
        "#;
        let cfg: SecurityConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.allowed_paths.len(), 2);
    }

    #[test]
    fn tool_policy_config_defaults() {
        let cfg = ToolPolicyConfig::default();
        assert_eq!(cfg.profile, "full");
        assert!(cfg.allow.is_empty());
        assert!(cfg.deny.is_empty());
        assert_eq!(cfg.subagent_deny, vec!["schedule", "browser"]);
    }

    #[test]
    fn tool_policy_config_from_toml() {
        let toml_str = r#"
            profile = "minimal"
            allow = ["run_shell"]
            deny = ["browser"]
            subagent_deny = []
        "#;
        let cfg: ToolPolicyConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.profile, "minimal");
        assert_eq!(cfg.allow, vec!["run_shell"]);
        assert_eq!(cfg.deny, vec!["browser"]);
        assert!(cfg.subagent_deny.is_empty());
    }

    #[test]
    fn sandbox_config_defaults() {
        let cfg = SandboxConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.mode, "strict");
    }

    #[test]
    fn sandbox_config_from_toml() {
        let toml_str = r#"
            enabled = false
            mode = "permissive"
        "#;
        let cfg: SandboxConfig = toml::from_str(toml_str).expect("parse");
        assert!(!cfg.enabled);
        assert_eq!(cfg.mode, "permissive");
    }
}
