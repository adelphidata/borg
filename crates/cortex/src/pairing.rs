use anyhow::Result;
use rand::rngs::OsRng;
use rand::TryRngCore;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::constants;
use crate::db::Database;

/// Number of words in a pairing phrase. 6 words sampled uniformly from the
/// 2048-word BIP-39 English wordlist via `OsRng` → ~66 bits of entropy per
/// code.
const PHRASE_WORDS: usize = 6;

/// Channel name → lowercase prefix for self-describing pairing codes.
/// Each channel gets a unique prefix to avoid ambiguity during approval.
/// Format is `<prefix>/<word>-<word>-...-<word>`, e.g.
/// `tg/voyage-kitten-orbit-marble-cinder-pluto`.
const CHANNEL_PREFIXES: &[(&str, &str)] = &[
    ("telegram", "tg"),
    ("slack", "sl"),
    ("discord", "dc"),
    ("teams", "tm"),
    ("google_chat", "gc"),
    ("signal", "sg"),
    ("twilio", "tw"),
    ("whatsapp", "wa"),
    ("sms", "sm"),
    ("imessage", "im"),
];

/// Look up the 2-letter lowercase prefix for a channel name.
pub fn channel_to_prefix(channel_name: &str) -> Option<&'static str> {
    let lower = channel_name.to_lowercase();
    CHANNEL_PREFIXES
        .iter()
        .find(|(ch, _)| *ch == lower)
        .map(|(_, pfx)| *pfx)
}

/// Reverse lookup: 2-letter prefix → canonical channel name. Case-insensitive.
pub fn prefix_to_channel(prefix: &str) -> Option<&'static str> {
    let lower = prefix.to_lowercase();
    CHANNEL_PREFIXES
        .iter()
        .find(|(_, pfx)| *pfx == lower)
        .map(|(ch, _)| *ch)
}

/// Parse a prefixed code like `tg/voyage-kitten-orbit-marble-cinder-pluto`
/// into `(channel_name, full_code)`. Returns `None` if the code has no valid
/// prefix OR the suffix is not a well-formed 6-word BIP-39 phrase. Accepts
/// `tg/` (canonical) and `tg_` (legacy underscore separator) but in both cases
/// the phrase itself must validate — a hostile sender cannot route a 1-word
/// stub like `tg_voyage` into the telegram approval path.
pub fn parse_prefixed_code(code: &str) -> Option<(&'static str, &str)> {
    let trimmed = code.trim();
    let sep_pos = trimmed.find(['/', '_'])?;
    let prefix = &trimmed[..sep_pos];
    let channel = prefix_to_channel(prefix)?;
    let suffix = &trimmed[sep_pos + 1..];
    // Reject malformed suffixes at parse time so the routing layer never
    // hands a junk code to the approval flow.
    validate_phrase(suffix).ok()?;
    Some((channel, code))
}

/// Validate that a pairing phrase is exactly `PHRASE_WORDS` BIP-39 English
/// words. Hyphen and space separators both accepted (mobile keyboards
/// often autocorrect hyphens to spaces). Returns the canonical
/// hyphen-joined phrase on success.
pub fn validate_phrase(phrase: &str) -> Result<String> {
    let words: Vec<&str> = phrase.split(['-', ' ']).filter(|w| !w.is_empty()).collect();
    if words.len() != PHRASE_WORDS {
        anyhow::bail!(
            "pairing phrase must be {PHRASE_WORDS} words, got {}",
            words.len()
        );
    }
    let wordlist = bip39::Language::English.word_list();
    for w in &words {
        let lower = w.to_lowercase();
        if !wordlist.contains(&lower.as_str()) {
            anyhow::bail!("'{w}' is not a BIP-39 English word");
        }
    }
    Ok(words
        .into_iter()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-"))
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

/// Generate a 6-word pairing phrase from the BIP-39 English wordlist using
/// a cryptographically secure RNG. Output is hyphen-joined lowercase.
///
/// `expect` on `try_fill_bytes` is intentional: if `OsRng` fails, the
/// process has no usable entropy source and there is no safe degraded
/// behavior — emitting a deterministic pairing code would be worse than
/// crashing. The workspace `expect_used` lint is overridden locally rather
/// than globally so this remains the only entropy site that may panic.
#[allow(clippy::expect_used)]
fn generate_phrase() -> String {
    let wordlist = bip39::Language::English.word_list();
    let n = wordlist.len() as u32; // 2048
    let mut rng = OsRng;
    (0..PHRASE_WORDS)
        .map(|_| {
            // Reject-sample to avoid modulo bias against the top of u32's
            // range. With n = 2048 = 2^11 this is essentially never a retry,
            // but the loop costs nothing and pins the invariant.
            let bound = u32::MAX - (u32::MAX % n);
            loop {
                let mut buf = [0u8; 4];
                rng.try_fill_bytes(&mut buf)
                    .expect("OsRng must succeed for pairing code generation");
                let v = u32::from_le_bytes(buf);
                if v < bound {
                    return wordlist[(v % n) as usize];
                }
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Generate a pairing code with channel prefix
/// (e.g. `tg/voyage-kitten-orbit-marble-cinder-pluto`).
/// Falls back to unprefixed phrase for unknown channels.
pub fn generate_code(channel_name: &str) -> String {
    let phrase = generate_phrase();
    match channel_to_prefix(channel_name) {
        Some(prefix) => format!("{prefix}/{phrase}"),
        None => phrase,
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
    fn test_generate_phrase_is_six_bip39_words() {
        // Real failure mode: a code-generator that emits non-wordlist
        // tokens (typo'd words, mixed case, hex chars) would silently
        // bypass the validate_phrase guard at the approval site. Sample
        // 100 codes and assert every word is in the canonical wordlist.
        let wordlist = bip39::Language::English.word_list();
        for _ in 0..100 {
            let phrase = generate_phrase();
            let words: Vec<&str> = phrase.split('-').collect();
            assert_eq!(words.len(), PHRASE_WORDS, "got: {phrase}");
            for w in words {
                assert!(
                    wordlist.iter().any(|listed| *listed == w),
                    "word '{w}' from '{phrase}' not in BIP-39 wordlist"
                );
            }
        }
    }

    #[test]
    fn test_generate_code_with_prefix() {
        let code = generate_code("telegram");
        assert!(code.starts_with("tg/"), "expected tg/ prefix, got: {code}");
        // tg/ + 6 hyphen-joined words → at least 5 hyphens.
        let phrase = code.strip_prefix("tg/").expect("prefix");
        assert_eq!(phrase.split('-').count(), PHRASE_WORDS);
    }

    #[test]
    fn test_generate_code_unknown_channel_no_prefix() {
        let code = generate_code("custom_channel");
        assert!(
            !code.contains('/'),
            "unknown channel should have no slash prefix: {code}"
        );
        assert_eq!(code.split('-').count(), PHRASE_WORDS);
    }

    #[test]
    fn test_generate_phrase_uniqueness_2048_to_the_six() {
        // Real failure mode: a non-CSPRNG source (or constant seed) collapses
        // entropy. With ~66 bits, 1000 samples should be unique with
        // overwhelming probability.
        let codes: HashSet<String> = (0..1000).map(|_| generate_phrase()).collect();
        assert_eq!(codes.len(), 1000, "expected 1000 unique phrases");
    }

    #[test]
    fn test_validate_phrase_round_trip_generated() {
        // Generated codes must always pass validation — guards the contract
        // between the generator and the approval-side validator.
        for _ in 0..50 {
            let phrase = generate_phrase();
            let canon = validate_phrase(&phrase).expect("generated phrase must validate");
            assert_eq!(canon, phrase);
        }
    }

    #[test]
    fn test_validate_phrase_rejects_unknown_word() {
        // Mostly-valid phrases with a single typo'd word must be rejected
        // — that's the entire point of the BIP-39 wordlist.
        let result = validate_phrase("voyage kitten orbit marble cinder NOTAWORD");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_phrase_accepts_space_or_hyphen() {
        // Mobile keyboards autocorrect hyphens to spaces; both must work
        // and produce the same canonical output.
        let phrase = generate_phrase();
        let with_spaces = phrase.replace('-', " ");
        let canon_a = validate_phrase(&phrase).unwrap();
        let canon_b = validate_phrase(&with_spaces).unwrap();
        assert_eq!(canon_a, canon_b);
    }

    #[test]
    fn test_validate_phrase_wrong_word_count_rejected() {
        let phrase = generate_phrase();
        let words: Vec<&str> = phrase.split('-').collect();
        // Drop one word.
        let short = words[..PHRASE_WORDS - 1].join("-");
        assert!(validate_phrase(&short).is_err());
        // Add an extra word.
        let long = format!("{phrase}-marble");
        assert!(validate_phrase(&long).is_err());
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
                assert!(code.starts_with("tg/"), "expected tg/ prefix, got: {code}");
                let phrase = code.strip_prefix("tg/").expect("prefix");
                assert_eq!(phrase.split('-').count(), PHRASE_WORDS);
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
    fn test_channel_prefix_table_round_trips() {
        // Table-driven (per CLAUDE.md): one row per channel, both directions.
        // A typo in either map breaks routing — the per-channel parse_prefixed_code
        // smoke test below pins the end-to-end shape.
        for (channel, expected_prefix) in CHANNEL_PREFIXES {
            assert_eq!(channel_to_prefix(channel), Some(*expected_prefix));
            assert_eq!(prefix_to_channel(expected_prefix), Some(*channel));
            // Case-insensitive parse on both sides.
            assert_eq!(
                channel_to_prefix(&channel.to_uppercase()),
                Some(*expected_prefix)
            );
            assert_eq!(
                prefix_to_channel(&expected_prefix.to_uppercase()),
                Some(*channel)
            );
        }
        assert_eq!(channel_to_prefix("unknown"), None);
        assert_eq!(prefix_to_channel("xx"), None);
    }

    #[test]
    fn test_parse_prefixed_code_canonical_slash() {
        // Use real BIP-39 words so validate_phrase passes. "voyage", "kitten",
        // "orbit", "marble", "cinder", "pluto" — let me pick verified ones:
        // use a freshly generated phrase so the test never drifts from the
        // wordlist.
        let phrase = generate_phrase();
        let tg = format!("tg/{phrase}");
        let sl = format!("sl/{phrase}");
        assert_eq!(parse_prefixed_code(&tg).unwrap().0, "telegram");
        assert_eq!(parse_prefixed_code(&sl).unwrap().0, "slack");
    }

    #[test]
    fn test_parse_prefixed_code_legacy_underscore_still_routes() {
        // In-flight pairing requests minted by older daemons used `_` rather
        // than `/`. We accept both as separators so a stale code isn't bricked
        // mid-flight; new codes always emit `/`. The phrase itself still has
        // to validate, so use a freshly generated one that's guaranteed to
        // be in the BIP-39 wordlist.
        let phrase = generate_phrase();
        let code = format!("tg_{phrase}");
        let (channel, _) = parse_prefixed_code(&code).unwrap();
        assert_eq!(channel, "telegram");
    }

    #[test]
    fn test_parse_prefixed_code_rejects_malformed_suffix() {
        // Real failure mode: a hostile sender crafts `tg_voyage` (or any
        // prefixed-but-not-a-phrase string) and the parser hands it to
        // telegram routing. Pre-validating the suffix at parse time means
        // the approval layer never sees junk codes.
        assert!(parse_prefixed_code("tg_voyage").is_none());
        assert!(parse_prefixed_code("tg/voyage-kitten").is_none()); // 2 words
        assert!(parse_prefixed_code("tg/AAAAA-BBBBB-CCCCC-DDDDD-EEEEE-FFFFF").is_none());
    }

    #[test]
    fn test_parse_prefixed_code_invalid() {
        assert!(parse_prefixed_code("voyage-kitten").is_none());
        assert!(parse_prefixed_code("xx/voyage-kitten").is_none());
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
    fn test_generate_all_channel_prefixes_use_slash_separator() {
        for (channel, expected_prefix) in CHANNEL_PREFIXES {
            let code = generate_code(channel);
            let prefix = format!("{expected_prefix}/");
            assert!(
                code.starts_with(&prefix),
                "channel {channel}: expected prefix {prefix}, got: {code}"
            );
        }
    }
}
