//! Windows Filtering Platform — per-account outbound block.
//!
//! Installs persistent WFP filter rules that deny all outbound
//! connections for the `BorgSandboxOffline` account SID. With these
//! rules in place, the operating system enforces the `network=false`
//! policy bit at the kernel filter layer (ALE connect-authorization)
//! rather than relying on application-layer politeness.
//!
//! # Provider and sublayer GUIDs
//!
//! Both are persistent — created at `borg service install
//! --windows-sandbox` time and removed by the matching uninstall.
//! They identify rules as ours so we can locate + remove them on
//! upgrade or reset. The GUIDs below are fresh for borg; do NOT reuse
//! them across forks or other projects.
//!
//! # Filter shape
//!
//! For each of (IPv4, IPv6):
//!
//! ```text
//! layer      = FWPM_LAYER_ALE_AUTH_CONNECT_V{4,6}
//! action     = FWP_ACTION_BLOCK
//! conditions = [ALE_USER_ID matches a security descriptor granting the offline SID]
//! flags      = PERSISTENT
//! ```
//!
//! `PERSISTENT` is load-bearing: the filter survives reboot and is
//! enforced from the moment the Base Filtering Engine starts, which is
//! before `borgd` (the only thing that spawns sandboxed processes) can
//! run. A non-persistent filter could be raced by a child that starts in
//! the window between boot and `borgd` reinstalling its filters.
//! `BOOTTIME` is not used: it covers only the window before BFE starts,
//! when no sandboxed process can exist, and WFP rejects it combined with
//! `PERSISTENT` (`FWP_E_INVALID_FLAGS`).
//!
//! # ALE_USER_ID matching
//!
//! WFP's `FWPM_CONDITION_ALE_USER_ID` field is matched against a
//! *security descriptor*, not a raw SID: the kernel runs an access
//! check of the connecting process's token against the descriptor and
//! the condition matches when the check passes. We build a self-relative
//! SD from the offline account SID via
//! `ConvertStringSecurityDescriptorToSecurityDescriptorW` so any process
//! running as that account (and only that account) trips the block.
//!
//! # Cross-platform
//!
//! The filter *specifications* ([`filter_specs`]) are plain data and
//! compile + unit-test on every runner. The `Fwpm*` FFI that consumes
//! them is gated to Windows; on other targets the public entry points
//! return [`io::ErrorKind::Unsupported`] so callers compile but never
//! claim enforcement off-platform.

use std::io;

/// Persistent WFP provider GUID for borg. Identifies rules created by
/// `borg service install --windows-sandbox` so they can be located +
/// removed on uninstall or upgrade.
///
/// Generated fresh for borg, NOT a re-used or well-known GUID. If you
/// fork the codebase, generate your own to avoid filter-collision with
/// upstream borg installs on the same host.
pub const PROVIDER_GUID: &str = "B07B6E8A-1F4C-4D2E-9A60-BA1F4C0DE001";

/// Persistent WFP sublayer GUID for borg. The sublayer determines
/// filter ordering relative to other vendors' WFP filters; a dedicated
/// sublayer lets the borg block-rule sit above well-known allow-rules
/// from antivirus / VPN vendors.
pub const SUBLAYER_GUID: &str = "B07B6E8A-1F4C-4D2E-9A60-BA1F4C0DE002";

/// Filter GUID for the IPv4 outbound block. Deterministic so uninstall
/// can delete by key without enumerating.
pub const FILTER_V4_GUID: &str = "B07B6E8A-1F4C-4D2E-9A60-BA1F4C0DE004";

/// Filter GUID for the IPv6 outbound block.
pub const FILTER_V6_GUID: &str = "B07B6E8A-1F4C-4D2E-9A60-BA1F4C0DE006";

/// Cross-platform filter specifications. Pure data describing *what*
/// filters Tier 2 installs, separated from the Win32 FFI that installs
/// them so the policy is unit-testable on every runner.
pub mod filter_specs {
    /// One WFP filter to install. The Windows installer maps each spec
    /// onto an `FWPM_FILTER0` at the corresponding ALE connect layer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FilterSpec {
        /// Human-readable name shown in `netsh wfp show filters` output.
        pub name: &'static str,
        /// Stable filter GUID (string form) used as the filter key so
        /// uninstall can delete it without enumeration.
        pub key: &'static str,
        /// `false` → IPv4 (`ALE_AUTH_CONNECT_V4`); `true` → IPv6.
        pub ipv6: bool,
        /// `true` → `FWP_ACTION_BLOCK`. Always block for the offline
        /// account; the field exists to make the intent explicit and
        /// testable rather than implied by the function name.
        pub block: bool,
        /// `true` → `FWPM_FILTER_FLAG_PERSISTENT` (survives reboot).
        pub persistent: bool,
    }

    /// The two filters that block all outbound network for the offline
    /// sandbox account: one at the IPv4 ALE connect layer, one at IPv6.
    /// Both persistent so the block can't be raced across a reboot.
    pub fn offline_outbound_block() -> Vec<FilterSpec> {
        vec![
            FilterSpec {
                name: "Borg sandbox offline outbound block (IPv4)",
                key: super::FILTER_V4_GUID,
                ipv6: false,
                block: true,
                persistent: true,
            },
            FilterSpec {
                name: "Borg sandbox offline outbound block (IPv6)",
                key: super::FILTER_V6_GUID,
                ipv6: true,
                block: true,
                persistent: true,
            },
        ]
    }
}

/// Install persistent WFP filters that deny all outbound (IPv4 + IPv6)
/// for the supplied user SID.
///
/// Caller must hold admin rights. The install runs inside a WFP
/// transaction so a partial failure aborts cleanly and never leaves the
/// host with half the rules. Idempotent: re-installing over existing
/// borg filters succeeds (existing objects are tolerated).
#[cfg(target_os = "windows")]
pub fn install_offline_outbound_block(offline_sid: &str) -> io::Result<()> {
    ffi::install(offline_sid)
}

/// Non-Windows stub. The Tier 2 installer only calls this on Windows;
/// the body exists so the crate compiles on every target.
#[cfg(not(target_os = "windows"))]
pub fn install_offline_outbound_block(_offline_sid: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "WFP filters are a Windows-only feature",
    ))
}

/// Remove the WFP filters, sublayer, and provider installed by
/// [`install_offline_outbound_block`]. Tolerates already-absent objects
/// so `borg service uninstall --windows-sandbox` is safe to run twice.
#[cfg(target_os = "windows")]
pub fn uninstall_offline_outbound_block() -> io::Result<()> {
    ffi::uninstall()
}

/// Non-Windows stub.
#[cfg(not(target_os = "windows"))]
pub fn uninstall_offline_outbound_block() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "WFP filters are a Windows-only feature",
    ))
}

#[cfg(target_os = "windows")]
mod ffi {
    use std::io;
    use std::ptr;

    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
        FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0, FwpmFilterDeleteByKey0,
        FwpmProviderAdd0, FwpmProviderDeleteByKey0, FwpmSubLayerAdd0, FwpmSubLayerDeleteByKey0,
        FwpmTransactionAbort0, FwpmTransactionBegin0, FwpmTransactionCommit0, FWPM_ACTION0,
        FWPM_ACTION0_0, FWPM_CONDITION_ALE_USER_ID, FWPM_DISPLAY_DATA0, FWPM_FILTER0,
        FWPM_FILTER0_0, FWPM_FILTER_CONDITION0, FWPM_FILTER_FLAG_PERSISTENT,
        FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWPM_PROVIDER0,
        FWPM_PROVIDER_FLAG_PERSISTENT, FWPM_SESSION0, FWPM_SUBLAYER0,
        FWPM_SUBLAYER_FLAG_PERSISTENT, FWP_ACTION_BLOCK, FWP_CONDITION_VALUE0,
        FWP_CONDITION_VALUE0_0, FWP_EMPTY, FWP_MATCH_EQUAL, FWP_SECURITY_DESCRIPTOR_TYPE,
        FWP_VALUE0, FWP_VALUE0_0,
    };
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};

    // FWP_BYTE_BLOB lives under the WFP module in windows-sys.
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::FWP_BYTE_BLOB;

    /// RPC authentication service for the local WFP engine. `0xFFFF_FFFF`
    /// is `RPC_C_AUTHN_DEFAULT`; using it avoids pulling in the
    /// `Win32_System_Rpc` feature just for the named constant.
    const RPC_C_AUTHN_DEFAULT: u32 = 0xFFFF_FFFF;
    /// `SDDL_REVISION_1` for the SD-from-SDDL conversion.
    const SDDL_REVISION_1: u32 = 1;

    use crate::windows::wide_nul as wide;

    /// Parse a hyphenated GUID string (the form stored in our consts)
    /// into a windows-sys `GUID`. Our GUID consts are compile-time
    /// constants we control, so a parse failure is a programming error,
    /// surfaced as an `io::Error` rather than panicking the installer.
    fn guid_from_str(s: &str) -> io::Result<GUID> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 {
            return Err(io::Error::other(format!("malformed GUID const: {s}")));
        }
        let v = u128::from_str_radix(&hex, 16)
            .map_err(|e| io::Error::other(format!("GUID parse {s}: {e}")))?;
        Ok(GUID::from_u128(v))
    }

    fn check(code: u32, ctx: &str) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32).into_ctx(ctx))
        }
    }

    /// Tolerant variant: treats a specific "already exists" / "not
    /// found" code as success so install/uninstall are idempotent.
    fn check_or(code: u32, ctx: &str, tolerate: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS || code == tolerate {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32).into_ctx(ctx))
        }
    }

    // FWP error codes (HRESULT-shaped). Mirrored locally — windows-sys
    // exposes them but pinning the two we tolerate keeps intent clear.
    const FWP_E_ALREADY_EXISTS: u32 = 0x80320009;
    const FWP_E_FILTER_NOT_FOUND: u32 = 0x80320003;
    const FWP_E_PROVIDER_NOT_FOUND: u32 = 0x80320005;
    const FWP_E_SUBLAYER_NOT_FOUND: u32 = 0x80320007;

    trait IoCtx {
        fn into_ctx(self, ctx: &str) -> io::Error;
    }
    impl IoCtx for io::Error {
        fn into_ctx(self, ctx: &str) -> io::Error {
            io::Error::new(self.kind(), format!("{ctx}: {self}"))
        }
    }

    /// RAII handle to an open WFP engine session.
    struct Engine(HANDLE);
    impl Engine {
        fn open() -> io::Result<Self> {
            let mut session: FWPM_SESSION0 = unsafe { std::mem::zeroed() };
            session.txnWaitTimeoutInMSec = 0;
            let mut handle: HANDLE = ptr::null_mut();
            // SAFETY: session is a zeroed valid struct; out-pointer is
            // stack-local. Server name NULL = local engine.
            let rc = unsafe {
                FwpmEngineOpen0(
                    ptr::null(),
                    RPC_C_AUTHN_DEFAULT,
                    ptr::null(),
                    &session,
                    &mut handle,
                )
            };
            check(rc, "FwpmEngineOpen0")?;
            Ok(Self(handle))
        }
        fn raw(&self) -> HANDLE {
            self.0
        }
    }
    impl Drop for Engine {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: handle came from a successful FwpmEngineOpen0.
                unsafe {
                    FwpmEngineClose0(self.0);
                }
            }
        }
    }

    /// Transaction guard: aborts on drop unless `commit()` was called.
    struct Transaction<'e> {
        engine: &'e Engine,
        done: bool,
    }
    impl<'e> Transaction<'e> {
        fn begin(engine: &'e Engine) -> io::Result<Self> {
            // SAFETY: engine handle is valid for the borrow.
            let rc = unsafe { FwpmTransactionBegin0(engine.raw(), 0) };
            check(rc, "FwpmTransactionBegin0")?;
            Ok(Self {
                engine,
                done: false,
            })
        }
        fn commit(mut self) -> io::Result<()> {
            // SAFETY: in-progress transaction on a valid engine.
            let rc = unsafe { FwpmTransactionCommit0(self.engine.raw()) };
            self.done = true;
            check(rc, "FwpmTransactionCommit0")
        }
    }
    impl Drop for Transaction<'_> {
        fn drop(&mut self) {
            if !self.done {
                // SAFETY: aborting an in-progress transaction is the
                // documented rollback path; ignore the result on the
                // teardown path (there is nothing to recover to).
                unsafe {
                    FwpmTransactionAbort0(self.engine.raw());
                }
            }
        }
    }

    /// Self-relative security descriptor built from a SID, used as the
    /// ALE_USER_ID condition value. Frees the SD on drop.
    struct UserSd {
        psd: PSECURITY_DESCRIPTOR,
        blob: FWP_BYTE_BLOB,
    }
    impl UserSd {
        fn for_sid(sid: &str) -> io::Result<Self> {
            // WFP matches FWPM_CONDITION_ALE_USER_ID by running an access
            // check of the connecting process's token against this SD.
            // Grant the offline SID FULL access (`FA`) so that check
            // unambiguously succeeds for that account regardless of which
            // access right the engine probes with — and fails for every
            // other principal (single-ACE DACL = implicit deny). A
            // narrower mask risks the check not intersecting the probe
            // right, silently leaving the block un-applied. Owner/group =
            // LocalSystem (`SY`) to form a valid self-relative SD.
            let sddl = wide(&format!("O:SYG:SYD:(A;;FA;;;{sid})"));
            let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let mut len: u32 = 0;
            // SAFETY: sddl is null-terminated; out-pointers are local.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    &mut len,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error()
                    .into_ctx("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
            }
            // `len` from the conversion can be 0 on some SDKs; fall back
            // to GetSecurityDescriptorLength which is always populated.
            // SAFETY: psd is a valid SD from the successful conversion.
            let real_len = unsafe { GetSecurityDescriptorLength(psd) };
            Ok(Self {
                psd,
                blob: FWP_BYTE_BLOB {
                    size: real_len,
                    data: psd as *mut u8,
                },
            })
        }
        fn blob_ptr(&self) -> *mut FWP_BYTE_BLOB {
            &self.blob as *const FWP_BYTE_BLOB as *mut FWP_BYTE_BLOB
        }
    }
    impl Drop for UserSd {
        fn drop(&mut self) {
            if !self.psd.is_null() {
                // SAFETY: psd was allocated by the conversion API, which
                // documents LocalFree as the matching deallocator.
                unsafe {
                    LocalFree(self.psd as HLOCAL);
                }
            }
        }
    }

    fn empty_value() -> FWP_VALUE0 {
        FWP_VALUE0 {
            r#type: FWP_EMPTY,
            Anonymous: FWP_VALUE0_0 { uint8: 0 },
        }
    }

    pub(super) fn install(offline_sid: &str) -> io::Result<()> {
        let provider_key = guid_from_str(super::PROVIDER_GUID)?;
        let sublayer_key = guid_from_str(super::SUBLAYER_GUID)?;

        let engine = Engine::open()?;
        let txn = Transaction::begin(&engine)?;

        // Provider.
        let mut prov_name = wide("Borg sandbox");
        let mut provider: FWPM_PROVIDER0 = unsafe { std::mem::zeroed() };
        provider.providerKey = provider_key;
        provider.displayData = FWPM_DISPLAY_DATA0 {
            name: prov_name.as_mut_ptr(),
            description: ptr::null_mut(),
        };
        provider.flags = FWPM_PROVIDER_FLAG_PERSISTENT;
        // SAFETY: provider is fully initialized; engine handle valid.
        let rc = unsafe { FwpmProviderAdd0(engine.raw(), &provider, ptr::null_mut()) };
        check_or(rc, "FwpmProviderAdd0", FWP_E_ALREADY_EXISTS)?;

        // Sublayer.
        let mut sub_name = wide("Borg sandbox sublayer");
        let mut sublayer: FWPM_SUBLAYER0 = unsafe { std::mem::zeroed() };
        sublayer.subLayerKey = sublayer_key;
        sublayer.displayData = FWPM_DISPLAY_DATA0 {
            name: sub_name.as_mut_ptr(),
            description: ptr::null_mut(),
        };
        sublayer.flags = FWPM_SUBLAYER_FLAG_PERSISTENT;
        sublayer.providerKey = &provider_key as *const GUID as *mut GUID;
        sublayer.weight = 0x8000;
        // SAFETY: sublayer fully initialized; engine handle valid.
        let rc = unsafe { FwpmSubLayerAdd0(engine.raw(), &sublayer, ptr::null_mut()) };
        check_or(rc, "FwpmSubLayerAdd0", FWP_E_ALREADY_EXISTS)?;

        // One condition shared by both filters: connecting token matches
        // the offline account SID.
        let user_sd = UserSd::for_sid(offline_sid)?;
        let mut condition = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_ALE_USER_ID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    sd: user_sd.blob_ptr(),
                },
            },
        };

        for spec in super::filter_specs::offline_outbound_block() {
            let layer = if spec.ipv6 {
                FWPM_LAYER_ALE_AUTH_CONNECT_V6
            } else {
                FWPM_LAYER_ALE_AUTH_CONNECT_V4
            };
            let mut flags = 0u32;
            if spec.persistent {
                flags |= FWPM_FILTER_FLAG_PERSISTENT;
            }
            let mut filter_name = wide(spec.name);
            let filter_key = guid_from_str(spec.key)?;
            let mut filter: FWPM_FILTER0 = unsafe { std::mem::zeroed() };
            filter.filterKey = filter_key;
            filter.displayData = FWPM_DISPLAY_DATA0 {
                name: filter_name.as_mut_ptr(),
                description: ptr::null_mut(),
            };
            filter.flags = flags;
            filter.providerKey = &provider_key as *const GUID as *mut GUID;
            filter.layerKey = layer;
            filter.subLayerKey = sublayer_key;
            filter.weight = empty_value();
            filter.numFilterConditions = 1;
            filter.filterCondition = &mut condition;
            filter.action = FWPM_ACTION0 {
                r#type: FWP_ACTION_BLOCK,
                Anonymous: FWPM_ACTION0_0 {
                    filterType: GUID::from_u128(0),
                },
            };
            filter.Anonymous = FWPM_FILTER0_0 { rawContext: 0 };
            // effectiveWeight is an FWP_VALUE0; zeroed() set it to a
            // valid FWP_EMPTY already, but set explicitly for clarity.
            filter.effectiveWeight = empty_value();

            let mut filter_id: u64 = 0;
            // SAFETY: filter fully initialized; condition + names + SD
            // outlive this call (all on this stack frame / borrowed).
            let rc =
                unsafe { FwpmFilterAdd0(engine.raw(), &filter, ptr::null_mut(), &mut filter_id) };
            check_or(rc, "FwpmFilterAdd0", FWP_E_ALREADY_EXISTS)?;
        }

        txn.commit()
    }

    pub(super) fn uninstall() -> io::Result<()> {
        let provider_key = guid_from_str(super::PROVIDER_GUID)?;
        let sublayer_key = guid_from_str(super::SUBLAYER_GUID)?;
        let v4 = guid_from_str(super::FILTER_V4_GUID)?;
        let v6 = guid_from_str(super::FILTER_V6_GUID)?;

        let engine = Engine::open()?;
        let txn = Transaction::begin(&engine)?;

        // Delete filters first (they reference the sublayer + provider).
        // SAFETY: keys are valid GUIDs; engine handle valid.
        let rc = unsafe { FwpmFilterDeleteByKey0(engine.raw(), &v4) };
        check_or(rc, "FwpmFilterDeleteByKey0(v4)", FWP_E_FILTER_NOT_FOUND)?;
        let rc = unsafe { FwpmFilterDeleteByKey0(engine.raw(), &v6) };
        check_or(rc, "FwpmFilterDeleteByKey0(v6)", FWP_E_FILTER_NOT_FOUND)?;
        let rc = unsafe { FwpmSubLayerDeleteByKey0(engine.raw(), &sublayer_key) };
        check_or(rc, "FwpmSubLayerDeleteByKey0", FWP_E_SUBLAYER_NOT_FOUND)?;
        let rc = unsafe { FwpmProviderDeleteByKey0(engine.raw(), &provider_key) };
        check_or(rc, "FwpmProviderDeleteByKey0", FWP_E_PROVIDER_NOT_FOUND)?;

        txn.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_sublayer_and_filter_guids_are_all_distinct() {
        // Spec: provider, sublayer, and the two filter keys share the
        // GUID namespace inside WFP. A collision would make the second
        // *Add0 fail with FWP_E_ALREADY_EXISTS at a confusing call site.
        let all = [PROVIDER_GUID, SUBLAYER_GUID, FILTER_V4_GUID, FILTER_V6_GUID];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "GUIDs {i} and {j} collide");
            }
        }
    }

    #[test]
    fn offline_block_has_one_v4_and_one_v6_filter() {
        // Spec: dual-stack hosts route over IPv6 by default; a v4-only
        // block would silently leak all traffic over IPv6. Both layers
        // must be covered.
        let specs = filter_specs::offline_outbound_block();
        assert_eq!(specs.len(), 2, "expected exactly v4 + v6");
        assert_eq!(specs.iter().filter(|s| !s.ipv6).count(), 1, "one v4");
        assert_eq!(specs.iter().filter(|s| s.ipv6).count(), 1, "one v6");
    }

    #[test]
    fn every_offline_filter_blocks_and_is_persistent() {
        // Spec: an allow-by-accident or a non-persistent filter would
        // defeat the network=false guarantee — silently (allow) or across
        // a reboot window (persistence). Pin both bits for every spec.
        for spec in filter_specs::offline_outbound_block() {
            assert!(spec.block, "{} must BLOCK", spec.name);
            assert!(spec.persistent, "{} must be persistent", spec.name);
        }
    }

    #[test]
    fn filter_keys_match_the_dedicated_filter_guids() {
        // Spec: uninstall deletes filters by these exact keys. If a spec
        // key drifts from the FILTER_V*_GUID consts, uninstall would
        // leave orphaned filters that keep blocking after teardown.
        let specs = filter_specs::offline_outbound_block();
        let v4 = specs.iter().find(|s| !s.ipv6).unwrap();
        let v6 = specs.iter().find(|s| s.ipv6).unwrap();
        assert_eq!(v4.key, FILTER_V4_GUID);
        assert_eq!(v6.key, FILTER_V6_GUID);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn non_windows_install_is_unsupported_not_silent_success() {
        // Spec: off-platform the entry points must error, never claim a
        // no-op install succeeded (which would let setup.json record
        // wfp_filters_installed = true on a host with no WFP).
        assert_eq!(
            install_offline_outbound_block("S-1-5-21-1-2-3-1001")
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            uninstall_offline_outbound_block().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
