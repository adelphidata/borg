//! Platform-neutral ACE request types.
//!
//! Split out from `acl.rs` so [`policy_resolve`](super::policy_resolve)
//! and its tests compile and run on every host. The actual ACE
//! application (`SetEntriesInAclW` + `SetNamedSecurityInfoW`) lives in
//! `acl.rs` and is `cfg(target_os = "windows")`.

use std::path::PathBuf;

/// One ACE we want applied to a single path.
#[derive(Debug, Clone)]
pub struct AceRequest {
    /// Absolute path the ACE will be attached to.
    pub path: PathBuf,
    /// DENY (cannot use) or ALLOW (can use) the rights below.
    pub mode: AceMode,
    /// Bitmask of rights (e.g. `FILE_GENERIC_READ | FILE_GENERIC_WRITE`).
    pub rights: AceRights,
}

/// DENY vs ALLOW.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceMode {
    /// The trustee may NOT exercise the rights.
    Deny,
    /// The trustee may exercise the rights.
    Allow,
}

/// Symbolic right set. Translated to Win32 access masks at apply time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AceRights {
    /// `FILE_GENERIC_READ`.
    Read,
    /// `FILE_GENERIC_WRITE`.
    Write,
    /// Both read and write.
    ReadWrite,
}
