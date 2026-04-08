use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db::Database;

/// Unambiguous character set (no 0/O/1/I/L/5/S).
const CODE_CHARS: &[u8] = b"ABCDEFGHJKMNPQRTUVWXYZ2346789";
const CODE_LENGTH: usize = 8;

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

/// Generate a cryptographically random pairing code.
pub fn generate_code() -> String {
    let mut rng = rand::rng();
    (0..CODE_LENGTH)
        .map(|_| {
            let idx = rng.random_range(0..CODE_CHARS.len());
            CODE_CHARS[idx] as char
        })
        .collect()
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
pub fn check_sender_access(
    db: &Database,
    config: &Config,
    channel_name: &str,
    sender_id: &str,
) -> Result<AccessCheckResult> {
    let policy = resolve_dm_policy(config, channel_name);

    match policy {
        DmPolicy::Open => Ok(AccessCheckResult::Allowed),
        DmPolicy::Disabled => Ok(AccessCheckResult::Denied {
            reason: "This bot is not accepting messages on this channel.".into(),
        }),
        DmPolicy::Pairing => check_pairing(db, config, channel_name, sender_id),
    }
}

fn check_pairing(
    db: &Database,
    config: &Config,
    channel_name: &str,
    sender_id: &str,
) -> Result<AccessCheckResult> {
    // Check if sender is already approved
    if db.is_sender_approved(channel_name, sender_id)? {
        return Ok(AccessCheckResult::Allowed);
    }

    let ttl = config.gateway.pairing_ttl_secs;

    // Check for existing non-expired pending request (reuse code)
    if let Some(existing) = db.find_pending_for_sender(channel_name, sender_id)? {
        let message = format_challenge(channel_name, sender_id, &existing.code, ttl);
        return Ok(AccessCheckResult::Challenge {
            code: existing.code,
            message,
        });
    }

    // Generate new pairing code and create request
    let code = generate_code();
    db.create_pairing_request(channel_name, sender_id, &code, None, ttl)?;

    let message = format_challenge(channel_name, sender_id, &code, ttl);
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

fn format_challenge(channel_name: &str, sender_id: &str, code: &str, ttl_secs: i64) -> String {
    let ttl_hint = format_ttl(ttl_secs);
    format!(
        "Borg: access not configured.\n\n\
         Your sender ID: {sender_id}\n\
         Pairing code: {code}  (expires in {ttl_hint})\n\n\
         Ask the bot owner to approve with:\n  \
         borg pairing approve {channel_name} {code}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_generate_code_length() {
        let code = generate_code();
        assert_eq!(code.len(), CODE_LENGTH);
    }

    #[test]
    fn test_generate_code_characters() {
        let charset: HashSet<char> = std::str::from_utf8(CODE_CHARS).unwrap().chars().collect();
        for _ in 0..100 {
            let code = generate_code();
            for ch in code.chars() {
                assert!(charset.contains(&ch), "unexpected char: {ch}");
            }
        }
    }

    #[test]
    fn test_generate_code_uniqueness() {
        let codes: HashSet<String> = (0..1000).map(|_| generate_code()).collect();
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
        let result = check_sender_access(&db, &config, "telegram", "anyone").unwrap();
        assert!(matches!(result, AccessCheckResult::Allowed));
    }

    #[test]
    fn test_check_sender_access_disabled_policy() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Disabled);
        let result = check_sender_access(&db, &config, "telegram", "anyone").unwrap();
        assert!(matches!(result, AccessCheckResult::Denied { .. }));
    }

    #[test]
    fn test_check_sender_access_pairing_issues_challenge() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let result = check_sender_access(&db, &config, "telegram", "new_user").unwrap();
        match result {
            AccessCheckResult::Challenge { code, message } => {
                assert_eq!(code.len(), CODE_LENGTH);
                assert!(message.contains("new_user"));
                assert!(message.contains(&code));
                assert!(message.contains("borg pairing approve"));
                // TTL hint is visible so the user knows how long they have.
                assert!(message.contains("expires in"));
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
        let result = check_sender_access(&db, &config, "telegram", "user1").unwrap();
        let code = match result {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        // Approve the code
        db.approve_pairing("telegram", &code).unwrap();

        // Second call should be allowed
        let result = check_sender_access(&db, &config, "telegram", "user1").unwrap();
        assert!(matches!(result, AccessCheckResult::Allowed));
    }

    #[test]
    fn test_check_sender_access_reuses_existing_code() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);

        // Two calls for the same sender should return the same code
        let code1 = match check_sender_access(&db, &config, "telegram", "user2").unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };
        let code2 = match check_sender_access(&db, &config, "telegram", "user2").unwrap() {
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
        let result = check_sender_access(&db, &config, "slack", "anyone").unwrap();
        assert!(matches!(result, AccessCheckResult::Allowed));

        // Telegram still uses default pairing
        let result = check_sender_access(&db, &config, "telegram", "anyone").unwrap();
        assert!(matches!(result, AccessCheckResult::Challenge { .. }));
    }

    #[test]
    fn test_find_pending_by_code_returns_match() {
        let db = test_db();
        let config = test_config_with_policy(DmPolicy::Pairing);
        let code = match check_sender_access(&db, &config, "telegram", "user_code").unwrap() {
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
        let code = match check_sender_access(&db, &config, "telegram", "user_ci").unwrap() {
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
        let code = match check_sender_access(&db, &config, "telegram", "user_appr").unwrap() {
            AccessCheckResult::Challenge { code, .. } => code,
            other => panic!("expected Challenge, got {other:?}"),
        };

        // Approve the code
        db.approve_pairing("telegram", &code).unwrap();

        // Should no longer be found as pending
        let found = db.find_pending_by_code(&code).unwrap();
        assert!(found.is_none());
    }
}
