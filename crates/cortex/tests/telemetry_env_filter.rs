//! Regression test: the `EnvFilter` handed to `install_subscriber` must
//! actually gate events for the installed stack.
//!
//! The original composition pushed the filter into the `Vec<BoxedLayer>` as a
//! sibling layer. `Vec<Layer>::register_callsite` keeps the *most* interested
//! child, so the unfiltered fmt layer's `Interest::always` won and tracing's
//! callsite fast path skipped `enabled()` entirely — TRACE/DEBUG events from
//! h2/tonic reached stdout on every daemon-connected CLI command regardless of
//! `RUST_LOG`.
//!
//! This must be a single-test integration binary: the fast path only engages
//! for the process-global dispatcher (scoped `with_default` dispatchers are
//! demoted to `Interest::sometimes`, which would mask the bug).

use std::sync::{Arc, Mutex};

use tracing::{Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

#[derive(Clone, Default)]
struct RecordingLayer {
    seen: Arc<Mutex<Vec<Level>>>,
}

impl<S: Subscriber> Layer<S> for RecordingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        self.seen
            .lock()
            .expect("recording mutex")
            .push(*event.metadata().level());
    }
}

#[test]
fn env_filter_gates_the_installed_global_subscriber() {
    let recorder = RecordingLayer::default();
    let seen = Arc::clone(&recorder.seen);

    let subscriber = cortex::telemetry::build_subscriber(
        Vec::new(),
        tracing_subscriber::EnvFilter::new("error"),
        Box::new(recorder),
    );
    tracing::subscriber::set_global_default(subscriber).expect("install global subscriber");

    tracing::trace!("h2-style connection chatter");
    tracing::debug!("binding client connection");
    tracing::info!("informational");
    tracing::error!("real failure");

    let seen = seen.lock().expect("recording mutex");
    assert_eq!(
        seen.as_slice(),
        &[Level::ERROR],
        "an `error` filter must suppress trace/debug/info and still deliver errors, got {seen:?}"
    );
}
