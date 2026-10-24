//! `subscribe` tool: let the agent set up and look after subscriptions in
//! chat ("wake me when Sentry reports a new error and look into it").
//!
//! Every rule lives in [`crate::subscriptions::manage`], shared with the
//! app's Admin RPCs. This file turns arguments into those calls and the
//! results into short text. Refusals come back as `Error: …` text so the
//! model can correct the call and the user sees why.

use std::fmt::Write as _;

use anyhow::Result;
use serde_json::Value;

use super::optional_str_param;
use crate::config::Config;
use crate::db::{Database, SubscriptionEventRow, SubscriptionRow, SubscriptionUpdate};
use crate::heartbeat_task::{resolve_origin_delivery, ORIGIN_DELIVERY};
use crate::subscriptions::manage::{self, Spec, KEY_PLACEHOLDER};
use crate::subscriptions::{presets, Source};

/// Actions that change nothing. They stay allowed in Plan mode, inside
/// subscription runs and for sub-agents.
pub const READ_ACTIONS: &[&str] = &["list", "get", "events", "sources"];

/// Events `events` and `get` show by default.
const DEFAULT_EVENTS: u32 = 10;

/// Whether `args` asks for an action that changes nothing.
pub fn is_read_action(args: &Value) -> bool {
    optional_str_param(args, "action").is_some_and(|a| READ_ACTIONS.contains(&a))
}

/// Run one `subscribe` call.
pub async fn handle_subscribe(args: &Value, config: &Config) -> Result<String> {
    if !config.subscriptions.enabled {
        return Ok(
            "Subscriptions are turned off. Turn on subscriptions.enabled to use them.".into(),
        );
    }
    let action = optional_str_param(args, "action").unwrap_or("");
    let db = match Database::open().await {
        Ok(db) => db,
        Err(e) => return Ok(format!("Error opening database: {e}")),
    };
    let reply = match action {
        "sources" => Ok(sources()),
        "list" => list(&db).await,
        "create" => create(&db, config, args).await,
        "get" | "events" | "update" | "pause" | "resume" | "delete" | "test" => {
            let Some(id) = optional_str_param(args, "id").filter(|s| !s.is_empty()) else {
                return Ok(format!("Error: '{action}' needs the subscription 'id'. Use action='list' to find it."));
            };
            match action {
                "get" => get(&db, config, id).await,
                "events" => events(&db, id, limit(args)).await,
                "update" => update(&db, id, args).await,
                "pause" => set_active(&db, id, false).await,
                "resume" => set_active(&db, id, true).await,
                "delete" => delete(&db, id).await,
                _ => test(&db, config, id).await,
            }
        }
        other => Ok(format!(
            "Error: unknown action '{other}'. Use one of: sources, create, list, get, update, pause, resume, delete, events, test."
        )),
    };
    Ok(reply.unwrap_or_else(|e| format!("Error: {e}")))
}

/// A Unix time as UTC text.
fn when(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0).map_or_else(
        || unix.to_string(),
        |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

fn limit(args: &Value) -> u32 {
    args.get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_EVENTS, |n| n.clamp(1, 50) as u32)
}

fn sources() -> String {
    let mut out = String::from("Sources:\n");
    for p in presets::all() {
        let _ = writeln!(out, "- {} ({}): {}", p.id, p.label, p.blurb);
    }
    out
}

fn status_words(row: &SubscriptionRow) -> String {
    let mut s = row.status.clone();
    if let Some(at) = row.last_event_at {
        let _ = write!(s, ", last event {}", when(at));
    } else {
        s.push_str(", no event yet");
    }
    if let Some(run) = &row.last_run_status {
        let _ = write!(s, ", last run {run}");
    }
    if row.dropped_count > 0 {
        let _ = write!(s, ", {} dropped", row.dropped_count);
    }
    s
}

async fn list(db: &Database) -> Result<String> {
    let rows = db.list_subscriptions().await?;
    if rows.is_empty() {
        return Ok(
            "No subscriptions yet. Use action='sources' to see what Borg can listen to.".into(),
        );
    }
    let mut out = String::new();
    for row in &rows {
        let _ = writeln!(
            out,
            "- {} \"{}\" ({}): {}",
            row.id,
            row.name,
            row.provider,
            status_words(row)
        );
    }
    Ok(out)
}

/// Read the filter argument: an object, or JSON text.
fn filter_arg(args: &Value) -> Result<Option<String>> {
    match args.get("filter") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => {
            let parsed: Value = serde_json::from_str(s)
                .map_err(|e| anyhow::anyhow!("'filter' is not valid JSON: {e}"))?;
            Ok(Some(parsed.to_string()))
        }
        Some(v @ Value::Object(_)) => Ok(Some(v.to_string())),
        Some(_) => anyhow::bail!(
            "'filter' must be an object, for example {{\"kinds\": [\"issue.created\"]}}"
        ),
    }
}

/// The delivery a chat request asks for. "origin" on a channel source means
/// "reply in the thread each message came from" and is kept as is; on any
/// other source it means "this conversation" and becomes its concrete
/// route now, or no delivery when there is no chat to send to.
fn delivery_arg(
    args: &Value,
    source: Source,
) -> (Option<String>, Option<String>, Option<&'static str>) {
    let channel = optional_str_param(args, "delivery_channel")
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    let target = optional_str_param(args, "delivery_target")
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    if channel.as_deref() != Some(ORIGIN_DELIVERY) || source == Source::Channel {
        return (channel, target, None);
    }
    let (channel, target) = resolve_origin_delivery(channel, target);
    let note = channel.is_none().then_some(
        "There is no chat to reply to from here, so results stay in the subscription's own conversation.",
    );
    (channel, target, note)
}

async fn create(db: &Database, config: &Config, args: &Value) -> Result<String> {
    let Some(provider) = optional_str_param(args, "provider").filter(|p| !p.is_empty()) else {
        anyhow::bail!("'create' needs a 'provider'. Use action='sources' to see them.");
    };
    let Some(preset) = presets::get(provider) else {
        anyhow::bail!("There is no \"{provider}\" source. Use action='sources' to see them.");
    };
    let (delivery_channel, delivery_target, note) = delivery_arg(args, preset.source);
    let spec = Spec {
        provider: provider.to_string(),
        name: optional_str_param(args, "name").unwrap_or("").to_string(),
        instruction: optional_str_param(args, "instruction")
            .unwrap_or("")
            .to_string(),
        filter_json: filter_arg(args)?.unwrap_or_default(),
        delivery_channel,
        delivery_target,
        signing_secret: optional_str_param(args, "signing_secret").map(str::to_string),
        rate_limit_per_hour: None,
        timeout_secs: None,
        // Asked for in chat: the user wants it on. An endpoint source waits
        // for the sender to be set up either way.
        active: true,
    };
    let created = manage::create(db, config, &spec).await?;
    let row = &created.row;
    let mut out = format!(
        "Created subscription {} \"{}\" ({}).\n",
        row.id, row.name, row.status
    );
    if let Some(key) = &created.key {
        let endpoint = manage::endpoint_for(config, &row.id);
        let _ = writeln!(out, "Endpoint: {}", endpoint.url);
        let _ = writeln!(out, "Key: {key}");
        if !endpoint.public {
            let _ = writeln!(out, "{}", endpoint.hint);
        }
        let steps = manage::steps(row, &endpoint, key);
        if !steps.is_empty() {
            out.push_str("Set up the sender:\n");
            for (i, step) in steps.iter().enumerate() {
                let _ = writeln!(out, "{}. {step}", i + 1);
            }
        }
        out.push_str("Give the user the endpoint, the key and the steps. The key is shown again only in the app.\n");
    }
    if created.waiting_for_secret {
        let _ = writeln!(
            out,
            "It stays paused until the signing secret {} shows is pasted: call update with signing_secret, then resume.",
            preset.label
        );
    }
    if let Some(note) = note {
        let _ = writeln!(out, "{note}");
    }
    if preset.id == "github" {
        out.push_str("For Borg to comment on pull requests or issues, the GitHub MCP server must be connected.\n");
    }
    Ok(out)
}

async fn get(db: &Database, config: &Config, id: &str) -> Result<String> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(format!("No subscription {id}."));
    };
    let mut out = format!(
        "{} \"{}\" ({}): {}\nInstruction: {}\nFilter: {}\nReports to: {}\n",
        row.id,
        row.name,
        row.provider,
        status_words(&row),
        row.instruction,
        row.filter,
        match row.delivery_channel.as_deref() {
            None => "its own conversation".to_string(),
            Some(ORIGIN_DELIVERY) => "where each message came from".to_string(),
            Some(c) => format!("{c} and its own conversation"),
        },
    );
    if let Some(err) = &row.last_error {
        let _ = writeln!(out, "Last error: {err}");
    }
    if Source::parse(&row.source).is_some_and(Source::uses_endpoint) {
        let endpoint = manage::endpoint_for(config, &row.id);
        let _ = writeln!(
            out,
            "Endpoint: {} (key hidden; the app can show it)",
            endpoint.url
        );
        let steps = manage::steps(&row, &endpoint, KEY_PLACEHOLDER);
        for (i, step) in steps.iter().enumerate() {
            let _ = writeln!(out, "{}. {step}", i + 1);
        }
    }
    out.push_str(&events_text(
        &db.list_subscription_events(id, DEFAULT_EVENTS).await?,
    ));
    Ok(out)
}

fn events_text(events: &[SubscriptionEventRow]) -> String {
    if events.is_empty() {
        return "No events yet.\n".into();
    }
    let mut out = String::from("Recent events:\n");
    for e in events {
        let _ = write!(
            out,
            "- {} [{}] {}",
            when(e.received_at),
            e.status,
            e.summary
        );
        if let Some(err) = &e.error {
            let _ = write!(out, " ({err})");
        }
        out.push('\n');
    }
    out
}

async fn events(db: &Database, id: &str, limit: u32) -> Result<String> {
    if db.get_subscription(id).await?.is_none() {
        return Ok(format!("No subscription {id}."));
    }
    Ok(events_text(&db.list_subscription_events(id, limit).await?))
}

async fn update(db: &Database, id: &str, args: &Value) -> Result<String> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(format!("No subscription {id}."));
    };
    let source = Source::parse(&row.source).unwrap_or(Source::Webhook);
    let wants_delivery =
        args.get("delivery_channel").is_some() || args.get("delivery_target").is_some();
    let (channel, target, note) = delivery_arg(args, source);
    let change = SubscriptionUpdate {
        name: optional_str_param(args, "name")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        instruction: optional_str_param(args, "instruction")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        filter: filter_arg(args)?,
        delivery_channel: wants_delivery.then(|| channel.clone()),
        delivery_target: wants_delivery.then_some(target),
        signing_secret_ref: manage::signing_secret_change(optional_str_param(
            args,
            "signing_secret",
        ))?,
        ..Default::default()
    };
    let Some(updated) = manage::update(db, id, &change).await? else {
        return Ok(format!("No subscription {id}."));
    };
    let mut out = format!(
        "Updated {} \"{}\": {}\n",
        updated.id,
        updated.name,
        status_words(&updated)
    );
    if let Some(note) = note {
        let _ = writeln!(out, "{note}");
    }
    Ok(out)
}

async fn set_active(db: &Database, id: &str, active: bool) -> Result<String> {
    Ok(match manage::set_active(db, id, active).await? {
        Some(row) if active => format!(
            "Resumed {} \"{}\". It reacts to new events now.",
            row.id, row.name
        ),
        Some(row) => format!(
            "Paused {} \"{}\". Events sent while paused are not kept.",
            row.id, row.name
        ),
        None => format!("No subscription {id}."),
    })
}

async fn delete(db: &Database, id: &str) -> Result<String> {
    Ok(if manage::delete(db, id).await? {
        format!("Deleted subscription {id} and its history.")
    } else {
        format!("No subscription {id}.")
    })
}

async fn test(db: &Database, config: &Config, id: &str) -> Result<String> {
    Ok(match manage::test(db, config, id).await? {
        Some(event) => format!(
            "Queued a test event ({}). It runs like a real one, in the subscription's own conversation.",
            event.summary
        ),
        None => format!("No subscription {id}."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_listed_actions_count_as_reads() {
        for (args, read) in [
            (json!({"action": "list"}), true),
            (json!({"action": "events", "id": "x"}), true),
            (json!({"action": "sources"}), true),
            (json!({"action": "create"}), false),
            (json!({"action": "resume"}), false),
            (json!({}), false),
        ] {
            assert_eq!(is_read_action(&args), read, "{args}");
        }
    }

    #[test]
    fn plan_mode_admits_reads_and_blocks_changes() {
        assert!(crate::tool::readonly_tool_call(
            "subscribe",
            r#"{"action":"events","id":"x"}"#
        ));
        assert!(!crate::tool::readonly_tool_call(
            "subscribe",
            r#"{"action":"create"}"#
        ));
        assert!(!crate::tool::readonly_tool_call("subscribe", "not json"));
    }

    #[test]
    fn a_filter_is_an_object_or_json_text() {
        assert_eq!(
            filter_arg(&json!({"filter": {"kinds": ["push"]}}))
                .unwrap()
                .as_deref(),
            Some(r#"{"kinds":["push"]}"#)
        );
        assert_eq!(
            filter_arg(&json!({"filter": "{\"repos\":[\"a/b\"]}"}))
                .unwrap()
                .as_deref(),
            Some(r#"{"repos":["a/b"]}"#)
        );
        assert_eq!(filter_arg(&json!({})).unwrap(), None);
        assert!(filter_arg(&json!({"filter": "{nope"})).is_err());
        assert!(filter_arg(&json!({"filter": 3})).is_err());
    }

    #[test]
    fn origin_outside_a_chat_falls_back_to_the_conversation_with_a_note() {
        let args = json!({"delivery_channel": "origin"});
        let (channel, target, note) = delivery_arg(&args, Source::Webhook);
        assert_eq!((channel, target), (None, None));
        assert!(note.is_some());
        let (channel, _, note) = delivery_arg(&args, Source::Channel);
        assert_eq!(
            channel.as_deref(),
            Some("origin"),
            "a channel source keeps it"
        );
        assert!(note.is_none());
    }

    #[tokio::test]
    async fn origin_inside_a_chat_becomes_that_conversation() {
        let ctx = crate::gateway_context::GatewayOriginContext {
            channel_name: "telegram".into(),
            sender_id: "42".into(),
            thread_id: None,
            session_key: "42".into(),
        };
        let (channel, target, note) = crate::gateway_context::scope(ctx, async {
            delivery_arg(&json!({"delivery_channel": "origin"}), Source::Webhook)
        })
        .await;
        assert_eq!(channel.as_deref(), Some("telegram"));
        assert!(target.unwrap().contains("\"42\""));
        assert!(note.is_none());
    }
}
