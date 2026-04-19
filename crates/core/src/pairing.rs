use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::constants;
use crate::db::Database;

/// Unambiguous character set (no 0/O/1/I/L/5/S).
const CODE_CHARS: &[u8] = b"ABCDEFGHJKMNPQRTUVWXYZ2346789";
const CODE_LENGTH: usize = 8;

/// Channel name → 2-letter prefix for self-describing pairing codes.
/// Each channel gets a unique prefix to avoid ambiguity during approval.
const CHANNEL_PREFIXES: &[(&str, &str)] = &[
    ("telegram", "TG"),
    ("slack", "SL"),
    ("discord", "DC"),
    ("teams", "TM"),
    ("google_chat", "GC"),
    ("signal", "SG"),
    ("twilio", "TW"),
    ("whatsapp", "WA"),
    ("sms", "SM"),
    ("imessage", "IM"),
];

/// Look up the 2-letter prefix for a channel name.
pub fn channel_to_prefix(channel_name: &str) -> Option<&'static str> {
    let lower = channel_name.to_lowercase();
    CHANNEL_PREFIXES
        .iter()
        .find(|(ch, _)| *ch == lower)
        .map(|(_, pfx)| *pfx)
}

/// Reverse lookup: 2-letter prefix → canonical channel name.
pub fn prefix_to_channel(prefix: &str) -> Option<&'static str> {
    let upper = prefix.to_uppercase();
    CHANNEL_PREFIXES
        .iter()
        .find(|(_, pfx)| *pfx == upper)
        .map(|(ch, _)| *ch)
}

/// Parse a prefixed code like `TG_H4BRWMRW` into `(channel_name, full_code)`.
/// Returns `None` if the code has no valid prefix.
pub fn parse_prefixed_code(code: &str) -> Option<(&'static str, &str)> {
    let code_upper = code.to_uppercase();
    if let Some(underscore) = code_upper.find('_') {
        let prefix = &code_upper[..underscore];
        if let Some(channel) = prefix_to_channel(prefix) {
            return Some((channel, code));
        }
    }
    None
}

/// Display name for a channel (capitalized).
///
/// Recognized built-in channels come from `ChannelName`. Unknown strings
/// (custom script-based channels, legacy aliases like "whatsapp"/"sms")
/// fall back to specific mappings or the raw string.
pub fn channel_display_name(channel_name: &str) -> String {
    let lower = channel_name.to_ascii_lowercase();
    if let Ok(known) = lower.parse::<crate::channel_names::ChannelName>() {
        return known.display_name().to_string();
    }
    match lower.as_str() {
        "whatsapp" => "WhatsApp".to_string(),
        "sms" => "SMS".to_string(),
        _ => channel_name.to_string(),
    }
}

/// Access policy for direct messages on a channel.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Require pairing code approval before processing messages.
    #[default]
    Pairing,
    /// Allow all senders (no access control).
    Open,
    /// Reject all DMs silently.
    Disabled,
}

/// Result of checking whether a sender is allowed to interact with the bot.
#[derive(Debug, Clone)]
pub enum AccessCheckResult {
    /// Sender is approved — proceed with message processing.
    Allowed,
    /// Sender needs pairing approval — return challenge message.
    Challenge { code: String, message: String },
    /// Sender is denied — return denial message.
    Denied { reason: String },
}

/// Generate a random 8-character base code (no prefix).
fn generate_raw_code() -> String {
    let mut rng = rand::rng();
    (0..CODE_LENGTH)
        .map(|_| {
            let idx = rng.random_range(0..CODE_CHARS.len());
            CODE_CHARS[idx] as char
        })
        .collect()
}

/// Generate a pairing code with channel prefix (e.g. `TG_H4BRWMRW`).
/// Falls back to unprefixed code for unknown channels.
pub fn generate_code(channel_name: &str) -> String {
    let raw = generate_raw_code();
    match channel_to_prefix(channel_name) {
        Some(prefix) => format!("{prefix}_{raw}"),
        None => raw,
    }
}

/// Resolve the effective DM policy for a channel.
pub fn resolve_dm_policy(config: &Config, channel_name: &str) -> DmPolicy {
    config
        .gateway
        .channel_policies
        .get(channel_name)
        .copied()
        .unwrap_or(config.gateway.dm_policy)
}

/// Check whether a sender is allowed to interact with the bot on a given channel.
///
/// `agent_name` is used to personalize the challenge message (defaults to "Borg").
pub fn check_sender_access(
    db: &Database,
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    agent_name: Option<&str>,
) -> Result<AccessCheckResult> {
    let policy = resolve_dm_policy(config, channel_name);

    match policy {
        DmPolicy::Open => Ok(AccessCheckResult::Allowed),
        DmPolicy::Disabled => Ok(AccessCheckResult::Denied {
            reason: "This Borg is not accepting messages on this channel.".into(),
        }),
        DmPolicy::Pairing => check_pairing(db, config, channel_name, sender_id, agent_name),
    }
}

fn check_pairing(
    db: &Database,
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    agent_name: Option<&str>,
) -> Result<AccessCheckResult> {
    // Check if sender is already approved
    if db.is_sender_approved(channel_name, sender_id)? {
        return Ok(AccessCheckResult::Allowed);
    }

    let ttl = config.gateway.pairing_ttl_secs;
    let name = agent_name.unwrap_or("Borg");

    // Rate limit: max N new codes per sender per hour (checked before reuse
    // so that even reused-code responses are gated after excessive attempts)
    let attempts = db.count_pairing_attempts(channel_name, sender_id, constants::SECS_PER_HOUR)?;
    if attempts >= constants::PAIRING_MAX_ATTEMPTS_PER_HOUR {
        tracing::warn!(
            channel = channel_name,
            sender = sender_id,
            "pairing rate limit exceeded"
        );
        return Ok(AccessCheckResult::Denied {
            reason: "Too many pairing attempts. Please try again later.".into(),
        });
    }

    // Check for existing non-expired pending request (reuse code)
    if let Some(existing) = db.find_pending_for_sender(channel_name, sender_id)? {
        let message = format_challenge(name, sender_id, &existing.code, ttl);
        return Ok(AccessCheckResult::Challenge {
            code: existing.code,
            message,
        });
    }

    // Generate new pairing code and create request
    let code = generate_code(channel_name);
    db.create_pairing_request(channel_name, sender_id, &code, None, ttl)?;

    let message = format_challenge(name, sender_id, &code, ttl);
    Ok(AccessCheckResult::Challenge { code, message })
}

/// Render a TTL duration as a human-readable string (e.g. `60 min`, `2 hr`).
fn format_ttl(secs: i64) -> String {
    if secs <= 0 {
        return "soon".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} min");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours} hr");
    }
    let days = hours / 24;
    format!("{days} days")
}

fn format_challenge(agent_name: &str, sender_id: &str, code: &str, ttl_secs: i64) -> String {
    let ttl_hint = format_ttl(ttl_secs);
    format!(
        "{agent_name}: access not configured.\n\n\
         Your sender ID: {sender_id}\n\
         Pairing code: {code}  (expires in {ttl_hint})\n\n\
         {agent_name}'s owner can approve with:  /pairing approve {code}"
    )
}

/// Format the approval notification message sent to the user on their channel.
pub fn format_approval_message(channel_name: &str, agent_name: &str) -> String {
    let display = channel_display_name(channel_name);
    format!("You are approved on {display}. {agent_name} is waiting!")
}

/// Send an approval notification to the user on their messaging channel.
///
/// Currently supports Telegram (sender_id == chat_id for DMs).
/// Fire-and-forget: logs errors but never fails the caller.
pub async fn send_approval_notification(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    agent_name: &str,
) {
    let result =
        send_approval_notification_inner(config, channel_name, sender_id, agent_name).await;
    if let Err(e) = result {
        tracing::warn!(
            channel = channel_name,
            sender = sender_id,
            "Failed to send approval notification: {e}"
        );
    }
}

async fn send_approval_notification_inner(
    config: &Config,
    channel_name: &str,
    sender_id: &str,
    agent_name: &str,
) -> Result<()> {
    let message = format_approval_message(channel_name, agent_name);

    match channel_name.to_lowercase().as_str() {
        "telegram" => {
            let token = config
                .resolve_credential_or_env("TELEGRAM_BOT_TOKEN")
                .ok_or_else(|| anyhow::anyhow!("TELEGRAM_BOT_TOKEN not configured"))?;
            let chat_id: i64 = sender_id
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid Telegram chat_id: {sender_id}"))?;
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            let client = reqwest::Client::new();
            let resp = client
                .post(&url)
                .json(&serde_json::json!({
                    "chat_id": chat_id,
                    "text": message,
                }))
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram API error {status}: {body}");
            }
            Ok(())
        }
        _ => {
            tracing::debug!(
                channel = channel_name,
                "Approval notification not yet supported for this channel"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_generate_raw_code_length() {
        let code = generate_raw_code();
        assert_eq!(code.len(), CODE_LENGTH);
    }

    #[test]
    fn test_generate_code_with_prefix() {
        let code = generate_code("telegram");
        assert!(code.starts_with("TG_"), "expected TG_ prefix, got: {code}");
        assert_eq!(code.len(), 3 + CODE_LENGTH); // "TG_" + 8 chars
    }

    #[test]
    fn test_generate_code_unknown_channel_no_prefix() {
        let code = generate_code("custom_channel");
        assert!(!code.contains('_'), "unknown channel should have no prefix");
        assert_eq!(code.len(), CODE_LENGTH);
    }

    #[test]
    fn test_generate_code_characters() {
        let charset: HashSet<char> = std::str::from_utf8(CODE_CHARS).unwrap().chars().collect();
        for _ in 0..100 {
            let code = generate_raw_code();
            for ch in code.chars() {
                assert!(charset.contains(&ch), "unexpected char: {ch}");
            }
        }
    }

    #[test]
    fn test_generate_code_uniqueness() {
        let codes: HashSet<String> = (0..1000).map(|_| generate_raw_code()).collect();
        assert_eq!(codes.len(), 1000, "expected 1000 unique codes");
    }

    #[test]
    fn test_dm_policy_default() {
        assert_eq!(DmPolicy::default(), DmPolicy::Pairing);
    }

    #[test]
    fn test_dm_policy_serde() {
        for (variant, expected) in [
            (DmPolicy::Pairing, "\"pairing\""),
            (DmPolicy::Open, "\"open\""),
            (DmPolicy::Disabled, "\"disabled\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: DmPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    fn test_config_with_policy(policy: DmPolicy) -> Config {
        let mut config = Config::default();
        config.gateway.dm_policy = policy;
        config
    }

    fn test_db() -> Database {
        Database::test_db()
    }

    #[test]
    fn test_check_sender_access_open_policy() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Open);
        let result = check_sender_access(&db, &config, "telegram", "anyone", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Allowed));
    }

    #[test]
    fn test_check_sender_access_disabled_policy() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Disabled);
        let result = check_sender_access(&db, &config, "telegram", "anyone", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Denied { .. }));
    }

    #[test]
    fn test_check_sender_access_pairing_issues_challenge() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let result = check_sender_access(&db, &config, "telegram", "new_user", None).unwrap();
        match result {
            AccessCheckResult::Challenge { code, message } => {
                assert!(code.starts_with("TG_"), "expected TG_ prefix, got: {code}");
                assert_eq!(code.len(), 3 + CODE_LENGTH);
                assert!(message.contains("new_user"));
                assert!(message.contains(&code));
                // Default agent name when None
                assert!(message.contains("Borg: access not configured"));
                assert!(message.contains("Borg's owner can approve with:"));
                // TTL hint is visible so the user knows how long they have.
                assert!(message.contains("expires in"));
                // Should contain TUI approve command
                assert!(message.contains("/pairing approve"));
            }
            other => panic!("expected Challenge, got {other:?}"),
        }
    }

    #[test]
    fn test_check_sender_access_pairing_custom_agent_name() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let result =
            check_sender_access(&db, &config, "telegram", "named_user", Some("Nova")).unwrap();
        match result {
            AccessCheckResult::Challenge { code, message } => {
                assert!(message.contains("Nova: access not configured"));
                assert!(message.contains("Nova's owner can approve with:"));
                assert!(message.contains(&format!("/pairing approve {code}")));
            }
            other => panic!("expected Challenge, got {other:?}"),
        }
    }

    #[test]
    fn test_format_ttl_variants() {
        assert_eq!(format_ttl(0), "soon");
        assert_eq!(format_ttl(30), "0 min");
        assert_eq!(format_ttl(60), "1 min");
        assert_eq!(format_ttl(3600), "1 hr");
        assert_eq!(format_ttl(3600 * 24), "1 days");
        assert_eq!(format_ttl(3600 * 48), "2 days");
    }

    #[test]
    fn test_check_sender_access_approved_sender_allowed() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);

        // First call creates a challenge
        let result = check_sender_access(&db, &config, "telegram", "user1", None).unwrap();
        let code = match result {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        // Approve the code
        db.approve_pairing("telegram", &code).unwrap();

        // Second call should be allowed
        let result = check_sender_access(&db, &config, "telegram", "user1", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Allowed));
    }

    #[test]
    fn test_check_sender_access_reuses_existing_code() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);

        // Two calls for the same sender should return the same code
        let code1 = match check_sender_access(&db, &config, "telegram", "user2", None).unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };
        let code2 = match check_sender_access(&db, &config, "telegram", "user2", None).unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };
        assert_eq!(code1, code2);
    }

    #[test]
    fn test_channel_policy_override() {
        let db = test_db();
        let mut config = Config::default();
        config.gateway.dm_policy = DmPolicy::Pairing;
        config
            .gateway
            .channel_policies
            .insert("slack".into(), DmPolicy::Open);

        // Slack should be open despite default being pairing
        let result = check_sender_access(&db, &config, "slack", "anyone", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Allowed));

        // Telegram still uses default pairing
        let result = check_sender_access(&db, &config, "telegram", "anyone", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Challenge { .. }));
    }

    #[test]
    fn test_find_pending_by_code_returns_match() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let code = match check_sender_access(&db, &config, "telegram", "user_code", None).unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        let found = db.find_pending_by_code(&code).unwrap();
        assert!(found.is_some());
        let row = found.unwrap();
        assert_eq!(row.code, code);
        assert_eq!(row.channel_name, "telegram");
        assert_eq!(row.sender_id, "user_code");
    }

    #[test]
    fn test_find_pending_by_code_unknown_returns_none() {
        let db = test_db();
        let found = db.find_pending_by_code("ZZZZZZZZ").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_find_pending_by_code_case_insensitive() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let code = match check_sender_access(&db, &config, "telegram", "user_ci", None).unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        // Lookup with lowercase should still find it
        let found = db.find_pending_by_code(&code.to_lowercase()).unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_find_pending_by_code_approved_returns_none() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let code = match check_sender_access(&db, &config, "telegram", "user_appr", None).unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        // Approve the code
        db.approve_pairing("telegram", &code).unwrap();

        // Should no longer be found as pending
        let found = db.find_pending_by_code(&code).unwrap();
        assert!(found.is_none());
    }

    // ── Channel prefix tests ──

    #[test]
    fn test_channel_to_prefix() {
        assert_eq!(channel_to_prefix("telegram"), Some("TG"));
        assert_eq!(channel_to_prefix("Telegram"), Some("TG"));
        assert_eq!(channel_to_prefix("slack"), Some("SL"));
        assert_eq!(channel_to_prefix("discord"), Some("DC"));
        assert_eq!(channel_to_prefix("teams"), Some("TM"));
        assert_eq!(channel_to_prefix("google_chat"), Some("GC"));
        assert_eq!(channel_to_prefix("signal"), Some("SG"));
        assert_eq!(channel_to_prefix("twilio"), Some("TW"));
        assert_eq!(channel_to_prefix("whatsapp"), Some("WA"));
        assert_eq!(channel_to_prefix("sms"), Some("SM"));
        assert_eq!(channel_to_prefix("imessage"), Some("IM"));
        assert_eq!(channel_to_prefix("unknown"), None);
    }

    #[test]
    fn test_prefix_to_channel() {
        assert_eq!(prefix_to_channel("TG"), Some("telegram"));
        assert_eq!(prefix_to_channel("tg"), Some("telegram"));
        assert_eq!(prefix_to_channel("SL"), Some("slack"));
        assert_eq!(prefix_to_channel("DC"), Some("discord"));
        assert_eq!(prefix_to_channel("TM"), Some("teams"));
        assert_eq!(prefix_to_channel("GC"), Some("google_chat"));
        assert_eq!(prefix_to_channel("SG"), Some("signal"));
        assert_eq!(prefix_to_channel("TW"), Some("twilio"));
        assert_eq!(prefix_to_channel("WA"), Some("whatsapp"));
        assert_eq!(prefix_to_channel("SM"), Some("sms"));
        assert_eq!(prefix_to_channel("IM"), Some("imessage"));
        assert_eq!(prefix_to_channel("XX"), None);
    }

    #[test]
    fn test_parse_prefixed_code() {
        let (channel, _) = parse_prefixed_code("TG_H4BRWMRW").unwrap();
        assert_eq!(channel, "telegram");

        let (channel, _) = parse_prefixed_code("SL_ABCD1234").unwrap();
        assert_eq!(channel, "slack");

        // Case insensitive
        let (channel, _) = parse_prefixed_code("tg_h4brwmrw").unwrap();
        assert_eq!(channel, "telegram");
    }

    #[test]
    fn test_parse_prefixed_code_invalid() {
        assert!(parse_prefixed_code("H4BRWMRW").is_none());
        assert!(parse_prefixed_code("XX_H4BRWMRW").is_none());
        assert!(parse_prefixed_code("").is_none());
    }

    #[test]
    fn test_channel_display_name() {
        assert_eq!(channel_display_name("telegram"), "Telegram");
        assert_eq!(channel_display_name("slack"), "Slack");
        assert_eq!(channel_display_name("custom"), "custom");
    }

    #[test]
    fn test_pairing_rate_limit_exceeded() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);

        // Create max attempts directly in the DB for the same sender
        for _ in 0..constants::PAIRING_MAX_ATTEMPTS_PER_HOUR {
            let code = generate_code("telegram");
            db.create_pairing_request("telegram", "rate_limit_user", &code, None, 3600)
                .unwrap();
        }

        // The next attempt should be denied
        let result =
            check_sender_access(&db, &config, "telegram", "rate_limit_user", None).unwrap();
        assert!(
            matches!(result, AccessCheckResult::Denied { .. }),
            "expected Denied after exceeding rate limit, got {result:?}"
        );
    }

    #[test]
    fn test_pairing_rate_limit_reuse_doesnt_count() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);

        // First call creates a code
        let result = check_sender_access(&db, &config, "telegram", "reuse_user", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Challenge { .. }));

        // Repeated calls reuse the same code (don't create new ones)
        let result = check_sender_access(&db, &config, "telegram", "reuse_user", None).unwrap();
        assert!(matches!(result, AccessCheckResult::Challenge { .. }));

        // Count should be 1, not 2
        let count = db
            .count_pairing_attempts("telegram", "reuse_user", 3600)
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_count_pairing_attempts_db_method() {
        let db = test_db();

        // No attempts yet
        let count = db
            .count_pairing_attempts("telegram", "counter_user", 3600)
            .unwrap();
        assert_eq!(count, 0);

        // Create some requests
        for _ in 0..5 {
            let code = generate_code("telegram");
            db.create_pairing_request("telegram", "counter_user", &code, None, 3600)
                .unwrap();
        }

        let count = db
            .count_pairing_attempts("telegram", "counter_user", 3600)
            .unwrap();
        assert_eq!(count, 5);

        // Different sender should have 0
        let count = db
            .count_pairing_attempts("telegram", "other_user", 3600)
            .unwrap();
        assert_eq!(count, 0);

        // Different channel should have 0
        let count = db
            .count_pairing_attempts("slack", "counter_user", 3600)
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_format_approval_message() {
        let msg = format_approval_message("telegram", "Nova");
        assert_eq!(msg, "You are approved on Telegram. Nova is waiting!");
    }

    #[test]
    fn test_generate_all_channel_prefixes() {
        for (channel, expected_prefix) in CHANNEL_PREFIXES {
            let code = generate_code(channel);
            let prefix = format!("{expected_prefix}_");
            assert!(
                code.starts_with(&prefix),
                "channel {channel}: expected prefix {prefix}, got: {code}"
            );
        }
    }
}
