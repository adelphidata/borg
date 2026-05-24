//! Tier selection — Tier 1 (restricted-token only) vs Tier 2
//! (elevated runner + sandbox accounts).
//!
//! Called by the trampoline before applying any policy. Reads
//! `setup.json` and decides which path to take. The decision is
//! `cfg!(target_os = "windows")` — non-Windows hosts can call
//! `detect()` for testing, but the answer is always `Tier1` because
//! the elevated runner is Windows-only.

use super::setup::{self, SetupRecord};

/// Tier currently usable on this host.
#[derive(Debug, Clone)]
pub enum Tier {
    /// Restricted-token only. Always available; the fallback when
    /// `setup.json` is absent or unusable.
    Tier1,
    /// Elevated runner + sandbox accounts + (optionally) WFP filters.
    /// `record` carries the loaded `setup.json`.
    Tier2(Box<SetupRecord>),
}

impl Tier {
    /// `true` if WFP filters were installed and the offline account
    /// will see network blocking. False on Tier 1 (no WFP at all) and
    /// on Tier 2 records where the operator skipped the WFP step.
    pub fn enforces_offline_network_block(&self) -> bool {
        matches!(self, Tier::Tier2(r) if r.wfp_filters_installed)
    }
}

/// Auto-detect the active tier. Looks for `setup.json` at the
/// canonical location, validates schema version, and returns
/// `Tier::Tier2` only when everything checks out.
///
/// Falls back to `Tier::Tier1` for any failure mode — missing file,
/// malformed JSON, version mismatch, no home directory. This makes
/// Tier 2 strictly additive: an operator who hasn't run `borg service
/// install --windows-sandbox` sees identical Tier 1 behavior.
pub fn detect() -> Tier {
    let Some(path) = setup::setup_path() else {
        return Tier::Tier1;
    };
    match setup::load(&path) {
        Some(record) => Tier::Tier2(Box::new(record)),
        None => Tier::Tier1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(wfp: bool) -> SetupRecord {
        SetupRecord {
            schema_version: setup::SCHEMA_VERSION,
            installed_at: "2026-05-22T12:00:00Z".into(),
            offline_account_sid: "S-1-5-21-1-2-3-1001".into(),
            online_account_sid: "S-1-5-21-1-2-3-1002".into(),
            wfp_filters_installed: wfp,
            last_validated_at: None,
        }
    }

    #[test]
    fn tier1_does_not_enforce_offline_network_block() {
        // Spec: Tier 1 has no WFP code at all. Reporting "network
        // blocking enforced" for Tier 1 would mislead the caller into
        // thinking `network=false` policies are network-isolated when
        // they're really only filesystem-isolated.
        assert!(!Tier::Tier1.enforces_offline_network_block());
    }

    #[test]
    fn tier2_with_wfp_installed_enforces_offline_network_block() {
        // Spec: the happy path. WFP rules in place + offline account
        // SID known + filters_installed=true → trampoline can promise
        // the operator that `network=false` is enforced at the
        // kernel filter layer.
        let t = Tier::Tier2(Box::new(sample_record(true)));
        assert!(t.enforces_offline_network_block());
    }

    #[test]
    fn tier2_without_wfp_does_not_enforce_network_block() {
        // Spec: an operator who skipped the WFP step (e.g. on a host
        // where WFP is unavailable) still gets Tier 2 filesystem
        // isolation via the sandbox account, but `network=false`
        // semantics fall back to "best effort at the gateway layer."
        // The bool exists to surface this difference truthfully.
        let t = Tier::Tier2(Box::new(sample_record(false)));
        assert!(!t.enforces_offline_network_block());
    }
}
