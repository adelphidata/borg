//! Pure posture resolver. Read-only over [`super::PostureSnapshot`]; called
//! from both write-time emission and replay so the rules stay deterministic.
//!
//! State-resolution rule: `Reduced` wins on any single deliberate
//! downgrade; `Boosted` requires the full positive set; otherwise `Default`.

use super::{AuthBackend, ListenerBindKind, PostureSnapshot, PostureState, SecretStorageBackend};

/// Resolve to `Reduced` / `Default` / `Boosted`.
pub fn resolve_state(p: &PostureSnapshot) -> PostureState {
    if is_reduced(p) {
        return PostureState::Reduced;
    }
    if is_boosted(p) {
        return PostureState::Boosted;
    }
    PostureState::Default
}

/// `Reduced` triggers: ANY one deliberate downgrade. Each branch is a
/// security knob the user must explicitly flip. None happens by accident.
pub fn is_reduced(p: &PostureSnapshot) -> bool {
    !p.global.sandbox_enabled
        || !p.global.privileged_channels.is_empty()
        || !p.global.blocked_paths_strict
        || !p.global.catastrophic_denylist_intact
        // File-encrypted KEK is the headless-VPS fallback. Posture-wise
        // it's strictly weaker than the OS keychain (the passphrase has
        // to live on disk somewhere), so we register it as a Reduced
        // trigger. Memory backend is treated as Reduced too — it implies
        // a never-persisted install which is a non-production shape.
        || matches!(
            p.global.secret_storage_backend,
            SecretStorageBackend::FileEncrypted | SecretStorageBackend::Memory
        )
        // PublicTofu listener with `auth_backend = None` = anyone can connect.
        || p.listeners.iter().any(|l| {
            matches!(l.bind, ListenerBindKind::PublicTofu) && l.auth_backend == AuthBackend::None
        })
}

/// `Boosted` triggers: every positive signal that exists in v1 is on, AND
/// the daemon is actually reachable from a remote (mTLS or Tailscale).
/// The remote-reachability requirement is what prevents a fresh local-only
/// install from claiming Boosted simply for being well-defaulted.
pub fn is_boosted(p: &PostureSnapshot) -> bool {
    p.global.sandbox_enabled
        && p.global.audit_chain_enabled
        && p.global.blocked_paths_strict
        && p.global.privileged_channels.is_empty()
        && p.global.catastrophic_denylist_intact
        // Every non-loopback listener has real auth (no naked PublicTofu).
        && p.listeners.iter().all(|l| {
            matches!(l.bind, ListenerBindKind::Loopback)
                || l.auth_backend != AuthBackend::None
        })
        // …and at least one mTLS or Tailscale listener exists (i.e., the
        // daemon does more than just talk to localhost).
        && p.listeners.iter().any(|l| {
            matches!(l.auth_backend, AuthBackend::Tofu | AuthBackend::Tailscale)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::security::{
        GlobalSignals, ListenerPosture, PostureSnapshot, PostureSource, SecretStorageBackend,
    };

    /// Default-fresh install — Default state, by construction. Anchor for
    /// the table tests below: every other case starts from this and flips
    /// exactly the field under test.
    fn fresh_install_snapshot() -> PostureSnapshot {
        PostureSnapshot {
            schema_version: 1,
            state: PostureState::Default,
            global: GlobalSignals {
                sandbox_enabled: true,
                blocked_paths_strict: true,
                privileged_channels: vec![],
                audit_chain_enabled: true,
                catastrophic_denylist_intact: true,
                secret_storage_backend: SecretStorageBackend::OsKeychain,
            },
            listeners: vec![ListenerPosture {
                name: "loopback".into(),
                bind: ListenerBindKind::Loopback,
                auth_backend: AuthBackend::None,
            }],
            computed_at: 1_700_000_000,
            computed_from: PostureSource::DaemonStartReconcile,
        }
    }

    #[test]
    fn fresh_install_resolves_to_default() {
        let s = fresh_install_snapshot();
        assert_eq!(resolve_state(&s), PostureState::Default);
        assert!(!is_reduced(&s));
        assert!(!is_boosted(&s));
    }

    /// Each row flips exactly one Reduced trigger from the fresh-install
    /// baseline and asserts the resolver catches it. Failure mode:
    /// removing a `||` branch in `is_reduced` silently demotes the
    /// install's safety surface — this table fails per missing branch.
    #[test]
    fn each_reduced_disqualifier_triggers_reduced() {
        let cases: Vec<(&'static str, fn(&mut PostureSnapshot))> = vec![
            ("sandbox_off", |s| s.global.sandbox_enabled = false),
            ("blocked_paths_loose", |s| {
                s.global.blocked_paths_strict = false
            }),
            ("denylist_tampered", |s| {
                s.global.catastrophic_denylist_intact = false
            }),
            ("privileged_channel", |s| {
                s.global.privileged_channels.push("telegram".into())
            }),
            ("public_tofu_no_auth", |s| {
                s.listeners.push(ListenerPosture {
                    name: "public".into(),
                    bind: ListenerBindKind::PublicTofu,
                    auth_backend: AuthBackend::None,
                })
            }),
        ];
        for (label, mutate) in cases {
            let mut s = fresh_install_snapshot();
            mutate(&mut s);
            assert_eq!(
                resolve_state(&s),
                PostureState::Reduced,
                "{label}: expected Reduced"
            );
        }
    }

    /// Boosted requires *both* full-positive globals AND a non-loopback
    /// listener. A pristine install with only loopback stays Default —
    /// regression target: an earlier draft accidentally bucketed this as
    /// Boosted because every global was true.
    #[test]
    fn boosted_requires_remote_listener_not_just_pristine_globals() {
        let s = fresh_install_snapshot();
        assert_eq!(resolve_state(&s), PostureState::Default);
    }

    /// Add a Tailscale listener to the pristine install → Boosted.
    #[test]
    fn tailnet_listener_promotes_to_boosted() {
        let mut s = fresh_install_snapshot();
        s.listeners.push(ListenerPosture {
            name: "tailnet".into(),
            bind: ListenerBindKind::Tailnet,
            auth_backend: AuthBackend::Tailscale,
        });
        assert!(is_boosted(&s));
        assert_eq!(resolve_state(&s), PostureState::Boosted);
    }

    /// mTLS-Tofu remote listener also promotes (`PublicTofu + Tofu` is the
    /// canonical "paired phone over public IP" deployment).
    #[test]
    fn mtls_tofu_listener_promotes_to_boosted() {
        let mut s = fresh_install_snapshot();
        s.listeners.push(ListenerPosture {
            name: "public".into(),
            bind: ListenerBindKind::PublicTofu,
            auth_backend: AuthBackend::Tofu,
        });
        assert_eq!(resolve_state(&s), PostureState::Boosted);
    }

    /// Reduced wins over Boosted on any conflict. Test setup: a Boosted-
    /// shaped install plus exactly one Reduced trigger → Reduced.
    /// Failure mode: re-ordering `is_boosted`/`is_reduced` in
    /// `resolve_state` would let Boosted suppress a real downgrade.
    #[test]
    fn reduced_wins_on_conflict_with_boosted_signals() {
        let mut s = fresh_install_snapshot();
        // Make it Boosted-shaped:
        s.listeners.push(ListenerPosture {
            name: "tailnet".into(),
            bind: ListenerBindKind::Tailnet,
            auth_backend: AuthBackend::Tailscale,
        });
        // Now flip one Reduced trigger:
        s.global.sandbox_enabled = false;

        assert!(is_reduced(&s));
        assert!(!is_boosted(&s));
        assert_eq!(resolve_state(&s), PostureState::Reduced);
    }

    /// A Channel-bind listener with no auth is *not* a Reduced trigger
    /// (channels carry their own webhook-signature auth — modeled as
    /// `AuthBackend::None` here is acceptable because the trigger is
    /// specifically `PublicTofu + None`).
    #[test]
    fn channel_listener_with_no_auth_does_not_trigger_reduced() {
        let mut s = fresh_install_snapshot();
        s.listeners.push(ListenerPosture {
            name: "telegram".into(),
            bind: ListenerBindKind::Channel,
            auth_backend: AuthBackend::None,
        });
        assert!(!is_reduced(&s));
        // Without a Tofu/Tailscale listener it stays Default, not Boosted.
        assert_eq!(resolve_state(&s), PostureState::Default);
    }

    /// `FileEncrypted` KEK forces Reduced even when every other signal is
    /// pristine. This is the headless-VPS path: a `borg service install`
    /// without `--secure` lands here. The posture multiplier (×0.85) is
    /// the ongoing cost; the milestone bonus on transition to OsKeychain
    /// is the one-time reward. Drift here would silently grant Default to
    /// installs that have no OS-level secret protection.
    #[test]
    fn file_encrypted_kek_pushes_to_reduced() {
        let mut s = fresh_install_snapshot();
        s.global.secret_storage_backend = SecretStorageBackend::FileEncrypted;
        assert!(is_reduced(&s));
        assert!(!is_boosted(&s));
        assert_eq!(resolve_state(&s), PostureState::Reduced);
    }

    /// Memory-only KEK is a non-production shape (test fixtures, ad-hoc
    /// CI). Treated as Reduced so any accidental ship with the in-memory
    /// keystore loudly drops the multiplier.
    #[test]
    fn memory_kek_is_reduced() {
        let mut s = fresh_install_snapshot();
        s.global.secret_storage_backend = SecretStorageBackend::Memory;
        assert_eq!(resolve_state(&s), PostureState::Reduced);
    }

    /// `OsKeychain` + Tailnet listener on otherwise-pristine install →
    /// Boosted. This is the "rewarded" headless path users reach via
    /// `borg service install --secure`. If a future field on
    /// `GlobalSignals` accidentally trips Reduced for OsKeychain installs,
    /// this regression fires.
    #[test]
    fn os_keychain_with_mtls_listener_resolves_boosted() {
        let mut s = fresh_install_snapshot();
        s.global.secret_storage_backend = SecretStorageBackend::OsKeychain;
        s.listeners.push(ListenerPosture {
            name: "tailnet".into(),
            bind: ListenerBindKind::Tailnet,
            auth_backend: AuthBackend::Tailscale,
        });
        assert!(!is_reduced(&s));
        assert!(is_boosted(&s));
        assert_eq!(resolve_state(&s), PostureState::Boosted);
    }

    /// Defense-in-depth: a v1 chain that pre-dates this field deserializes
    /// with `OsKeychain` (the `Default`). New code reading old snapshots
    /// must NOT flip those installs to Reduced retroactively.
    #[test]
    fn pre_field_snapshot_deserializes_as_os_keychain() {
        // The literal historical shape — no `secret_storage_backend` field.
        let json = r#"{
            "schema_version": 1,
            "state": "Default",
            "global": {
                "sandbox_enabled": true,
                "blocked_paths_strict": true,
                "privileged_channels": [],
                "audit_chain_enabled": true,
                "catastrophic_denylist_intact": true
            },
            "listeners": [],
            "computed_at": 1700000000,
            "computed_from": "DaemonStartReconcile"
        }"#;
        let parsed: PostureSnapshot = serde_json::from_str(json).expect("legacy snapshot parses");
        assert_eq!(
            parsed.global.secret_storage_backend,
            SecretStorageBackend::OsKeychain
        );
        assert!(!is_reduced(&parsed));
    }

    /// audit_chain_enabled=false is *not* a Reduced trigger in v1 (audit
    /// chain is on-by-default, can't be turned off via supported config),
    /// but it *does* block Boosted. This guards the asymmetry between
    /// `is_reduced` and `is_boosted` — easy to mistakenly add it to both.
    #[test]
    fn audit_chain_disabled_blocks_boosted_but_does_not_trigger_reduced() {
        let mut s = fresh_install_snapshot();
        s.listeners.push(ListenerPosture {
            name: "tailnet".into(),
            bind: ListenerBindKind::Tailnet,
            auth_backend: AuthBackend::Tailscale,
        });
        s.global.audit_chain_enabled = false;
        assert!(!is_reduced(&s));
        assert!(!is_boosted(&s));
        assert_eq!(resolve_state(&s), PostureState::Default);
    }
}
