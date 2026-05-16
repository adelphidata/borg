//! Restricted-token construction for the Tier 1 Windows sandbox.
//!
//! `RestrictedToken::for_current_with_cap_sid` opens a primary-token
//! handle on the current process, then calls `CreateRestrictedToken`
//! to add the supplied capability SID as a *restricting* SID.
//!
//! Restricting SIDs are the keystone of the sandbox model. After the
//! call, every access check the kernel performs against the new token
//! is the AND of (the original token's groups + privileges) and (the
//! restricting SIDs). A path with no ACE for the cap SID becomes
//! unreachable from the child even if the parent user has access — the
//! kernel intersects the two ACL evaluations and refuses if either
//! refuses. This is the difference between "deny-by-default" and
//! "user's normal authority", and is what makes the sandbox real.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, DuplicateTokenEx, SecurityImpersonation, TokenPrimary,
    SID_AND_ATTRIBUTES, TOKEN_ALL_ACCESS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::sid::LocalSid;

/// Owning wrapper over a token `HANDLE`. `Drop` calls `CloseHandle`.
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// Wrap a raw handle. Caller transfers ownership; `Drop` will close it.
    pub fn from_raw(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// Borrow the inner handle.
    pub fn as_handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: We own this handle (caller passed ownership via
            // `from_raw`). CloseHandle is the documented release path.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Build a restricted primary token derived from the current process,
/// with `cap_sid` added as a restricting SID.
///
/// Returns a primary token suitable for `CreateProcessAsUserW`. The
/// token type matters: `CreateRestrictedToken` produces an
/// impersonation-ish handle by default; we duplicate it as
/// `TokenPrimary` so the spawn API accepts it.
pub fn for_current_with_cap_sid(cap_sid: &LocalSid) -> io::Result<OwnedHandle> {
    // 1. Open the current process token. We need TOKEN_ALL_ACCESS
    //    because CreateRestrictedToken wants TOKEN_DUPLICATE +
    //    TOKEN_ASSIGN_PRIMARY + TOKEN_QUERY at minimum and asking for
    //    less means churning the access mask whenever the child is
    //    spawned.
    let mut current_token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that does not
    // need to be closed. OpenProcessToken's out-parameter receives a
    // real handle we own and must close — captured by OwnedHandle below.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut current_token) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    let current = OwnedHandle::from_raw(current_token);

    // 2. CreateRestrictedToken with the cap SID as a restricting SID.
    //    No SIDs to disable, no privileges to delete — just intersect
    //    the existing groups with our cap SID on every access check.
    let restrict = [SID_AND_ATTRIBUTES {
        Sid: cap_sid.as_psid(),
        // Per Win32 docs, restricting SIDs ignore the Attributes field;
        // it must still be a valid value, 0 is accepted.
        Attributes: 0,
    }];
    let mut restricted_token: HANDLE = ptr::null_mut();
    // SAFETY: All pointers refer to stack data that outlives the call.
    // disablesidcount=0 + sidstodisable=null and
    // deleteprivilegecount=0 + privilegestodelete=null are explicitly
    // documented as acceptable.
    let ok = unsafe {
        CreateRestrictedToken(
            current.as_handle(),
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            restrict.len() as u32,
            restrict.as_ptr(),
            &mut restricted_token,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    let restricted = OwnedHandle::from_raw(restricted_token);

    // 3. Duplicate as TokenPrimary so CreateProcessAsUserW accepts it.
    //    The token returned by CreateRestrictedToken is the same type
    //    as the source (primary in our case), but explicitly
    //    duplicating to TokenPrimary with TOKEN_ALL_ACCESS makes the
    //    intent unambiguous and ensures the access mask is right for
    //    the spawn call.
    let mut primary: HANDLE = ptr::null_mut();
    // SAFETY: All pointers valid; lptokenattributes=null gives default
    // security descriptor and non-inheritable handle, which is what we
    // want — the child does NOT inherit the token handle itself.
    let ok = unsafe {
        DuplicateTokenEx(
            restricted.as_handle(),
            TOKEN_ALL_ACCESS,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle::from_raw(primary))
}
