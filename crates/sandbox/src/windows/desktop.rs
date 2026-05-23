//! Desktop / window-station isolation for the sandboxed child.
//!
//! Without this, a sandboxed child runs on the same desktop
//! (`WinSta0\Default`) as the operator's interactive session. A
//! malicious or merely buggy child could enumerate top-level windows,
//! send synthetic input via `SendInput` / `PostMessageW`, or capture
//! the screen — none of which the restricted token blocks at the
//! window-manager layer. The classic name for this attack family is
//! "shatter": a low-privilege process drives a high-privilege UI
//! element by message injection.
//!
//! We create a fresh non-default desktop with an empty security
//! descriptor for every spawn and pass its name in
//! `STARTUPINFOEXW.StartupInfo.lpDesktop`. The child cannot reach the
//! operator's desktop because:
//!
//! 1. It runs under a restricted token whose discretionary access on
//!    `WinSta0\Default` is intentionally not granted.
//! 2. Its `lpDesktop` is the isolated desktop's name; `USER32` binds
//!    the child to that desktop at process init.
//!
//! Lifetime: [`OwnedDesktop::drop`] calls `CloseDesktop` only after
//! the child has exited. The desktop is held by `SpawnedChild` and
//! drops after `wait_for_exit` returns.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CREATEWINDOW, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS,
    HDESK,
};

/// Per-spawn random suffix length for the desktop name. 16 hex chars
/// = 64 bits of entropy; collision probability with concurrent spawns
/// is negligible even on a long-running daemon.
const DESKTOP_NAME_ENTROPY_HEX: usize = 16;

/// Owning handle to an isolated desktop. Closed on Drop.
pub struct OwnedDesktop {
    handle: HDESK,
    /// UTF-16, null-terminated. The kernel reads this from
    /// `STARTUPINFO.lpDesktop` at child creation; the buffer must
    /// outlive `CreateProcess*`, so we keep it on the struct rather
    /// than reallocating on every `name_ptr()` call.
    name_wide: Vec<u16>,
}

// SAFETY: HDESK is a kernel handle. CloseDesktop is the only operation
// we perform; it is safe to call from any thread.
unsafe impl Send for OwnedDesktop {}
unsafe impl Sync for OwnedDesktop {}

impl OwnedDesktop {
    /// UTF-16 null-terminated pointer to embed in
    /// `STARTUPINFO.lpDesktop`. The pointer is valid for the lifetime
    /// of `self`.
    pub fn lp_desktop(&self) -> *mut u16 {
        // STARTUPINFO::lpDesktop is `*mut u16`; the kernel does not
        // write through it, but the type signature is non-const.
        self.name_wide.as_ptr() as *mut u16
    }
}

impl Drop for OwnedDesktop {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: we own this HDESK. CloseDesktop fails (and is
            // logged via GetLastError) if any thread is currently
            // using the desktop, but the sandboxed child runs the
            // *only* threads bound to it and `wait_for_exit` has
            // returned by the time we get here — so the close
            // succeeds in practice.
            unsafe {
                CloseDesktop(self.handle);
            }
        }
    }
}

/// Create a fresh isolated desktop and return its handle + qualified
/// name. The name has a per-spawn random suffix; collision with a
/// concurrent borg spawn would surface as `ERROR_ACCESS_DENIED` from
/// `CreateDesktopW`, which we propagate verbatim.
pub fn create_isolated() -> io::Result<OwnedDesktop> {
    let suffix = random_hex(DESKTOP_NAME_ENTROPY_HEX);
    let name = format!("borg-sandbox-{suffix}");
    let mut name_wide: Vec<u16> = OsStr::new(&name).encode_wide().collect();
    name_wide.push(0);

    // Access mask. We need:
    //   DESKTOP_CREATEWINDOW   — the child's USER32 init creates a
    //                            window; without this, the process
    //                            fails at startup.
    //   DESKTOP_READOBJECTS    — required for any GDI/USER read path
    //                            the child takes (font metrics, etc).
    //   DESKTOP_WRITEOBJECTS   — required for the child to render to
    //                            its own implicit desktop.
    //
    // We deliberately omit DESKTOP_HOOKCONTROL / DESKTOP_JOURNALPLAYBACK
    // / DESKTOP_JOURNALRECORD — those would let the child install
    // hooks that survive its lifetime or inject playback into other
    // desktops.
    let access = DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS;
    // SAFETY: name pointer is null-terminated UTF-16; device + SD are
    // documented optional nulls. Flags = 0 means no special creation
    // behavior.
    let handle = unsafe {
        CreateDesktopW(
            name_wide.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            0,
            access,
            ptr::null_mut(),
        )
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedDesktop { handle, name_wide })
}

/// Lower-case hex string of `n` chars from the rand RNG. We avoid the
/// `hex` crate for a six-byte helper; this is the same primitive used
/// in `sid::generate_random_cap_sid_string`.
fn random_hex(n: usize) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..n)
        .map(|_| {
            let c = rng.random_range(0u32..16);
            char::from_digit(c, 16).unwrap_or('0')
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_returns_requested_length() {
        // Spec: the suffix must always be exactly DESKTOP_NAME_ENTROPY_HEX
        // chars. A short suffix would silently weaken collision
        // resistance for the desktop-name namespace.
        for n in [1, 8, 16, 32] {
            assert_eq!(random_hex(n).len(), n);
        }
    }

    #[test]
    fn random_hex_chars_are_lowercase_hex() {
        // Spec: only `0-9a-f` are allowed in the suffix. A stray
        // unicode char would round-trip through UTF-16 unchanged but
        // could fail Windows' desktop-name validation in surprising
        // ways.
        let s = random_hex(64);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }
}
