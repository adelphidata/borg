//! Compute a live [`PostureSnapshot`] from the current `Config` plus DB
//! state. Pure read-only — used by both daemon-startup reconciliation
//! and `SettingsResolver` change hooks (when those land).
//!
//! The mapping from `Config` → `GlobalSignals`:
//! - `sandbox_enabled` ← `config.sandbox.enabled`.
//! - `blocked_paths_strict` ← `config.security.blocked_paths` is a
//!   superset of the seven defaults defined in `SecurityConfig::default`.
//! - `privileged_channels` ← any gateway channel binding flagged with
//!   `allow_privileged = true`. Empty by default — the gateway config
//!   doesn't expose this knob yet, so the list is always empty until
//!   the channel-binding flag ships.
//! - `audit_chain_enabled` ← always `true` in v1 (audit chain is
//!   unconditional after migration V46; cannot be disabled via config).
//! - `catastrophic_denylist_intact` ← always `true` in v1 (the sandbox
//!   denylist is compiled into `borg-sandbox`; user can't tamper without
//!   rebuilding the binary).
//!
//! Listener inventory comes from `paired_devices`: each active row
//! contributes a `ListenerPosture { bind: PublicTofu, auth: Tofu }`. If
//! none are active, the only listener is the synthetic `loopback` (UDS).
//! This is the simplification the v1 spec calls for — full listener
//! inventory lands when the `listeners` table ships.

use super::{
    AuthBackend, GlobalSignals, ListenerBindKind, ListenerPosture, PostureSnapshot, PostureSource,
    PostureState,
};
use crate::config::{Config, SecurityConfig};
use crate::db::Database;

use anyhow::Result;

const SCHEMA_VERSION_V1: u32 = 1;

/// Compute the live posture snapshot from `config` and the paired-devices
/// table. The returned `state` is resolved from the same inputs via the
/// pure resolver so write-time and replay produce identical states.
pub fn compute_live_snapshot(
    config: &Config,
    db: &Database,
    source: PostureSource,
) -> Result<PostureSnapshot> {
    let global = compute_global_signals(config);
    let listeners = compute_listeners(db)?;
    let mut snap = PostureSnapshot {
        schema_version: SCHEMA_VERSION_V1,
        // Placeholder; replaced by the resolver below.
        state: PostureState::Default,
        global,
        listeners,
        computed_at: chrono::Utc::now().timestamp(),
        computed_from: source,
    };
    snap.state = super::resolver::resolve_state(&snap);
    Ok(snap)
}

fn compute_global_signals(config: &Config) -> GlobalSignals {
    GlobalSignals {
        sandbox_enabled: config.sandbox.enabled,
        blocked_paths_strict: blocked_paths_match_defaults(&config.security),
        // No `allow_privileged` channel flag in the config schema yet —
        // wire-up follows when the flag ships.
        privileged_channels: Vec::new(),
        audit_chain_enabled: true,
        catastrophic_denylist_intact: true,
    }
}

/// True when `cfg.blocked_paths` contains every entry from the
/// fresh-install default. Adding additional entries is fine (stricter);
/// removing or replacing entries demotes posture to `Reduced`.
fn blocked_paths_match_defaults(cfg: &SecurityConfig) -> bool {
    let defaults = SecurityConfig::default().blocked_paths;
    defaults.iter().all(|d| cfg.blocked_paths.contains(d))
}

/// Translate the `paired_devices` table into the listener inventory
/// expected by the resolver. Always includes a synthetic `loopback`
/// row so the resolver can distinguish "no listeners at all" (impossible
/// in v1 — daemon always binds UDS) from "loopback only".
fn compute_listeners(db: &Database) -> Result<Vec<ListenerPosture>> {
    let mut listeners = vec![ListenerPosture {
        name: "loopback".to_string(),
        bind: ListenerBindKind::Loopback,
        auth_backend: AuthBackend::None,
    }];
    let devices = db.list_paired_devices().unwrap_or_default();
    let active: Vec<_> = devices
        .into_iter()
        .filter(|d| d.status == "active")
        .collect();
    if !active.is_empty() {
        // One synthetic remote listener per active paired device. We
        // can't tell apart Tailscale from public-Tofu at this layer in
        // v1; surface as `Tofu` (the conservative choice — Tailscale
        // would also resolve to Boosted via the `Tailscale` arm of
        // `is_boosted`).
        for d in active {
            listeners.push(ListenerPosture {
                name: format!("device:{}", d.fingerprint),
                bind: ListenerBindKind::PublicTofu,
                auth_backend: AuthBackend::Tofu,
            });
        }
    }
    Ok(listeners)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_config() -> Config {
        Config::default()
    }

    /// A fresh install with no paired devices is `Default`. Loopback
    /// listener alone, every global signal positive.
    #[test]
    fn fresh_install_resolves_to_default() {
        let db = Database::test_db();
        let snap = compute_live_snapshot(&fresh_config(), &db, PostureSource::DaemonStartReconcile)
            .expect("snap");
        assert_eq!(snap.state, PostureState::Default);
        assert_eq!(snap.listeners.len(), 1);
        assert_eq!(snap.listeners[0].bind, ListenerBindKind::Loopback);
        assert!(snap.global.sandbox_enabled);
        assert!(snap.global.blocked_paths_strict);
    }

    /// Removing a default `blocked_paths` entry trips Reduced. Failure
    /// mode: a config-validation regression that silently accepts a
    /// loosened blocklist would also make the install Boosted-eligible.
    #[test]
    fn loosened_blocked_paths_trip_reduced() {
        let db = Database::test_db();
        let mut cfg = fresh_config();
        cfg.security.blocked_paths.retain(|p| p != ".ssh");
        let snap = compute_live_snapshot(&cfg, &db, PostureSource::ConfigChange).expect("snap");
        assert_eq!(snap.state, PostureState::Reduced);
        assert!(!snap.global.blocked_paths_strict);
    }

    /// Disabled sandbox trips Reduced.
    #[test]
    fn sandbox_off_trips_reduced() {
        let db = Database::test_db();
        let mut cfg = fresh_config();
        cfg.sandbox.enabled = false;
        let snap = compute_live_snapshot(&cfg, &db, PostureSource::ConfigChange).expect("snap");
        assert_eq!(snap.state, PostureState::Reduced);
    }

    /// Adding *extra* blocked paths is stricter, not looser — must stay
    /// `Default` (or `Boosted` with a remote listener). Regression
    /// guard: a check that compares for set-equality instead of
    /// superset would falsely flag a hardened install as Reduced.
    #[test]
    fn additional_blocked_paths_stay_default() {
        let db = Database::test_db();
        let mut cfg = fresh_config();
        cfg.security.blocked_paths.push(".docker".into());
        cfg.security.blocked_paths.push(".kube".into());
        let snap = compute_live_snapshot(&cfg, &db, PostureSource::ConfigChange).expect("snap");
        assert_eq!(snap.state, PostureState::Default);
        assert!(snap.global.blocked_paths_strict);
    }

    /// An active paired device promotes a fresh install to `Boosted`.
    /// This is the canonical "pair a remote device" UX path.
    #[test]
    fn paired_device_promotes_to_boosted() {
        let db = Database::test_db();
        // Insert a CA cert + paired device row directly. The migrations
        // create both tables; we don't need a real CA chain.
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-1", "alice", 0, 1_000_000)
            .expect("cert");
        db.insert_paired_device("fp-1", "alice-laptop", "{}", "principal-1", serial)
            .expect("paired");

        let snap =
            compute_live_snapshot(&fresh_config(), &db, PostureSource::ListenerBind).expect("snap");
        assert_eq!(snap.state, PostureState::Boosted);
        assert_eq!(snap.listeners.len(), 2, "loopback + paired device");
    }

    /// Revoked devices don't count toward Boosted (they're not active).
    #[test]
    fn revoked_devices_do_not_promote_to_boosted() {
        let db = Database::test_db();
        let serial = db
            .insert_internal_cert("client", "PEM", "fp-rev", "bob", 0, 1_000_000)
            .expect("cert");
        db.insert_paired_device("fp-rev", "bob-old-laptop", "{}", "principal-1", serial)
            .expect("paired");
        db.revoke_paired_device("fp-rev", Some("test"))
            .expect("revoke");

        let snap =
            compute_live_snapshot(&fresh_config(), &db, PostureSource::ListenerBind).expect("snap");
        assert_eq!(snap.state, PostureState::Default);
        assert_eq!(snap.listeners.len(), 1, "only loopback after revocation");
    }
}
