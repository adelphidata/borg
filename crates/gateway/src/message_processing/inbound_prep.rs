//! Inbound message preparation pipeline.
//!
//! Stages — order is the prompt-injection-defense invariant; do not reorder
//! without re-reading `docs/prompt_injection.md`:
//! 1. Link understanding (optional URL expansion).
//! 2. Injection scan on the full text **before** truncation so patterns that
//!    straddle the truncation boundary still trip the detector.
//! 3. Truncate to `MAX_INBOUND_TEXT_BYTES` (UTF-8-safe).
//! 4. Wrap with the trust marker selected by the scan result.
//!
//! Returns `(message_text, has_image_attachments)`. The bool is co-returned
//! because the caller routes images via a separate code path; computing it
//! here keeps the inbound-message inspection in one place.

use cortex::config::Config;
use cortex::constants;
use cortex::security::sanitize::{
    scan_for_injection, wrap_untrusted, wrap_with_injection_warning, ThreatLevel,
};
use tracing::{info, warn};

use crate::handler::InboundMessage;

pub(crate) async fn prepare_message_text(
    channel_name: &str,
    inbound: &InboundMessage,
    config: &Config,
    cleaned_text: String,
) -> (String, bool) {
    // Stage 1: link understanding.
    let cleaned_text = if config.gateway.link_understanding.enabled {
        let (augmented, fetched) = super::link_understanding::augment_with_links(
            &cleaned_text,
            &config.gateway.link_understanding,
            &config.security.url_policy,
        )
        .await;
        if !fetched.is_empty() {
            info!(
                "Link understanding: fetched {} URL(s) for channel '{}'",
                fetched.len(),
                channel_name
            );
        }
        augmented
    } else {
        cleaned_text
    };

    // Stage 2: scan BEFORE truncation, capped at MAX_RESPONSE_SIZE so a
    // pathological payload can't DoS the regex pass.
    let scan_limit = constants::MAX_RESPONSE_SIZE;
    let scan_input = if cleaned_text.len() > scan_limit {
        let mut end = scan_limit;
        while end > 0 && !cleaned_text.is_char_boundary(end) {
            end -= 1;
        }
        &cleaned_text[..end]
    } else {
        &cleaned_text
    };
    let injection_level = scan_for_injection(scan_input);

    // Stage 3: truncate to keep LLM token cost bounded.
    let text = if cleaned_text.len() > constants::MAX_INBOUND_TEXT_BYTES {
        let original_len = cleaned_text.len();
        let mut truncated = cleaned_text;
        truncated.truncate(constants::MAX_INBOUND_TEXT_BYTES);
        while !truncated.is_char_boundary(truncated.len()) {
            truncated.pop();
        }
        warn!(
            "Truncated inbound message from {} bytes to {} bytes for channel '{}'",
            original_len,
            truncated.len(),
            channel_name
        );
        truncated
    } else {
        cleaned_text
    };

    // Stage 4: apply the trust wrapper chosen by the (pre-truncation) scan.
    let message_text = {
        let base = format!(
            "[Channel: {}, Sender: {}]\n{}",
            channel_name, inbound.sender_id, text
        );
        match injection_level {
            ThreatLevel::HighRisk { .. } => wrap_with_injection_warning(channel_name, &base),
            ThreatLevel::Flagged { .. } => wrap_untrusted(channel_name, &base),
            ThreatLevel::Clean => base,
        }
    };

    let has_image_attachments = inbound
        .attachments
        .iter()
        .any(|a| a.mime_type.starts_with("image/"));

    (message_text, has_image_attachments)
}
