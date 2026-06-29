//! `setup.json` — operator-visible state of the elevated Tier 2
//! backend.
//!
//! Written by `borg service install --windows-sandbox` after a
//! successful UAC-elevated provisioning pass, read by the trampoline
//! on every spawn to decide between Tier 1 (restricted token only)
//! and Tier 2 (elevated runner + sandbox accounts + WFP).
//!
//! # Versioning
//!
//! `schema_version` is bumped whenever the trampoline-side
//! interpretation changes. The auto-promote check in [`mode::detect`]
//! refuses to use a `setup.json` with a future version (don't drive a
//! newer setup with an older binary) or a too-old version (the field
//! layout it expects may be gone).
//!
//! # Why JSON
//!
//! The file is operator-readable on purpose. An admin debugging a
//! sandbox issue should be able to `type %USERPROFILE%\.borg\sandbox-windows\setup.json`
//! and see what state the installer left behind.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::windows::AUDIT_TARGET;

/// Current schema version. Bump only when the on-disk meaning of an
/// existing field changes, or a new field becomes load-bearing.
pub const SCHEMA_VERSION: u32 = 1;

/// Minimum schema version this binary understands. Setup files older
/// than this trigger a fallback to Tier 1; the operator must re-run
/// `borg service install --windows-sandbox` to upgrade.
pub const MIN_SCHEMA_VERSION: u32 = 1;

/// Persisted state for the elevated Tier 2 backend. Written atomically
/// (tmpfile + rename) at the end of `borg service install
/// --windows-sandbox`. Absence of the file means Tier 1 only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRecord {
    /// `SCHEMA_VERSION` at install time. The trampoline refuses to
    /// auto-promote if this is outside `[MIN_SCHEMA_VERSION,
    /// SCHEMA_VERSION]`.
    pub schema_version: u32,
    /// Wall-clock time of the install pass (ISO-8601). Diagnostic
    /// only — Tier 2 doesn't change behavior based on age.
    pub installed_at: String,
    /// SID of the `BorgSandboxOffline` account, in string form. Used
    /// by the WFP filter installer to bind ALE rules to this principal.
    pub offline_account_sid: String,
    /// SID of the `BorgSandboxOnline` account.
    pub online_account_sid: String,
    /// True if WFP filters denying outbound for the offline SID were
    /// installed successfully. `false` means the operator skipped
    /// network blocking (e.g. on a host where WFP is unavailable);
    /// the trampoline can still use the offline account for filesystem
    /// isolation but should warn that `network=false` is not enforced
    /// at the network layer.
    pub wfp_filters_installed: bool,
    /// Last `borg doctor`-validated date. Re-validation cadence is
    /// future work; this field is reserved.
    #[serde(default)]
    pub last_validated_at: Option<String>,
}

/// Resolved path: `<home>/.borg/sandbox-windows/setup.json`. Returns
/// `None` if no home dir is resolvable (which on Windows is a
/// genuinely unusual state — installs without `%USERPROFILE%`).
pub fn setup_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".borg")
            .join("sandbox-windows")
            .join("setup.json"),
    )
}

/// Directory holding Tier 2 state (`setup.json` + sealed credentials).
pub fn setup_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".borg").join("sandbox-windows"))
}

/// Resolved path of the DPAPI-sealed password blob for a sandbox
/// account: `<home>/.borg/sandbox-windows/<account>.cred`.
///
/// The installer seals the generated password here; the runner client
/// unseals it just before `CreateProcessWithLogonW`. The file is
/// machine-scoped DPAPI ciphertext (see [`super::super::dpapi`]) — never
/// the plaintext password.
pub fn cred_path(account: &str) -> Option<PathBuf> {
    Some(setup_dir()?.join(format!("{account}.cred")))
}

/// Domain-separation entropy for sealing a sandbox-account password.
/// Binding the DPAPI blob to the account name means a blob copied to a
/// different account's file cannot be unsealed in its place.
pub fn cred_entropy(account: &str) -> Vec<u8> {
    let mut e = b"borg-sandbox-cred:".to_vec();
    e.extend_from_slice(account.as_bytes());
    e
}

/// Read and validate `setup.json`. Returns `None` if the file is
/// absent, malformed, version-incompatible, or otherwise unusable —
/// the caller should fall back to Tier 1 in any of those cases.
///
/// Errors are logged via `tracing::warn!` so an operator who *intended*
/// Tier 2 sees why it didn't kick in.
pub fn load(path: &Path) -> Option<SetupRecord> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                target = AUDIT_TARGET,
                path = %path.display(),
                error = %e,
                "sandbox setup.json read failed; falling back to Tier 1",
            );
            return None;
        }
    };
    let record: SetupRecord = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target = AUDIT_TARGET,
                path = %path.display(),
                error = %e,
                "sandbox setup.json malformed; falling back to Tier 1",
            );
            return None;
        }
    };
    if record.schema_version < MIN_SCHEMA_VERSION || record.schema_version > SCHEMA_VERSION {
        tracing::warn!(
            target = AUDIT_TARGET,
            schema_version = record.schema_version,
            supported_range = format!("{MIN_SCHEMA_VERSION}..={SCHEMA_VERSION}"),
            "sandbox setup.json version unsupported; re-run `borg service install --windows-sandbox`",
        );
        return None;
    }
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(version: u32) -> SetupRecord {
        SetupRecord {
            schema_version: version,
            installed_at: "2026-05-22T12:00:00Z".into(),
            offline_account_sid: "S-1-5-21-1-2-3-1001".into(),
            online_account_sid: "S-1-5-21-1-2-3-1002".into(),
            wfp_filters_installed: true,
            last_validated_at: None,
        }
    }

    #[test]
    fn load_returns_none_for_absent_file() {
        // Spec: missing `setup.json` is the steady-state "Tier 2 not
        // installed" signal. It must NOT surface as an error or
        // warn-spam in the log — a fresh install would otherwise
        // generate one warning per spawn forever.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("setup.json");
        assert!(load(&p).is_none());
    }

    #[test]
    fn load_returns_none_for_malformed_json() {
        // Spec: a corrupted file falls back to Tier 1 (with a warning).
        // We must not panic or error-propagate; the agent should keep
        // running.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("setup.json");
        std::fs::write(&p, b"{not valid").unwrap();
        assert!(load(&p).is_none());
    }

    #[test]
    fn load_returns_none_for_future_version() {
        // Spec: a `setup.json` written by a *newer* borg version
        // could have field semantics this binary doesn't understand.
        // Refusing to auto-promote (and falling back to Tier 1) is
        // safer than guessing.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("setup.json");
        let body = serde_json::to_vec(&sample(SCHEMA_VERSION + 99)).unwrap();
        std::fs::write(&p, body).unwrap();
        assert!(load(&p).is_none());
    }

    #[test]
    fn load_returns_record_for_current_version() {
        // Spec: a well-formed setup.json at the current schema version
        // is the auto-promote happy path. The record must round-trip
        // verbatim — any field dropped here would let the trampoline
        // proceed to Tier 2 with stale state.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("setup.json");
        let record = sample(SCHEMA_VERSION);
        let body = serde_json::to_vec(&record).unwrap();
        std::fs::write(&p, body).unwrap();
        let loaded = load(&p).expect("should load");
        assert_eq!(loaded.schema_version, record.schema_version);
        assert_eq!(loaded.offline_account_sid, record.offline_account_sid);
        assert_eq!(loaded.online_account_sid, record.online_account_sid);
        assert!(loaded.wfp_filters_installed);
    }

    #[test]
    fn load_returns_none_for_too_old_version() {
        // Spec: same logic as future-version, lower-bound side. A
        // schema_version=0 file is from a pre-release that didn't
        // record the WFP-filter-installed bit; we cannot infer it,
        // so we fall back rather than guess.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let p = tmp.path().join("setup.json");
        let mut record = sample(0);
        record.schema_version = 0;
        let body = serde_json::to_vec(&record).unwrap();
        std::fs::write(&p, body).unwrap();
        assert!(load(&p).is_none());
    }
}
