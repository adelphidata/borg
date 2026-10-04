//! Outbound phone calls (`phone_call` tool).
//!
//! Which provider places the call is one id; everything else is the handful
//! of values every provider needs in some form. Provider-specific meaning
//! (a Twilio number vs a Vapi phone-number id) is documented on the
//! registry row in `crate::telephony`, not here.

use serde::{Deserialize, Serialize};

/// Phone-call settings. An empty `provider` turns phone calls off and keeps
/// the tool out of the model's tool list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelephonyConfig {
    /// Provider id from `crate::telephony::providers()`; empty = off.
    pub provider: String,
    /// Where calls come from: an E.164 number, or the provider's id for a
    /// number it hosts (Vapi, ElevenLabs).
    pub caller_id: String,
    /// The provider's voice agent / assistant to run the call. Empty sends
    /// the tool's `message` as the call's instructions instead.
    pub agent_id: String,
    /// Account id that pairs with the key (the Twilio Account SID).
    pub account_id: String,
    /// Voice for the call; empty uses the provider's default.
    pub voice: String,
    /// Comma-separated E.164 numbers or prefixes the agent may call (for
    /// example `+1415555, +442071234567`). Empty allows any number.
    pub allowed_numbers: String,
    /// Hard cap on one call's length in seconds; providers that bill per
    /// minute hang up at this mark.
    pub max_call_secs: u32,
}

impl Default for TelephonyConfig {
    fn default() -> Self {
        Self {
            provider: String::new(),
            caller_id: String::new(),
            agent_id: String::new(),
            account_id: String::new(),
            voice: String::new(),
            allowed_numbers: String::new(),
            max_call_secs: 300,
        }
    }
}

impl TelephonyConfig {
    /// Whether the operator picked a provider.
    pub fn enabled(&self) -> bool {
        !self.provider.trim().is_empty()
    }
}
