//! W3C Trace Context (`traceparent`) propagation across HTTP boundaries.
//!
//! These helpers bridge the active `tracing` span to the globally installed
//! OpenTelemetry text-map propagator (set by
//! [`super::init_telemetry`]). When telemetry is disabled the global
//! propagator is the no-op default, so both helpers become inert — injection
//! adds no headers and extraction yields an empty context.

use opentelemetry::Context;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Inject the current span's trace context as `traceparent` (+ `tracestate`)
/// into outbound request `headers`.
///
/// No-op when there is no recording span or no global propagator is installed:
/// the propagator only writes a `traceparent` for a *valid* span context, so a
/// disabled pipeline or an out-of-span call leaves `headers` untouched.
pub fn inject_current_context(headers: &mut http::HeaderMap) {
    let cx = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(headers));
    });
}

/// Extract an upstream trace context from inbound request `headers`.
///
/// Returns a `Context` carrying the remote span context when a well-formed
/// `traceparent` is present; otherwise an empty context (callers can still
/// `set_parent` it harmlessly). Malformed headers never panic — they simply
/// produce an invalid/absent remote span context.
pub fn extract_parent_context(headers: &http::HeaderMap) -> Context {
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(headers))
    })
}

/// Set `span`'s parent to the upstream trace context carried in `headers`.
///
/// Lets callers (e.g. the webhook gateway) chain an inbound request to its
/// upstream trace without taking a direct `tracing-opentelemetry` dependency.
/// Inert when no valid `traceparent` is present.
pub fn set_span_parent_from_headers(span: &tracing::Span, headers: &http::HeaderMap) {
    // Best-effort: `set_parent` errors only when the span is disabled (no OTel
    // layer installed), which is the normal telemetry-off case. Never let a
    // missing parent affect request handling.
    let _ = span.set_parent(extract_parent_context(headers));
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{TraceContextExt, Tracer, TracerProvider};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;

    /// Build a real (in-memory) tracer + span so `Span::current().context()`
    /// carries a valid span context, and install the W3C propagator globally.
    fn with_w3c_propagator() {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    }

    #[test]
    fn inject_then_extract_round_trips_trace_id() {
        with_w3c_propagator();
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");

        // Start an OTel span and make it the active tracing-span context.
        let span = tracer.start("outbound");
        let cx = Context::current_with_span(span);
        let trace_id = cx.span().span_context().trace_id();
        let _guard = cx.clone().attach();

        // Inject using the OTel context directly (mirrors what
        // inject_current_context does via the tracing bridge).
        let mut headers = http::HeaderMap::new();
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut HeaderInjector(&mut headers));
        });

        let tp = headers
            .get("traceparent")
            .expect("traceparent injected for a valid span")
            .to_str()
            .unwrap();
        // Format: 00-<32 hex trace id>-<16 hex span id>-<2 hex flags>
        let parts: Vec<&str> = tp.split('-').collect();
        assert_eq!(parts.len(), 4, "traceparent has 4 fields: {tp}");
        assert_eq!(parts[0], "00", "version");
        assert_eq!(parts[1].len(), 32, "trace id is 32 hex");
        assert_eq!(parts[2].len(), 16, "span id is 16 hex");

        // Extracting from those headers recovers the same trace id.
        let extracted = extract_parent_context(&headers);
        assert_eq!(
            extracted.span().span_context().trace_id(),
            trace_id,
            "extracted trace id must equal injected trace id"
        );
        assert!(
            extracted.span().span_context().is_remote(),
            "extracted context must be marked remote"
        );
    }

    #[test]
    fn inject_current_context_propagates_active_tracing_span() {
        use tracing_subscriber::prelude::*;
        with_w3c_propagator();
        let provider = SdkTracerProvider::builder().build();
        let otel_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
        let subscriber = tracing_subscriber::registry().with(otel_layer);

        // Drive inject_current_context through the real tracing→OTel bridge:
        // an entered span must surface a traceparent on outbound headers.
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("outbound");
            let _enter = span.enter();
            let mut headers = http::HeaderMap::new();
            inject_current_context(&mut headers);
            assert!(
                headers.get("traceparent").is_some(),
                "an active recording span must inject a traceparent"
            );
        });
    }

    #[test]
    fn inject_without_active_span_adds_no_header() {
        with_w3c_propagator();
        // No span attached: the current context has an invalid span context,
        // so the W3C propagator must not write a traceparent.
        let mut headers = http::HeaderMap::new();
        inject_current_context(&mut headers);
        assert!(
            headers.get("traceparent").is_none(),
            "no traceparent without a recording span"
        );
    }

    #[test]
    fn extract_malformed_header_yields_invalid_remote_context() {
        with_w3c_propagator();
        let mut headers = http::HeaderMap::new();
        headers.insert("traceparent", "not-a-valid-traceparent".parse().unwrap());

        // Must not panic, and must not produce a usable remote span context.
        let cx = extract_parent_context(&headers);
        assert!(
            !cx.span().span_context().is_valid(),
            "malformed traceparent must not yield a valid span context"
        );
    }
}
