//! `borg service install --windows-sandbox` orchestration.
//!
//! Provisions both sandbox accounts, seals their passwords with DPAPI,
//! installs the WFP outbound block for the offline account, and writes
//! `setup.json` so the trampoline auto-promotes to Tier 2. The matching
//! [`uninstall_windows_sandbox`] reverses it.
//!
//! Cross-platform shell; the real work is Windows-only FFI and the
//! entry points return [`io::ErrorKind::Unsupported`] off-platform.

use std::io;

/// Install the Tier 2 elevated sandbox: accounts + sealed creds + WFP +
/// `setup.json`. Requires admin. `installed_at` is a caller-supplied
/// ISO-8601 timestamp recorded in `setup.json` (diagnostic only).
#[cfg(target_os = "windows")]
pub fn install_windows_sandbox(installed_at: &str) -> io::Result<()> {
    ffi::install(installed_at)
}

/// Non-Windows stub.
#[cfg(not(target_os = "windows"))]
pub fn install_windows_sandbox(_installed_at: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the Windows sandbox is only available on Windows",
    ))
}

/// Remove Tier 2: delete WFP rules, sandbox accounts, and the on-disk
/// state. Tolerant of partially-installed state.
#[cfg(target_os = "windows")]
pub fn uninstall_windows_sandbox() -> io::Result<()> {
    ffi::uninstall()
}

/// Non-Windows stub.
#[cfg(not(target_os = "windows"))]
pub fn uninstall_windows_sandbox() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the Windows sandbox is only available on Windows",
    ))
}

#[cfg(target_os = "windows")]
mod ffi {
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::super::account::{OFFLINE_ACCOUNT, ONLINE_ACCOUNT};
    use super::super::provision;
    use super::super::setup::{self, SetupRecord, SCHEMA_VERSION};
    use crate::windows::dpapi;
    use crate::windows::wfp;
    use crate::windows::AUDIT_TARGET;

    /// True if the current process token is elevated (running as admin).
    fn is_elevated() -> io::Result<bool> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: GetCurrentProcess is a pseudo-handle; out-param local.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len: u32 = 0;
        // SAFETY: token valid; buffer sized to TOKEN_ELEVATION.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
        };
        let err = io::Error::last_os_error();
        // SAFETY: token is a valid handle we opened.
        unsafe {
            CloseHandle(token);
        }
        if ok == FALSE {
            return Err(err);
        }
        Ok(elevation.TokenIsElevated != 0)
    }

    fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    pub(super) fn install(installed_at: &str) -> io::Result<()> {
        if !is_elevated()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "`borg service install --windows-sandbox` must run elevated; \
                 re-run from an Administrator terminal",
            ));
        }

        // Provision both accounts; seal each password under DPAPI.
        let offline_sid = provision_and_seal(OFFLINE_ACCOUNT)?;
        let online_sid = provision_and_seal(ONLINE_ACCOUNT)?;

        // WFP outbound block for the offline account. A failure here is
        // not fatal: the account still gives filesystem isolation, and
        // setup.json records that network blocking is NOT enforced so the
        // trampoline can surface the gap honestly.
        let wfp_filters_installed = match wfp::install_offline_outbound_block(&offline_sid) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    target = AUDIT_TARGET,
                    error = %e,
                    "WFP offline block install failed; network=false will not be \
                     enforced at the kernel layer for the offline account",
                );
                false
            }
        };

        let record = SetupRecord {
            schema_version: SCHEMA_VERSION,
            installed_at: installed_at.to_string(),
            offline_account_sid: offline_sid,
            online_account_sid: online_sid,
            wfp_filters_installed,
            last_validated_at: None,
        };
        let path = setup::setup_path()
            .ok_or_else(|| io::Error::other("no home dir to resolve setup.json path"))?;
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|e| io::Error::other(format!("serialize setup.json: {e}")))?;
        atomic_write(&path, &bytes)?;
        Ok(())
    }

    fn provision_and_seal(account: &str) -> io::Result<String> {
        let password = provision::generate_account_password();
        let sid = provision::provision_account(account, &password)?;
        let sealed = dpapi::seal(password.as_bytes(), &setup::cred_entropy(account))?;
        let cred = setup::cred_path(account)
            .ok_or_else(|| io::Error::other("no home dir to resolve cred path"))?;
        atomic_write(&cred, &sealed)?;
        Ok(sid)
    }

    pub(super) fn uninstall() -> io::Result<()> {
        if !is_elevated()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "`borg service uninstall --windows-sandbox` must run elevated",
            ));
        }
        // Best-effort teardown: keep going past individual failures so a
        // partially-installed state can always be fully cleaned.
        let mut first_err: Option<io::Error> = None;
        let mut note = |r: io::Result<()>| {
            if let Err(e) = r {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        };

        note(wfp::uninstall_offline_outbound_block());
        note(provision::delete_account(OFFLINE_ACCOUNT));
        note(provision::delete_account(ONLINE_ACCOUNT));

        for account in [OFFLINE_ACCOUNT, ONLINE_ACCOUNT] {
            if let Some(p) = setup::cred_path(account) {
                if let Err(e) = std::fs::remove_file(&p) {
                    if e.kind() != io::ErrorKind::NotFound {
                        note(Err(e));
                    }
                }
            }
        }
        if let Some(p) = setup::setup_path() {
            if let Err(e) = std::fs::remove_file(&p) {
                if e.kind() != io::ErrorKind::NotFound {
                    note(Err(e));
                }
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
