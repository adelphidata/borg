//! Native channel send helpers used by heartbeat + approval greeting.

use anyhow::{anyhow, Result};
use cortex::config::Config;

/// The credential each native delivery channel needs.
///
/// [`send_threaded`] and [`precheck`] both read this table, so a channel can
/// never be deliverable by one and unknown to the other — which is exactly the
/// drift that would let a pre-dispatch check pass a task that delivery then
/// drops.
///
/// Deliberately narrower than `Config::detected_native_channels`, which lists
/// every native channel Borg can *receive* on. This is the set `send_threaded`
/// has an arm for; merging them would make delivery claim channels it cannot
/// send to. Rename an env key there and it must be renamed here too.
const CHANNEL_TOKENS: &[(&str, &str)] = &[
    ("telegram", "TELEGRAM_BOT_TOKEN"),
    ("slack", "SLACK_BOT_TOKEN"),
    ("discord", "DISCORD_BOT_TOKEN"),
];

/// The credential name `channel` sends with, or `None` if delivery does not
/// support the channel at all.
pub fn channel_token_env(channel: &str) -> Option<&'static str> {
    CHANNEL_TOKENS
        .iter()
        .find(|(name, _)| *name == channel)
        .map(|(_, env)| *env)
}

/// Whether a send to `channel` could succeed at all, without touching the
/// network. `Err` carries a reason phrased for the operator.
///
/// This checks configuration only. A reachability probe would fail closed on a
/// flaky link and block a task that is perfectly well configured.
///
/// `has_credential` is injected rather than read from `Config` directly:
/// `resolve_credential_or_env` consults the process environment and the OS
/// keychain, so a test that called it would pass or fail depending on the
/// developer's machine.
pub fn precheck(
    channel: &str,
    has_credential: impl Fn(&str) -> bool,
) -> std::result::Result<(), String> {
    let Some(env) = channel_token_env(channel) else {
        // Names the supported channels from the table itself, so adding a send
        // arm cannot leave the advice listing a stale set.
        let supported = CHANNEL_TOKENS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "delivery channel '{channel}' is not supported — \
             ask Borg to set the delivery channel to one of {supported}"
        ));
    };
    if !has_credential(env) {
        return Err(format!(
            "delivery channel '{channel}' has no credential — set {env}"
        ));
    }
    Ok(())
}

/// Send `text` to `sender_id` on `channel`, optionally targeted at a thread.
pub async fn send(config: &Config, channel: &str, sender_id: &str, text: &str) -> Result<()> {
    send_threaded(config, channel, sender_id, text, None).await
}

/// The text that really goes to the channel: `text` with secrets redacted.
///
/// Every daemon-originated send (task results, failure and skip notices,
/// heartbeat, greetings) passes through here, so no lane can hand a raw
/// credential to a chat channel. Clean text comes back borrowed and
/// byte-identical, so a caller that already redacted pays nothing.
pub(crate) fn prepare_outbound<'a>(
    channel: &str,
    sender_id: &str,
    text: &'a str,
) -> std::borrow::Cow<'a, str> {
    cortex::output_redaction::current_policy().apply(
        cortex::output_redaction::OutputSink::Channel(channel),
        Some(sender_id),
        text,
    )
}

async fn send_threaded(
    config: &Config,
    channel: &str,
    sender_id: &str,
    text: &str,
    thread_id: Option<&str>,
) -> Result<()> {
    let text = prepare_outbound(channel, sender_id, text);
    let text = text.as_ref();
    let token_env = channel_token_env(channel)
        .ok_or_else(|| anyhow!("Channel '{channel}' not supported for delivery"))?;
    let token = config
        .resolve_credential_or_env(token_env)
        .ok_or_else(|| anyhow!("{token_env} not configured"))?;
    match channel {
        "telegram" => {
            let client = gateway::telegram::api::TelegramClient::new(&token)?;
            let chat_id: i64 = sender_id
                .parse()
                .map_err(|_| anyhow!("Invalid Telegram chat_id: {sender_id}"))?;
            let message_thread_id: Option<i64> = thread_id.and_then(|s| s.parse().ok());
            client
                .send_message(chat_id, text, None, None, message_thread_id)
                .await
        }
        "slack" => {
            let client = gateway::slack::api::SlackClient::new(&token)?;
            client
                .post_message(sender_id, text, thread_id)
                .await
                .map(|_| ())
        }
        "discord" => {
            let client = gateway::discord::api::DiscordClient::new(&token)?;
            let channel_id = thread_id.unwrap_or(sender_id);
            client.send_message(channel_id, text).await
        }
        // Unreachable: `channel_token_env` above already rejected anything not
        // in the table. Kept as a hard error rather than a panic so adding a
        // table row without a send arm degrades instead of aborting.
        other => Err(anyhow!("Channel '{other}' not supported for delivery")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_text_has_its_secrets_redacted() {
        let cases = [
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk",
            "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ012345678",
        ];
        for token in cases {
            let text = format!("[Task: sync]\nError: auth failed for {token} on push");
            for (channel, _) in CHANNEL_TOKENS {
                let out = prepare_outbound(channel, "12345", &text);
                assert!(
                    !out.contains(token),
                    "{channel}: the token must not reach the channel: {out}"
                );
                assert!(
                    out.contains("[REDACTED"),
                    "{channel}: the cut must be visible: {out}"
                );
                assert!(
                    out.starts_with("[Task: sync]\nError: auth failed for ")
                        && out.ends_with(" on push"),
                    "{channel}: the text around the secret must survive: {out}"
                );
            }
        }
    }

    #[test]
    fn clean_outbound_text_is_byte_identical() {
        let text = "[Task: brief]\n3 PRs open, 1 has a failed check.\n*bold* `code` <tag>";
        let out = prepare_outbound("telegram", "12345", text);
        assert_eq!(out, text);
        assert!(
            matches!(out, std::borrow::Cow::Borrowed(_)),
            "clean text must not be copied"
        );
    }

    #[test]
    fn a_credentialled_channel_passes() {
        for (channel, _) in CHANNEL_TOKENS {
            assert!(
                precheck(channel, |_| true).is_ok(),
                "{channel} should pass once its credential resolves"
            );
        }
    }

    #[test]
    fn an_unsupported_channel_is_refused_by_name() {
        let err = precheck("carrier-pigeon", |_| true).unwrap_err();
        assert!(err.contains("carrier-pigeon"), "{err}");
        assert!(err.contains("telegram"), "the reason must name a way out");
    }

    #[test]
    fn a_missing_credential_names_the_variable_to_set() {
        for (channel, env) in CHANNEL_TOKENS {
            let err = precheck(channel, |_| false).unwrap_err();
            assert!(
                err.contains(env),
                "{channel} must name {env} as the fix, got: {err}"
            );
        }
    }
}
