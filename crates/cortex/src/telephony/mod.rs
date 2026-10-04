//! Outbound phone calls behind a swappable provider.
//!
//! [`PhoneDialer`] is what the `phone_call` tool talks to. It owns the rules
//! that hold whichever provider is picked (number format, the operator's
//! allowlist, the call-length cap) and hands the call to one
//! [`TelephonyProvider`] from the registry in [`providers`]. Two kinds of
//! provider sit behind the same trait:
//!
//! - **plain calls** (Twilio): the message is spoken verbatim, then the call
//!   ends;
//! - **voice agents** (Vapi, Bland, ElevenLabs Agents): the message is the
//!   agent's task, and it holds a conversation on Borg's behalf. Status then
//!   carries the transcript and summary back.

use std::fmt;

use anyhow::{bail, Result};

use crate::config::{Config, TelephonyConfig};

mod providers;

pub use providers::{providers, TelephonyProviderDef};

/// One outbound call, after [`PhoneDialer`] validated it.
#[derive(Debug, Clone)]
pub(crate) struct CallRequest {
    /// Callee in E.164.
    pub to: String,
    /// Spoken verbatim (plain calls) or the agent's task (voice agents).
    pub message: String,
    /// Number or provider number id the call comes from.
    pub caller_id: String,
    /// Provider agent to run the call; empty means none.
    pub agent_id: String,
    /// Provider voice; empty means the provider's default.
    pub voice: String,
    /// Upper bound on the call's length.
    pub max_call_secs: u32,
}

/// Coarse call lifecycle, the same across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// Accepted, not ringing yet.
    Queued,
    /// The callee's phone is ringing.
    Ringing,
    /// Connected.
    InProgress,
    /// Finished normally.
    Ended,
    /// Never connected or broke off (busy, no answer, error).
    Failed,
}

impl fmt::Display for CallState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Queued => "queued",
            Self::Ringing => "ringing",
            Self::InProgress => "in progress",
            Self::Ended => "ended",
            Self::Failed => "failed",
        })
    }
}

/// What a provider reports about a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallStatus {
    /// Provider call id, used for later `status` / `hang_up`.
    pub id: String,
    /// Where the call is in its lifecycle.
    pub state: CallState,
    /// The provider's own status word, kept for detail ("busy", "no-answer").
    pub detail: String,
    /// Talk time once known.
    pub duration_secs: Option<u64>,
    /// What was said, for voice-agent calls that record it.
    pub transcript: Option<String>,
    /// The provider's summary of the conversation.
    pub summary: Option<String>,
}

impl CallStatus {
    /// A status with only the id and state known yet.
    pub(crate) fn new(id: impl Into<String>, state: CallState, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state,
            detail: detail.into(),
            duration_secs: None,
            transcript: None,
            summary: None,
        }
    }

    /// Plain-text report for the model.
    pub fn render(&self) -> String {
        let mut out = format!("Call {}: {}", self.id, self.state);
        if !self.detail.is_empty() && !self.detail.eq_ignore_ascii_case(&self.state.to_string()) {
            out.push_str(&format!(" ({})", self.detail));
        }
        if let Some(secs) = self.duration_secs {
            out.push_str(&format!(", {secs}s"));
        }
        if let Some(summary) = self.summary.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("\nSummary: {summary}"));
        }
        if let Some(transcript) = self.transcript.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("\nTranscript:\n{transcript}"));
        }
        out
    }
}

/// A telephony backend. Implementations own only the wire protocol; the
/// cross-provider rules live in [`PhoneDialer`].
#[async_trait::async_trait]
pub(crate) trait TelephonyProvider: Send + Sync {
    /// Start a call; returns its initial status (with the provider id).
    async fn place_call(&self, call: &CallRequest) -> Result<CallStatus>;
    /// Current status of a call this provider placed.
    async fn call_status(&self, call_id: &str) -> Result<CallStatus>;
    /// End a call early.
    async fn hang_up(&self, call_id: &str) -> Result<()> {
        let _ = call_id;
        bail!("this provider ends calls on its own and cannot hang up early")
    }
}

/// The configured phone provider plus the operator's call rules.
pub struct PhoneDialer {
    provider: Box<dyn TelephonyProvider>,
    settings: TelephonyConfig,
    def: &'static TelephonyProviderDef,
}

impl PhoneDialer {
    /// Build the dialer from config. The error names the next step in plain
    /// words, since it is shown to the operator as-is.
    pub fn from_config(config: &Config) -> Result<Self> {
        let settings = &config.telephony;
        if !settings.enabled() {
            bail!(
                "Phone calls are off. Pick a provider under Settings → Advanced → Voice & phone."
            );
        }
        let Some(def) = providers::find(&settings.provider) else {
            bail!(
                "Unknown phone provider '{}'. Pick one under Settings → Advanced → Voice & phone.",
                settings.provider
            );
        };
        let provider = def.build(config)?;
        Ok(Self {
            provider,
            settings: settings.clone(),
            def,
        })
    }

    /// Label of the provider in use.
    pub fn provider_label(&self) -> &'static str {
        self.def.info.label
    }

    /// Place a call to `to` after checking its format and the allowlist.
    pub async fn call(&self, to: &str, message: &str) -> Result<CallStatus> {
        let to = normalize_e164(to)?;
        if !number_allowed(&to, &self.settings.allowed_numbers) {
            bail!(
                "{to} is not in the allowed numbers for phone calls. Add it under Settings → \
                 Advanced → Voice & phone."
            );
        }
        let message = message.trim();
        if message.is_empty() {
            bail!("A call needs a message: what to say, or the agent's task.");
        }
        let request = CallRequest {
            to,
            message: message.to_string(),
            caller_id: self.settings.caller_id.trim().to_string(),
            agent_id: self.settings.agent_id.trim().to_string(),
            voice: self.settings.voice.trim().to_string(),
            max_call_secs: self.settings.max_call_secs.max(30),
        };
        self.provider.place_call(&request).await
    }

    /// Status of an earlier call.
    pub async fn status(&self, call_id: &str) -> Result<CallStatus> {
        self.provider.call_status(checked_call_id(call_id)?).await
    }

    /// End an earlier call.
    pub async fn hang_up(&self, call_id: &str) -> Result<()> {
        self.provider.hang_up(checked_call_id(call_id)?).await
    }
}

/// Call ids are spliced into provider URL paths, so they are held to the
/// characters real ids use.
fn checked_call_id(call_id: &str) -> Result<&str> {
    let id = call_id.trim();
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("invalid call id: {call_id:?}");
    }
    Ok(id)
}

/// Normalize a phone number to E.164: `+` then 8–15 digits. Spaces, dashes,
/// dots and parentheses people type are dropped; anything else is refused,
/// since a call to a mistyped number costs money and reaches a stranger.
fn normalize_e164(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix('+') else {
        bail!(
            "Phone numbers need the country code in E.164 form, like +14155550123 (got {raw:?})."
        );
    };
    let digits: String = rest
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
        .collect();
    if !(8..=15).contains(&digits.len()) || !digits.chars().all(|c| c.is_ascii_digit()) {
        bail!("{raw:?} is not a valid E.164 phone number, like +14155550123.");
    }
    Ok(format!("+{digits}"))
}

/// Whether `number` (already E.164) matches the allowlist. Entries are
/// numbers or prefixes, normalized the same way; an empty list allows any.
fn number_allowed(number: &str, allowlist: &str) -> bool {
    let entries: Vec<String> = allowlist
        .split([',', '\n', ';'])
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            let digits: String = e.chars().filter(char::is_ascii_digit).collect();
            format!("+{digits}")
        })
        .collect();
    entries.is_empty()
        || entries
            .iter()
            .any(|prefix| number.starts_with(prefix.as_str()))
}

/// Escape text for an XML body (TwiML).
pub(crate) fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e164_normalization_accepts_typed_forms_and_rejects_the_rest() {
        let ok = [
            ("+1 (415) 555-0123", "+14155550123"),
            ("+44 20 7123 4567", "+442071234567"),
            ("  +4915112345678 ", "+4915112345678"),
        ];
        for (raw, want) in ok {
            assert_eq!(normalize_e164(raw).unwrap(), want, "{raw}");
        }
        for bad in [
            "4155550123",
            "+1",
            "+1415555012345678",
            "+1-415-CALL-NOW",
            "",
            "+",
        ] {
            assert!(normalize_e164(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn allowlist_matches_numbers_and_prefixes_and_empty_allows_all() {
        assert!(number_allowed("+14155550123", ""));
        assert!(number_allowed("+14155550123", "+1 415"));
        assert!(number_allowed("+14155550123", "+44, +1415555"));
        assert!(!number_allowed("+14155550123", "+44"));
        assert!(!number_allowed("+19005550123", "+1415; +1650"));
    }

    #[test]
    fn call_ids_with_path_characters_are_refused() {
        assert_eq!(checked_call_id(" CA123abc_-9 ").unwrap(), "CA123abc_-9");
        for bad in ["", "../calls", "a/b", "id?x=1", "id with space"] {
            assert!(checked_call_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn xml_escape_neutralizes_markup() {
        assert_eq!(
            xml_escape(r#"<Hangup/> & "quotes" 'too'"#),
            "&lt;Hangup/&gt; &amp; &quot;quotes&quot; &apos;too&apos;"
        );
    }

    #[test]
    fn status_render_includes_what_is_known() {
        let mut s = CallStatus::new("c1", CallState::Ended, "completed");
        s.duration_secs = Some(42);
        s.summary = Some("Booked for 7pm".into());
        s.transcript = Some("agent: hi".into());
        assert_eq!(
            s.render(),
            "Call c1: ended (completed), 42s\nSummary: Booked for 7pm\nTranscript:\nagent: hi"
        );
        // The provider word is not repeated when it says the same thing.
        assert_eq!(
            CallStatus::new("c2", CallState::Queued, "queued").render(),
            "Call c2: queued"
        );
    }

    #[test]
    fn dialer_reports_the_next_step_when_off_or_unknown() {
        let config = Config::default();
        let err = PhoneDialer::from_config(&config).err().unwrap().to_string();
        assert!(err.contains("Phone calls are off"), "{err}");

        let mut config = Config::default();
        config.telephony.provider = "carrier-pigeon".into();
        let err = PhoneDialer::from_config(&config).err().unwrap().to_string();
        assert!(err.contains("Unknown phone provider"), "{err}");
    }
}
