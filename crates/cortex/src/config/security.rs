use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Verifier strategy selected for the daemon's mTLS auth interceptor.
///
/// `Tofu` (default) trusts the first connection from a fingerprint that's
/// already been paired and locks future connections to the same key. `Tailscale`
/// additionally cross-checks the peer IP against `tailscale status` and refuses
/// principals whose IP doesn't appear as a peer node — opt-in via the
/// `security.verifier` setting.
///
/// External-CA verifiers (SCEP, EST, ACME, …) are intentionally not
/// supported; do not extend this enum for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VerifierKind {
    #[default]
    Tofu,
    Tailscale,
}

impl FromStr for VerifierKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "tofu" => Ok(VerifierKind::Tofu),
            "tailscale" => Ok(VerifierKind::Tailscale),
            other => anyhow::bail!("Unknown verifier '{other}'. Valid: tofu, tailscale"),
        }
    }
}

impl std::fmt::Display for VerifierKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VerifierKind::Tofu => "tofu",
            VerifierKind::Tailscale => "tailscale",
        })
    }
}

/// Backend storing the KEK (root CA + server cert keys at rest).
///
/// `Auto` is the install-time default — borgd probes the OS keychain on
/// first start and falls back to `File` (passphrase-encrypted) when the
/// keychain is unavailable (e.g. headless Linux without dbus). The
/// resolved choice is persisted as `OsKeychain` or `File` so subsequent
/// starts are deterministic and the posture system gets a stable signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KekBackend {
    /// Probe `OsKeychain`, fall back to `File` on failure. Install default.
    #[default]
    Auto,
    /// OS keychain via the `keyring` crate (Apple, Windows, secret-service).
    /// Posture: contributes to `Default` / `Boosted`.
    OsKeychain,
    /// Passphrase-encrypted file at `~/.borg/secrets/kek.enc`. Posture:
    /// forces `Reduced`. Always-works fallback for headless servers.
    File,
}

impl FromStr for KekBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(KekBackend::Auto),
            "os_keychain" | "os-keychain" | "keychain" => Ok(KekBackend::OsKeychain),
            "file" => Ok(KekBackend::File),
            other => anyhow::bail!("Unknown kek_backend '{other}'. Valid: auto, os_keychain, file"),
        }
    }
}

impl std::fmt::Display for KekBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            KekBackend::Auto => "auto",
            KekBackend::OsKeychain => "os_keychain",
            KekBackend::File => "file",
        })
    }
}

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

/// Per-site URL policy applied pre-flight to every `web_fetch` and
/// `browser.navigate` URL. Composes scheme gating, SSRF defense
/// (cloud-metadata + private-IP blocking), allowlist/denylist, transforms
/// (strip query params, rewrite host, force https, strip fragment), and
/// per-host throttling. Detailed semantics in [`crate::url_policy`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UrlPolicyConfig {
    /// Master toggle. When false, classification is bypassed entirely —
    /// ops escape hatch only; cloud-metadata IPs are no longer protected.
    pub enabled: bool,
    /// When true, private/loopback/link-local/CGNAT IPs may be fetched.
    /// Cloud-metadata endpoints (169.254.169.254, metadata.google.internal,
    /// etc.) remain blocked regardless — they are never legitimate targets.
    pub allow_private_urls: bool,
    /// When non-empty, only hosts matching one of these patterns pass.
    /// Patterns support exact match, subdomain suffix, and `*.foo.com`
    /// wildcards (see [`crate::url_policy::rules`]).
    pub allowlist: Vec<String>,
    /// Hosts matching any pattern here are blocked. Same matcher syntax
    /// as `allowlist`. Denylist is checked after allowlist.
    pub denylist: Vec<String>,
    /// Per-host minimum interval between requests.
    pub throttle_rules: Vec<ThrottleRule>,
    /// Pre-flight URL rewrites applied in declared order.
    pub transform_rules: Vec<TransformRule>,
}

impl Default for UrlPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_private_urls: false,
            allowlist: Vec::new(),
            denylist: Vec::new(),
            throttle_rules: Vec::new(),
            transform_rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleRule {
    /// Domain pattern (e.g. `api.example.com`, `*.example.com`).
    pub pattern: String,
    /// Minimum milliseconds between consecutive requests to a matching host.
    pub min_interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransformRule {
    pub pattern: String,
    pub action: TransformAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TransformAction {
    /// Remove named query parameters (exact names, case-sensitive).
    StripQueryParams { names: Vec<String> },
    /// Replace the URL's host with `to`. Path/query preserved.
    RewriteHost { to: String },
    /// Upgrade `http://` to `https://`.
    ForceHttps,
    /// Remove the URL fragment (`#section`).
    StripFragment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub secret_detection: bool,
    /// Path component names that, when present anywhere in a canonicalized
    /// file path, cause file-read/list operations to be denied.
    pub blocked_paths: Vec<String>,
    /// Explicit allow list that overrides `blocked_paths`. Each entry is a
    /// tilde-expandable prefix; any file whose canonical path starts with one
    /// of these is allowed regardless of blocklist matches.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    pub host_audit: bool,
    /// Strip detected secrets from agent-generated output before it reaches
    /// channels, the TUI transcript, or logs. Pairs with input-side detection
    /// (`secret_detection`); this is the output-side counterpart. Default true
    /// (security-first). Operators can disable per-deploy via
    /// `security.output_redaction`.
    #[serde(default = "default_output_redaction")]
    pub output_redaction: bool,
    #[serde(default)]
    pub action_limits: crate::rate_guard::ActionLimits,
    #[serde(default = "default_gateway_action_limits")]
    pub gateway_action_limits: crate::rate_guard::ActionLimits,
    /// mTLS peer-cert verifier strategy. `Tofu` is the default; `Tailscale`
    /// is opt-in and binds pairs to a Tailscale node identity by
    /// cross-checking the peer IP.
    #[serde(default)]
    pub verifier: VerifierKind,
    /// KEK storage backend. `Auto` probes the OS keychain at first start
    /// and persists the resolved choice (`OsKeychain` or `File`).
    #[serde(default)]
    pub kek_backend: KekBackend,
    /// Pre-flight gate for outbound URLs (`web_fetch`, `browser.navigate`).
    #[serde(default)]
    pub url_policy: UrlPolicyConfig,
    /// Base64-encoded Ed25519 public keys trusted to sign WASM plugin
    /// manifests. `borg plugin install` rejects any package whose publisher
    /// key is not in this list. Empty by default — operators must opt in.
    #[serde(default)]
    pub trusted_publisher_keys: Vec<String>,
}

fn default_gateway_action_limits() -> crate::rate_guard::ActionLimits {
    crate::rate_guard::ActionLimits::gateway_defaults()
}

fn default_output_redaction() -> bool {
    true
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
            ],
            allowed_paths: Vec::new(),
            host_audit: true,
            output_redaction: true,
            action_limits: crate::rate_guard::ActionLimits::default(),
            gateway_action_limits: crate::rate_guard::ActionLimits::gateway_defaults(),
            verifier: VerifierKind::Tofu,
            kek_backend: KekBackend::Auto,
            url_policy: UrlPolicyConfig::default(),
            trusted_publisher_keys: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_kind_round_trip_through_str() {
        // Real failure mode: a typo in `FromStr` (e.g. accepting "Tailscale"
        // case-sensitively) silently leaves the daemon on TOFU when the user
        // explicitly picked Tailscale. Round-trip every variant against the
        // canonical lowercase form and a mixed-case form.
        for (kind, canonical, mixed) in [
            (VerifierKind::Tofu, "tofu", "ToFu"),
            (VerifierKind::Tailscale, "tailscale", "TailScale"),
        ] {
            assert_eq!(kind.to_string(), canonical);
            assert_eq!(VerifierKind::from_str(canonical).unwrap(), kind);
            assert_eq!(VerifierKind::from_str(mixed).unwrap(), kind);
        }
        assert!(VerifierKind::from_str("nope").is_err());
    }

    #[test]
    fn verifier_kind_serde_round_trip() {
        // Persisted JSON must round-trip — tests pair this with the
        // settings_table apply path so a saved value comes back identical.
        for kind in [VerifierKind::Tofu, VerifierKind::Tailscale] {
            let s = serde_json::to_string(&kind).unwrap();
            let back: VerifierKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn kek_backend_round_trip_through_str() {
        for (kind, canonical, mixed, alt) in [
            (KekBackend::Auto, "auto", "Auto", None),
            (
                KekBackend::OsKeychain,
                "os_keychain",
                "OS_KEYCHAIN",
                Some("os-keychain"),
            ),
            (KekBackend::File, "file", "File", None),
        ] {
            assert_eq!(kind.to_string(), canonical);
            assert_eq!(KekBackend::from_str(canonical).unwrap(), kind);
            assert_eq!(KekBackend::from_str(mixed).unwrap(), kind);
            if let Some(alt) = alt {
                // os_keychain accepts os-keychain and `keychain` as aliases —
                // necessary because shell/users will reach for either form.
                assert_eq!(KekBackend::from_str(alt).unwrap(), kind);
            }
        }
        assert_eq!(
            KekBackend::from_str("keychain").unwrap(),
            KekBackend::OsKeychain
        );
        assert!(KekBackend::from_str("nope").is_err());
    }

    #[test]
    fn kek_backend_default_is_auto() {
        // Install-time default. Must stay `Auto` so that fresh deploys probe
        // the keychain rather than locking themselves into `File` and
        // permanently posting `Reduced`.
        assert_eq!(KekBackend::default(), KekBackend::Auto);
    }

    #[test]
    fn security_config_defaults() {
        let cfg = SecurityConfig::default();
        assert!(cfg.secret_detection);
        assert!(cfg.host_audit);
        // Security-first default invariant. If a future change silently flips
        // this to false, fresh deploys would stop stripping secrets from
        // outgoing channel messages / TUI / logs without operator action.
        assert!(
            cfg.output_redaction,
            "output_redaction must default to true (security-first)"
        );
        assert_eq!(cfg.verifier, VerifierKind::Tofu);
        assert_eq!(cfg.kek_backend, KekBackend::Auto);
        assert_eq!(cfg.blocked_paths.len(), 7);
        assert!(cfg.blocked_paths.contains(&".ssh".to_string()));
        assert!(cfg.blocked_paths.contains(&".aws".to_string()));
        assert!(cfg.blocked_paths.contains(&".gnupg".to_string()));
        assert!(cfg.blocked_paths.contains(&".config/gh".to_string()));
        assert!(cfg.blocked_paths.contains(&".env".to_string()));
        assert!(cfg.blocked_paths.contains(&"credentials".to_string()));
        assert!(cfg.blocked_paths.contains(&"private_key".to_string()));
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
    fn security_config_output_redaction_toml_round_trip() {
        let toml_str = r#"output_redaction = false"#;
        let cfg: SecurityConfig = toml::from_str(toml_str).expect("parse");
        assert!(!cfg.output_redaction);
        // Unset field should still respect security-first default.
        let cfg_default: SecurityConfig = toml::from_str("").expect("parse");
        assert!(cfg_default.output_redaction);
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
