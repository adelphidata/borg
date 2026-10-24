//! End-to-end: an outside service posts to a subscription's endpoint on the
//! real gateway, and the real dispatcher turns the stored event into a turn
//! in the subscription's own session.
//!
//! Real failure modes guarded:
//! 1. The `/v1/events/{id}` router is not mounted, or its state is wired
//!    to the wrong database, so the row the dispatcher reads never appears.
//! 2. The gateway's wake signal and the dispatcher's are different
//!    instances, so an event waits for the slow tick instead of running.
//! 3. The run lands in the wrong session, or its reply is not recorded on
//!    the event, so the app shows a run that never says what it did.
//! 4. Auth through the assembled stack: a paused subscription keeps
//!    nothing, a redelivery is a duplicate, a wrong key or a forged GitHub
//!    signature is refused, and GitHub's `ping` passes without a run.
//! 5. The gateway keeps the settings it started with, so turning
//!    subscriptions off in the app leaves the endpoint storing events, and
//!    turning them on leaves it refusing them, until a restart.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use borgd::subscriptions::{SubscriptionActivity, SubscriptionBus};
use cortex::agent::AgentEvent;
use cortex::config::Config;
use cortex::db::Database;
use cortex::subscriptions::manage::{self, Spec};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[path = "ingress_e2e_common.rs"]
mod common;

/// A subscription created and turned on the way the app does it.
async fn subscription(db: &Database, provider: &str) -> (String, String) {
    let created = manage::create(
        db,
        &Config::default(),
        &Spec {
            provider: provider.into(),
            instruction: "Look into it and say what you found.".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create subscription");
    (
        created.row.id,
        created.key.expect("an endpoint source has a key"),
    )
}

fn start_dispatcher(bus: &SubscriptionBus) -> CancellationToken {
    let cancel = CancellationToken::new();
    let (_tx, rx) = tokio::sync::watch::channel(Config::default());
    let run = borgd::subscriptions::run_dispatcher(
        common::shared_executor(),
        rx,
        bus.clone(),
        cancel.clone(),
        Arc::new(AtomicU64::new(0)),
    );
    tokio::spawn(async move {
        // Keep the sender alive for the dispatcher's lifetime.
        let _tx = _tx;
        if let Err(e) = run.await {
            panic!("dispatcher stopped: {e:#}");
        }
    });
    cancel
}

async fn post(url: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.expect("send");
    let code = resp.status().as_u16();
    (code, resp.json().await.unwrap_or(Value::Null))
}

/// Wait for the event to finish; the dispatcher is woken by the gateway, so
/// this takes well under the dispatcher's 15 s fallback tick.
async fn finished(db: &Database, event_id: &str) -> cortex::db::SubscriptionEventRow {
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let row = db
            .get_subscription_event(event_id)
            .await
            .unwrap()
            .expect("the 202 named a stored event");
        if !matches!(row.status.as_str(), "pending" | "running") {
            return row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "event {event_id} still {} — the dispatcher was not woken",
            row.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn finished_activity(
    rx: &mut broadcast::Receiver<SubscriptionActivity>,
    id: &str,
) -> SubscriptionActivity {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let a = rx.recv().await.expect("bus open");
            if a.subscription_id == id && a.kind == "finished" {
                return a;
            }
        }
    })
    .await
    .expect("the app hears that the run finished")
}

fn github_signature(key: &str, body: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

#[tokio::test]
async fn a_webhook_event_wakes_borg_in_the_subscription_session() {
    let h = common::boot_gateway(true).await;
    let db = Database::open().await.unwrap();
    let (id, key) = subscription(&db, "custom").await;
    let url = h.url(&format!("/v1/events/{id}"));
    let bearer = format!("Bearer {key}");
    let auth = [("authorization", bearer.as_str())];
    let body = r#"{"id":"alert-1","title":"CPU high on web-3"}"#;

    let (code, reply) = post(&url, &auth, body).await;
    assert_eq!(code, 200, "{reply}");
    assert_eq!(
        reply["accepted"],
        json!(false),
        "a new subscription starts paused: {reply}"
    );

    manage::set_active(&db, &id, true).await.unwrap();
    common::register_stub_for_session(
        &cortex::subscriptions::session_key(&id),
        vec![
            AgentEvent::TextDelta("web-3 CPU is back to normal.".into()),
            AgentEvent::TurnComplete,
        ],
    );
    let bus = SubscriptionBus::new();
    let mut activity = bus.subscribe();
    let stop = start_dispatcher(&bus);

    let (code, reply) = post(&url, &auth, body).await;
    assert_eq!(code, 202, "{reply}");
    let event_id = reply["event_id"].as_str().expect("event id").to_string();
    let row = finished(&db, &event_id).await;
    assert_eq!(row.status, "succeeded", "{row:?}");
    assert!(
        row.result
            .as_deref()
            .unwrap_or("")
            .contains("back to normal"),
        "the reply is recorded on the event: {row:?}"
    );
    let done = finished_activity(&mut activity, &id).await;
    assert_eq!(done.status.as_deref(), Some("succeeded"));

    let (code, reply) = post(&url, &auth, body).await;
    assert_eq!((code, &reply["duplicate"]), (200, &json!(true)), "{reply}");
    let (code, _) = post(&url, &[("authorization", "Bearer wrong")], body).await;
    assert_eq!(code, 401);
    let runs = db.list_subscription_events(&id, 10).await.unwrap();
    assert_eq!(runs.len(), 1, "one event, one run: {runs:?}");

    stop.cancel();
    h.shutdown().await;
}

/// Wait until the dispatcher has published `enabled` to the gateway.
async fn live_enabled_is(enabled: bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while cortex::subscriptions::wake::live_settings().map(|s| s.enabled) != Some(enabled) {
        assert!(
            std::time::Instant::now() < deadline,
            "the dispatcher did not publish enabled={enabled} well before its 15 s tick"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_subscriptions_switch_reaches_a_gateway_that_is_already_running() {
    // The gateway is built with subscriptions on.
    let h = common::boot_gateway(true).await;
    let db = Database::open().await.unwrap();
    let (id, key) = subscription(&db, "custom").await;
    manage::set_active(&db, &id, true).await.unwrap();
    common::register_stub_for_session(
        &cortex::subscriptions::session_key(&id),
        vec![
            AgentEvent::TextDelta("Checked.".into()),
            AgentEvent::TurnComplete,
        ],
    );
    let url = h.url(&format!("/v1/events/{id}"));
    let bearer = format!("Bearer {key}");
    let auth = [("authorization", bearer.as_str())];

    let mut off = Config::default();
    off.subscriptions.enabled = false;
    let (tx, rx) = tokio::sync::watch::channel(off);
    let cancel = CancellationToken::new();
    let run = borgd::subscriptions::run_dispatcher(
        common::shared_executor(),
        rx,
        SubscriptionBus::new(),
        cancel.clone(),
        Arc::new(AtomicU64::new(0)),
    );
    let dispatcher = tokio::spawn(run);

    live_enabled_is(false).await;
    let (code, reply) = post(&url, &auth, r#"{"id":"while-off"}"#).await;
    assert_eq!(code, 503, "turned off in the app: {reply}");

    tx.send_replace(Config::default());
    live_enabled_is(true).await;
    let (code, reply) = post(&url, &auth, r#"{"id":"while-on"}"#).await;
    assert_eq!(code, 202, "turned back on: {reply}");
    let event_id = reply["event_id"].as_str().expect("event id").to_string();
    assert_eq!(finished(&db, &event_id).await.status, "succeeded");
    let events = db.list_subscription_events(&id, 10).await.unwrap();
    assert_eq!(events.len(), 1, "nothing was kept while off: {events:?}");

    cancel.cancel();
    dispatcher.await.unwrap().unwrap();
    h.shutdown().await;
}

#[tokio::test]
async fn a_signed_github_delivery_runs_and_its_ping_does_not() {
    let h = common::boot_gateway(true).await;
    let db = Database::open().await.unwrap();
    let (id, key) = subscription(&db, "github").await;
    let url = h.url(&format!("/v1/events/{id}"));

    // GitHub sends `ping` as soon as the webhook is added, before the user
    // has turned the subscription on; its setup check must still pass.
    let ping = r#"{"zen":"Keep it logically awesome.","hook_id":1}"#;
    let sig = github_signature(&key, ping);
    let (code, reply) = post(
        &url,
        &[("x-github-event", "ping"), ("x-hub-signature-256", &sig)],
        ping,
    )
    .await;
    assert_eq!((code, &reply["handshake"]), (200, &json!(true)), "{reply}");

    manage::set_active(&db, &id, true).await.unwrap();
    common::register_stub_for_session(
        &cortex::subscriptions::session_key(&id),
        vec![
            AgentEvent::TextDelta("Reviewed PR #42.".into()),
            AgentEvent::TurnComplete,
        ],
    );
    let bus = SubscriptionBus::new();
    let stop = start_dispatcher(&bus);

    let body = json!({
        "action": "opened",
        "pull_request": {"number": 42, "title": "Fix login", "draft": false},
        "repository": {"full_name": "octo/app"},
        "sender": {"login": "octocat"}
    })
    .to_string();
    let sig = github_signature(&key, &body);
    let pr = [
        ("x-github-event", "pull_request"),
        ("x-github-delivery", "d-1"),
        ("x-hub-signature-256", sig.as_str()),
    ];
    let (code, reply) = post(&url, &pr, &body).await;
    assert_eq!(code, 202, "{reply}");
    let row = finished(&db, reply["event_id"].as_str().unwrap()).await;
    assert_eq!(row.status, "succeeded", "{row:?}");
    assert_eq!(row.kind.as_deref(), Some("pull_request.opened"));
    assert!(row.summary.contains("Fix login"), "{}", row.summary);

    // A forged signature is refused even with the right bearer key.
    let bearer = format!("Bearer {key}");
    let forged = [
        ("x-github-event", "pull_request"),
        ("x-hub-signature-256", "sha256=00"),
        ("authorization", bearer.as_str()),
    ];
    let (code, _) = post(&url, &forged, &body.replace("42", "43")).await;
    assert_eq!(code, 401);
    let events = db.list_subscription_events(&id, 10).await.unwrap();
    assert_eq!(
        events.len(),
        1,
        "the ping and the forgery left no rows: {events:?}"
    );

    stop.cancel();
    h.shutdown().await;
}
