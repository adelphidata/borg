//! Create and change subscriptions: the one implementation behind the app's
//! Admin RPCs and the agent's `subscribe` tool, so both make the same rows,
//! the same keys and the same refusals.

use std::sync::RwLock;

use anyhow::{bail, Context, Result};

use super::presets::{self, Preset};
use super::{generate_key, session_key, status, wake, Source};
use crate::config::Config;
use crate::db::{
    Database, NewSubscription, SubscriptionEventRow, SubscriptionRow, SubscriptionUpdate,
};
use crate::security::secrets_resolve::SecretRef;

/// Channels a reply can be sent to.
pub const DELIVERY_CHANNELS: &[&str] = &["slack", "telegram", "discord"];

/// What the sender must be pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The URL to paste into the sender.
    pub url: String,
    /// Whether a service on the internet can reach it.
    pub public: bool,
    /// What to do when it cannot, in plain words. Empty when public.
    pub hint: String,
}

static TUNNEL_URL: RwLock<Option<String>> = RwLock::new(None);

/// Record the running tunnel's public base URL (`None` when it stops). The
/// tunnel supervisor in `borgd` calls this, so every endpoint shown after
/// that names the address the internet can reach.
pub fn set_tunnel_url(url: Option<String>) {
    let url = url.filter(|u| !u.trim().is_empty());
    match TUNNEL_URL.write() {
        Ok(mut g) => *g = url,
        Err(poisoned) => *poisoned.into_inner() = url,
    }
}

fn tunnel_url() -> Option<String> {
    match TUNNEL_URL.read() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The endpoint of subscription `id`: the tunnel's address when one runs,
/// else `gateway.public_url`, else the local gateway (reachable only from
/// this computer or its network).
pub fn endpoint_for(config: &Config, id: &str) -> Endpoint {
    let path = format!("/v1/events/{id}");
    if let Some(base) = tunnel_url() {
        return Endpoint {
            url: format!("{}{path}", base.trim_end_matches('/')),
            public: true,
            hint: String::new(),
        };
    }
    if let Some(base) = config
        .gateway
        .public_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
    {
        return Endpoint {
            url: format!("{}{path}", base.trim_end_matches('/')),
            public: true,
            hint: String::new(),
        };
    }
    let host = match config.gateway.host.as_str() {
        "0.0.0.0" | "::" | "" => "localhost",
        h => h,
    };
    Endpoint {
        url: format!("http://{host}:{}{path}", config.gateway.port),
        public: false,
        hint: "Only this computer can reach this address. Turn on a tunnel so the service can send events to Borg."
            .to_string(),
    }
}

/// What a new subscription needs. Empty strings take the preset's defaults.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    /// Preset id (`github`, `sentry`, `slack`, …).
    pub provider: String,
    /// Display name.
    pub name: String,
    /// What Borg should do.
    pub instruction: String,
    /// Filter JSON.
    pub filter_json: String,
    /// `None`: the subscription's session only.
    pub delivery_channel: Option<String>,
    /// Recipient on `delivery_channel`.
    pub delivery_target: Option<String>,
    /// The signing secret the provider issued, when it issues one.
    pub signing_secret: Option<String>,
    /// Runs per hour; `None` takes the setting.
    pub rate_limit_per_hour: Option<u32>,
    /// Seconds per run; `None` takes the setting.
    pub timeout_secs: Option<u32>,
    /// Start listening at once. The app starts new subscriptions paused
    /// until the sender is set up; a chat request starts them active.
    pub active: bool,
}

/// A subscription just made, with its key when it has an endpoint. The key
/// is shown again only on an explicit reveal.
#[derive(Debug, Clone)]
pub struct Created {
    /// The stored row.
    pub row: SubscriptionRow,
    /// Borg's endpoint key, for endpoint sources.
    pub key: Option<String>,
}

fn literal_ref(value: &str) -> Result<String> {
    serde_json::to_string(&SecretRef::Literal {
        value: value.to_string(),
    })
    .context("encode secret")
}

/// Check a delivery route against the source.
pub fn check_delivery(preset: &Preset, channel: Option<&str>, target: Option<&str>) -> Result<()> {
    let Some(channel) = channel.filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    if channel == crate::heartbeat_task::ORIGIN_DELIVERY {
        if preset.source != Source::Channel {
            bail!("\"Reply where it came from\" works only for channel messages. Pick a channel and a recipient instead.");
        }
        return Ok(());
    }
    if !DELIVERY_CHANNELS.contains(&channel) {
        bail!(
            "Borg cannot send to \"{channel}\". Use one of: {}.",
            DELIVERY_CHANNELS.join(", ")
        );
    }
    if target.is_none_or(|t| t.trim().is_empty()) {
        bail!("Say who on {channel} should get the reply.");
    }
    Ok(())
}

/// Create a subscription.
pub async fn create(db: &Database, config: &Config, spec: &Spec) -> Result<Created> {
    let preset = presets::get(&spec.provider)
        .with_context(|| format!("There is no \"{}\" source.", spec.provider))?;
    check_delivery(
        preset,
        spec.delivery_channel.as_deref(),
        spec.delivery_target.as_deref(),
    )?;
    let key = preset.source.uses_endpoint().then(generate_key);
    let secret_ref = key.as_deref().map(literal_ref).transpose()?;
    let name = non_empty(&spec.name).unwrap_or(preset.default_name);
    let instruction = non_empty(&spec.instruction).unwrap_or(preset.default_instruction);
    let filter = non_empty(&spec.filter_json).unwrap_or("{}");
    let cfg = &config.subscriptions;
    let row = db
        .create_subscription(&NewSubscription {
            name,
            source: preset.source.as_str(),
            provider: preset.id,
            auth_scheme: preset.auth.as_str(),
            secret_ref: secret_ref.as_deref(),
            filter,
            instruction,
            delivery_channel: spec.delivery_channel.as_deref().filter(|c| !c.is_empty()),
            delivery_target: spec.delivery_target.as_deref().filter(|t| !t.is_empty()),
            status: if spec.active {
                status::ACTIVE
            } else {
                status::PAUSED
            },
            rate_limit_per_hour: i64::from(
                spec.rate_limit_per_hour
                    .unwrap_or(cfg.default_rate_limit_per_hour)
                    .max(1),
            ),
            timeout_secs: i64::try_from(
                spec.timeout_secs
                    .map_or(cfg.default_timeout_secs, u64::from)
                    .max(10),
            )
            .unwrap_or(i64::MAX),
        })
        .await?;
    let row = match spec
        .signing_secret
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        Some(secret) if preset.auth.provider_issues_secret() => {
            let update = SubscriptionUpdate {
                signing_secret_ref: Some(Some(literal_ref(secret.trim())?)),
                ..Default::default()
            };
            db.update_subscription(&row.id, &update)
                .await?
                .context("subscription vanished right after insert")?
        }
        _ => row,
    };
    wake::poke();
    Ok(Created { row, key })
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// Turn a subscription on or off. Turning it on after it paused itself is
/// "Resume": the burst windows and the dropped counter start again.
pub async fn set_active(db: &Database, id: &str, active: bool) -> Result<Option<SubscriptionRow>> {
    let row = db
        .set_subscription_status(
            id,
            if active {
                status::ACTIVE
            } else {
                status::PAUSED
            },
            None,
        )
        .await?;
    wake::poke();
    Ok(row)
}

/// Apply a change and check its delivery route against the source.
pub async fn update(
    db: &Database,
    id: &str,
    change: &SubscriptionUpdate,
) -> Result<Option<SubscriptionRow>> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(None);
    };
    let preset = presets::get(&row.provider)
        .with_context(|| format!("There is no \"{}\" source.", row.provider))?;
    if matches!(change.signing_secret_ref, Some(Some(_))) && !preset.auth.provider_issues_secret() {
        bail!(
            "{} does not issue a signing secret. Borg checks its events with the subscription's own key.",
            preset.label
        );
    }
    if change.delivery_channel.is_some() || change.delivery_target.is_some() {
        let channel = change
            .delivery_channel
            .clone()
            .unwrap_or(row.delivery_channel.clone());
        let target = change
            .delivery_target
            .clone()
            .unwrap_or(row.delivery_target.clone());
        check_delivery(preset, channel.as_deref(), target.as_deref())?;
    }
    let updated = db.update_subscription(id, change).await?;
    wake::poke();
    Ok(updated)
}

/// The signing-secret part of an update: `Some("")` clears it.
pub fn signing_secret_change(value: Option<&str>) -> Result<Option<Option<String>>> {
    match value {
        None => Ok(None),
        Some(v) if v.trim().is_empty() => Ok(Some(None)),
        Some(v) => Ok(Some(Some(literal_ref(v.trim())?))),
    }
}

/// Delete a subscription and its history.
pub async fn delete(db: &Database, id: &str) -> Result<bool> {
    let removed = db.delete_subscription(id).await?;
    wake::poke();
    Ok(removed)
}

/// Read the endpoint key. `None` when the subscription has none.
pub async fn reveal_key(db: &Database, id: &str) -> Result<Option<String>> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(None);
    };
    let Some(text) = row.secret_ref else {
        return Ok(None);
    };
    let secret: SecretRef = serde_json::from_str(&text).context("stored key reference")?;
    let key = tokio::task::spawn_blocking(move || secret.resolve())
        .await
        .context("read key")??;
    Ok(Some(key))
}

/// Replace the endpoint key. The old one stops working at once.
pub async fn rotate_key(db: &Database, id: &str) -> Result<Option<String>> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(None);
    };
    let uses_endpoint = Source::parse(&row.source).is_some_and(Source::uses_endpoint);
    if !uses_endpoint {
        bail!("This subscription has no endpoint, so it has no key.");
    }
    let key = generate_key();
    db.set_subscription_secret(id, &literal_ref(&key)?).await?;
    Ok(Some(key))
}

/// The sender steps with the endpoint filled in and `key` in place of the
/// key (pass a placeholder when it is not revealed).
pub fn steps(row: &SubscriptionRow, endpoint: &Endpoint, key: &str) -> Vec<String> {
    presets::get(&row.provider).map_or_else(Vec::new, |p| p.steps_for(&endpoint.url, key))
}

/// What the steps say instead of the key until it is revealed.
pub const KEY_PLACEHOLDER: &str = "YOUR_KEY";

/// Queue the source's sample event, marked as a test, through the real
/// path: the dispatcher runs it like any other event, even while the
/// subscription is paused, so Test shows what a real event would do.
pub async fn test(db: &Database, id: &str) -> Result<Option<SubscriptionEventRow>> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(None);
    };
    let preset = presets::get(&row.provider)
        .with_context(|| format!("There is no \"{}\" source.", row.provider))?;
    let sample = preset.sample();
    let normalized = preset.normalize(&|_| None, &sample);
    let summary = format!("Test: {}", normalized.summary);
    let event = db
        .insert_subscription_test_event(&row.id, &summary, preset.sample_payload)
        .await?;
    wake::poke();
    Ok(Some(event))
}

/// The session a subscription's runs land in.
pub fn session_of(row: &SubscriptionRow) -> String {
    session_key(&row.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(provider: &str) -> Spec {
        Spec {
            provider: provider.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn endpoint_sources_get_a_key_and_others_do_not() {
        let db = Database::test_db().await;
        let config = Config::default();
        let github = create(&db, &config, &spec("github")).await.unwrap();
        let key = github.key.expect("an endpoint source has a key");
        assert!(key.starts_with("bsk_"));
        assert_eq!(github.row.status, "paused", "the app starts it paused");
        assert_eq!(
            github.row.name, "GitHub activity",
            "the preset fills the name"
        );
        assert_eq!(
            reveal_key(&db, &github.row.id).await.unwrap().as_deref(),
            Some(key.as_str())
        );

        let slack = create(
            &db,
            &config,
            &Spec {
                active: true,
                ..spec("slack")
            },
        )
        .await
        .unwrap();
        assert!(slack.key.is_none());
        assert_eq!(slack.row.status, "active");
        assert!(rotate_key(&db, &slack.row.id).await.is_err());

        let rotated = rotate_key(&db, &github.row.id).await.unwrap().unwrap();
        assert_ne!(rotated, key, "rotation makes a new key");
        assert_eq!(
            reveal_key(&db, &github.row.id).await.unwrap(),
            Some(rotated)
        );
    }

    #[tokio::test]
    async fn a_signing_secret_is_kept_only_for_providers_that_issue_one() {
        let db = Database::test_db().await;
        let config = Config::default();
        let with = |p: &str| Spec {
            signing_secret: Some("whsec_abc".into()),
            ..spec(p)
        };
        let stripe = create(&db, &config, &with("stripe")).await.unwrap();
        assert!(stripe.row.signing_secret_ref.is_some());
        let github = create(&db, &config, &with("github")).await.unwrap();
        assert!(
            github.row.signing_secret_ref.is_none(),
            "GitHub signs with Borg's key"
        );
    }

    #[tokio::test]
    async fn a_signing_secret_change_is_refused_where_the_provider_issues_none() {
        let db = Database::test_db().await;
        let config = Config::default();
        let github = create(&db, &config, &spec("github")).await.unwrap();
        let change = SubscriptionUpdate {
            signing_secret_ref: signing_secret_change(Some("abc")).unwrap(),
            ..Default::default()
        };
        assert!(update(&db, &github.row.id, &change).await.is_err());
        let stripe = create(&db, &config, &spec("stripe")).await.unwrap();
        let updated = update(&db, &stripe.row.id, &change).await.unwrap().unwrap();
        assert!(updated.signing_secret_ref.is_some());
        let cleared = SubscriptionUpdate {
            signing_secret_ref: signing_secret_change(Some("")).unwrap(),
            ..Default::default()
        };
        let updated = update(&db, &stripe.row.id, &cleared)
            .await
            .unwrap()
            .unwrap();
        assert!(updated.signing_secret_ref.is_none());
    }

    #[test]
    fn delivery_routes_are_checked_against_the_source() {
        let slack = presets::get("slack").unwrap();
        let sentry = presets::get("sentry").unwrap();
        assert!(check_delivery(slack, Some("origin"), None).is_ok());
        assert!(check_delivery(sentry, Some("origin"), None).is_err());
        assert!(
            check_delivery(sentry, Some("telegram"), None).is_err(),
            "needs a recipient"
        );
        assert!(check_delivery(sentry, Some("telegram"), Some("42")).is_ok());
        assert!(check_delivery(sentry, Some("pager"), Some("x")).is_err());
        assert!(check_delivery(sentry, None, None).is_ok());
        assert!(check_delivery(sentry, Some(""), None).is_ok());
    }

    #[test]
    fn the_endpoint_prefers_the_tunnel_then_the_public_url_then_local() {
        let mut config = Config::default();
        config.gateway.host = "0.0.0.0".into();
        config.gateway.port = 7777;
        set_tunnel_url(None);
        let local = endpoint_for(&config, "sub_x");
        assert_eq!(local.url, "http://localhost:7777/v1/events/sub_x");
        assert!(!local.public);
        assert!(!local.hint.is_empty());

        config.gateway.public_url = Some("https://borg.example.com/".into());
        let public = endpoint_for(&config, "sub_x");
        assert_eq!(public.url, "https://borg.example.com/v1/events/sub_x");
        assert!(public.public);

        set_tunnel_url(Some("https://abc.trycloudflare.com".into()));
        assert_eq!(
            endpoint_for(&config, "sub_x").url,
            "https://abc.trycloudflare.com/v1/events/sub_x"
        );
        set_tunnel_url(None);
    }

    #[tokio::test]
    async fn a_test_event_runs_even_while_paused() {
        let db = Database::test_db().await;
        let config = Config::default();
        let created = create(&db, &config, &spec("sentry")).await.unwrap();
        let event = test(&db, &created.row.id).await.unwrap().unwrap();
        assert_eq!(event.kind.as_deref(), Some(crate::subscriptions::TEST_KIND));
        assert!(event.summary.starts_with("Test: "));
        let now = chrono::Utc::now().timestamp();
        let claimed = db
            .claim_next_subscription_event("run_t", now)
            .await
            .unwrap();
        assert_eq!(
            claimed.map(|e| e.id),
            Some(event.id),
            "the paused subscription's test runs"
        );
    }
}
