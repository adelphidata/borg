//! Create and change subscriptions: the one implementation behind the app's
//! Admin RPCs and the agent's `subscribe` tool, so both make the same rows,
//! the same keys and the same refusals.
//!
//! A request Borg will not carry out fails with a [`Refusal`], whose text is
//! a sentence for the user. Any other error is a fault in Borg (a locked
//! database, an unreadable key), and callers report it as one.

use std::sync::{PoisonError, RwLock};

use anyhow::{Context, Result};

use super::presets::{self, AuthScheme, Preset};
use super::{generate_key, session_key, status, wake, Source};
use crate::config::Config;
use crate::db::{
    Database, NewSubscription, SubscriptionEventRow, SubscriptionRow, SubscriptionUpdate,
};
use crate::security::secrets_resolve::SecretRef;

/// Channels a reply can be sent to.
pub const DELIVERY_CHANNELS: &[&str] = &["slack", "telegram", "discord"];

/// Shortest run timeout, in seconds.
const MIN_TIMEOUT_SECS: i64 = 10;

/// A request Borg will not carry out. The text is meant for the user.
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

fn refuse(text: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal(text.into()))
}

/// The user-facing text when `e` is a [`Refusal`], `None` for a fault.
pub fn refusal_text(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<Refusal>().map(|r| r.0.as_str())
}

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
    *TUNNEL_URL.write().unwrap_or_else(PoisonError::into_inner) = url;
}

fn tunnel_url() -> Option<String> {
    TUNNEL_URL
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The endpoint of subscription `id`: `gateway.public_url` when the user set
/// one (a stable address they chose), else the running tunnel's address,
/// else the local gateway (reachable only from this computer or its
/// network). A quick tunnel's address changes when it restarts, so it never
/// wins over an address the user set.
pub fn endpoint_for(config: &Config, id: &str) -> Endpoint {
    let path = format!("/v1/events/{id}");
    let public = config
        .gateway
        .public_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .or_else(tunnel_url);
    if let Some(base) = public {
        return Endpoint {
            url: format!("{}{path}", base.trim_end_matches('/')),
            public: true,
            hint: String::new(),
        };
    }
    let host = match config.gateway.host.as_str() {
        "0.0.0.0" | "::" | "" => "localhost".to_string(),
        h if h.contains(':') && !h.starts_with('[') => format!("[{h}]"),
        h => h.to_string(),
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
    /// The signing secret the provider shows, for providers that sign.
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
    /// It was asked to start active but waits paused for the provider's
    /// signing secret, which the provider shows only once the endpoint is
    /// added there.
    pub waiting_for_secret: bool,
}

fn literal_ref(value: &str) -> Result<String> {
    serde_json::to_string(&SecretRef::Literal {
        value: value.to_string(),
    })
    .context("encode secret")
}

/// Whether the source can use a signing secret: those that issue their own,
/// and GitHub, which signs with whatever secret its webhook holds (Borg's
/// key by default, or the user's own).
fn takes_signing_secret(preset: &Preset) -> bool {
    preset.auth.provider_issues_secret() || preset.auth == AuthScheme::Github
}

/// The provider signs every request with a secret only it issues, so Borg
/// cannot check its events until that secret is pasted.
fn needs_signing_secret(preset: &Preset, has_secret: bool) -> bool {
    preset.auth.provider_issues_secret() && !has_secret
}

fn secret_missing(preset: &Preset) -> anyhow::Error {
    refuse(format!(
        "Paste the signing secret {label} shows first. Without it Borg cannot check that events really come from {label}.",
        label = preset.label
    ))
}

fn preset_of(provider: &str) -> Result<&'static Preset> {
    presets::get(provider).ok_or_else(|| refuse(format!("There is no \"{provider}\" source.")))
}

fn check_filter(filter: &str) -> Result<()> {
    if crate::subscriptions::filter::Filter::parse(filter).is_none() {
        return Err(refuse(
            "The filter is not valid. Use only the fields the source offers (kinds, severity_min, phrase, …).",
        ));
    }
    Ok(())
}

/// Check a delivery route against the source.
pub fn check_delivery(preset: &Preset, channel: Option<&str>, target: Option<&str>) -> Result<()> {
    let Some(channel) = channel.filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    if channel == crate::heartbeat_task::ORIGIN_DELIVERY {
        if preset.source != Source::Channel {
            return Err(refuse("\"Reply where it came from\" works only for channel messages. Pick a channel and a recipient instead."));
        }
        return Ok(());
    }
    if !DELIVERY_CHANNELS.contains(&channel) {
        return Err(refuse(format!(
            "Borg cannot send to \"{channel}\". Use one of: {}.",
            DELIVERY_CHANNELS.join(", ")
        )));
    }
    if target.is_none_or(|t| t.trim().is_empty()) {
        return Err(refuse(format!(
            "Say who on {channel} should get the reply."
        )));
    }
    Ok(())
}

/// Create a subscription.
pub async fn create(db: &Database, config: &Config, spec: &Spec) -> Result<Created> {
    let preset = preset_of(&spec.provider)?;
    check_delivery(
        preset,
        spec.delivery_channel.as_deref(),
        spec.delivery_target.as_deref(),
    )?;
    let filter = non_empty(&spec.filter_json).unwrap_or("{}");
    check_filter(filter)?;
    let secret = spec
        .signing_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if secret.is_some() && !takes_signing_secret(preset) {
        return Err(refuse(format!(
            "{} does not use a signing secret. Borg checks its events with the subscription's own key.",
            preset.label
        )));
    }
    let waiting_for_secret = spec.active && needs_signing_secret(preset, secret.is_some());
    let active = spec.active && !waiting_for_secret;
    let key = preset.source.uses_endpoint().then(generate_key);
    let secret_ref = key.as_deref().map(literal_ref).transpose()?;
    let name = non_empty(&spec.name).unwrap_or(preset.default_name);
    let instruction = non_empty(&spec.instruction).unwrap_or(preset.default_instruction);
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
            status: if active {
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
                    .map_or(cfg.default_timeout_secs, u64::from),
            )
            .unwrap_or(i64::MAX)
            .max(MIN_TIMEOUT_SECS),
        })
        .await?;
    let row = match secret {
        Some(secret) => {
            let update = SubscriptionUpdate {
                signing_secret_ref: Some(Some(literal_ref(secret)?)),
                ..Default::default()
            };
            db.update_subscription(&row.id, &update)
                .await?
                .context("subscription vanished right after insert")?
        }
        None => row,
    };
    wake::poke();
    Ok(Created {
        row,
        key,
        waiting_for_secret,
    })
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// Turn a subscription on or off. Turning it on after it paused itself is
/// "Resume": the burst windows and the dropped counter start again. A
/// provider that signs its requests cannot be turned on before its signing
/// secret is pasted: every event would be refused, and nothing would say why.
pub async fn set_active(db: &Database, id: &str, active: bool) -> Result<Option<SubscriptionRow>> {
    if active {
        let Some(row) = db.get_subscription(id).await? else {
            return Ok(None);
        };
        let preset = preset_of(&row.provider)?;
        if needs_signing_secret(preset, row.signing_secret_ref.is_some()) {
            return Err(secret_missing(preset));
        }
    }
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

/// Apply a change after checking it against the source.
pub async fn update(
    db: &Database,
    id: &str,
    change: &SubscriptionUpdate,
) -> Result<Option<SubscriptionRow>> {
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(None);
    };
    let preset = preset_of(&row.provider)?;
    match &change.signing_secret_ref {
        Some(Some(_)) if !takes_signing_secret(preset) => {
            return Err(refuse(format!(
                "{} does not use a signing secret. Borg checks its events with the subscription's own key.",
                preset.label
            )));
        }
        Some(None) if row.status == status::ACTIVE && preset.auth.provider_issues_secret() => {
            return Err(refuse(
                "Pause the subscription before you remove its signing secret, or paste the new secret instead.",
            ));
        }
        _ => {}
    }
    if let Some(filter) = &change.filter {
        check_filter(filter)?;
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
    let change = SubscriptionUpdate {
        rate_limit_per_hour: change.rate_limit_per_hour.map(|r| r.max(1)),
        timeout_secs: change.timeout_secs.map(|t| t.max(MIN_TIMEOUT_SECS)),
        ..change.clone()
    };
    let updated = db.update_subscription(id, &change).await?;
    // The conversation list names the session after the subscription, so a
    // rename follows it there. The subscription is already renamed; a
    // failure here only leaves the old title in the list.
    if let (Some(name), Some(_)) = (&change.name, &updated) {
        if let Err(e) = db.update_session_title(&session_key(id), name).await {
            tracing::warn!(subscription = %id, "rename: session title not updated: {e:#}");
        }
    }
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
        return Err(refuse(
            "This subscription has no endpoint, so it has no key.",
        ));
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
/// subscription is paused, so Test shows what a real event would do. It
/// runs alone, never digested with real events.
pub async fn test(
    db: &Database,
    config: &Config,
    id: &str,
) -> Result<Option<SubscriptionEventRow>> {
    if !config.subscriptions.enabled {
        return Err(refuse(
            "Subscriptions are turned off, so a test would never run. Turn on \"Let subscriptions wake Borg\" in Settings first.",
        ));
    }
    let Some(row) = db.get_subscription(id).await? else {
        return Ok(None);
    };
    let preset = preset_of(&row.provider)?;
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
        let err = rotate_key(&db, &slack.row.id).await.unwrap_err();
        assert!(refusal_text(&err).is_some(), "a refusal, not a fault");

        let rotated = rotate_key(&db, &github.row.id).await.unwrap().unwrap();
        assert_ne!(rotated, key, "rotation makes a new key");
        assert_eq!(
            reveal_key(&db, &github.row.id).await.unwrap(),
            Some(rotated)
        );
    }

    #[tokio::test]
    async fn signing_secrets_are_kept_where_the_source_uses_one_and_refused_elsewhere() {
        let db = Database::test_db().await;
        let config = Config::default();
        let with = |p: &str| Spec {
            signing_secret: Some(" whsec_abc ".into()),
            ..spec(p)
        };
        let stripe = create(&db, &config, &with("stripe")).await.unwrap();
        assert!(stripe.row.signing_secret_ref.is_some());
        let github = create(&db, &config, &with("github")).await.unwrap();
        assert!(
            github.row.signing_secret_ref.is_some(),
            "GitHub can sign with the user's own secret"
        );
        let err = create(&db, &config, &with("datadog")).await.unwrap_err();
        assert!(refusal_text(&err)
            .unwrap()
            .contains("does not use a signing secret"));

        let custom = create(&db, &config, &spec("custom")).await.unwrap();
        let set = SubscriptionUpdate {
            signing_secret_ref: signing_secret_change(Some("abc")).unwrap(),
            ..Default::default()
        };
        assert!(update(&db, &custom.row.id, &set).await.is_err());
        assert!(update(&db, &github.row.id, &set).await.is_ok());
    }

    #[tokio::test]
    async fn a_signing_provider_waits_paused_until_its_secret_is_pasted() {
        let db = Database::test_db().await;
        let config = Config::default();
        let asked_active = Spec {
            active: true,
            ..spec("stripe")
        };
        let created = create(&db, &config, &asked_active).await.unwrap();
        assert_eq!(created.row.status, "paused");
        assert!(created.waiting_for_secret);
        let id = created.row.id;

        let err = set_active(&db, &id, true).await.unwrap_err();
        assert!(refusal_text(&err).unwrap().contains("signing secret"));

        let paste = SubscriptionUpdate {
            signing_secret_ref: signing_secret_change(Some("whsec_x")).unwrap(),
            ..Default::default()
        };
        update(&db, &id, &paste).await.unwrap();
        let on = set_active(&db, &id, true).await.unwrap().unwrap();
        assert_eq!(on.status, "active");

        let clear = SubscriptionUpdate {
            signing_secret_ref: signing_secret_change(Some("")).unwrap(),
            ..Default::default()
        };
        assert!(
            update(&db, &id, &clear).await.is_err(),
            "an active one keeps its secret"
        );
        set_active(&db, &id, false).await.unwrap();
        let cleared = update(&db, &id, &clear).await.unwrap().unwrap();
        assert!(cleared.signing_secret_ref.is_none());
    }

    #[tokio::test]
    async fn a_rename_renames_the_subscription_session_too() {
        let db = Database::test_db().await;
        let created = create(&db, &Config::default(), &spec("custom"))
            .await
            .unwrap();
        let id = created.row.id;
        db.upsert_session(&session_key(&id), 1, 1, 0, "m", &created.row.name)
            .await
            .unwrap();
        let change = SubscriptionUpdate {
            name: Some("Checkout errors".into()),
            ..Default::default()
        };
        update(&db, &id, &change).await.unwrap().unwrap();
        let session = db.session_by_id(&session_key(&id)).await.unwrap().unwrap();
        assert_eq!(session.title, "Checkout errors");
    }

    #[tokio::test]
    async fn updates_keep_the_limits_and_refuse_a_bad_filter_in_plain_words() {
        let db = Database::test_db().await;
        let config = Config::default();
        let created = create(&db, &config, &spec("custom")).await.unwrap();
        let change = SubscriptionUpdate {
            rate_limit_per_hour: Some(0),
            timeout_secs: Some(1),
            ..Default::default()
        };
        let row = update(&db, &created.row.id, &change)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.rate_limit_per_hour, 1);
        assert_eq!(row.timeout_secs, MIN_TIMEOUT_SECS);

        let bad = SubscriptionUpdate {
            filter: Some(r#"{"severity":"error"}"#.into()),
            ..Default::default()
        };
        let err = update(&db, &created.row.id, &bad).await.unwrap_err();
        assert!(refusal_text(&err).is_some());
        let err = create(
            &db,
            &config,
            &Spec {
                filter_json: "{nope".into(),
                ..spec("custom")
            },
        )
        .await
        .unwrap_err();
        assert!(refusal_text(&err).is_some());
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
    fn the_endpoint_prefers_the_users_address_then_the_tunnel_then_local() {
        let mut config = Config::default();
        config.gateway.host = "0.0.0.0".into();
        config.gateway.port = 7777;
        set_tunnel_url(None);
        let local = endpoint_for(&config, "sub_x");
        assert_eq!(local.url, "http://localhost:7777/v1/events/sub_x");
        assert!(!local.public);
        assert!(!local.hint.is_empty());

        config.gateway.host = "::1".into();
        assert_eq!(
            endpoint_for(&config, "sub_x").url,
            "http://[::1]:7777/v1/events/sub_x"
        );

        set_tunnel_url(Some("https://abc.trycloudflare.com".into()));
        let tunnel = endpoint_for(&config, "sub_x");
        assert_eq!(tunnel.url, "https://abc.trycloudflare.com/v1/events/sub_x");
        assert!(tunnel.public);

        config.gateway.public_url = Some("https://borg.example.com/".into());
        assert_eq!(
            endpoint_for(&config, "sub_x").url,
            "https://borg.example.com/v1/events/sub_x",
            "an address the user set wins over a tunnel's"
        );
        set_tunnel_url(None);
    }

    #[tokio::test]
    async fn a_test_event_runs_even_while_paused() {
        let db = Database::test_db().await;
        let config = Config::default();
        let created = create(&db, &config, &spec("sentry")).await.unwrap();
        let event = test(&db, &config, &created.row.id).await.unwrap().unwrap();
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

    #[tokio::test]
    async fn test_is_refused_while_subscriptions_are_off() {
        let db = Database::test_db().await;
        let mut config = Config::default();
        let created = create(&db, &config, &spec("sentry")).await.unwrap();
        config.subscriptions.enabled = false;
        let err = test(&db, &config, &created.row.id).await.unwrap_err();
        assert!(refusal_text(&err).unwrap().contains("turned off"));
    }
}
