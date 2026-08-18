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
//!
//! 3. **No reparse traversal.** Every ACE is applied through a handle
//!    opened with `FILE_FLAG_OPEN_REPARSE_POINT`, after rejecting the
//!    leaf and every ancestor that is a reparse point (see
//!    [`open_no_reparse`]). The path-based `SetNamedSecurityInfoW`
//!    follows junctions, so a junction anywhere in a policy path would
//!    land the ACE on the directory it points at instead: a DENY would
//!    silently protect the wrong place, and — far worse — an ALLOW
//!    WRITE for an `fs_write` root would hand the sandbox account write
//!    access to whatever the junction targets. Using the handle for
//!    both the DACL read and the DACL write also removes the gap
//!    between reading a descriptor and replacing it — no second path
//!    resolution happens in between.
//!
//!    The ancestor scan is still path-based, so it is not a
//!    check-then-use guarantee: an attacker who can swap a parent
//!    directory for a junction in the window between the scan and the
//!    leaf open would defeat it. Closing that would need each
//!    component opened relative to its parent's handle
//!    (`NtCreateFile` with `RootDirectory`). It is a real narrowing,
//!    not a proof.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, ACCESS_MODE, DENY_ACCESS, GRANT_ACCESS,
    SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::Authorization::{EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE};
use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSID};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};

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

/// Owning wrapper over a Win32 file handle. Drop calls `CloseHandle`.
#[derive(Debug)]
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Open `path` for DACL read/write without ever traversing a reparse point.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` makes the open stop *at* a reparse point
/// rather than follow it, and `FILE_FLAG_BACKUP_SEMANTICS` is what lets
/// `CreateFileW` open a directory at all. `desired_access` is only
/// `READ_CONTROL | WRITE_DAC`: this handle exists to edit a DACL, not to read
/// or write file contents.
///
/// Fails when the leaf, or any ancestor, is a reparse point, and when `path`
/// is not absolute. A relative path would resolve against the process working
/// directory, which is not a stable base for a security decision.
///
/// Failing closed is the point. Skipping an ACE we cannot place safely would
/// leave the caller believing a deny rule is in force when it is not.
fn open_no_reparse(path: &Path) -> io::Result<OwnedHandle> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to set an ACL through the relative path {}",
                path.display()
            ),
        ));
    }

    // Check ancestors first, shallowest to deepest. `ancestors()` yields the
    // leaf first, so reverse it; skip the leaf itself, which the real open
    // below checks with the very handle it goes on to use.
    let ancestors: Vec<&Path> = path.ancestors().skip(1).collect();
    for ancestor in ancestors.into_iter().rev() {
        // A drive root ("C:\") has no reparse point to speak of, and opening
        // one can fail on some volumes; nothing above it is ours to police.
        if ancestor.parent().is_none() {
            continue;
        }
        let handle = open_handle(ancestor, READ_CONTROL)?;
        if is_reparse_point(&handle)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "refusing to set an ACL under the reparse point {}: it can redirect \
                     the rule to another directory",
                    ancestor.display()
                ),
            ));
        }
    }

    let handle = open_handle(path, READ_CONTROL | WRITE_DAC)?;
    if is_reparse_point(&handle)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to set an ACL on the reparse point {}: it can redirect the \
                 rule to another directory",
                path.display()
            ),
        ));
    }
    Ok(handle)
}

/// `CreateFileW` with the no-follow, directory-capable flag pair.
fn open_handle(path: &Path, desired_access: u32) -> io::Result<OwnedHandle> {
    let path_w = wide_path(path);
    // SAFETY: path_w is NUL-terminated and outlives the call. Null SA means
    // default security; OPEN_EXISTING never creates. Sharing everything keeps
    // us from blocking other processes for the moment we hold the handle.
    let h = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(h))
}

/// True when the object behind `handle` carries `FILE_ATTRIBUTE_REPARSE_POINT`.
fn is_reparse_point(handle: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: zeroed BY_HANDLE_FILE_INFORMATION is a valid out-param; the
    // handle is live for the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle.raw(), &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
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
    // One handle for both the read and the write. Opening it also rejects a
    // reparse point on the leaf or any ancestor, so the ACE cannot be
    // redirected to another directory, and no second path resolution happens
    // between the check and the write.
    let handle = open_no_reparse(path)?;
    let ea = [make_ea(psid, mode, rights_mask)];

    let mut new_acl: *mut ACL = ptr::null_mut();
    // SAFETY: `ea` is a stack array we keep alive across the call.
    // `oldacl=null` means "merge with empty"; we don't need to query
    // the existing DACL first because SetSecurityInfo with
    // DACL_SECURITY_INFORMATION REPLACES the DACL, not merges it.
    // Passing only our own entries would *erase* the user's existing
    // ACEs — disastrous. So we fetch the current DACL first below.
    //
    // We use the simpler "fetch + merge + set" sequence:
    let current_dacl = read_current_dacl(&handle)?;
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

    // SAFETY: the handle is live and carries WRITE_DAC; new_acl is the kernel
    // buffer SetEntriesInAclW just produced; null owner/group means "leave as
    // is"; null SACL means "don't touch SACL"; DACL_SECURITY_INFORMATION
    // is the only flag set so only the DACL field is interpreted.
    let rc = unsafe {
        SetSecurityInfo(
            handle.raw(),
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
    // Same no-follow open as `apply_one`. The restore path must not resolve a
    // junction either: removing our ACE through one would rewrite the DACL of
    // a directory we never touched.
    let handle = open_no_reparse(path)?;
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

    let current_dacl = read_current_dacl(&handle)?;
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
        SetSecurityInfo(
            handle.raw(),
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
/// `GetSecurityInfo` plus a borrowed pointer into its embedded
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
            // SAFETY: GetSecurityInfo docs require LocalFree on
            // ppSecurityDescriptor. The DACL pointer is a view into
            // the same allocation and must NOT be freed separately.
            unsafe {
                LocalFree(self.sd as HLOCAL);
            }
        }
    }
}

/// Read the existing DACL behind `handle` so we can merge instead of replace.
///
/// Handle-based rather than path-based on purpose: the caller already proved
/// this handle does not point through a reparse point, and reading the DACL
/// from the same handle we later write means no second path resolution can
/// disagree with the first.
fn read_current_dacl(handle: &OwnedHandle) -> io::Result<CurrentDacl> {
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = ptr::null_mut();
    // SAFETY: the handle is live and carries READ_CONTROL. We pass null for
    // owner/group/SACL out-pointers because we only need the DACL. The
    // function allocates `sd` via LocalAlloc; we own it via CurrentDacl::Drop.
    let rc = unsafe {
        GetSecurityInfo(
            handle.raw(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a directory junction at `link` pointing at `target`.
    ///
    /// `mklink /J` rather than `symlink_dir` because a junction needs no
    /// privilege, so this runs on an ordinary CI account. Returns false when
    /// the shell-out fails, letting a case skip rather than fail for a reason
    /// that is not about our code.
    fn make_junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Spec: a relative path must never reach `CreateFileW` here. It would
    /// resolve against the process working directory, which is not a stable
    /// base for a security decision — the same policy entry could land on a
    /// different directory depending on where the daemon was started.
    #[test]
    fn relative_paths_are_rejected_before_any_handle_is_opened() {
        let err = open_no_reparse(Path::new("relative\\dir"))
            .expect_err("a relative path must not be ACL'd");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("relative"),
            "error should name the problem: {err}"
        );
    }

    /// Spec: an ordinary directory is the normal case and must still open.
    /// Without this the reparse checks could reject everything and the other
    /// cases would pass for the wrong reason.
    #[test]
    fn plain_directory_opens() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("plain");
        std::fs::create_dir(&dir).expect("create dir");
        open_no_reparse(&dir).expect("a plain directory must be ACL-able");
    }

    /// Spec: applying an ACE to a junction lands it on the junction's target.
    /// For an ALLOW WRITE entry — which the sandbox emits for every `fs_write`
    /// root — that hands the sandbox account write access to whatever the
    /// junction points at. Reject instead.
    #[test]
    fn junction_at_the_leaf_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("target");
        let link = tmp.path().join("link");
        std::fs::create_dir(&target).expect("create target");
        if !make_junction(&link, &target) {
            eprintln!("skipping: mklink /J unavailable");
            return;
        }

        let err = open_no_reparse(&link).expect_err("a junction must not be ACL'd");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("reparse point"),
            "error should name the problem: {err}"
        );
    }

    /// Spec: the leaf being ordinary is not enough. A junction anywhere above
    /// it redirects the whole subtree just as effectively, so ancestors are
    /// checked too.
    #[test]
    fn junction_in_an_ancestor_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("target");
        let link = tmp.path().join("link");
        std::fs::create_dir_all(target.join("child")).expect("create target tree");
        if !make_junction(&link, &target) {
            eprintln!("skipping: mklink /J unavailable");
            return;
        }

        // `link\child` is a real directory reached through a junction: the
        // leaf carries no reparse attribute, so only the ancestor walk
        // catches this.
        let err =
            open_no_reparse(&link.join("child")).expect_err("a junctioned ancestor must be caught");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("reparse point"),
            "error should name the problem: {err}"
        );
    }
}
