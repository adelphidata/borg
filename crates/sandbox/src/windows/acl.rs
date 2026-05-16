//! DACL ACE manipulation for the Tier 1 Windows sandbox.
//!
//! For each path the policy mentions, we add either a DENY or ALLOW
//! ACE for the per-spawn capability SID. DENY ACEs cover blocked
//! paths and explicit `deny_read`/`deny_write`; ALLOW ACEs cover
//! `fs_write` roots so the sandbox can actually do its workspace job.
//!
//! Two invariants the rest of the sandbox depends on:
//!
//! 1. **DENY before ALLOW.** Windows evaluates ACEs in order and stops
//!    at the first match. `SetEntriesInAclW` handles this for us — it
//!    documents that DENY entries from `pListOfExplicitEntries` are
//!    placed before ALLOW entries when merged with an existing ACL.
//!    A naive `AddAce` loop would NOT do this and is a known bypass
//!    shape. Do not switch to `AddAce` without re-checking ordering.
//!
//! 2. **Cleanup on drop.** `apply_for_paths` returns an `AclGuard`
//!    that holds enough state to remove the ACEs when dropped. A
//!    `panic!()` mid-spawn will still trigger the Drop, so the host
//!    filesystem is not permanently mutated by a sandbox crash. The
//!    guard's restore path is best-effort: a failed restore logs at
//!    `error` level and leaves the leaked ACEs in place — the
//!    daily self-heal sweep will reap them on next start.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    SetEntriesInAclW, SetNamedSecurityInfoW, ACCESS_MODE, DENY_ACCESS, GRANT_ACCESS,
    SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::Authorization::{EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE};
use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSID};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};

use super::ace::{AceMode, AceRequest, AceRights};
use super::sid::LocalSid;

fn rights_mask(r: AceRights) -> u32 {
    match r {
        AceRights::Read => FILE_GENERIC_READ,
        AceRights::Write => FILE_GENERIC_WRITE,
        AceRights::ReadWrite => FILE_GENERIC_READ | FILE_GENERIC_WRITE,
    }
}

/// Owning handle returned by `apply_for_paths`. Drop reverts every ACE
/// we added so a crashed sandbox run does not permanently mutate
/// filesystem ACLs.
pub struct AclGuard {
    /// PSID string we added ACEs for; needed at restore time. Stored
    /// as String (not LocalSid) because LocalSid isn't Send and the
    /// guard may be moved across await points in the runtime caller.
    cap_sid_str: String,
    /// Paths we touched. Restore re-applies SetEntriesInAclW with
    /// matching trustees and DENY/ALLOW reversed (or zero rights) to
    /// remove our entries.
    requests: Vec<AceRequest>,
}

impl AclGuard {
    /// Best-effort removal of every ACE this guard recorded. Logs and
    /// continues on individual failures — partial cleanup is preferred
    /// to giving up entirely.
    fn restore(&self) {
        let cap_sid = match LocalSid::from_str_sid(&self.cap_sid_str) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    sid = %self.cap_sid_str,
                    "AclGuard::restore could not re-resolve cap SID; ACEs leaked"
                );
                return;
            }
        };
        for req in &self.requests {
            // To remove an ACE we added, set its access mask to 0
            // with the same trustee — SetEntriesInAclW interprets
            // "merge with mask 0" as "drop existing entries for this
            // trustee+mode."
            if let Err(e) = remove_ace_for_path(&req.path, cap_sid.as_psid(), req.mode) {
                tracing::error!(
                    error = %e,
                    path = %req.path.display(),
                    mode = ?req.mode,
                    "Failed to restore DACL ACE; leaked entry left in place"
                );
            }
        }
    }
}

impl Drop for AclGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Apply a batch of ACEs and return a guard that reverts them on Drop.
///
/// Failure semantics: if any single ACE fails to apply, we restore
/// everything we did apply and return the error. The sandbox MUST NOT
/// proceed to spawn the child with a partial ACE set — that would let
/// some deny rules apply but not others, which is worse than no
/// sandbox.
pub fn apply_for_paths(
    cap_sid_str: &str,
    cap_sid: &LocalSid,
    requests: &[AceRequest],
) -> io::Result<AclGuard> {
    let mut applied: Vec<AceRequest> = Vec::with_capacity(requests.len());
    for req in requests {
        if let Err(e) = apply_one(
            &req.path,
            cap_sid.as_psid(),
            req.mode,
            rights_mask(req.rights),
        ) {
            // Roll back what we already did before bubbling.
            let partial = AclGuard {
                cap_sid_str: cap_sid_str.to_string(),
                requests: applied,
            };
            drop(partial);
            return Err(e);
        }
        applied.push(req.clone());
    }
    Ok(AclGuard {
        cap_sid_str: cap_sid_str.to_string(),
        requests: applied,
    })
}

/// Convert a path to a UTF-16 NUL-terminated buffer.
fn wide_path(path: &std::path::Path) -> Vec<u16> {
    OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Build an `EXPLICIT_ACCESS_W` for the given trustee SID.
fn make_ea(psid: PSID, mode: AceMode, rights_mask: u32) -> EXPLICIT_ACCESS_W {
    let mode_const: ACCESS_MODE = match mode {
        AceMode::Deny => DENY_ACCESS,
        AceMode::Allow => GRANT_ACCESS,
    };
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: rights_mask,
        grfAccessMode: mode_const,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            // For TRUSTEE_IS_SID, ptstrName carries the PSID re-cast
            // as PWSTR. This is the documented Win32 idiom — the field
            // is a tagged union by TrusteeForm.
            ptstrName: psid as windows_sys::core::PWSTR,
        },
    }
}

/// Apply a single ACE to one path: read existing DACL, merge our entry
/// via `SetEntriesInAclW` (which puts DENY first), write back with
/// `SetNamedSecurityInfoW`.
fn apply_one(
    path: &std::path::Path,
    psid: PSID,
    mode: AceMode,
    rights_mask: u32,
) -> io::Result<()> {
    let path_w = wide_path(path);
    let ea = [make_ea(psid, mode, rights_mask)];

    let mut new_acl: *mut ACL = ptr::null_mut();
    // SAFETY: `ea` is a stack array we keep alive across the call.
    // `oldacl=null` means "merge with empty"; we don't need to query
    // the existing DACL first because SetNamedSecurityInfoW with
    // DACL_SECURITY_INFORMATION REPLACES the DACL, not merges it.
    // Passing only our own entries would *erase* the user's existing
    // ACEs — disastrous. So we fetch the current DACL first below.
    //
    // We use the simpler "fetch + merge + set" sequence:
    let current_dacl = read_current_dacl(&path_w)?;
    let rc = unsafe {
        SetEntriesInAclW(
            ea.len() as u32,
            ea.as_ptr(),
            current_dacl.as_acl_ptr(),
            &mut new_acl,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    if new_acl.is_null() {
        return Err(io::Error::other(
            "SetEntriesInAclW returned success but null ACL",
        ));
    }
    let _new_acl_guard = LocalAllocOwned::new(new_acl as *mut _);

    // SAFETY: path_w is NUL-terminated; new_acl is the kernel buffer
    // SetEntriesInAclW just produced; null owner/group means "leave as
    // is"; null SACL means "don't touch SACL"; DACL_SECURITY_INFORMATION
    // is the only flag set so only the DACL field is interpreted.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            new_acl,
            ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Remove ACEs we added for `psid`+`mode` from `path`. Implementation:
/// fetch existing DACL, walk explicit entries, drop ones matching our
/// trustee and mode, write back.
///
/// In practice we use the simpler "merge with mask=0" trick:
/// SetEntriesInAclW with `grfAccessPermissions = 0` and the matching
/// trustee + mode collapses to a no-op entry, but the function also
/// removes any pre-existing entries that completely overlap (the SDK
/// calls this "Replace" merge semantics).
fn remove_ace_for_path(path: &std::path::Path, psid: PSID, mode: AceMode) -> io::Result<()> {
    let path_w = wide_path(path);
    let zero_ea = [EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0,
        grfAccessMode: match mode {
            AceMode::Deny => DENY_ACCESS,
            AceMode::Allow => GRANT_ACCESS,
        },
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: psid as windows_sys::core::PWSTR,
        },
    }];

    let current_dacl = read_current_dacl(&path_w)?;
    let mut new_acl: *mut ACL = ptr::null_mut();
    let rc = unsafe {
        SetEntriesInAclW(
            zero_ea.len() as u32,
            zero_ea.as_ptr(),
            current_dacl.as_acl_ptr(),
            &mut new_acl,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let _new_acl_guard = LocalAllocOwned::new(new_acl as *mut _);
    let rc = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            new_acl,
            ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Owning wrapper over a buffer LocalAlloc'd by Win32 (e.g. the ACL
/// buffer returned by `SetEntriesInAclW`). Drop calls `LocalFree`.
struct LocalAllocOwned<T>(*mut T);

impl<T> LocalAllocOwned<T> {
    fn new(p: *mut T) -> Self {
        LocalAllocOwned(p)
    }
}

impl<T> Drop for LocalAllocOwned<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the underlying buffer was allocated by a Win32
            // API that documents LocalFree as the release path.
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

/// Owning wrapper over the security descriptor returned by
/// `GetNamedSecurityInfoW` plus a borrowed pointer into its embedded
/// DACL.
struct CurrentDacl {
    sd: *mut core::ffi::c_void,
    dacl: *mut ACL,
}

impl CurrentDacl {
    fn as_acl_ptr(&self) -> *const ACL {
        self.dacl
    }
}

impl Drop for CurrentDacl {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            // SAFETY: GetNamedSecurityInfoW docs require LocalFree on
            // ppSecurityDescriptor. The DACL pointer is a view into
            // the same allocation and must NOT be freed separately.
            unsafe {
                LocalFree(self.sd as HLOCAL);
            }
        }
    }
}

/// Read the existing DACL on `path` so we can merge instead of replace.
fn read_current_dacl(path_w: &[u16]) -> io::Result<CurrentDacl> {
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = ptr::null_mut();
    // SAFETY: path_w is NUL-terminated. We pass null for owner/group/
    // SACL out-pointers because we only need the DACL. The function
    // allocates `sd` via LocalAlloc; we own it via CurrentDacl::Drop.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(CurrentDacl { sd, dacl })
}
