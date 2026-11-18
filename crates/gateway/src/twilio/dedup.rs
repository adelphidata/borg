//! Bounded deduplicator for Twilio inbound message SIDs.
//!
//! Twilio's webhook signature covers the URL and the form parameters and
//! nothing else — there is no timestamp or nonce — so a retried or replayed
//! POST verifies exactly like the first delivery. Without deduplication each
//! copy runs its own agent turn and sends its own reply. A `MessageSid` names
//! one inbound message for good, which makes it the dedup key.

use crate::constants;

crate::dedup_wrapper!(
    /// Bounded deduplicator for Twilio inbound message SIDs.
    pub struct MessageDeduplicator(String, constants::TWILIO_DEDUP_CAPACITY);
    is_duplicate(message_sid: &str) => message_sid.to_string();
);
