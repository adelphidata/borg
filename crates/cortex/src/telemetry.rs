use anyhow::Result;
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Registry;

use crate::config::TelemetryConfig;

pub mod propagation;

/// Single source of truth for every `borg.*` metric name. CLAUDE.md, dashboards,
/// and emission sites all reference these constants — typos fail the build, not
/// silently produce a second metric series.
pub mod names {
    pub const AGENT_TURNS: &str = "borg.agent.turns";
    pub const AGENT_ITERATIONS: &str = "borg.agent.iterations";
    pub const LLM_REQUESTS: &str = "borg.llm.requests";
    pub const LLM_DURATION: &str = "borg.llm.duration";
    pub const LLM_TOKENS: &str = "borg.llm.tokens";
    /// Counter for LLM request re-attempts, labelled by `kind`:
    /// `retry` (same provider, same request — including the ones taken after a
    /// lost connection) and `failover` (switched to a fallback slot).
    /// `borg.llm.requests` counts turns, so a retry storm is invisible there.
    ///
    /// Fed from the streaming path only; the non-streaming HTTP retry loop has
    /// no event channel and reports nothing, so the sum covers streaming
    /// traffic — which is every interactive turn.
    ///
    /// The kinds are disjoint, so summing them gives total re-attempts. Why a
    /// connection-lost retry is not its own kind: the reconnect notice is
    /// emitted *before* the backoff sleep and the loop then starts the next
    /// attempt, so counting both would count one re-attempt twice. The cause
    /// is in the logs; the counter stays summable.
    pub const LLM_RETRIES: &str = "borg.llm.retries";
    pub const TOOL_EXECUTIONS: &str = "borg.tool.executions";
    pub const TOOL_DURATION: &str = "borg.tool.duration";
    pub const TOOL_REPEATS_BLOCKED: &str = "borg.tool.repeats_blocked";
    pub const UNKNOWN_TOOLS_REFUSED: &str = "borg.tool.unknown_refused";
    pub const GATEWAY_REQUESTS: &str = "borg.gateway.requests";
    pub const GATEWAY_DURATION: &str = "borg.gateway.duration";
    pub const COST_EVENTS_DROPPED: &str = "borg.telemetry.cost_events_dropped";
    /// Counter for context-window pressure events that triggered history
    /// compaction (labelled by `trigger`: proactive | reactive).
    pub const MEMORY_PRESSURE: &str = "borg.memory.pressure";
    /// Counter of history tokens reclaimed by compaction passes.
    pub const COMPACTION_TOKENS_SAVED: &str = "borg.compaction.tokens_saved";
    /// Histogram of post-compaction context occupancy as a percentage of the
    /// effective history budget — the number that says how much headroom a
    /// session has left, which "tokens saved" alone does not.
    pub const CONTEXT_OCCUPANCY: &str = "borg.context.occupancy_pct";
    /// Counter incremented once per harness boot (labelled by `harness`).
    pub const LIFECYCLE_STARTS: &str = "borg.lifecycle.starts";
    /// Counter incremented once per graceful harness shutdown (labelled by `harness`).
    pub const LIFECYCLE_SHUTDOWNS: &str = "borg.lifecycle.shutdowns";

    pub const WASM_INSTALLS: &str = "borg.wasm.installs_total";
    pub const WASM_EXECUTES: &str = "borg.wasm.executes_total";
    pub const WASM_EXECUTE_DURATION_MS: &str = "borg.wasm.execute_duration_ms";
    pub const WASM_QUARANTINES: &str = "borg.wasm.quarantines_total";
    pub const WASM_CACHE_HITS: &str = "borg.wasm.cache_hits_total";
    pub const WASM_CACHE_MISSES: &str = "borg.wasm.cache_misses_total";
    pub const WASM_RATE_LIMITED: &str = "borg.wasm.rate_limited_total";
    pub const WASM_RESOURCE_EXHAUSTED: &str = "borg.wasm.resource_exhausted_total";
}

/// Guard that shuts down OpenTelemetry providers on drop.
/// Must live for the program's duration.
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    /// Create a no-op guard that does not manage any providers.
    pub fn noop() -> Self {
        Self {
            tracer_provider: None,
            meter_provider: None,
            logger_provider: None,
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(tp) = self.tracer_provider.take() {
            if let Err(e) = tp.shutdown() {
                tracing::warn!("OpenTelemetry tracer shutdown error: {e}");
            }
        }
        if let Some(mp) = self.meter_provider.take() {
            if let Err(e) = mp.shutdown() {
                tracing::warn!("OpenTelemetry meter shutdown error: {e}");
            }
        }
        if let Some(lp) = self.logger_provider.take() {
            if let Err(e) = lp.shutdown() {
                tracing::warn!("OpenTelemetry logger shutdown error: {e}");
            }
        }
    }
}

/// Type alias for a boxed tracing layer compatible with Registry.
pub type BoxedLayer = Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync>;

/// Install the global `tracing` subscriber from the OTel layers, an env
/// filter, and a fmt layer. Centralizes the registry composition so each
/// binary (`borg`, `borgd`) does not hand-roll it. The `EnvFilter` is a global
/// filter, so it gates the OTel exporters and the fmt sink alike regardless of
/// layer order.
///
/// The filter must wrap the layer stack via `with_filter`, NOT be pushed into
/// the `Vec` as a sibling layer: `Vec<Layer>::register_callsite` keeps the
/// *most* interested child, so an unfiltered fmt layer (`Interest::always`)
/// overrides the filter's `never` and tracing then skips per-event `enabled()`
/// checks entirely — TRACE/DEBUG spam reaches stdout regardless of `RUST_LOG`.
pub fn install_subscriber(
    layers: Vec<BoxedLayer>,
    env_filter: tracing_subscriber::EnvFilter,
    fmt_layer: BoxedLayer,
) {
    use tracing_subscriber::util::SubscriberInitExt;
    build_subscriber(layers, env_filter, fmt_layer).init();
}

/// Compose (but do not install) the subscriber `install_subscriber` uses.
/// Split out so tests can install it as a scoped/global dispatcher and verify
/// the env filter actually gates events end-to-end.
pub fn build_subscriber(
    mut layers: Vec<BoxedLayer>,
    env_filter: tracing_subscriber::EnvFilter,
    fmt_layer: BoxedLayer,
) -> impl tracing::Subscriber + Send + Sync {
    use tracing_subscriber::layer::{Layer as _, SubscriberExt};
    layers.push(fmt_layer);
    tracing_subscriber::registry().with(layers.with_filter(env_filter))
}

/// RAII guard that records a harness boot on construction and a graceful
/// shutdown on drop, so the `borg.lifecycle.{starts,shutdowns}` counters stay
/// balanced without each harness having to remember a paired shutdown call.
/// Hold it for the lifetime of the harness (`borgd` daemon, TUI session).
pub struct LifecycleGuard {
    metrics: BorgMetrics,
    harness: &'static str,
}

impl LifecycleGuard {
    /// Record a `start` for `harness` now; the matching `shutdown` fires on drop.
    pub fn new(metrics: BorgMetrics, harness: &'static str) -> Self {
        metrics.record_lifecycle_start(harness);
        Self { metrics, harness }
    }
}

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        self.metrics.record_lifecycle_shutdown(self.harness);
    }
}

/// Initializes the OpenTelemetry export pipeline from `config`.
///
/// Returns the `tracing` layers that must be added to the subscriber (a span
/// export layer when tracing is enabled and a logs-bridge layer when logs are
/// enabled — the vec is empty when neither is on) plus a guard that flushes and
/// shuts the providers down on drop, so it must be held for the program's
/// lifetime.
///
/// Also installs the global W3C `TraceContextPropagator` whenever any signal is
/// enabled, so [`propagation::inject_current_context`] /
/// [`propagation::extract_parent_context`] carry `traceparent` across the
/// outbound-HTTP and inbound-webhook boundaries.
pub fn init_telemetry(config: &TelemetryConfig) -> Result<(Vec<BoxedLayer>, TelemetryGuard)> {
    if !config.tracing_enabled && !config.metrics_enabled && !config.logs_enabled {
        return Ok((Vec::new(), TelemetryGuard::noop()));
    }

    // Any enabled signal means cross-process trace correlation is wanted.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let resource = || {
        opentelemetry_sdk::Resource::builder()
            .with_service_name(config.service_name.clone())
            .build()
    };

    let mut tracer_provider = None;
    let mut meter_provider = None;
    let mut logger_provider = None;
    let mut layers: Vec<BoxedLayer> = Vec::new();

    if config.tracing_enabled {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&config.otlp_endpoint)
            .build()?;

        let sampler = opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(config.sampling_ratio);

        let tp = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_sampler(sampler)
            .with_resource(resource())
            .build();

        let tracer = tp.tracer("borg");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        layers.push(Box::new(layer));
        tracer_provider = Some(tp);
    }

    if config.metrics_enabled {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(&config.otlp_endpoint)
            .build()?;

        let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter)
            .with_interval(crate::constants::TELEMETRY_FLUSH_INTERVAL)
            .build();

        let mp = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource())
            .build();

        opentelemetry::global::set_meter_provider(mp.clone());
        meter_provider = Some(mp);
    }

    if config.logs_enabled {
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(&config.otlp_endpoint)
            .build()?;

        let lp = SdkLoggerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource())
            .build();

        // Bridge `tracing` events into OTel logs. Boxed as a Registry layer so
        // it composes with the span layer + fmt layer at the call site.
        let bridge = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&lp);
        layers.push(Box::new(bridge));
        logger_provider = Some(lp);
    }

    Ok((
        layers,
        TelemetryGuard {
            tracer_provider,
            meter_provider,
            logger_provider,
        },
    ))
}

/// A meter provider with no readers — instruments are real but data never
/// reaches an exporter. Used by [`BorgMetrics::noop`] so that disabling metrics
/// is genuinely free of side effects regardless of what the global provider is
/// (`global::meter("borg")` returns SDK instruments under an installed provider,
/// so it cannot stand in as a noop here).
fn noop_meter_provider() -> SdkMeterProvider {
    SdkMeterProvider::builder().build()
}

/// Metrics instruments for Borg. Clone-friendly (all instruments are Arc-backed).
#[derive(Clone)]
pub struct BorgMetrics {
    pub agent_turns: Counter<u64>,
    pub agent_iterations: Counter<u64>,
    pub llm_requests: Counter<u64>,
    pub llm_duration: Histogram<f64>,
    pub llm_tokens: Counter<u64>,
    /// LLM request re-attempts, labelled by `kind` (retry/failover).
    pub llm_retries: Counter<u64>,
    pub tool_executions: Counter<u64>,
    pub tool_duration: Histogram<f64>,
    /// Counter for tool calls blocked because they were the Nth+ identical
    /// consecutive call within a turn (`loop_guard.identical_call_max`).
    pub tool_repeats_blocked: Counter<u64>,
    /// Counter for hallucinated tool names refused after exceeding
    /// `loop_guard.unknown_tool_max_per_name` per-turn attempts.
    pub unknown_tools_refused: Counter<u64>,
    pub gateway_requests: Counter<u64>,
    pub gateway_duration: Histogram<f64>,
    pub cost_events_dropped: Counter<u64>,
    /// Context-window pressure events that triggered history compaction.
    pub memory_pressure: Counter<u64>,
    /// History tokens reclaimed by compaction passes.
    pub compaction_tokens_saved: Counter<u64>,
    /// Post-compaction occupancy (percent of the effective history budget).
    pub context_occupancy: Histogram<f64>,
    /// Harness boots (REPL/TUI/daemon).
    pub lifecycle_starts: Counter<u64>,
    /// Graceful harness shutdowns.
    pub lifecycle_shutdowns: Counter<u64>,
}

impl Default for BorgMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl BorgMetrics {
    /// Build against the globally installed meter provider.
    pub fn new() -> Self {
        Self::from_meter(&opentelemetry::global::meter("borg"))
    }

    /// Real instruments when metrics are enabled in `config`, otherwise a true
    /// noop instance unaffected by global provider state.
    pub fn from_config(config: &crate::config::Config) -> Self {
        if config.telemetry.metrics_enabled {
            Self::new()
        } else {
            Self::noop()
        }
    }

    /// Self-contained noop: each instrument is bound to a private
    /// reader-less meter provider, so emissions allocate aggregation state
    /// nowhere of consequence and never reach any exporter.
    pub fn noop() -> Self {
        let provider = noop_meter_provider();
        Self::from_meter(&provider.meter("borg-noop"))
    }

    /// Public so tests (and any caller wanting provider isolation) can build a
    /// metrics handle bound to a specific meter rather than the global one.
    pub fn from_meter(meter: &Meter) -> Self {
        Self {
            agent_turns: meter
                .u64_counter(names::AGENT_TURNS)
                .with_description("Number of agent turns completed")
                .build(),
            agent_iterations: meter
                .u64_counter(names::AGENT_ITERATIONS)
                .with_description("Number of agent loop iterations")
                .build(),
            llm_requests: meter
                .u64_counter(names::LLM_REQUESTS)
                .with_description("Number of LLM API requests")
                .build(),
            llm_duration: meter
                .f64_histogram(names::LLM_DURATION)
                .with_description("LLM request duration in seconds")
                .with_unit("s")
                .build(),
            llm_tokens: meter
                .u64_counter(names::LLM_TOKENS)
                .with_description("Total LLM tokens used")
                .build(),
            llm_retries: meter
                .u64_counter(names::LLM_RETRIES)
                .with_description("LLM request re-attempts by kind (retry/failover)")
                .build(),
            tool_executions: meter
                .u64_counter(names::TOOL_EXECUTIONS)
                .with_description("Number of tool executions")
                .build(),
            tool_duration: meter
                .f64_histogram(names::TOOL_DURATION)
                .with_description("Tool execution duration in seconds")
                .with_unit("s")
                .build(),
            tool_repeats_blocked: meter
                .u64_counter(names::TOOL_REPEATS_BLOCKED)
                .with_description("Tool calls blocked as repeated identical consecutive calls")
                .build(),
            unknown_tools_refused: meter
                .u64_counter(names::UNKNOWN_TOOLS_REFUSED)
                .with_description(
                    "Hallucinated tool names refused after exceeding per-name attempt cap",
                )
                .build(),
            gateway_requests: meter
                .u64_counter(names::GATEWAY_REQUESTS)
                .with_description("Number of gateway webhook requests")
                .build(),
            gateway_duration: meter
                .f64_histogram(names::GATEWAY_DURATION)
                .with_description("Gateway request processing duration in seconds")
                .with_unit("s")
                .build(),
            cost_events_dropped: meter
                .u64_counter(names::COST_EVENTS_DROPPED)
                .with_description("Number of dropped cost events due to lagged consumers")
                .build(),
            memory_pressure: meter
                .u64_counter(names::MEMORY_PRESSURE)
                .with_description("Context-window pressure events that triggered compaction")
                .build(),
            compaction_tokens_saved: meter
                .u64_counter(names::COMPACTION_TOKENS_SAVED)
                .with_description("History tokens reclaimed by compaction passes")
                .build(),
            context_occupancy: meter
                .f64_histogram(names::CONTEXT_OCCUPANCY)
                .with_description("Post-compaction occupancy as % of the effective history budget")
                .build(),
            lifecycle_starts: meter
                .u64_counter(names::LIFECYCLE_STARTS)
                .with_description("Harness boots")
                .build(),
            lifecycle_shutdowns: meter
                .u64_counter(names::LIFECYCLE_SHUTDOWNS)
                .with_description("Graceful harness shutdowns")
                .build(),
        }
    }

    /// Record a context-window pressure event. `trigger` is `"proactive"`
    /// (pre-send budget check) or `"reactive"` (mid-stream context overflow).
    pub fn record_memory_pressure(&self, trigger: &'static str) {
        self.memory_pressure
            .add(1, &[KeyValue::new("trigger", trigger)]);
    }

    /// Record a completed compaction pass: tokens reclaimed plus the
    /// occupancy (%) the session settled at against its effective budget.
    /// Occupancy is the headline — a pass can save few tokens and still
    /// leave the session with plenty of headroom (or none), which the
    /// savings counter alone cannot show.
    pub fn record_compaction_result(
        &self,
        before_tokens: usize,
        after_tokens: usize,
        budget: usize,
    ) {
        self.compaction_tokens_saved
            .add(before_tokens.saturating_sub(after_tokens) as u64, &[]);
        if budget > 0 {
            let pct = (after_tokens as f64 / budget as f64) * 100.0;
            self.context_occupancy.record(pct.min(100.0), &[]);
        }
    }

    /// Record an LLM request re-attempt. `kind` is `"retry"` (same provider,
    /// same request) or `"failover"` (switched to a fallback slot) — the two
    /// are disjoint, so they sum to total re-attempts.
    ///
    /// Do not add a kind for connection-lost retries. The reconnect notice is
    /// emitted before the backoff and the loop then starts the next attempt,
    /// which is already counted as `"retry"`; a second kind there counts one
    /// re-attempt twice and breaks the sum.
    ///
    /// The first attempt of a request is not a retry — `borg.llm.requests`
    /// already counts it, and double-counting would hide the storm this metric
    /// exists to show.
    pub fn record_llm_retry(&self, kind: &'static str) {
        self.llm_retries.add(1, &[KeyValue::new("kind", kind)]);
    }

    /// Record a harness boot. `harness` is e.g. `"borgd"` or `"tui"`.
    pub fn record_lifecycle_start(&self, harness: &'static str) {
        self.lifecycle_starts
            .add(1, &[KeyValue::new("harness", harness)]);
    }

    /// Record a graceful harness shutdown.
    pub fn record_lifecycle_shutdown(&self, harness: &'static str) {
        self.lifecycle_shutdowns
            .add(1, &[KeyValue::new("harness", harness)]);
    }
}

/// WASM-plugin metric instruments and emission helpers.
///
/// Public emission helpers (`record_*`) go through a global [`Instruments`]
/// constructed against `opentelemetry::global::meter("borg")` on first call.
/// Tests construct their own [`Instruments`] against a private meter to avoid
/// global pollution — the helper functions delegate to `Instruments` so the
/// emission logic itself is shared between prod and test paths.
pub mod wasm_metrics {
    use std::sync::OnceLock;

    use opentelemetry::metrics::{Counter, Histogram, Meter};
    use opentelemetry::KeyValue;

    use plugins::wasm::WasmExecuteErrorKind;

    use super::names;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum WasmCacheOutcome {
        Hit,
        Miss,
    }

    /// All WASM-plugin instruments bound to a single meter. Construct one per
    /// meter; `record_*` helpers on this struct contain the actual emission
    /// logic that the module-level functions reuse via the global instance.
    pub struct Instruments {
        installs: Counter<u64>,
        executes: Counter<u64>,
        execute_duration_ms: Histogram<u64>,
        quarantines: Counter<u64>,
        cache_hits: Counter<u64>,
        cache_misses: Counter<u64>,
        rate_limited: Counter<u64>,
        resource_exhausted: Counter<u64>,
    }

    impl Instruments {
        pub fn from_meter(m: &Meter) -> Self {
            Self {
                installs: m
                    .u64_counter(names::WASM_INSTALLS)
                    .with_description("Successful WASM plugin installs")
                    .build(),
                executes: m
                    .u64_counter(names::WASM_EXECUTES)
                    .with_description("WASM plugin tool dispatches (labelled by outcome)")
                    .build(),
                execute_duration_ms: m
                    .u64_histogram(names::WASM_EXECUTE_DURATION_MS)
                    .with_description("WASM plugin tool execution duration in milliseconds")
                    .with_unit("ms")
                    .build(),
                quarantines: m
                    .u64_counter(names::WASM_QUARANTINES)
                    .with_description("Plugins moved into quarantine (labelled by reason)")
                    .build(),
                cache_hits: m
                    .u64_counter(names::WASM_CACHE_HITS)
                    .with_description("Compiled-module cache hits")
                    .build(),
                cache_misses: m
                    .u64_counter(names::WASM_CACHE_MISSES)
                    .with_description("Compiled-module cache misses (JIT compile)")
                    .build(),
                rate_limited: m
                    .u64_counter(names::WASM_RATE_LIMITED)
                    .with_description("Plugin dispatches refused by the per-plugin token bucket")
                    .build(),
                resource_exhausted: m
                    .u64_counter(names::WASM_RESOURCE_EXHAUSTED)
                    .with_description("Plugin executions stopped by a resource ceiling")
                    .build(),
            }
        }

        pub fn record_install(&self, publisher_key: &str) {
            self.installs.add(
                1,
                &[KeyValue::new("publisher_key", publisher_key.to_string())],
            );
        }

        pub fn record_execute(
            &self,
            plugin_id: &str,
            tool_name: &str,
            duration_ms: u64,
            error_category: Option<WasmExecuteErrorKind>,
        ) {
            let outcome = error_category
                .map(WasmExecuteErrorKind::as_str)
                .unwrap_or("ok");
            let attrs = [
                KeyValue::new("plugin_id", plugin_id.to_string()),
                KeyValue::new("tool_name", tool_name.to_string()),
                KeyValue::new("outcome", outcome),
            ];
            self.executes.add(1, &attrs);
            self.execute_duration_ms.record(duration_ms, &attrs);
            if let Some(kind) = error_category {
                if is_resource_exhaustion(kind) {
                    self.resource_exhausted.add(
                        1,
                        &[
                            KeyValue::new("plugin_id", plugin_id.to_string()),
                            KeyValue::new("kind", kind.as_str()),
                        ],
                    );
                }
            }
        }

        pub fn record_quarantine(&self, plugin_id: &str, reason_category: &str) {
            self.quarantines.add(
                1,
                &[
                    KeyValue::new("plugin_id", plugin_id.to_string()),
                    KeyValue::new("reason_category", reason_category.to_string()),
                ],
            );
        }

        pub fn record_cache(&self, outcome: WasmCacheOutcome) {
            match outcome {
                WasmCacheOutcome::Hit => self.cache_hits.add(1, &[]),
                WasmCacheOutcome::Miss => self.cache_misses.add(1, &[]),
            }
        }

        pub fn record_rate_limited(&self, plugin_id: &str) {
            self.rate_limited
                .add(1, &[KeyValue::new("plugin_id", plugin_id.to_string())]);
        }
    }

    /// The five buckets we explicitly surface on the resource-exhausted counter.
    /// Kept as a free function so both prod emission and tests assert against
    /// the same predicate.
    pub(crate) fn is_resource_exhaustion(kind: WasmExecuteErrorKind) -> bool {
        matches!(
            kind,
            WasmExecuteErrorKind::FuelExhausted
                | WasmExecuteErrorKind::Timeout
                | WasmExecuteErrorKind::MemoryExceeded
                | WasmExecuteErrorKind::OutputTooLarge
                | WasmExecuteErrorKind::LogBytesExceeded
        )
    }

    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

    fn global() -> &'static Instruments {
        INSTRUMENTS.get_or_init(|| Instruments::from_meter(&opentelemetry::global::meter("borg")))
    }

    pub fn record_install(publisher_key: &str) {
        global().record_install(publisher_key);
    }

    pub fn record_execute(
        plugin_id: &str,
        tool_name: &str,
        duration_ms: u64,
        error_category: Option<WasmExecuteErrorKind>,
    ) {
        global().record_execute(plugin_id, tool_name, duration_ms, error_category);
    }

    pub fn record_quarantine(plugin_id: &str, reason_category: &str) {
        global().record_quarantine(plugin_id, reason_category);
    }

    pub fn record_cache(outcome: WasmCacheOutcome) {
        global().record_cache(outcome);
    }

    pub fn record_rate_limited(plugin_id: &str) {
        global().record_rate_limited(plugin_id);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::Resource;
    use plugins::wasm::WasmExecuteErrorKind;
    use wasm_metrics::{is_resource_exhaustion, Instruments, WasmCacheOutcome};

    /// Stand up an isolated meter provider whose exports are captured by an
    /// in-memory exporter.
    pub(crate) fn test_metrics() -> (
        SdkMeterProvider,
        opentelemetry::metrics::Meter,
        InMemoryMetricExporter,
    ) {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone())
            .with_interval(std::time::Duration::from_secs(3600))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(Resource::builder().with_service_name("test").build())
            .build();
        let meter = provider.meter("borg-test");
        (provider, meter, exporter)
    }

    /// Force-flush the provider and return the exported `ResourceMetrics`.
    pub(crate) fn collect(
        provider: &SdkMeterProvider,
        exporter: &InMemoryMetricExporter,
    ) -> Vec<ResourceMetrics> {
        provider.force_flush().expect("flush succeeds");
        exporter
            .get_finished_metrics()
            .expect("exporter returns metrics")
    }

    /// Walks every metric across every scope and invokes `f` once per match.
    pub(crate) fn with_metric<F: FnMut(&AggregatedMetrics)>(
        rms: &[ResourceMetrics],
        name: &str,
        mut f: F,
    ) -> usize {
        let mut hits = 0;
        for rm in rms {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() == name {
                        hits += 1;
                        f(m.data());
                    }
                }
            }
        }
        hits
    }

    fn has_metric(rms: &[ResourceMetrics], name: &str) -> bool {
        with_metric(rms, name, |_| {}) > 0
    }

    fn u64_sum_for(rms: &[ResourceMetrics], name: &str) -> u64 {
        let mut total = 0u64;
        let hits = with_metric(rms, name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(s)) => {
                total += s.data_points().map(|dp| dp.value()).sum::<u64>();
            }
            other => panic!("{name}: expected u64 sum, got {other:?}"),
        });
        assert!(hits > 0, "missing metric: {name}");
        total
    }

    fn u64_hist_sum_for(rms: &[ResourceMetrics], name: &str) -> u64 {
        let mut total = 0u64;
        let hits = with_metric(rms, name, |data| match data {
            AggregatedMetrics::U64(MetricData::Histogram(h)) => {
                total += h.data_points().map(|dp| dp.sum()).sum::<u64>();
            }
            other => panic!("{name}: expected u64 histogram, got {other:?}"),
        });
        assert!(hits > 0, "missing histogram: {name}");
        total
    }

    fn f64_hist_count_for(rms: &[ResourceMetrics], name: &str) -> u64 {
        let mut total = 0u64;
        let hits = with_metric(rms, name, |data| match data {
            AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                total += h.data_points().map(|dp| dp.count()).sum::<u64>();
            }
            other => panic!("{name}: expected f64 histogram, got {other:?}"),
        });
        assert!(hits > 0, "missing histogram: {name}");
        total
    }

    fn f64_hist_sum_for(rms: &[ResourceMetrics], name: &str) -> f64 {
        let mut total = 0f64;
        let hits = with_metric(rms, name, |data| match data {
            AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                total += h.data_points().map(|dp| dp.sum()).sum::<f64>();
            }
            other => panic!("{name}: expected f64 histogram, got {other:?}"),
        });
        assert!(hits > 0, "missing histogram: {name}");
        total
    }

    fn attr_value(attrs: &[KeyValue], key: &str) -> Option<String> {
        attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    }

    /// Sum a u64 Sum metric's data points grouped by one attribute's value.
    /// Used where a metric's whole point is that it splits into labelled
    /// series, so a single total would not prove anything.
    pub(crate) fn sum_points_by_attr(
        rms: &[ResourceMetrics],
        name: &str,
        key: &str,
    ) -> std::collections::HashMap<String, u64> {
        let mut by_value: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let hits = with_metric(rms, name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(s)) => {
                for dp in s.data_points() {
                    let attrs: Vec<KeyValue> = dp.attributes().cloned().collect();
                    if let Some(v) = attr_value(&attrs, key) {
                        *by_value.entry(v).or_default() += dp.value();
                    }
                }
            }
            other => panic!("{name}: expected u64 sum, got {other:?}"),
        });
        assert!(hits > 0, "missing metric: {name}");
        by_value
    }

    /// Collect the attributes of the single data point of a u64 Sum metric.
    /// Panics if the metric is missing, has the wrong shape, or has != 1 dp.
    fn sole_sum_attrs(rms: &[ResourceMetrics], name: &str) -> Vec<KeyValue> {
        let mut found: Option<Vec<KeyValue>> = None;
        with_metric(rms, name, |data| match data {
            AggregatedMetrics::U64(MetricData::Sum(s)) => {
                let dps: Vec<_> = s.data_points().collect();
                assert_eq!(
                    dps.len(),
                    1,
                    "{name}: expected 1 data point, got {}",
                    dps.len()
                );
                found = Some(dps[0].attributes().cloned().collect());
            }
            other => panic!("{name}: expected u64 sum, got {other:?}"),
        });
        found.unwrap_or_else(|| panic!("missing metric: {name}"))
    }

    #[test]
    fn init_telemetry_off_yields_no_layer_and_no_export() {
        // Spec: with both flags off, init returns no tracing layer AND
        // BorgMetrics::noop() emits nothing observable via any exporter.
        let config = TelemetryConfig::default();
        assert!(!config.tracing_enabled);
        assert!(!config.metrics_enabled);

        let (layers, _guard) = init_telemetry(&config).expect("init succeeds when disabled");
        assert!(layers.is_empty(), "no layers when all signals disabled");

        // Sanity: a noop BorgMetrics emits without panicking AND aggregates
        // nowhere observable. We can't easily inspect the private noop
        // provider, but the noop_metrics_isolated_from_global test below
        // covers the cross-provider isolation property.
        let m = BorgMetrics::noop();
        m.agent_turns.add(1, &[]);
        m.gateway_duration.record(0.42, &[]);
    }

    #[tokio::test]
    async fn logs_enabled_yields_only_the_bridge_layer() {
        // Spec: enabling logs (and nothing else) installs exactly one tracing
        // layer — the OTLP logs bridge — and a guard that owns a logger
        // provider. Building the exporter must not require a live collector.
        let config = TelemetryConfig {
            logs_enabled: true,
            ..TelemetryConfig::default()
        };
        let (layers, guard) = init_telemetry(&config).expect("init with logs succeeds");
        assert_eq!(
            layers.len(),
            1,
            "logs-only init yields exactly the logs-bridge layer"
        );
        assert!(
            guard.logger_provider.is_some(),
            "guard must own the logger provider so it is flushed on drop"
        );
        assert!(guard.tracer_provider.is_none());
    }

    #[tokio::test]
    async fn tracing_plus_logs_yields_two_layers() {
        // Spec: tracing and logs each contribute a layer; both flags on => 2.
        let config = TelemetryConfig {
            tracing_enabled: true,
            logs_enabled: true,
            ..TelemetryConfig::default()
        };
        let (layers, guard) = init_telemetry(&config).expect("init succeeds");
        assert_eq!(layers.len(), 2, "span layer + logs-bridge layer");
        assert!(guard.tracer_provider.is_some());
        assert!(guard.logger_provider.is_some());
    }

    #[test]
    fn lifecycle_and_memory_pressure_emit_under_canonical_names() {
        // Spec: the harness-lifecycle and memory-pressure signals export under
        // their `names::*` constants with the expected label keys. A rename
        // that desyncs const from builder drops the series and fails here.
        let (provider, meter, exporter) = test_metrics();
        let m = BorgMetrics::from_meter(&meter);

        m.record_memory_pressure("reactive");
        m.record_memory_pressure("proactive");
        m.record_lifecycle_start("borgd");
        m.record_lifecycle_shutdown("borgd");
        m.record_compaction_result(10_000, 4_000, 8_000);

        let rms = collect(&provider, &exporter);

        assert_eq!(
            u64_sum_for(&rms, names::MEMORY_PRESSURE),
            2,
            "two pressure events recorded"
        );
        assert_eq!(u64_sum_for(&rms, names::LIFECYCLE_STARTS), 1);
        assert_eq!(u64_sum_for(&rms, names::LIFECYCLE_SHUTDOWNS), 1);

        // Compaction pass: 6k tokens reclaimed, settled at 4k/8k = 50%
        // occupancy of the effective budget.
        assert_eq!(u64_sum_for(&rms, names::COMPACTION_TOKENS_SAVED), 6_000);
        assert_eq!(f64_hist_count_for(&rms, names::CONTEXT_OCCUPANCY), 1);
        let occupancy = f64_hist_sum_for(&rms, names::CONTEXT_OCCUPANCY);
        assert!(
            (occupancy - 50.0).abs() < f64::EPSILON,
            "expected 50% occupancy, got {occupancy}"
        );

        // The single-datapoint lifecycle counters carry the harness label.
        let start_attrs = sole_sum_attrs(&rms, names::LIFECYCLE_STARTS);
        assert_eq!(
            attr_value(&start_attrs, "harness").as_deref(),
            Some("borgd")
        );
    }

    #[test]
    fn lifecycle_guard_records_start_then_shutdown_on_drop() {
        // Spec: the RAII guard emits a start on construction and a matching
        // shutdown on drop, so a harness that holds it cannot leave the
        // counters imbalanced. Before drop: 1 start, 0 shutdowns; after: 1/1.
        let (provider, meter, exporter) = test_metrics();
        let m = BorgMetrics::from_meter(&meter);

        let guard = LifecycleGuard::new(m.clone(), "tui");
        let mid = collect(&provider, &exporter);
        assert_eq!(u64_sum_for(&mid, names::LIFECYCLE_STARTS), 1);
        assert!(
            !has_metric(&mid, names::LIFECYCLE_SHUTDOWNS),
            "shutdown must not fire before the guard drops"
        );

        drop(guard);
        let after = collect(&provider, &exporter);
        assert_eq!(u64_sum_for(&after, names::LIFECYCLE_SHUTDOWNS), 1);
        let attrs = sole_sum_attrs(&after, names::LIFECYCLE_SHUTDOWNS);
        assert_eq!(attr_value(&attrs, "harness").as_deref(), Some("tui"));
    }

    /// Spec: a retry storm has to be separable from ordinary traffic, so each
    /// re-attempt must land on its own `kind` series rather than being summed
    /// into one opaque total (or into `borg.llm.requests`, which counts the
    /// requests themselves).
    #[test]
    fn llm_retries_are_labelled_by_kind() {
        let (provider, meter, exporter) = test_metrics();
        let m = BorgMetrics::from_meter(&meter);

        m.record_llm_retry("retry");
        m.record_llm_retry("retry");
        m.record_llm_retry("retry");
        m.record_llm_retry("failover");

        let rms = collect(&provider, &exporter);
        assert_eq!(
            u64_sum_for(&rms, names::LLM_RETRIES),
            4,
            "every re-attempt must be counted"
        );
        assert!(
            !has_metric(&rms, names::LLM_REQUESTS),
            "a retry is not a new request; double-counting hides the storm"
        );

        let kinds = sum_points_by_attr(&rms, names::LLM_RETRIES, "kind");
        assert_eq!(kinds.get("retry"), Some(&3));
        assert_eq!(kinds.get("failover"), Some(&1));
    }

    #[test]
    fn borg_metrics_emit_under_centralized_names() {
        // Spec: every instrument BorgMetrics owns must export under the
        // canonical name from `names::*`. If a future rename forgets to
        // update both the const and the builder, the resulting series
        // won't appear in the exporter and this test fails.
        let (provider, meter, exporter) = test_metrics();
        let m = BorgMetrics::from_meter(&meter);

        m.agent_turns.add(2, &[]);
        m.agent_iterations.add(3, &[]);
        m.llm_requests.add(1, &[]);
        m.llm_duration.record(0.5, &[]);
        m.llm_tokens.add(123, &[]);
        m.tool_executions.add(4, &[]);
        m.tool_duration.record(0.1, &[]);
        m.tool_repeats_blocked.add(1, &[]);
        m.unknown_tools_refused.add(1, &[]);
        m.gateway_requests.add(7, &[]);
        m.gateway_duration.record(0.2, &[]);
        m.cost_events_dropped.add(1, &[]);

        let rms = collect(&provider, &exporter);

        let counter_expectations: &[(&str, u64)] = &[
            (names::AGENT_TURNS, 2),
            (names::AGENT_ITERATIONS, 3),
            (names::LLM_REQUESTS, 1),
            (names::LLM_TOKENS, 123),
            (names::TOOL_EXECUTIONS, 4),
            (names::TOOL_REPEATS_BLOCKED, 1),
            (names::UNKNOWN_TOOLS_REFUSED, 1),
            (names::GATEWAY_REQUESTS, 7),
            (names::COST_EVENTS_DROPPED, 1),
        ];
        for (name, expected_sum) in counter_expectations {
            assert_eq!(
                u64_sum_for(&rms, name),
                *expected_sum,
                "{name} sum mismatch"
            );
        }

        for name in [
            names::LLM_DURATION,
            names::TOOL_DURATION,
            names::GATEWAY_DURATION,
        ] {
            assert_eq!(
                f64_hist_count_for(&rms, name),
                1,
                "{name} should have exactly 1 observation"
            );
        }
    }

    #[test]
    fn noop_metrics_isolated_from_global_provider() {
        // Spec: BorgMetrics::noop() must NOT export through whatever provider
        // is currently global. This is the regression guard for the prior
        // `global::meter("borg-noop")` hack, which returned real SDK
        // instruments under any installed provider.
        let (provider, _meter, exporter) = test_metrics();
        opentelemetry::global::set_meter_provider(provider.clone());

        let noop = BorgMetrics::noop();
        noop.agent_turns.add(999, &[]);
        noop.llm_duration.record(1.0, &[]);
        noop.gateway_requests.add(7, &[]);

        let rms = collect(&provider, &exporter);
        let any = rms
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .any(|_| true);
        assert!(!any, "noop metrics leaked into global provider");
    }

    #[test]
    fn wasm_record_execute_success_emits_executes_and_duration_only() {
        let (provider, meter, exporter) = test_metrics();
        let inst = Instruments::from_meter(&meter);

        inst.record_execute("plug-a", "tool-x", 42, None);

        let rms = collect(&provider, &exporter);

        assert_eq!(u64_sum_for(&rms, names::WASM_EXECUTES), 1);
        let attrs = sole_sum_attrs(&rms, names::WASM_EXECUTES);
        assert_eq!(attr_value(&attrs, "outcome").as_deref(), Some("ok"));
        assert_eq!(attr_value(&attrs, "plugin_id").as_deref(), Some("plug-a"));
        assert_eq!(attr_value(&attrs, "tool_name").as_deref(), Some("tool-x"));

        assert_eq!(u64_hist_sum_for(&rms, names::WASM_EXECUTE_DURATION_MS), 42);

        assert!(
            !has_metric(&rms, names::WASM_RESOURCE_EXHAUSTED),
            "resource_exhausted leaked on success"
        );
    }

    #[test]
    fn wasm_record_execute_exhaustion_kinds_bump_both_counters() {
        // Spec: the 5 exhaustion kinds bump executes AND resource_exhausted.
        // Non-exhaustion kinds bump only executes. A widening of the
        // `matches!` arm or a regression that drops a kind from the bucket
        // fails this table.
        let exhaustion = [
            WasmExecuteErrorKind::FuelExhausted,
            WasmExecuteErrorKind::Timeout,
            WasmExecuteErrorKind::MemoryExceeded,
            WasmExecuteErrorKind::OutputTooLarge,
            WasmExecuteErrorKind::LogBytesExceeded,
        ];
        let non_exhaustion = [
            WasmExecuteErrorKind::HostHttpDenied,
            WasmExecuteErrorKind::HostHttpSecretLeak,
            WasmExecuteErrorKind::HostHttpUpstream,
            WasmExecuteErrorKind::CredentialDenied,
            WasmExecuteErrorKind::InvalidGuestOutput,
            WasmExecuteErrorKind::GuestTrap,
        ];

        for kind in exhaustion {
            assert!(
                is_resource_exhaustion(kind),
                "{kind:?} should classify as exhaustion"
            );
            let (provider, meter, exporter) = test_metrics();
            let inst = Instruments::from_meter(&meter);
            inst.record_execute("p", "t", 1, Some(kind));
            let rms = collect(&provider, &exporter);

            assert_eq!(
                u64_sum_for(&rms, names::WASM_EXECUTES),
                1,
                "{kind:?}: executes wrong"
            );
            assert_eq!(
                u64_sum_for(&rms, names::WASM_RESOURCE_EXHAUSTED),
                1,
                "{kind:?}: resource_exhausted wrong"
            );

            let attrs = sole_sum_attrs(&rms, names::WASM_RESOURCE_EXHAUSTED);
            assert_eq!(
                attr_value(&attrs, "kind").as_deref(),
                Some(kind.as_str()),
                "{kind:?}: wrong kind label"
            );
        }

        for kind in non_exhaustion {
            assert!(
                !is_resource_exhaustion(kind),
                "{kind:?} must NOT classify as exhaustion"
            );
            let (provider, meter, exporter) = test_metrics();
            let inst = Instruments::from_meter(&meter);
            inst.record_execute("p", "t", 1, Some(kind));
            let rms = collect(&provider, &exporter);

            assert!(
                has_metric(&rms, names::WASM_EXECUTES),
                "{kind:?}: executes missing"
            );
            assert!(
                !has_metric(&rms, names::WASM_RESOURCE_EXHAUSTED),
                "{kind:?}: resource_exhausted leaked"
            );
        }
    }

    #[test]
    fn wasm_helpers_each_emit_under_canonical_name() {
        // One test, table-driven, exercises every remaining helper and asserts
        // the metric name + key label. Collapses what would otherwise be 5
        // near-duplicate per-helper tests.
        let (provider, meter, exporter) = test_metrics();
        let inst = Instruments::from_meter(&meter);

        inst.record_install("pubkey-1");
        inst.record_quarantine("plug-q", "integrity_failure");
        inst.record_cache(WasmCacheOutcome::Hit);
        inst.record_cache(WasmCacheOutcome::Miss);
        inst.record_rate_limited("plug-r");

        let rms = collect(&provider, &exporter);

        struct Expect {
            name: &'static str,
            sum: u64,
            label: Option<(&'static str, &'static str)>,
        }
        let expectations = [
            Expect {
                name: names::WASM_INSTALLS,
                sum: 1,
                label: Some(("publisher_key", "pubkey-1")),
            },
            Expect {
                name: names::WASM_QUARANTINES,
                sum: 1,
                label: Some(("reason_category", "integrity_failure")),
            },
            Expect {
                name: names::WASM_CACHE_HITS,
                sum: 1,
                label: None,
            },
            Expect {
                name: names::WASM_CACHE_MISSES,
                sum: 1,
                label: None,
            },
            Expect {
                name: names::WASM_RATE_LIMITED,
                sum: 1,
                label: Some(("plugin_id", "plug-r")),
            },
        ];

        for Expect {
            name,
            sum: expected_sum,
            label,
        } in &expectations
        {
            assert_eq!(
                u64_sum_for(&rms, name),
                *expected_sum,
                "{name} sum mismatch"
            );
            if let Some((lk, lv)) = label {
                let attrs = sole_sum_attrs(&rms, name);
                assert_eq!(
                    attr_value(&attrs, lk).as_deref(),
                    Some(*lv),
                    "{name}: label {lk} mismatch"
                );
            }
        }
    }
}
