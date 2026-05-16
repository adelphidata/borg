//! Capability-SID generation for the Tier 1 Windows sandbox.
//!
//! The sandbox uses a per-spawn random "capability SID" as the
//! restricting SID on the child's restricted token, and as the trustee
//! on every DACL ACE the sandbox writes. The intersection of (original
//! token's groups) ∩ (restricting SIDs) governs every access check, so
//! a path with no ALLOW ACE for the cap SID is unreachable from the
//! child even if the parent user has access to it.
//!
//! The string-formatted SID generator and the `LocalSid` PSID wrapper
//! both compile only on Windows (the SID is consumed by Win32 APIs and
//! `rand` is a Windows-only target dep on this crate).

#[cfg(target_os = "windows")]
pub use win::{generate_random_cap_sid_string, LocalSid};

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use rand::Rng;
    use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::PSID;

    /// Build a random capability-SID string of the form
    /// `S-1-5-21-{a}-{b}-{c}-{d}` where each suffix is a 32-bit
    /// unsigned random value.
    ///
    /// `S-1-5-21-...` is the SID authority + RID prefix Microsoft
    /// reserves for "issued by a domain or local SAM" — the same shape
    /// Windows uses for real user/group SIDs. Using this prefix means
    /// the kernel treats our cap SID like any other principal in
    /// access checks (DACLs, group membership, etc.) without special-
    /// casing.
    ///
    /// Each invocation produces a fresh SID; we do NOT persist these
    /// across spawns. ACEs leaked from a crashed run reference a cap
    /// SID no future run will ever regenerate, so they are inert —
    /// they still occupy DACL slots and should be reaped, but they
    /// cannot grant or deny to any real principal.
    pub fn generate_random_cap_sid_string() -> String {
        let mut rng = rand::rng();
        format!(
            "S-1-5-21-{}-{}-{}-{}",
            rng.random::<u32>(),
            rng.random::<u32>(),
            rng.random::<u32>(),
            rng.random::<u32>(),
        )
    }

    /// Owning wrapper over a `PSID` allocated by
    /// `ConvertStringSidToSidW`. `Drop` calls `LocalFree` so the
    /// allocation is released even on early-return / panic paths.
    pub struct LocalSid {
        psid: PSID,
    }

    impl LocalSid {
        /// Convert a string-form SID (`"S-1-5-21-..."`) into a kernel
        /// `PSID`. Returns the OS error on conversion failure (almost
        /// always means the input is not a valid string SID).
        pub fn from_str_sid(sid_str: &str) -> io::Result<Self> {
            // SAFETY: ConvertStringSidToSidW takes a UTF-16 wide string
            // and an out-pointer to a PSID. We pass valid pointers to
            // both. The function returns 0 on failure, in which case
            // the out-pointer is unmodified and we return the OS error
            // without calling LocalFree.
            let wide: Vec<u16> = OsStr::new(sid_str)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut psid: PSID = ptr::null_mut();
            let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if psid.is_null() {
                return Err(io::Error::other(
                    "ConvertStringSidToSidW returned success but null PSID",
                ));
            }
            Ok(LocalSid { psid })
        }

        /// Borrow the inner `PSID`. The pointer is valid for the
        /// lifetime of this `LocalSid`.
        pub fn as_psid(&self) -> PSID {
            self.psid
        }
    }

    impl Drop for LocalSid {
        fn drop(&mut self) {
            if !self.psid.is_null() {
                // SAFETY: `psid` was returned by ConvertStringSidToSidW
                // which docs explicitly say must be freed with
                // LocalFree. We only free once because Drop runs once.
                unsafe {
                    LocalFree(self.psid as HLOCAL);
                }
            }
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn generated_sid_has_expected_shape() {
        // Spec: must match S-1-5-21-{u32}-{u32}-{u32}-{u32}. The kernel
        // SID parser is strict — a malformed prefix (e.g. S-1-5-22) or
        // a non-numeric subauthority would be rejected by
        // ConvertStringSidToSidW at runtime. Catch the bug at the unit-
        // test layer instead of waiting for a Windows-only failure.
        let s = generate_random_cap_sid_string();
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 8, "expected 8 dash-separated parts: {s}");
        assert_eq!(parts[0], "S");
        assert_eq!(parts[1], "1");
        assert_eq!(parts[2], "5");
        assert_eq!(parts[3], "21");
        for part in &parts[4..] {
            assert!(
                part.parse::<u32>().is_ok(),
                "subauthority must be a u32: {part:?}"
            );
        }
    }

    #[test]
    fn generated_sids_are_unique_across_calls() {
        // Spec: each spawn gets a fresh SID. Reuse would mean a DACL
        // ACE leaked from a previous run becomes live again the next
        // time the same SID is regenerated — a path the operator
        // intended to deny could become writable. With 128 random bits
        // this collision is astronomically unlikely; the test is a
        // sanity check that we're actually consuming the RNG.
        let a = generate_random_cap_sid_string();
        let b = generate_random_cap_sid_string();
        assert_ne!(a, b, "two SIDs in a row should not collide");
    }
}
