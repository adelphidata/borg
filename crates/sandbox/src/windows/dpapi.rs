//! DPAPI (`CryptProtectData` / `CryptUnprotectData`) wrapper.
//!
//! Seals sandbox-account credentials so they aren't readable as
//! plaintext from `~/.borg/sandbox-windows/setup.json`. Used by the
//! Phase 7 elevated-runner provisioning step: the install pass
//! generates a random password for `BorgSandboxOffline` /
//! `BorgSandboxOnline`, seals it with DPAPI, and stores the blob.
//! The trampoline / runner unseals on startup before calling
//! `LogonUserW`.
//!
//! # Why `CRYPTPROTECT_LOCAL_MACHINE`
//!
//! Without the flag, DPAPI keys the seal to the *user* who called
//! `CryptProtectData`. That's fine for a per-user borg install, but
//! a service-mode borgd often runs under `LocalSystem` or a service
//! account that does NOT match the user who ran `borg service install
//! --windows-sandbox`. `CRYPTPROTECT_LOCAL_MACHINE` keys to the
//! machine instead, so any process on the host (regardless of user)
//! can unseal. The trade-off: a different *local* user with admin
//! could also unseal — but they could also just read the sandbox
//! account password from LSA, so this isn't a new attack surface.
//!
//! # Threat model
//!
//! DPAPI defeats *file-system-level* secret exposure: a backup tape
//! or a stolen `setup.json` is not enough to recover the sandbox
//! account credentials. It does NOT defeat an attacker who has
//! already-running code on the host with the right privileges — that
//! attacker could call `CryptUnprotectData` directly. The intended
//! defense is "stop trivial offline reads," not "defeat local-admin
//! adversaries."

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{LocalFree, FALSE, HLOCAL};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPT_INTEGER_BLOB,
};

/// Seal `plaintext` using DPAPI machine-scope keys. The resulting
/// blob is safe to write to disk and persists across reboots; any
/// process on the same machine can unseal via [`unseal`].
///
/// `entropy` is optional caller-supplied additional entropy. Passing
/// the same `entropy` at unseal time is required; we use it as a
/// poor-man's domain separation between unrelated DPAPI blobs ("this
/// blob is for `BorgSandboxOffline`'s password, not for some other
/// borg subsystem's secret"). Empty `entropy` is legal.
pub fn seal(plaintext: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let ent_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    // SAFETY: input/entropy blobs reference stack-local slices that
    // outlive the call. Output blob is filled by the API; we LocalFree
    // its pbData below.
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            ptr::null(),
            if entropy.is_empty() {
                ptr::null()
            } else {
                &ent_blob
            },
            ptr::null_mut(),
            ptr::null(),
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    // Copy out, then LocalFree the API's allocation. The blob layout
    // is `[ciphertext bytes]` — no envelope header; DPAPI's own
    // metadata is inside.
    let len = out_blob.cbData as usize;
    // SAFETY: out_blob.pbData is a valid non-null pointer to `len`
    // bytes, per CryptProtectData's documented success path.
    let sealed = unsafe { std::slice::from_raw_parts(out_blob.pbData, len) }.to_vec();
    // SAFETY: out_blob.pbData was allocated by CryptProtectData and
    // must be freed with LocalFree per the API docs.
    unsafe {
        LocalFree(out_blob.pbData as HLOCAL);
    }
    Ok(sealed)
}

/// Reverse of [`seal`]. The same `entropy` must be supplied; mismatch
/// surfaces as `NTE_BAD_DATA` (or a similar OS error).
pub fn unseal(ciphertext: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: ciphertext.len() as u32,
        pbData: ciphertext.as_ptr() as *mut u8,
    };
    let ent_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    // SAFETY: input/entropy reference stack-local slices that outlive
    // the call. We pass NULL for the description out-parameter (we
    // never wrote one in seal); UI is forbidden so this is a pure
    // crypto call with no display interaction.
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            ptr::null_mut(),
            if entropy.is_empty() {
                ptr::null()
            } else {
                &ent_blob
            },
            ptr::null_mut(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    let len = out_blob.cbData as usize;
    // SAFETY: same lifetime contract as in `seal`.
    let plain = unsafe { std::slice::from_raw_parts(out_blob.pbData, len) }.to_vec();
    // SAFETY: API-allocated pointer; LocalFree per docs.
    unsafe {
        LocalFree(out_blob.pbData as HLOCAL);
    }
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests require a real DPAPI implementation, so they only
    // run on Windows. On macOS / Linux CI they're filtered out via
    // the outer `#[cfg(target_os = "windows")]` on the module
    // registration in mod.rs. Within Windows builds, they validate
    // the round-trip and entropy-binding contracts the caller relies on.

    #[test]
    fn round_trip_recovers_plaintext() {
        // Spec: seal followed by unseal with the same entropy must
        // recover the exact input bytes. Any framing drift between
        // the two calls would silently corrupt the unsealed credential.
        let plaintext = b"correct horse battery staple\xff\x00\x01";
        let entropy = b"borg.sandbox.password.v1";
        let sealed = seal(plaintext, entropy).expect("seal");
        let recovered = unseal(&sealed, entropy).expect("unseal");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn unseal_with_wrong_entropy_fails() {
        // Spec: entropy is a domain-separation primitive. Unsealing
        // with a different entropy string must NOT recover the
        // plaintext — otherwise an attacker who obtains one borg
        // DPAPI blob could feed it to a different unseal path that
        // would happily return its contents.
        let plaintext = b"top-secret password";
        let sealed = seal(plaintext, b"entropy-A").expect("seal");
        let attempt = unseal(&sealed, b"entropy-B");
        assert!(attempt.is_err(), "unseal with wrong entropy must error");
    }

    #[test]
    fn empty_entropy_round_trips() {
        // Spec: empty entropy is a documented legal input — we map
        // it to a null pointer at the FFI boundary. The round-trip
        // must still recover plaintext, otherwise our pointer-null
        // shortcut diverges from the API's actual semantics.
        let plaintext = b"no-entropy case";
        let sealed = seal(plaintext, b"").expect("seal empty entropy");
        let recovered = unseal(&sealed, b"").expect("unseal empty entropy");
        assert_eq!(recovered, plaintext);
    }
}
