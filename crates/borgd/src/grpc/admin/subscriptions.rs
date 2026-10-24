//! Subscription RPC bodies for the admin service (`super::AdminSvc`).
//!
//! Thin: every rule (defaults, key handling, delivery checks, Test) lives in
//! `cortex::subscriptions::manage`, which the `subscribe` tool also calls.
//! Refusals from there are plain sentences meant for the user, so they go
//! out as `InvalidArgument` with the text unchanged.

use super::*;
use cortex::config::Config;
use cortex::db::{SubscriptionEventRow, SubscriptionRow, SubscriptionUpdate};
use cortex::subscriptions::manage::{self, Endpoint, Spec, KEY_PLACEHOLDER};
use cortex::subscriptions::presets::{self, Preset};
use cortex::subscriptions::{session_key, Source};
use protocols::admin::{
    Subscription as PbSubscription, SubscriptionActivityEvent as PbSubscriptionActivity,
    SubscriptionChange, SubscriptionCreated, SubscriptionDetail,
    SubscriptionEndpoint as PbEndpoint, SubscriptionEvent as PbSubscriptionEvent,
    SubscriptionEventList, SubscriptionEventsRequest, SubscriptionKey, SubscriptionList,
    SubscriptionPreset as PbPreset, SubscriptionPresetList, SubscriptionRef, SubscriptionSpec,
    SubscriptionStatusRequest,
};

/// Payload bytes an event carries to the app. The ledger keeps up to
/// `subscriptions.payload_max_bytes`; a list view needs far less.
const DISPLAY_PAYLOAD_BYTES: usize = 16 * 1024;

/// Recent events on the detail page.
const DETAIL_EVENTS: u32 = 50;

/// Streamed activity type of `Admin.SubscribeSubscriptionActivity`.
pub type SubscriptionActivityStream =
    Pin<Box<dyn Stream<Item = Result<PbSubscriptionActivity, TStatus>> + Send + 'static>>;

#[allow(clippy::result_large_err)]
fn refused(e: anyhow::Error) -> TStatus {
    TStatus::invalid_argument(e.to_string())
}

fn not_found() -> TStatus {
    TStatus::not_found("That subscription no longer exists.")
}

async fn open() -> Result<(Database, Config), TStatus> {
    let db = Database::open().await.map_err(map_internal("open db"))?;
    let config = Config::load_from_db()
        .await
        .map_err(map_internal("load config"))?;
    Ok((db, config))
}

fn endpoint_of(row: &SubscriptionRow, config: &Config) -> Option<Endpoint> {
    Source::parse(&row.source)
        .is_some_and(Source::uses_endpoint)
        .then(|| manage::endpoint_for(config, &row.id))
}

pub(super) fn subscription_to_pb(row: SubscriptionRow, config: &Config) -> PbSubscription {
    let endpoint = endpoint_of(&row, config).map(|e| PbEndpoint {
        url: e.url,
        public: e.public,
        hint: e.hint,
    });
    PbSubscription {
        session_key: session_key(&row.id),
        endpoint,
        has_signing_secret: row.signing_secret_ref.is_some(),
        id: row.id,
        name: row.name,
        source: row.source,
        provider: row.provider,
        status: row.status,
        filter_json: row.filter,
        instruction: row.instruction,
        delivery_channel: row.delivery_channel.unwrap_or_default(),
        delivery_target: row.delivery_target.unwrap_or_default(),
        rate_limit_per_hour: u32::try_from(row.rate_limit_per_hour).unwrap_or(u32::MAX),
        timeout_secs: u32::try_from(row.timeout_secs).unwrap_or(u32::MAX),
        dropped_count: u64::try_from(row.dropped_count).unwrap_or(0),
        created_at_unix: row.created_at,
        updated_at_unix: row.updated_at,
        last_event_at_unix: row.last_event_at.unwrap_or(0),
        last_run_status: row.last_run_status.unwrap_or_default(),
        last_error: row.last_error.unwrap_or_default(),
    }
}

/// Cut `s` to at most `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

pub(super) fn event_to_pb(e: SubscriptionEventRow) -> PbSubscriptionEvent {
    let (payload, cut_here) = cut(&e.payload, DISPLAY_PAYLOAD_BYTES);
    PbSubscriptionEvent {
        payload: payload.to_string(),
        payload_truncated: e.payload_truncated || cut_here,
        id: e.id,
        subscription_id: e.subscription_id,
        kind: e.kind.unwrap_or_default(),
        summary: e.summary,
        status: e.status,
        run_id: e.run_id.unwrap_or_default(),
        received_at_unix: e.received_at,
        started_at_unix: e.started_at.unwrap_or(0),
        finished_at_unix: e.finished_at.unwrap_or(0),
        duration_ms: e.duration_ms.unwrap_or(0),
        result: e.result.unwrap_or_default(),
        error: e.error.unwrap_or_default(),
    }
}

fn preset_to_pb(p: &Preset) -> PbPreset {
    PbPreset {
        id: p.id.to_string(),
        label: p.label.to_string(),
        blurb: p.blurb.to_string(),
        group: p.group.to_string(),
        source: p.source.as_str().to_string(),
        auth_scheme: p.auth.as_str().to_string(),
        takes_signing_secret: p.auth.provider_issues_secret(),
        kinds: p.kinds.iter().map(|k| (*k).to_string()).collect(),
        filter_fields: p.filter_fields.iter().map(|f| (*f).to_string()).collect(),
        default_name: p.default_name.to_string(),
        default_instruction: p.default_instruction.to_string(),
        sample_payload: p.sample_payload.to_string(),
    }
}

fn steps_with(row: &SubscriptionRow, config: &Config, key: &str) -> Vec<String> {
    match endpoint_of(row, config) {
        Some(endpoint) => manage::steps(row, &endpoint, key),
        None => manage::steps(
            row,
            &Endpoint {
                url: String::new(),
                public: false,
                hint: String::new(),
            },
            key,
        ),
    }
}

pub(super) async fn list_presets() -> Result<Response<SubscriptionPresetList>, TStatus> {
    Ok(Response::new(SubscriptionPresetList {
        presets: presets::all().iter().map(preset_to_pb).collect(),
    }))
}

pub(super) async fn list() -> Result<Response<SubscriptionList>, TStatus> {
    let (db, config) = open().await?;
    let rows = db
        .list_subscriptions()
        .await
        .map_err(map_internal("list subscriptions"))?;
    Ok(Response::new(SubscriptionList {
        subscriptions: rows
            .into_iter()
            .map(|r| subscription_to_pb(r, &config))
            .collect(),
    }))
}

pub(super) async fn get(
    req: Request<SubscriptionRef>,
) -> Result<Response<SubscriptionDetail>, TStatus> {
    let id = req.into_inner().id;
    let (db, config) = open().await?;
    let row = db
        .get_subscription(&id)
        .await
        .map_err(map_internal("get subscription"))?
        .ok_or_else(not_found)?;
    let events = db
        .list_subscription_events(&id, DETAIL_EVENTS)
        .await
        .map_err(map_internal("list subscription events"))?;
    let day_ago = chrono::Utc::now().timestamp() - 24 * 3600;
    let runs_today = db
        .subscription_runs_of_since(&id, day_ago)
        .await
        .map_err(map_internal("count runs"))?;
    let sender_steps = steps_with(&row, &config, KEY_PLACEHOLDER);
    Ok(Response::new(SubscriptionDetail {
        subscription: Some(subscription_to_pb(row, &config)),
        events: events.into_iter().map(event_to_pb).collect(),
        sender_steps,
        runs_today: u32::try_from(runs_today).unwrap_or(u32::MAX),
    }))
}

fn non_zero(v: u32) -> Option<u32> {
    (v > 0).then_some(v)
}

fn non_empty(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}

pub(super) async fn create(
    req: Request<SubscriptionSpec>,
) -> Result<Response<SubscriptionCreated>, TStatus> {
    let req = req.into_inner();
    let (db, config) = open().await?;
    let spec = Spec {
        provider: req.provider,
        name: req.name,
        instruction: req.instruction,
        filter_json: req.filter_json,
        delivery_channel: non_empty(req.delivery_channel),
        delivery_target: non_empty(req.delivery_target),
        signing_secret: non_empty(req.signing_secret),
        rate_limit_per_hour: non_zero(req.rate_limit_per_hour),
        timeout_secs: non_zero(req.timeout_secs),
        active: req.active,
    };
    let created = manage::create(&db, &config, &spec).await.map_err(refused)?;
    let key = created.key.unwrap_or_default();
    let sender_steps = steps_with(&created.row, &config, &key);
    Ok(Response::new(SubscriptionCreated {
        subscription: Some(subscription_to_pb(created.row, &config)),
        key,
        sender_steps,
    }))
}

pub(super) async fn update(
    req: Request<SubscriptionChange>,
) -> Result<Response<PbSubscription>, TStatus> {
    let req = req.into_inner();
    let (db, config) = open().await?;
    let signing_secret_ref = if req.set_signing_secret {
        manage::signing_secret_change(Some(&req.signing_secret)).map_err(refused)?
    } else {
        None
    };
    let change = SubscriptionUpdate {
        name: non_empty(req.name),
        filter: non_empty(req.filter_json),
        instruction: non_empty(req.instruction),
        delivery_channel: req.set_delivery.then(|| non_empty(req.delivery_channel)),
        delivery_target: req.set_delivery.then(|| non_empty(req.delivery_target)),
        rate_limit_per_hour: non_zero(req.rate_limit_per_hour).map(i64::from),
        timeout_secs: non_zero(req.timeout_secs).map(i64::from),
        signing_secret_ref,
    };
    let row = manage::update(&db, &req.id, &change)
        .await
        .map_err(refused)?
        .ok_or_else(not_found)?;
    Ok(Response::new(subscription_to_pb(row, &config)))
}

pub(super) async fn delete(req: Request<SubscriptionRef>) -> Result<Response<Empty>, TStatus> {
    let id = req.into_inner().id;
    let db = Database::open().await.map_err(map_internal("open db"))?;
    if !manage::delete(&db, &id)
        .await
        .map_err(map_internal("delete subscription"))?
    {
        return Err(not_found());
    }
    Ok(Response::new(Empty {}))
}

pub(super) async fn set_status(
    req: Request<SubscriptionStatusRequest>,
) -> Result<Response<PbSubscription>, TStatus> {
    let req = req.into_inner();
    let (db, config) = open().await?;
    let row = manage::set_active(&db, &req.id, req.active)
        .await
        .map_err(map_internal("set subscription status"))?
        .ok_or_else(not_found)?;
    Ok(Response::new(subscription_to_pb(row, &config)))
}

pub(super) async fn reveal_key(
    req: Request<SubscriptionRef>,
) -> Result<Response<SubscriptionKey>, TStatus> {
    let id = req.into_inner().id;
    let (db, config) = open().await?;
    let row = db
        .get_subscription(&id)
        .await
        .map_err(map_internal("get subscription"))?
        .ok_or_else(not_found)?;
    let key = manage::reveal_key(&db, &id)
        .await
        .map_err(map_internal("read subscription key"))?
        .ok_or_else(|| TStatus::failed_precondition("This subscription has no key."))?;
    cortex::activity_log::log_activity(
        &db,
        "info",
        "subscriptions",
        &format!("The key of \"{}\" was shown", row.name),
    )
    .await;
    let sender_steps = steps_with(&row, &config, &key);
    Ok(Response::new(SubscriptionKey { key, sender_steps }))
}

pub(super) async fn rotate_key(
    req: Request<SubscriptionRef>,
) -> Result<Response<SubscriptionKey>, TStatus> {
    let id = req.into_inner().id;
    let (db, config) = open().await?;
    let key = manage::rotate_key(&db, &id)
        .await
        .map_err(refused)?
        .ok_or_else(not_found)?;
    let row = db
        .get_subscription(&id)
        .await
        .map_err(map_internal("get subscription"))?
        .ok_or_else(not_found)?;
    cortex::activity_log::log_activity(
        &db,
        "info",
        "subscriptions",
        &format!("The key of \"{}\" was replaced", row.name),
    )
    .await;
    let sender_steps = steps_with(&row, &config, &key);
    Ok(Response::new(SubscriptionKey { key, sender_steps }))
}

pub(super) async fn test(
    req: Request<SubscriptionRef>,
) -> Result<Response<PbSubscriptionEvent>, TStatus> {
    let id = req.into_inner().id;
    let db = Database::open().await.map_err(map_internal("open db"))?;
    let event = manage::test(&db, &id)
        .await
        .map_err(refused)?
        .ok_or_else(not_found)?;
    Ok(Response::new(event_to_pb(event)))
}

pub(super) async fn list_events(
    req: Request<SubscriptionEventsRequest>,
) -> Result<Response<SubscriptionEventList>, TStatus> {
    let req = req.into_inner();
    let limit = if req.limit == 0 {
        DETAIL_EVENTS
    } else {
        req.limit.min(500)
    };
    let db = Database::open().await.map_err(map_internal("open db"))?;
    let events = db
        .list_subscription_events(&req.id, limit)
        .await
        .map_err(map_internal("list subscription events"))?;
    Ok(Response::new(SubscriptionEventList {
        events: events.into_iter().map(event_to_pb).collect(),
    }))
}

pub(super) fn activity_stream(
    bus: crate::subscriptions::SubscriptionBus,
) -> Response<SubscriptionActivityStream> {
    let mut sub = bus.subscribe();
    let (tx, rx) = mpsc::channel::<Result<PbSubscriptionActivity, TStatus>>(32);
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(a) => {
                    let pb = PbSubscriptionActivity {
                        subscription_id: a.subscription_id,
                        kind: a.kind,
                        run_id: a.run_id.unwrap_or_default(),
                        status: a.status.unwrap_or_default(),
                        summary: a.summary,
                        event_count: a.event_count,
                        emitted_at_unix: a.emitted_at_unix,
                    };
                    if tx.send(Ok(pb)).await.is_err() {
                        return;
                    }
                }
                // The app refetches on any event, so a lag only costs a
                // few intermediate frames.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("subscription activity subscriber lagged by {n}; continuing");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Response::new(Box::pin(ReceiverStream::new(rx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_id(id: &str) -> Request<SubscriptionRef> {
        Request::new(SubscriptionRef { id: id.to_string() })
    }

    #[tokio::test]
    async fn a_subscription_round_trips_through_the_admin_api() {
        let _home = crate::test_env::IsolatedHome::new();
        let created = create(Request::new(SubscriptionSpec {
            provider: "github".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
        let sub = created.subscription.unwrap();
        assert_eq!(sub.status, "paused", "the app starts it paused");
        assert!(created.key.starts_with("bsk_"));
        assert!(
            created
                .sender_steps
                .iter()
                .any(|s| s.contains(&created.key)),
            "the steps right after create carry the key: {:?}",
            created.sender_steps
        );

        let detail = get(by_id(&sub.id)).await.unwrap().into_inner();
        assert!(
            detail
                .sender_steps
                .iter()
                .all(|s| !s.contains(&created.key)),
            "later views mask the key"
        );
        assert!(detail
            .sender_steps
            .iter()
            .any(|s| s.contains(KEY_PLACEHOLDER)));

        let shown = reveal_key(by_id(&sub.id)).await.unwrap().into_inner();
        assert_eq!(shown.key, created.key);
        let rotated = rotate_key(by_id(&sub.id)).await.unwrap().into_inner();
        assert_ne!(rotated.key, created.key);

        let on = set_status(Request::new(SubscriptionStatusRequest {
            id: sub.id.clone(),
            active: true,
        }))
        .await
        .unwrap()
        .into_inner();
        assert_eq!(on.status, "active");

        let changed = update(Request::new(SubscriptionChange {
            id: sub.id.clone(),
            instruction: "Summarise the PR.".into(),
            set_delivery: true,
            delivery_channel: "telegram".into(),
            delivery_target: "42".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
        assert_eq!(changed.instruction, "Summarise the PR.");
        assert_eq!(changed.name, sub.name, "an empty name leaves it unchanged");
        assert_eq!(changed.delivery_channel, "telegram");

        let refused = update(Request::new(SubscriptionChange {
            id: sub.id.clone(),
            set_delivery: true,
            delivery_channel: "origin".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(
            refused.message().contains("channel messages"),
            "{}",
            refused.message()
        );

        let queued = test(by_id(&sub.id)).await.unwrap().into_inner();
        assert_eq!(queued.kind, "test");
        let events = list_events(Request::new(SubscriptionEventsRequest {
            id: sub.id.clone(),
            limit: 0,
        }))
        .await
        .unwrap()
        .into_inner();
        assert_eq!(events.events.len(), 1);

        delete(by_id(&sub.id)).await.unwrap();
        assert_eq!(
            get(by_id(&sub.id)).await.unwrap_err().code(),
            tonic::Code::NotFound
        );
    }

    #[tokio::test]
    async fn an_unknown_source_is_refused_in_plain_words() {
        let _home = crate::test_env::IsolatedHome::new();
        let err = create(Request::new(SubscriptionSpec {
            provider: "myspace".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(err.message(), "There is no \"myspace\" source.");
    }

    #[test]
    fn display_payloads_are_cut_on_a_character_boundary() {
        let s = "é".repeat(10); // 20 bytes
        let (head, cut_here) = cut(&s, 5);
        assert_eq!(head, "éé", "never splits a character");
        assert!(cut_here);
        assert_eq!(cut("short", 5), ("short", false));
    }

    #[test]
    fn only_endpoint_sources_show_an_endpoint() {
        let config = Config::default();
        let mut row = SubscriptionRow {
            id: "sub_x".into(),
            name: "n".into(),
            source: "webhook".into(),
            provider: "github".into(),
            auth_scheme: "github".into(),
            secret_ref: None,
            signing_secret_ref: Some("{}".into()),
            filter: "{}".into(),
            instruction: "i".into(),
            delivery_channel: None,
            delivery_target: None,
            status: "paused".into(),
            rate_limit_per_hour: 60,
            timeout_secs: 300,
            dropped_count: -1,
            created_at: 1,
            updated_at: 2,
            last_event_at: None,
            last_run_status: None,
            last_error: None,
        };
        let pb = subscription_to_pb(row.clone(), &config);
        assert!(pb.endpoint.unwrap().url.ends_with("/v1/events/sub_x"));
        assert!(pb.has_signing_secret);
        assert_eq!(pb.session_key, "subscription:sub_x");
        assert_eq!(pb.dropped_count, 0, "a bad count never shows as huge");
        row.source = "channel".into();
        assert!(subscription_to_pb(row, &config).endpoint.is_none());
    }
}
