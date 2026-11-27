//! The browser's wire logs must stay off however `RUST_LOG` is set.
//!
//! `chromiumoxide::conn` logs each CDP command and reply verbatim at TRACE —
//! the text typed into a page and whatever a page evaluation returns — and
//! `tungstenite::protocol` logs the payload of every WebSocket frame sent,
//! which is the same commands one layer down. An operator who turns on
//! `RUST_LOG=trace` (or names a target outright) to debug something else must
//! not get passwords in `daemon.log`.
//!
//! Single-test integration binary for the same reason as
//! `telemetry_env_filter.rs`: the callsite fast path only engages for the
//! process-global dispatcher.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use tracing::{Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

#[derive(Clone, Default)]
struct RecordingLayer {
    seen: Arc<Mutex<Vec<(String, Level)>>>,
}

impl<S: Subscriber> Layer<S> for RecordingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        self.seen
            .lock()
            .expect("recording mutex")
            .push((meta.target().to_string(), *meta.level()));
    }
}

#[test]
fn rust_log_cannot_turn_on_the_browser_wire_logs() {
    let recorder = RecordingLayer::default();
    let seen = Arc::clone(&recorder.seen);

    // The loosest thing an operator can ask for: everything at trace, and
    // each wire-log target by name.
    let filter = cortex::telemetry::log_filter(
        Some("trace,chromiumoxide::conn=trace,tungstenite::protocol=trace"),
        "info",
    );
    let subscriber = cortex::telemetry::build_subscriber(Vec::new(), filter, Box::new(recorder));
    tracing::subscriber::set_global_default(subscriber).expect("install global subscriber");

    tracing::trace!(target: "chromiumoxide::conn", "Sending InsertText {{ text: \"hunter2\" }}");
    tracing::debug!(target: "chromiumoxide::conn", "Received evaluate result");
    tracing::error!(target: "chromiumoxide::conn", "websocket closed");
    tracing::trace!(target: "chromiumoxide::handler", "target attached");
    tracing::trace!(target: "tungstenite::protocol", "Sending frame: Frame {{ payload: .. }}");
    tracing::debug!(target: "tungstenite::protocol", "Received close frame");
    tracing::warn!(target: "tungstenite::protocol", "connection reset");
    tracing::trace!(target: "tungstenite::handshake", "client handshake done");
    tracing::trace!(target: "cortex::agent", "turn started");

    let seen = seen.lock().expect("recording mutex");
    assert_eq!(
        seen.as_slice(),
        &[
            ("chromiumoxide::conn".to_string(), Level::ERROR),
            ("chromiumoxide::handler".to_string(), Level::TRACE),
            ("tungstenite::protocol".to_string(), Level::WARN),
            ("tungstenite::handshake".to_string(), Level::TRACE),
            ("cortex::agent".to_string(), Level::TRACE),
        ],
        "only the payload-bearing targets are pinned; the rest of RUST_LOG is honoured"
    );
}
