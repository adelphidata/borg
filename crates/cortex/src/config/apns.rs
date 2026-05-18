//! APNs (Apple Push Notification Service) configuration.
//!
//! Borg pushes silent `content-available: 1` payloads to paired iOS
//! clients so they wake long enough to fetch a queued message. The
//! credentials below are Apple Developer-account scoped:
//!
//! - `team_id` — 10-char Apple Developer team identifier.
//! - `key_id`  — 10-char identifier of the .p8 push key.
//! - `bundle_id` — iOS app bundle ID (e.g. `com.borg.ios`).
//! - `private_key_p8_path` — path to the .p8 file (mode 0600). Relative
//!   paths resolve under `~/.borg/secrets/`.
//!
//! `use_sandbox = true` routes pushes to `api.sandbox.push.apple.com` for
//! development builds. Production builds use `api.push.apple.com`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// APNs push pipeline configuration. Disabled by default — the pipeline
/// requires an Apple Developer account and an iOS client app, so a fresh
/// install never tries to push anything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApnsConfig {
    /// Master kill switch. When `false`, `wake_paired_devices` is a no-op
    /// even if device tokens are registered in the DB.
    pub enabled: bool,
    /// iOS app bundle ID. Used both as the APNs `apns-topic` and as the
    /// RP ID during App Attest assertion verification.
    pub bundle_id: Option<String>,
    /// Apple Developer team ID (10-char alphanumeric).
    pub team_id: Option<String>,
    /// .p8 key ID (10-char alphanumeric, from the Apple Developer console).
    pub key_id: Option<String>,
    /// Path to the .p8 private key file. Relative paths resolve under
    /// `~/.borg/secrets/`. The file MUST be mode 0600.
    pub private_key_p8_path: Option<PathBuf>,
    /// Route pushes to the APNs sandbox endpoint. Set during development
    /// of the iOS client; production builds leave this off.
    pub use_sandbox: bool,
}

impl ApnsConfig {
    /// Returns `true` only when every field required to actually push is
    /// populated. The trigger path consults this before reaching for the
    /// a2 client; missing values short-circuit with a single warn-once
    /// log line rather than a per-push error.
    pub fn is_complete(&self) -> bool {
        self.enabled
            && self.bundle_id.is_some()
            && self.team_id.is_some()
            && self.key_id.is_some()
            && self.private_key_p8_path.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_disable_apns_entirely() {
        let cfg = ApnsConfig::default();
        assert!(
            !cfg.enabled,
            "APNs must default off; requires Apple account"
        );
        assert!(cfg.bundle_id.is_none());
        assert!(cfg.team_id.is_none());
        assert!(cfg.key_id.is_none());
        assert!(cfg.private_key_p8_path.is_none());
        assert!(!cfg.use_sandbox);
    }

    #[test]
    fn is_complete_false_when_any_required_field_missing() {
        // Each required field individually gates is_complete(). The trigger
        // path uses this as a single boolean check; if it ever drifts to a
        // partial check (e.g. forgetting key_id) we'd start hitting APNs
        // with malformed JWTs and 403s would fall through the cracks.
        let mut cfg = ApnsConfig {
            enabled: true,
            bundle_id: Some("com.borg.ios".into()),
            team_id: Some("ABCDE12345".into()),
            key_id: Some("ABC123DEF4".into()),
            private_key_p8_path: Some(PathBuf::from("apns.p8")),
            use_sandbox: false,
        };
        assert!(cfg.is_complete(), "all fields set should yield complete");

        type Mutator = Box<dyn Fn(&mut ApnsConfig)>;
        let mutators: Vec<Mutator> = vec![
            Box::new(|c| c.enabled = false),
            Box::new(|c| c.bundle_id = None),
            Box::new(|c| c.team_id = None),
            Box::new(|c| c.key_id = None),
            Box::new(|c| c.private_key_p8_path = None),
        ];
        for (i, mutator) in mutators.iter().enumerate() {
            let mut probe = cfg.clone();
            mutator(&mut probe);
            assert!(
                !probe.is_complete(),
                "mutator #{i} should have made config incomplete"
            );
        }
        // Sentinel: use_sandbox flipping must NOT affect completeness.
        cfg.use_sandbox = true;
        assert!(cfg.is_complete(), "use_sandbox is not a gating field");
    }

    #[test]
    fn serde_roundtrip_preserves_pathbuf() {
        let original = ApnsConfig {
            enabled: true,
            bundle_id: Some("com.borg.ios".into()),
            team_id: Some("ABCDE12345".into()),
            key_id: Some("ABC123DEF4".into()),
            private_key_p8_path: Some(PathBuf::from("/abs/path/apns.p8")),
            use_sandbox: true,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ApnsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.private_key_p8_path, original.private_key_p8_path);
        assert_eq!(parsed.bundle_id, original.bundle_id);
        assert!(parsed.use_sandbox);
    }
}
