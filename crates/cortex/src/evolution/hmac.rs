//! HMAC chain helpers for evolution events.
//!
//! `metadata` is part of the hashed payload. An earlier scheme omitted it, and
//! verification used to fall back to that scheme — which meant metadata could
//! be edited on an existing row without breaking the chain. There are no rows
//! predating the current scheme, so the fallback is gone and metadata is
//! covered.

use crate::hmac_chain;

use super::EvolutionEvent;

/// Domain string for HMAC key derivation. Combined with per-installation salt.
pub(crate) const EVOLUTION_HMAC_DOMAIN: &[u8] = b"borg-evolution-chain-v1";

/// Compute the HMAC for an evolution event.
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

/// Verify an event's HMAC against the expected chain.
pub(crate) fn verify_event_hmac(
    key: &[u8],
    event: &EvolutionEvent,
    expected_prev_hmac: &str,
) -> bool {
    let recomputed = compute_event_hmac(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.xp_delta,
        event.archetype.as_deref().unwrap_or(""),
        &event.source,
        event.metadata_json.as_deref().unwrap_or(""),
        event.created_at,
    );
    hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_event(metadata: &str) -> EvolutionEvent {
        EvolutionEvent {
            id: 1,
            event_type: "evolution".to_string(),
            xp_delta: 0,
            archetype: None,
            source: "gate_check".to_string(),
            metadata_json: Some(metadata.to_string()),
            created_at: 1_000,
            hmac: compute_event_hmac(
                EVOLUTION_HMAC_DOMAIN,
                "0",
                "evolution",
                0,
                "",
                "gate_check",
                metadata,
                1_000,
            ),
            prev_hmac: "0".to_string(),
            session_id: None,
            pubkey_id: None,
        }
    }

    #[test]
    fn metadata_is_covered_by_the_seal() {
        // Verification used to fall back to a scheme that left `metadata_json`
        // out of the MAC, so a row's metadata could be rewritten — including
        // flipping `gates_verified` — while the chain still verified. With the
        // fallback gone, editing metadata alone must break the seal.
        let mut event = sealed_event(r#"{"gates_verified":false}"#);
        assert!(
            verify_event_hmac(EVOLUTION_HMAC_DOMAIN, &event, "0"),
            "a freshly sealed event must verify"
        );

        event.metadata_json = Some(r#"{"gates_verified":true}"#.to_string());
        assert!(
            !verify_event_hmac(EVOLUTION_HMAC_DOMAIN, &event, "0"),
            "metadata tampering must invalidate the event"
        );
    }

    #[test]
    fn a_wrong_expected_prev_breaks_the_link() {
        let event = sealed_event("{}");
        assert!(!verify_event_hmac(
            EVOLUTION_HMAC_DOMAIN,
            &event,
            "deadbeef"
        ));
    }
}
