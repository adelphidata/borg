//! Sandbox-account provisioning for the Tier 2 elevated backend.
//!
//! Run once, elevated, by `borg service install --windows-sandbox`.
//! For each sandbox account (`BorgSandboxOnline` / `BorgSandboxOffline`)
//! it:
//!
//! 1. Creates a local user with a strong random password
//!    ([`generate_account_password`]) via `NetUserAdd`.
//! 2. Adds it to the local `Users` group (NOT Administrators) via
//!    `NetLocalGroupAddMembers`.
//! 3. Grants `SeBatchLogonRight` and denies interactive + network logon
//!    via the LSA policy API, so the account can only be used as the
//!    secondary-logon identity the runner launches under and can never
//!    be used to log in at the console or over the network.
//! 4. Resolves and returns the account SID (string form) for
//!    `setup.json` and the WFP filter installer.
//!
//! # Cross-platform
//!
//! [`generate_account_password`] is pure Rust and unit-tested on every
//! runner. The account-creation and rights-grant FFI is gated to
//! Windows; off-platform the entry points return
//! [`io::ErrorKind::Unsupported`].
//!
//! # Privilege model
//!
//! See [`super`] — the runner is launched *as* the sandbox account via
//! `CreateProcessWithLogonW` (which a non-admin operator can do with the
//! account password), so the accounts need only `SeBatchLogonRight`, not
//! the `SeAssignPrimaryTokenPrivilege` that `CreateProcessAsUserW` would
//! require of the caller.

use std::io;

/// Generate a strong random password for a sandbox account.
///
/// 28 characters drawn from four character classes, guaranteed to
/// include at least one of each (lower, upper, digit, symbol) so it
/// satisfies any local password-complexity policy `NetUserAdd` enforces.
/// Ambiguous glyphs (`0/O`, `1/l/I`) are excluded — the password is
/// never typed by a human, but excluding them avoids spurious confusion
/// if it ever surfaces in a diagnostic.
///
/// The password is held in memory only long enough to call `NetUserAdd`
/// and is sealed via DPAPI at rest by the installer; it is never logged.
pub fn generate_account_password() -> String {
    use rand::Rng;
    const LOWER: &[u8] = b"abcdefghijkmnpqrstuvwxyz";
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const DIGIT: &[u8] = b"23456789";
    const SYMBOL: &[u8] = b"!@#$%^&*()-_=+[]{}";
    const LEN: usize = 28;

    let classes: [&[u8]; 4] = [LOWER, UPPER, DIGIT, SYMBOL];
    let mut rng = rand::rng();
    let mut chars: Vec<u8> = Vec::with_capacity(LEN);

    // One guaranteed character from each class.
    for class in classes {
        chars.push(class[rng.random_range(0..class.len())]);
    }
    // Fill the remainder from the union of all classes.
    let pool: Vec<u8> = classes.concat();
    while chars.len() < LEN {
        chars.push(pool[rng.random_range(0..pool.len())]);
    }
    // Fisher–Yates shuffle so the guaranteed characters aren't always in
    // the first four positions.
    for i in (1..chars.len()).rev() {
        let j = rng.random_range(0..=i);
        chars.swap(i, j);
    }
    // Every byte is from the ASCII alphabets above, so `char::from` is a
    // lossless 1:1 mapping — no fallible `from_utf8` needed.
    chars.into_iter().map(char::from).collect()
}

/// Create + group + grant-rights a sandbox account, returning its SID
/// in string form (`S-1-5-21-…`). Idempotent: re-running over an
/// existing account succeeds.
///
/// `password` should come from [`generate_account_password`].
#[cfg(target_os = "windows")]
pub fn provision_account(name: &str, password: &str) -> io::Result<String> {
    ffi::provision_account(name, password)
}

/// Non-Windows stub.
#[cfg(not(target_os = "windows"))]
pub fn provision_account(_name: &str, _password: &str) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sandbox-account provisioning is Windows-only",
    ))
}

/// Delete a sandbox account. Tolerates an already-absent account so
/// `borg service uninstall --windows-sandbox` is safe to run twice.
#[cfg(target_os = "windows")]
pub fn delete_account(name: &str) -> io::Result<()> {
    ffi::delete_account(name)
}

/// Non-Windows stub.
#[cfg(not(target_os = "windows"))]
pub fn delete_account(_name: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sandbox-account provisioning is Windows-only",
    ))
}

#[cfg(target_os = "windows")]
mod ffi {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::core::PWSTR;
    use windows_sys::Win32::Foundation::{LocalFree, HLOCAL, NTSTATUS};
    use windows_sys::Win32::NetworkManagement::NetManagement::{
        NetLocalGroupAddMembers, NetUserAdd, NetUserDel, LOCALGROUP_MEMBERS_INFO_0,
        UF_DONT_EXPIRE_PASSWD, UF_NORMAL_ACCOUNT, UF_SCRIPT, USER_INFO_1, USER_PRIV_USER,
    };
    use windows_sys::Win32::Security::Authentication::Identity::{
        LsaAddAccountRights, LsaClose, LsaNtStatusToWinError, LsaOpenPolicy, LSA_HANDLE,
        LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};

    // NET_API_STATUS / Win32 codes we tolerate for idempotency.
    const NERR_SUCCESS: u32 = 0;
    const NERR_USER_EXISTS: u32 = 2224;
    const NERR_USER_NOT_FOUND: u32 = 2221;
    const ERROR_MEMBER_IN_ALIAS: u32 = 1378;

    use crate::windows::wide_nul;

    fn psid(bytes: &[u8]) -> PSID {
        bytes.as_ptr() as *mut core::ffi::c_void
    }

    pub(super) fn provision_account(name: &str, password: &str) -> io::Result<String> {
        create_user(name, password)?;
        let sid_bytes = resolve_sid(name)?;
        add_to_users_group(&sid_bytes)?;
        grant_logon_rights(&sid_bytes)?;
        sid_to_string(&sid_bytes)
    }

    pub(super) fn delete_account(name: &str) -> io::Result<()> {
        let name_w = wide_nul(name);
        // SAFETY: name_w is null-terminated; null server = local machine.
        let rc = unsafe { NetUserDel(ptr::null(), name_w.as_ptr()) };
        if rc == NERR_SUCCESS || rc == NERR_USER_NOT_FOUND {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc as i32))
        }
    }

    fn create_user(name: &str, password: &str) -> io::Result<()> {
        let mut name_w = wide_nul(name);
        let mut pass_w = wide_nul(password);
        let info = USER_INFO_1 {
            usri1_name: name_w.as_mut_ptr(),
            usri1_password: pass_w.as_mut_ptr(),
            usri1_password_age: 0,
            usri1_priv: USER_PRIV_USER,
            usri1_home_dir: ptr::null_mut(),
            usri1_comment: ptr::null_mut(),
            // UF_SCRIPT is mandatory for NetUserAdd; DONT_EXPIRE_PASSWD
            // keeps the long-lived sandbox account from locking us out.
            usri1_flags: UF_NORMAL_ACCOUNT | UF_DONT_EXPIRE_PASSWD | UF_SCRIPT,
            usri1_script_path: ptr::null_mut(),
        };
        let mut parm_err: u32 = 0;
        // SAFETY: info + its string buffers live across the call; level 1
        // matches USER_INFO_1. Null server = local machine.
        let rc = unsafe {
            NetUserAdd(
                ptr::null(),
                1,
                &info as *const USER_INFO_1 as *const u8,
                &mut parm_err,
            )
        };
        if rc == NERR_SUCCESS || rc == NERR_USER_EXISTS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc as i32))
        }
    }

    fn resolve_sid(name: &str) -> io::Result<Vec<u8>> {
        let name_w = wide_nul(name);
        let mut sid_len: u32 = 0;
        let mut dom_len: u32 = 0;
        let mut sid_use: SID_NAME_USE = 0;
        // First call sizes the SID + domain buffers (returns FALSE with
        // ERROR_INSUFFICIENT_BUFFER and the required lengths).
        // SAFETY: name_w null-terminated; out-params stack-local; SID +
        // domain pointers null for the size query.
        unsafe {
            LookupAccountNameW(
                ptr::null(),
                name_w.as_ptr(),
                ptr::null_mut(),
                &mut sid_len,
                ptr::null_mut(),
                &mut dom_len,
                &mut sid_use,
            );
        }
        if sid_len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sid = vec![0u8; sid_len as usize];
        let mut domain = vec![0u16; dom_len.max(1) as usize];
        // SAFETY: buffers are sized per the first call.
        let ok = unsafe {
            LookupAccountNameW(
                ptr::null(),
                name_w.as_ptr(),
                sid.as_mut_ptr() as PSID,
                &mut sid_len,
                domain.as_mut_ptr(),
                &mut dom_len,
                &mut sid_use,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sid)
    }

    fn sid_to_string(sid: &[u8]) -> io::Result<String> {
        let mut out: PWSTR = ptr::null_mut();
        // SAFETY: sid is a valid SID buffer from resolve_sid; out is a
        // stack-local that ConvertSidToStringSidW fills with a LocalAlloc
        // string we free below.
        let ok = unsafe { ConvertSidToStringSidW(psid(sid), &mut out) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `out` is a valid null-terminated wide string.
        let s = unsafe { pwstr_to_string(out) };
        // SAFETY: `out` was allocated by the conversion API (LocalAlloc).
        unsafe {
            LocalFree(out as HLOCAL);
        }
        Ok(s)
    }

    unsafe fn pwstr_to_string(p: PWSTR) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        // SAFETY: caller guarantees `p` is a valid null-terminated wide
        // string; we walk to the terminator.
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(p, len) };
        String::from_utf16_lossy(slice)
    }

    fn add_to_users_group(sid: &[u8]) -> io::Result<()> {
        let group = wide_nul("Users");
        let info = LOCALGROUP_MEMBERS_INFO_0 {
            lgrmi0_sid: psid(sid),
        };
        // SAFETY: group null-terminated; info + sid live across the call;
        // level 0 matches LOCALGROUP_MEMBERS_INFO_0; one entry.
        let rc = unsafe {
            NetLocalGroupAddMembers(
                ptr::null(),
                group.as_ptr(),
                0,
                &info as *const LOCALGROUP_MEMBERS_INFO_0 as *const u8,
                1,
            )
        };
        if rc == NERR_SUCCESS || rc == ERROR_MEMBER_IN_ALIAS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc as i32))
        }
    }

    fn grant_logon_rights(sid: &[u8]) -> io::Result<()> {
        let policy = open_policy()?;
        let result = (|| {
            grant_one(policy, sid, "SeBatchLogonRight")?;
            grant_one(policy, sid, "SeDenyInteractiveLogonRight")?;
            grant_one(policy, sid, "SeDenyNetworkLogonRight")
        })();
        // SAFETY: policy is a valid LSA handle from open_policy.
        unsafe {
            LsaClose(policy);
        }
        result
    }

    fn open_policy() -> io::Result<LSA_HANDLE> {
        let mut attrs: LSA_OBJECT_ATTRIBUTES = unsafe { std::mem::zeroed() };
        attrs.Length = std::mem::size_of::<LSA_OBJECT_ATTRIBUTES>() as u32;
        let mut handle: LSA_HANDLE = unsafe { std::mem::zeroed() };
        // SAFETY: attrs is a valid zeroed struct with Length set; null
        // system name = local machine; handle is a stack-local out-param.
        let status = unsafe {
            LsaOpenPolicy(
                ptr::null(),
                &attrs,
                (POLICY_CREATE_ACCOUNT | POLICY_LOOKUP_NAMES) as u32,
                &mut handle,
            )
        };
        nt_check(status, "LsaOpenPolicy")?;
        Ok(handle)
    }

    fn grant_one(policy: LSA_HANDLE, sid: &[u8], right: &str) -> io::Result<()> {
        // LSA_UNICODE_STRING length fields are byte counts and the buffer
        // need not be null-terminated.
        let mut w: Vec<u16> = OsStr::new(right).encode_wide().collect();
        let byte_len = (w.len() * 2) as u16;
        let lsa = LSA_UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: w.as_mut_ptr(),
        };
        // SAFETY: policy valid; sid valid; lsa + its buffer live across
        // the call; one right granted.
        let status = unsafe { LsaAddAccountRights(policy, psid(sid), &lsa, 1) };
        nt_check(status, "LsaAddAccountRights")
    }

    fn nt_check(status: NTSTATUS, ctx: &str) -> io::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            // SAFETY: pure value translation, no pointers.
            let win = unsafe { LsaNtStatusToWinError(status) };
            Err(io::Error::other(format!(
                "{ctx}: NTSTATUS 0x{status:08x} (win32 {win})"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_correct_length() {
        // Spec: a too-short password risks rejection by a strict local
        // complexity policy and weakens the account. Pin the length.
        assert_eq!(generate_account_password().len(), 28);
    }

    #[test]
    fn password_covers_all_four_character_classes() {
        // Spec: Windows local-account complexity policy can require ≥3 of
        // 4 classes. Guaranteeing all four means NetUserAdd never fails
        // with ERROR_PASSWORD_RESTRICTION on a hardened host.
        let p = generate_account_password();
        assert!(p.chars().any(|c| c.is_ascii_lowercase()), "no lower: {p}");
        assert!(p.chars().any(|c| c.is_ascii_uppercase()), "no upper: {p}");
        assert!(p.chars().any(|c| c.is_ascii_digit()), "no digit: {p}");
        assert!(
            p.chars().any(|c| !c.is_ascii_alphanumeric()),
            "no symbol: {p}"
        );
    }

    #[test]
    fn two_passwords_differ() {
        // Spec: each account gets a distinct secret. A constant or
        // low-entropy generator would make one account's compromise leak
        // the other.
        assert_ne!(generate_account_password(), generate_account_password());
    }

    #[test]
    fn password_excludes_ambiguous_glyphs() {
        // Spec: ambiguous characters are intentionally excluded from the
        // alphabet. Their presence would indicate the alphabet constants
        // regressed.
        let p = generate_account_password();
        for bad in ['0', 'O', '1', 'l', 'I'] {
            assert!(!p.contains(bad), "ambiguous glyph {bad} in {p}");
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn provisioning_is_unsupported_off_windows() {
        // Spec: off-platform the FFI entry points must error rather than
        // pretend to provision accounts.
        assert_eq!(
            provision_account("X", "Y").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            delete_account("X").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
