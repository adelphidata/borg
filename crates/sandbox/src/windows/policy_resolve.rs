//! Translate a [`SandboxPolicy`] into the concrete list of DACL ACEs
//! the Tier 1 backend will apply.
//!
//! The translation is straightforward but worth isolating from the
//! FFI: it makes the decision-making testable on every host (no Win32
//! calls), and keeps the runtime module focused on orchestration.
//!
//! Resolution rules:
//!
//! - Every `policy.deny_read` path → DENY READ ACE.
//! - Every `policy.deny_write` path → DENY WRITE ACE.
//! - Every `policy.fs_write` path → ALLOW WRITE ACE for the cap SID.
//!   Without this, the cap-SID restriction makes the workspace
//!   unwritable too — the sandbox would refuse legitimate writes.
//! - **No ALLOW READ ACE for `fs_read`.** The original token already
//!   grants the parent user read on those paths; cap-SID restriction
//!   intersects, so as long as the cap SID has *some* read authority
//!   somewhere, the AND gate doesn't slam shut. We grant the read
//!   authority globally on a per-process basis via the restricted
//!   token's group flags, not per-path. (If a path is in `deny_read`
//!   that wins, because DENY beats ALLOW.)
//!
//! Paths are filtered to those that exist on disk — applying an ACE
//! to a non-existent path is a Win32 error and would fail the whole
//! batch, defeating the "best-effort, fail closed only on real
//! conflicts" model.

use std::path::PathBuf;

use crate::policy::SandboxPolicy;

use super::ace::{AceMode, AceRequest, AceRights};

/// Compute the ACE batch for a policy. Caller passes the policy after
/// `with_blocked_paths_filtered` + `with_borg_dir_protected` +
/// `with_tildes_expanded` (the standard `configured()` chain), so deny
/// lists already include `~/.borg` and any `[security] blocked_paths`.
pub fn resolve(policy: &SandboxPolicy) -> Vec<AceRequest> {
    let mut out = Vec::new();

    for p in &policy.deny_read {
        let path = PathBuf::from(p);
        if !path.exists() {
            continue;
        }
        out.push(AceRequest {
            path,
            mode: AceMode::Deny,
            rights: AceRights::Read,
        });
    }
    for p in &policy.deny_write {
        let path = PathBuf::from(p);
        if !path.exists() {
            continue;
        }
        out.push(AceRequest {
            path,
            mode: AceMode::Deny,
            rights: AceRights::Write,
        });
    }
    for p in &policy.fs_write {
        let path = PathBuf::from(p);
        if !path.exists() {
            continue;
        }
        out.push(AceRequest {
            path,
            mode: AceMode::Allow,
            rights: AceRights::Write,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolve_emits_deny_read_first_then_deny_write_then_allow_write() {
        // Spec: this ordering is what the runtime hands to
        // SetEntriesInAclW. While SetEntriesInAclW does its own
        // DENY-before-ALLOW reordering, the *grouping* by intent in
        // the input keeps debugging and audit-log inspection sane.
        // A regression that interleaves modes here would still be
        // correct at the kernel level but would mask a logic error.
        let dir = TempDir::new().unwrap();
        let r = dir.path().join("r");
        let w = dir.path().join("w");
        let a = dir.path().join("a");
        fs::create_dir_all(&r).unwrap();
        fs::create_dir_all(&w).unwrap();
        fs::create_dir_all(&a).unwrap();

        let policy = SandboxPolicy {
            deny_read: vec![r.to_string_lossy().into_owned()],
            deny_write: vec![w.to_string_lossy().into_owned()],
            fs_write: vec![a.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let aces = resolve(&policy);
        assert_eq!(aces.len(), 3);
        assert_eq!(aces[0].mode, AceMode::Deny);
        assert_eq!(aces[0].rights, AceRights::Read);
        assert_eq!(aces[1].mode, AceMode::Deny);
        assert_eq!(aces[1].rights, AceRights::Write);
        assert_eq!(aces[2].mode, AceMode::Allow);
        assert_eq!(aces[2].rights, AceRights::Write);
    }

    #[test]
    fn resolve_skips_nonexistent_paths() {
        // Spec: applying an ACE to a path that doesn't exist is a hard
        // Win32 error (ERROR_FILE_NOT_FOUND) and would abort the
        // whole batch in apply_for_paths, leaving the sandbox unable
        // to launch. Filtering at resolve-time means a stale config
        // entry (deleted directory, typo) degrades the policy to
        // "this path is not enforced" rather than "no commands run."
        let policy = SandboxPolicy {
            deny_read: vec!["/this/path/definitely/does/not/exist/abcxyz".to_string()],
            ..Default::default()
        };
        assert!(resolve(&policy).is_empty());
    }

    #[test]
    fn resolve_handles_empty_policy() {
        // Spec: a default-constructed policy (no fs_read/write/deny_*)
        // produces zero ACEs — the runtime then only applies the
        // restricted token, which is still a security improvement
        // over running fully unsandboxed.
        assert!(resolve(&SandboxPolicy::default()).is_empty());
    }

    #[test]
    fn resolve_preserves_input_order_within_a_mode() {
        // Spec: when multiple deny_read paths exist, they should be
        // emitted in the order the operator wrote them. Order
        // independence at the kernel level (DENY ACEs all evaluate
        // independently) makes this purely an audit-trail concern,
        // but operators reading borg's tracing logs expect to see
        // their config order reflected.
        let dir = TempDir::new().unwrap();
        let p1 = dir.path().join("a");
        let p2 = dir.path().join("b");
        let p3 = dir.path().join("c");
        for p in [&p1, &p2, &p3] {
            fs::create_dir_all(p).unwrap();
        }
        let policy = SandboxPolicy {
            deny_read: vec![
                p1.to_string_lossy().into_owned(),
                p2.to_string_lossy().into_owned(),
                p3.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };
        let aces = resolve(&policy);
        assert_eq!(aces[0].path, p1);
        assert_eq!(aces[1].path, p2);
        assert_eq!(aces[2].path, p3);
    }
}
