//! Centralized channel name constants and enum.
//!
//! Single source of truth for channel name strings used across gateway
//! registries, DB rows, routing rules, and pairing flows. Mirrors the
//! [`crate::tool_names`] pattern.

use std::fmt;
use std::str::FromStr;

// ── String constants (for use where an enum is overkill — DB rows, serde
// defaults, literal lists) ──────────────────────────────────────────────

/// Telegram Bot API channel.
pub const TELEGRAM: &str = "telegram";
/// Slack Events API channel.
pub const SLACK: &str = "slack";
/// Discord interactions channel.
pub const DISCORD: &str = "discord";
/// Microsoft Teams Bot Framework channel.
pub const TEAMS: &str = "teams";
/// Google Chat (Chat API) channel.
pub const GOOGLE_CHAT: &str = "google_chat";
/// Signal via signal-cli JSON-RPC.
pub const SIGNAL: &str = "signal";
/// Twilio (WhatsApp + SMS) channel.
pub const TWILIO: &str = "twilio";
/// iMessage (macOS only) channel.
pub const IMESSAGE: &str = "imessage";

/// All native channel name constants, in stable order. Use this when you
/// need to iterate every supported channel (e.g. doctor checks, routing
/// docs, settings popup).
pub const ALL: &[&str] = &[
    TELEGRAM,
    SLACK,
    DISCORD,
    TEAMS,
    GOOGLE_CHAT,
    SIGNAL,
    TWILIO,
    IMESSAGE,
];

/// Typed identifier for the built-in native channels.
///
/// Use this when channel identity participates in logic (matches, dispatch,
/// display). For serde / DB keys prefer the `&'static str` constants above
/// — string form is canonical on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelName {
    /// Telegram Bot API.
    Telegram,
    /// Slack Events API.
    Slack,
    /// Discord interactions.
    Discord,
    /// Microsoft Teams (Bot Framework).
    Teams,
    /// Google Chat (Chat API).
    GoogleChat,
    /// Signal via signal-cli.
    Signal,
    /// Twilio (WhatsApp + SMS).
    Twilio,
    /// iMessage (macOS only).
    IMessage,
}

impl ChannelName {
    /// Canonical wire/DB string form (lowercase, snake_case where needed).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Telegram => TELEGRAM,
            Self::Slack => SLACK,
            Self::Discord => DISCORD,
            Self::Teams => TEAMS,
            Self::GoogleChat => GOOGLE_CHAT,
            Self::Signal => SIGNAL,
            Self::Twilio => TWILIO,
            Self::IMessage => IMESSAGE,
        }
    }

    /// Human-readable display form (capitalized, spaces where appropriate).
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Slack => "Slack",
            Self::Discord => "Discord",
            Self::Teams => "Teams",
            Self::GoogleChat => "Google Chat",
            Self::Signal => "Signal",
            Self::Twilio => "Twilio",
            Self::IMessage => "iMessage",
        }
    }

    /// All built-in native channels, in stable order.
    pub const fn all() -> &'static [ChannelName] {
        &[
            Self::Telegram,
            Self::Slack,
            Self::Discord,
            Self::Teams,
            Self::GoogleChat,
            Self::Signal,
            Self::Twilio,
            Self::IMessage,
        ]
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChannelName {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            TELEGRAM => Ok(Self::Telegram),
            SLACK => Ok(Self::Slack),
            DISCORD => Ok(Self::Discord),
            TEAMS => Ok(Self::Teams),
            GOOGLE_CHAT => Ok(Self::GoogleChat),
            SIGNAL => Ok(Self::Signal),
            TWILIO => Ok(Self::Twilio),
            IMESSAGE => Ok(Self::IMessage),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_round_trips() {
        for name in ChannelName::all() {
            assert_eq!(
                ChannelName::from_str(name.as_str()).unwrap(),
                *name,
                "round-trip failed for {name:?}"
            );
        }
    }

    #[test]
    fn all_strs_match_enum_all() {
        let strs: Vec<&str> = ChannelName::all().iter().map(|c| c.as_str()).collect();
        assert_eq!(strs, ALL);
    }

    #[test]
    fn case_insensitive_parse() {
        assert_eq!("TELEGRAM".parse::<ChannelName>(), Ok(ChannelName::Telegram));
        assert_eq!("Slack".parse::<ChannelName>(), Ok(ChannelName::Slack));
    }

    #[test]
    fn unknown_returns_err() {
        assert!("matrix".parse::<ChannelName>().is_err());
    }

    #[test]
    fn display_names_non_empty() {
        for name in ChannelName::all() {
            assert!(!name.display_name().is_empty());
        }
    }
}
