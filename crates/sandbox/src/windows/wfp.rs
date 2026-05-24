//! Windows Filtering Platform — per-account outbound block.
//!
//! Installs persistent WFP filter
//! rules that deny all outbound TCP/UDP for the `BorgSandboxOffline`
//! account SID. With these rules in place, the operating system
//! enforces the `network=false` policy bit at the kernel filter layer
//! rather than relying on application-layer politeness.
//!
//! # Provider and sublayer GUIDs
//!
//! Both are persistent — created at `borg service install
//! --windows-sandbox` time and removed by the matching uninstall.
//! They identify rules as ours so we can locate + remove them on
//! upgrade or reset. The GUIDs below are fresh for borg; do NOT
//! reuse them across forks or other projects.
//!
//! # Filter shape
//!
//! For each of (IPv4, IPv6):
//!
//! ```text
//! layer = FWPM_LAYER_ALE_AUTH_CONNECT_V{4,6}
//! action = FWP_ACTION_BLOCK
//! conditions = [ALE_USER_ID matches offline_sid]
//! flags = PERSISTENT | BOOTTIME
//! ```
//!
//! `BOOTTIME` plus `PERSISTENT` is the load-bearing pair: the filter
//! survives reboot AND is active before any sandboxed process runs,
//! even at first-boot before borgd has started. A non-persistent or
//! non-boottime filter could be raced by a child that starts in the
//! window between boot and `borgd` reinstalling its filters.
//!
//! # Status — scaffolding only
//!
//! This module ships the GUIDs, the API signature, and an
//! `Err(NotImplemented)` body. The full WFP install requires a
//! transactional `FwpmTransactionBegin0` → `FwpmFilterAdd0` ×2 →
//! `FwpmTransactionCommit0` plus matching teardown, which is several
//! hundred lines of FFI we don't ship until we can test it on a
//! Windows host with admin. The contract here is what
//! `borg service install --windows-sandbox` will plug into.

use std::io;

/// Persistent WFP provider GUID for borg. Identifies rules created by
/// `borg service install --windows-sandbox` so they can be located
/// + removed on uninstall or upgrade.
///
/// Format: `{B0123456-7890-4ABC-DEF0-123456789ABC}` — generated
/// fresh for borg, NOT a re-used or well-known GUID. If you fork the
/// codebase, generate your own to avoid filter-collision with
/// upstream borg installs on the same host.
pub const PROVIDER_GUID: &str = "B07B6E8A-1F4C-4D2E-9A60-BA1F4C0DE001";

/// Persistent WFP sublayer GUID for borg. Sublayer determines filter
/// ordering relative to other vendors' WFP filters; a dedicated
/// sublayer lets the borg block-rule sit above well-known
/// allow-rules from antivirus / VPN vendors.
pub const SUBLAYER_GUID: &str = "B07B6E8A-1F4C-4D2E-9A60-BA1F4C0DE002";

/// Install persistent WFP filters that deny all outbound TCP/UDP
/// (IPv4 + IPv6) for the supplied user SID.
///
/// Caller must hold admin rights. Returns `Err` with the OS error on
/// any sub-step failure; the WFP transaction is aborted so a partial
/// install never leaves the host in an inconsistent state.
///
/// # Status
///
/// Returns `Err(io::ErrorKind::Unsupported)` until the operational
/// FFI lands (see ROADMAP "Tier 2 elevated backend"). Callers should
/// treat the error as the "install couldn't enforce network blocking;
/// fall back to `wfp_filters_installed = false` and surface a
/// warning" branch.
pub fn install_offline_outbound_block(_offline_sid: &str) -> io::Result<()> {
    // TODO: FwpmEngineOpen0 → FwpmTransactionBegin0 →
    //   FwpmProviderAdd0 (provider GUID, persistent)
    //   FwpmSubLayerAdd0 (sublayer GUID, persistent, weight=max)
    //   FwpmFilterAdd0 (layer=ALE_AUTH_CONNECT_V4, action=BLOCK,
    //                   conditions=[ALE_USER_ID==offline_sid],
    //                   flags=PERSISTENT|BOOTTIME)
    //   FwpmFilterAdd0 (same with V6)
    // → FwpmTransactionCommit0
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "WFP filter install not yet implemented; setup.json will record \
         wfp_filters_installed = false so the trampoline surfaces the gap",
    ))
}

/// Remove the WFP filters installed by [`install_offline_outbound_block`].
/// Tear-down counterpart for `borg service uninstall
/// --windows-sandbox`.
///
/// # Status
///
/// `Err(io::ErrorKind::Unsupported)` until the operational FFI lands.
pub fn uninstall_offline_outbound_block() -> io::Result<()> {
    // TODO: FwpmEngineOpen0 → enumerate filters via
    // FwpmFilterEnum0 with provider_key = PROVIDER_GUID → delete
    // each via FwpmFilterDeleteByKey0 → FwpmSubLayerDeleteByKey0
    // → FwpmProviderDeleteByKey0.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "WFP filter uninstall not yet implemented",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_and_sublayer_guids_are_distinct() {
        // Spec: provider and sublayer must be different GUIDs.
        // Sharing them would cause `FwpmSubLayerAdd0` to fail with
        // `FWP_E_ALREADY_EXISTS` since the GUID namespace is
        // shared between WFP object types.
        assert_ne!(PROVIDER_GUID, SUBLAYER_GUID);
    }

    #[test]
    fn install_returns_unsupported_until_ffi_lands() {
        // Spec: scaffolding contract — the function exists and returns
        // a structured "not yet" error, not a panic or a silently
        // successful no-op. The latter would let the install pass
        // claim WFP-enforced when no filters were actually written.
        let r = install_offline_outbound_block("S-1-5-21-1-2-3-1001");
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn uninstall_returns_unsupported_until_ffi_lands() {
        // Spec: same scaffolding contract on the teardown side. A
        // silent success would let `borg service uninstall` claim
        // it cleaned up filters that are still in place.
        let r = uninstall_offline_outbound_block();
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
}
