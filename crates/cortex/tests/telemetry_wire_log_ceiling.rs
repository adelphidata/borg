//! The wire logs must stay off however `RUST_LOG` is set.
//!
//! `chromiumoxide::conn` logs each CDP command and reply verbatim at TRACE —
//! the text typed into a page and whatever a page evaluation returns — and
//! `tungstenite` logs the payload of every WebSocket frame sent, which is the
//! same commands one layer down, and the handshake request with its headers.
//! An operator who turns on `RUST_LOG=trace` (or names a target outright) to
//! debug something else must not get passwords in `daemon.log`.
//!
//! `chromiumoxide` logs with `tracing` macros; `tungstenite` logs through the
//! `log` crate, whose records reach `tracing` over a bridge that asks the
//! filter about each one by its real module path. Both roads are driven here.
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
        // A bridged `log` record carries its real target as a field.
        use tracing_log::NormalizeEvent as _;
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        self.seen
            .lock()
            .expect("recording mutex")
            .push((meta.target().to_string(), *meta.level()));
    }
}

fn install(rust_log: &str) -> Arc<Mutex<Vec<(String, Level)>>> {
    let recorder = RecordingLayer::default();
    let seen = Arc::clone(&recorder.seen);
    let filter = cortex::telemetry::log_filter(Some(rust_log), "info");
    let subscriber = cortex::telemetry::build_subscriber(Vec::new(), filter, Box::new(recorder));
    tracing::subscriber::set_global_default(subscriber).expect("install global subscriber");
    // What `install_subscriber` does for the binaries: `log` records go to
    // `tracing`. The `log` crate's own level gate is left wide open here so
    // the ceiling is the only thing in the way.
    tracing_log::LogTracer::init().expect("install the log bridge");
    log::set_max_level(log::LevelFilter::Trace);
    seen
}

#[test]
fn rust_log_cannot_turn_on_the_wire_logs() {
    // The loosest thing an operator can ask for: everything at trace, and
    // each wire-log target by name, children included.
    let seen = install(
        "trace,chromiumoxide::conn=trace,chromiumoxide::conn::raw_ws::parse_errors=trace,\
         tungstenite=trace,tungstenite::protocol=trace,tungstenite::handshake::client=trace",
    );

    tracing::trace!(target: "chromiumoxide::conn", "Sending InsertText {{ text: \"hunter2\" }}");
    tracing::debug!(target: "chromiumoxide::conn", "Received evaluate result");
    tracing::debug!(target: "chromiumoxide::conn::raw_ws::parse_errors", "Failed to parse raw WS message");
    tracing::error!(target: "chromiumoxide::conn", "websocket closed");
    tracing::trace!(target: "chromiumoxide::handler", "target attached");
    log::trace!(target: "tungstenite::protocol", "Sending frame: Frame {{ payload: .. }}");
    log::trace!(target: "tungstenite::protocol::frame", "Parsed headers [129, 5]");
    log::debug!(target: "tungstenite::protocol", "Received close frame");
    log::warn!(target: "tungstenite::protocol", "connection reset");
    log::trace!(target: "tungstenite::handshake::client", "Request: Authorization: Bot abc");
    log::info!(target: "tungstenite::handshake::client", "handshake done");
    log::trace!(target: "tokio_tungstenite", "WebSocketStream.poll_next");
    tracing::trace!(target: "cortex::agent", "turn started");

    let seen = seen.lock().expect("recording mutex");
    assert_eq!(
        seen.as_slice(),
        &[
            ("chromiumoxide::conn".to_string(), Level::ERROR),
            ("chromiumoxide::handler".to_string(), Level::TRACE),
            ("tungstenite::protocol".to_string(), Level::WARN),
            ("tungstenite::handshake::client".to_string(), Level::INFO),
            ("tokio_tungstenite".to_string(), Level::TRACE),
            ("cortex::agent".to_string(), Level::TRACE),
        ],
        "only the payload-bearing modules are capped; the rest of RUST_LOG is honoured"
    );
}
