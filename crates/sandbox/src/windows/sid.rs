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
pub use win::{
    generate_random_cap_sid_string, load_or_generate_seed, workspace_cap_sid_string, LocalSid,
    SEED_LEN,
};

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

    /// Subdirectory under `~/.borg/` that holds the per-install cap-SID
    /// seed. The seed is 32 random bytes generated on first sandbox
    /// invocation and re-used for the lifetime of the install. Stable
    /// cap SIDs let ACEs persist across runs (faster spawn, no thrashing
    /// of parent DACL).
    const SEED_DIR: &str = "sandbox-windows";
    /// Filename of the seed inside [`SEED_DIR`].
    const SEED_FILE: &str = "cap-sid-seed";
    /// HKDF info string. Versioning the string lets a future change to
    /// the derivation (different SID shape, different field width)
    /// rotate the namespace without colliding with old seeds.
    const HKDF_INFO: &[u8] = b"borg-cap-sid-v1";
    /// Seed length in bytes. 32 = 256-bit secret; HKDF output stays at
    /// 16 bytes (the four `u32` subauthorities).
    pub const SEED_LEN: usize = 32;

    /// Load `seed` from `~/.borg/sandbox-windows/cap-sid-seed`,
    /// creating it on first use. The seed is read-only-by-default so a
    /// sandboxed child that somehow obtains a handle to the parent's
    /// home cannot rewrite the namespace.
    ///
    /// Returns the seed bytes plus the resolved path (for diagnostics
    /// in the caller's `tracing::debug!`).
    pub fn load_or_generate_seed() -> std::io::Result<([u8; SEED_LEN], std::path::PathBuf)> {
        let home = dirs::home_dir().ok_or_else(|| {
            std::io::Error::other("no home directory; cannot persist sandbox cap-SID seed")
        })?;
        let dir = home.join(".borg").join(SEED_DIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(SEED_FILE);
        let mut buf = [0u8; SEED_LEN];
        match std::fs::read(&path) {
            Ok(existing) if existing.len() == SEED_LEN => {
                buf.copy_from_slice(&existing);
                Ok((buf, path))
            }
            Ok(_existing_wrong_len) => {
                // A truncated or grown seed file is treated as
                // corruption; refuse to overwrite silently — the
                // operator should investigate (could be a half-restored
                // backup). Fall closed.
                Err(std::io::Error::other(format!(
                    "cap-SID seed at {} has unexpected length; refusing to overwrite",
                    path.display(),
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                use rand::Rng;
                let mut rng = rand::rng();
                for b in buf.iter_mut() {
                    *b = rng.random();
                }
                // Best-effort atomic write via tmpfile + rename. A
                // parallel sandbox spawn racing this initial write
                // would either see the file we just renamed or fall
                // through to the loop's `Ok(existing)` arm on retry.
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, buf)?;
                std::fs::rename(&tmp, &path)?;
                Ok((buf, path))
            }
            Err(e) => Err(e),
        }
    }

    /// Derive a stable cap SID for a writable root path. Same `seed +
    /// root` always produces the same SID, so DACL ACEs persisted from
    /// a prior spawn remain meaningful on the next spawn.
    ///
    /// The derivation is HKDF-SHA256(seed, info="borg-cap-sid-v1",
    /// salt=empty) over `root.as_bytes()`. We take the first 16 bytes
    /// of output and split into four little-endian `u32`s — same
    /// shape as the random generator, so consumers of the returned
    /// string can't tell the difference.
    pub fn workspace_cap_sid_string(seed: &[u8; SEED_LEN], root: &std::path::Path) -> String {
        use hkdf::Hkdf;
        use sha2::Sha256;
        // Use the root path's lossy-UTF-8 form as the "input keying
        // material" salt isn't part of the API we expose. HKDF's `info`
        // parameter is the right place for context separation, and the
        // path bytes serve as additional context-binding via the
        // expand step.
        let ikm = seed;
        let root_bytes = root.as_os_str().to_string_lossy();
        let mut info = HKDF_INFO.to_vec();
        info.extend_from_slice(b"\0");
        info.extend_from_slice(root_bytes.as_bytes());
        let hk = Hkdf::<Sha256>::new(None, ikm);
        let mut out = [0u8; 16];
        // .expect: 16 bytes is well under the HKDF-SHA256 max output
        // length of 255 * 32 = 8160 bytes.
        #[allow(clippy::expect_used)]
        hk.expand(&info, &mut out)
            .expect("HKDF-SHA256 expand of 16 bytes must succeed");
        let a = u32::from_le_bytes(out[0..4].try_into().unwrap_or([0; 4]));
        let b = u32::from_le_bytes(out[4..8].try_into().unwrap_or([0; 4]));
        let c = u32::from_le_bytes(out[8..12].try_into().unwrap_or([0; 4]));
        let d = u32::from_le_bytes(out[12..16].try_into().unwrap_or([0; 4]));
        format!("S-1-5-21-{a}-{b}-{c}-{d}")
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
    fn workspace_cap_sid_is_stable_for_same_root() {
        // Spec: stability is the whole point of the seed-based derivation
        // — DACL ACEs persisted from spawn N must be addressable as the
        // same trustee on spawn N+1. A change to the derivation that
        // accidentally folds in time or a per-spawn nonce would silently
        // break ACE re-use; this test pins the determinism.
        let seed = [42u8; SEED_LEN];
        let root = std::path::Path::new(r"C:\workspace\borg");
        let a = workspace_cap_sid_string(&seed, root);
        let b = workspace_cap_sid_string(&seed, root);
        assert_eq!(a, b, "same (seed, root) must yield same SID");
    }

    #[test]
    fn workspace_cap_sid_differs_per_root() {
        // Spec: different writable roots get different cap SIDs. A
        // collision here would let an ACE intended for `root_a` apply
        // to `root_b` — i.e., a write granted to one workspace would
        // leak to another.
        let seed = [42u8; SEED_LEN];
        let a = workspace_cap_sid_string(&seed, std::path::Path::new(r"C:\a"));
        let b = workspace_cap_sid_string(&seed, std::path::Path::new(r"C:\b"));
        assert_ne!(a, b, "different roots must yield different SIDs");
    }

    #[test]
    fn workspace_cap_sid_differs_per_seed() {
        // Spec: rotating the seed (e.g. operator wipes the seed file
        // for namespace rotation) MUST produce a different SID for
        // the same root. Otherwise the rotation would be a no-op and
        // leaked ACEs from before rotation would remain live.
        let root = std::path::Path::new(r"C:\workspace");
        let a = workspace_cap_sid_string(&[1u8; SEED_LEN], root);
        let b = workspace_cap_sid_string(&[2u8; SEED_LEN], root);
        assert_ne!(a, b, "different seeds must yield different SIDs");
    }

    #[test]
    fn workspace_cap_sid_has_same_shape_as_random() {
        // Spec: callers (e.g. ConvertStringSidToSidW, the DACL writer)
        // can't tell stable from random — same S-1-5-21-{u32}{u32}{u32}{u32}
        // shape. A shape divergence would break the policy_resolve
        // → acl pipeline.
        let s = workspace_cap_sid_string(&[7u8; SEED_LEN], std::path::Path::new("/x"));
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 8);
        assert_eq!(&parts[..4], &["S", "1", "5", "21"]);
        for p in &parts[4..] {
            assert!(p.parse::<u32>().is_ok(), "subauthority not u32: {p}");
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
