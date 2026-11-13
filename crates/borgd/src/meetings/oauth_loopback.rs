//! The one-shot listener that catches Google's sign-in redirect.
//!
//! Google's installed-app flow sends the browser back to
//! `http://127.0.0.1:<port>/callback?code=…&state=…`. This binds an
//! ephemeral loopback port, answers that single request with a page the
//! person can close, and hands the code over. A request whose `state` does
//! not match is answered with an error and ignored, so another local
//! process cannot spend the listener with a forged callback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// How long the person has to finish signing in.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);

/// What the redirect carried: the code, or Google's error (`access_denied`
/// when the person clicked Cancel).
type Outcome = std::result::Result<String, String>;

struct Shared {
    expected_state: Mutex<Option<String>>,
    tx: Mutex<Option<oneshot::Sender<Outcome>>>,
}

/// A bound listener waiting for one redirect.
pub struct Loopback {
    /// The `redirect_uri` to put in the consent URL.
    pub redirect_uri: String,
    shared: Arc<Shared>,
    rx: oneshot::Receiver<Outcome>,
    stop: CancellationToken,
}

/// Bind `127.0.0.1` on a free port and start answering `/callback`.
pub async fn bind() -> Result<Loopback> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let (tx, rx) = oneshot::channel();
    let shared = Arc::new(Shared {
        expected_state: Mutex::new(None),
        tx: Mutex::new(Some(tx)),
    });
    let stop = CancellationToken::new();
    let app = Router::new()
        .route("/callback", get(callback))
        .with_state(shared.clone());
    let shutdown = stop.clone();
    // A bounded one-off, not a subsystem: it serves one request and stops
    // on success, cancellation or the sign-in timeout.
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
        {
            tracing::warn!("Google sign-in listener failed: {e}");
        }
    });
    Ok(Loopback {
        redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        shared,
        rx,
        stop,
    })
}

impl Loopback {
    /// The `state` a genuine redirect carries. Requests before this is set
    /// are refused.
    pub fn expect_state(&self, state: &str) {
        if let Ok(mut s) = self.shared.expected_state.lock() {
            *s = Some(state.to_string());
        }
    }

    /// A token that stops the listener when cancelled; a newer sign-in
    /// cancels the older one with it.
    pub fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }

    /// Wait for the redirect. Fails when the person declined, the time ran
    /// out, or a newer sign-in replaced this one. The listener stops either
    /// way.
    pub async fn wait(self, timeout: Duration) -> std::result::Result<String, SignInEnd> {
        let stop = self.stop.clone();
        let failed = |why: &str| Err(SignInEnd::Failed(why.to_string()));
        let result = tokio::select! {
            () = stop.cancelled() => Err(SignInEnd::Replaced),
            r = tokio::time::timeout(timeout, self.rx) => match r {
                Err(_) => failed("The sign-in timed out. Connect the calendar again."),
                Ok(Err(_)) => {
                    failed("The sign-in stopped before Google answered. Connect the calendar again.")
                }
                Ok(Ok(Err(e))) if e == "access_denied" => {
                    failed("Access was not granted, so the calendar is not connected.")
                }
                Ok(Ok(Err(e))) => Err(SignInEnd::Failed(format!(
                    "Google refused the sign-in ({e}). Connect the calendar again."
                ))),
                Ok(Ok(Ok(code))) => Ok(code),
            },
        };
        stop.cancel();
        result
    }
}

/// Why a sign-in ended without a code.
#[derive(Debug, PartialEq, Eq)]
pub enum SignInEnd {
    /// A newer sign-in took over; it reports for both.
    Replaced,
    /// The person should hear about this, in these words.
    Failed(String),
}

async fn callback(
    State(shared): State<Arc<Shared>>,
    Query(q): Query<HashMap<String, String>>,
) -> (axum::http::StatusCode, Html<&'static str>) {
    use axum::http::StatusCode;
    let expected = shared.expected_state.lock().ok().and_then(|s| s.clone());
    let state_ok = match (expected.as_deref(), q.get("state")) {
        (Some(want), Some(got)) => cortex::meetings::calendar::google::state_matches(want, got),
        _ => false,
    };
    if !state_ok {
        return (StatusCode::BAD_REQUEST, Html(page(false)));
    }
    let outcome = match (q.get("code"), q.get("error")) {
        (Some(code), _) if !code.is_empty() => Ok(code.clone()),
        (_, Some(err)) => Err(err.clone()),
        _ => Err("no code in the redirect".to_string()),
    };
    let granted = outcome.is_ok();
    let tx = shared.tx.lock().ok().and_then(|mut t| t.take());
    match tx {
        Some(tx) => {
            let _ = tx.send(outcome);
            (StatusCode::OK, Html(page(granted)))
        }
        // Already answered (a reload of the tab): the first one counted.
        None => (StatusCode::OK, Html(page(true))),
    }
}

fn page(ok: bool) -> &'static str {
    if ok {
        "<!doctype html><meta charset=utf-8><title>Borg</title>\
         <body style=\"font:16px -apple-system,system-ui,sans-serif;margin:15vh auto;max-width:28rem;text-align:center\">\
         <h1 style=\"font-weight:600\">Signed in</h1>\
         <p>You can close this tab. Borg shows the calendar once it has connected.</p></body>"
    } else {
        "<!doctype html><meta charset=utf-8><title>Borg</title>\
         <body style=\"font:16px -apple-system,system-ui,sans-serif;margin:15vh auto;max-width:28rem;text-align:center\">\
         <h1 style=\"font-weight:600\">That didn't work</h1>\
         <p>This sign-in link is out of date or was not approved. Go back to Borg and connect the calendar again.</p></body>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(url: &str) -> (u16, String) {
        let resp = reqwest::get(url).await.unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    }

    #[tokio::test]
    async fn a_forged_state_is_refused_and_the_real_redirect_still_lands() {
        let lb = bind().await.unwrap();
        assert!(lb.redirect_uri.starts_with("http://127.0.0.1:"));
        lb.expect_state("good-state");
        let base = lb.redirect_uri.clone();

        let (status, body) = get(&format!("{base}?code=evil&state=wrong")).await;
        assert_eq!(status, 400);
        assert!(body.contains("didn't work"));

        let (status, body) = get(&format!("{base}?code=the-code&state=good-state")).await;
        assert_eq!(status, 200);
        assert!(body.contains("Signed in"));
        assert_eq!(lb.wait(Duration::from_secs(5)).await.unwrap(), "the-code");
    }

    #[tokio::test]
    async fn declining_and_timing_out_are_errors() {
        let lb = bind().await.unwrap();
        lb.expect_state("s");
        let base = lb.redirect_uri.clone();
        let (status, _) = get(&format!("{base}?error=access_denied&state=s")).await;
        assert_eq!(status, 200);
        let SignInEnd::Failed(why) = lb.wait(Duration::from_secs(5)).await.unwrap_err() else {
            panic!("declining is a failure to report, not a replacement");
        };
        assert!(why.contains("not granted"), "{why}");

        let lb = bind().await.unwrap();
        let SignInEnd::Failed(why) = lb.wait(Duration::from_millis(50)).await.unwrap_err() else {
            panic!("a timeout is a failure to report");
        };
        assert!(why.contains("timed out"), "{why}");
    }

    #[tokio::test]
    async fn a_newer_sign_in_cancels_the_older_listener() {
        let lb = bind().await.unwrap();
        let stop = lb.stop_token();
        let waiting = tokio::spawn(lb.wait(Duration::from_secs(30)));
        stop.cancel();
        // Replaced, not failed: the newer sign-in is the one the page hears about.
        assert_eq!(waiting.await.unwrap().unwrap_err(), SignInEnd::Replaced);
    }
}
