//! HMAC chain helpers for evolution events.
//!
//! v2 includes `metadata` in the hashed payload; v1 did not. Verification
//! tries v2 first then falls back to v1 so events persisted before the
//! metadata field was added still validate.

use crate::hmac_chain;

use super::EvolutionEvent;

/// Domain string for HMAC key derivation. Combined with per-installation salt.
pub(crate) const EVOLUTION_HMAC_DOMAIN: &[u8] = b"borg-evolution-chain-v1";

/// Legacy compiled-in secret for installations without per-install salt.
#[cfg(test)]
pub(crate) const EVOLUTION_HMAC_LEGACY: &[u8] = b"borg-evolution-chain-v1";

/// Compute HMAC for an evolution event (v2: includes metadata).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_event_hmac(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    metadata: &str,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            event_type.as_bytes(),
            &xp_delta.to_le_bytes(),
            archetype.as_bytes(),
            source.as_bytes(),
            metadata.as_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Legacy HMAC computation (v1: without metadata). Used internally by
/// `verify_event_hmac` for backward-compat verification. Re-exposed to the
/// evolution test module under `#[cfg(test)]` only.
#[cfg(test)]
pub(crate) fn compute_event_hmac_legacy(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    created_at: i64,
) -> String {
    compute_event_hmac_legacy_inner(
        key, prev_hmac, event_type, xp_delta, archetype, source, created_at,
    )
}

fn compute_event_hmac_legacy_inner(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            event_type.as_bytes(),
            &xp_delta.to_le_bytes(),
            archetype.as_bytes(),
            source.as_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Verify an event's HMAC against the expected chain.
/// Tries v2 (with metadata) first, falls back to v1 (legacy) for existing events.
pub(crate) fn verify_event_hmac(
    key: &[u8],
    event: &EvolutionEvent,
    expected_prev_hmac: &str,
) -> bool {
    let meta = event.metadata_json.as_deref().unwrap_or("");
    let archetype = event.archetype.as_deref().unwrap_or("");

    // Try v2 HMAC (includes metadata)
    let recomputed_v2 = compute_event_hmac(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.xp_delta,
        archetype,
        &event.source,
        meta,
        event.created_at,
    );
    if hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed_v2,
    ) {
        return true;
    }

    // Fall back to v1 HMAC (legacy, without metadata)
    let recomputed_v1 = compute_event_hmac_legacy_inner(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.xp_delta,
        archetype,
        &event.source,
        event.created_at,
    );
    hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed_v1,
    )
}
