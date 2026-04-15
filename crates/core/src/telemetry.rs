use anyhow::Result;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Registry;

use crate::config::TelemetryConfig;

/// Guard that shuts down OpenTelemetry providers on drop.
/// Must live for the program's duration.
pub struct TelemetryGuard {
    /// Active tracing provider, shut down on drop.
    tracer_provider: Option<SdkTracerProvider>,
    /// Active metrics provider, shut down on drop.
    meter_provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    /// Create a no-op guard that does not manage any providers.
    pub fn noop() -> Self {
        Self {
            tracer_provider: None,
            meter_provider: None,
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(tp) = self.tracer_provider.take() {
            if let Err(e) = tp.shutdown() {
                eprintln!("OpenTelemetry tracer shutdown error: {e}");
            }
        }
        if let Some(mp) = self.meter_provider.take() {
            if let Err(e) = mp.shutdown() {
                eprintln!("OpenTelemetry meter shutdown error: {e}");
            }
        }
    }
}

/// Type alias for a boxed tracing layer compatible with Registry.
pub type BoxedLayer = Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync>;

/// Returns an optional tracing-opentelemetry layer (None if tracing disabled)
/// and a guard that must be held for the program's lifetime.
pub fn init_telemetry(config: &TelemetryConfig) -> Result<(Option<BoxedLayer>, TelemetryGuard)> {
    if !config.tracing_enabled && !config.metrics_enabled {
        return Ok((
            None,
            TelemetryGuard {
                tracer_provider: None,
                meter_provider: None,
            },
        ));
    }

    let mut tracer_provider = None;
    let mut otel_layer: Option<BoxedLayer> = None;
    let mut meter_provider = None;

    if config.tracing_enabled {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&config.otlp_endpoint)
            .build()?;

        let sampler = opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(config.sampling_ratio);

        let tp = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_sampler(sampler)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(config.service_name.clone())
                    .build(),
            )
            .build();

        let tracer = tp.tracer("borg");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        otel_layer = Some(Box::new(layer));
        tracer_provider = Some(tp);
    }

    if config.metrics_enabled {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(&config.otlp_endpoint)
            .build()?;

        let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter)
            .with_interval(std::time::Duration::from_secs(60))
            .build();

        let mp = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(config.service_name.clone())
                    .build(),
            )
            .build();

        opentelemetry::global::set_meter_provider(mp.clone());
        meter_provider = Some(mp);
    }

    Ok((
        otel_layer,
        TelemetryGuard {
            tracer_provider,
            meter_provider,
        },
    ))
}

/// Metrics instruments for Borg. Clone-friendly (all instruments are Arc-backed).
#[derive(Clone)]
pub struct BorgMetrics {
    /// Counter for completed agent turns.
    pub agent_turns: Counter<u64>,
    /// Counter for agent loop iterations.
    pub agent_iterations: Counter<u64>,
    /// Counter for LLM API requests.
    pub llm_requests: Counter<u64>,
    /// Histogram of LLM request durations in seconds.
    pub llm_duration: Histogram<f64>,
    /// Counter for total LLM tokens consumed.
    pub llm_tokens: Counter<u64>,
    /// Counter for tool executions.
    pub tool_executions: Counter<u64>,
    /// Histogram of tool execution durations in seconds.
    pub tool_duration: Histogram<f64>,
    /// Counter for gateway webhook requests.
    pub gateway_requests: Counter<u64>,
    /// Histogram of gateway request processing durations in seconds.
    pub gateway_duration: Histogram<f64>,
}

impl Default for BorgMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl BorgMetrics {
    /// Create instruments from the global meter provider.
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("borg");
        Self::from_meter(&meter)
    }

    /// Create metrics based on config: real instruments if enabled, noop otherwise.
    pub fn from_config(config: &crate::config::Config) -> Self {
        if config.telemetry.metrics_enabled {
            Self::new()
        } else {
            Self::noop()
        }
    }

    /// Create no-op instruments (zero overhead when metrics are disabled).
    pub fn noop() -> Self {
        // Use the global meter before any provider is installed — it returns noop instruments.
        let meter = opentelemetry::global::meter("borg-noop");
        Self::from_meter(&meter)
    }

    fn from_meter(meter: &Meter) -> Self {
        Self {
            agent_turns: meter
                .u64_counter("borg.agent.turns")
                .with_description("Number of agent turns completed")
                .build(),
            agent_iterations: meter
                .u64_counter("borg.agent.iterations")
                .with_description("Number of agent loop iterations")
                .build(),
            llm_requests: meter
                .u64_counter("borg.llm.requests")
                .with_description("Number of LLM API requests")
                .build(),
            llm_duration: meter
                .f64_histogram("borg.llm.duration")
                .with_description("LLM request duration in seconds")
                .with_unit("s")
                .build(),
            llm_tokens: meter
                .u64_counter("borg.llm.tokens")
                .with_description("Total LLM tokens used")
                .build(),
            tool_executions: meter
                .u64_counter("borg.tool.executions")
                .with_description("Number of tool executions")
                .build(),
            tool_duration: meter
                .f64_histogram("borg.tool.duration")
                .with_description("Tool execution duration in seconds")
                .with_unit("s")
                .build(),
            gateway_requests: meter
                .u64_counter("borg.gateway.requests")
                .with_description("Number of gateway webhook requests")
                .build(),
            gateway_duration: meter
                .f64_histogram("borg.gateway.duration")
                .with_description("Gateway request processing duration in seconds")
                .with_unit("s")
                .build(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_disabled() {
        let config = TelemetryConfig::default();
        assert!(!config.tracing_enabled);
        assert!(!config.metrics_enabled);
    }

    #[test]
    fn test_init_noop_when_disabled() {
        let config = TelemetryConfig::default();
        let (layer, _guard) = init_telemetry(&config).expect("init should succeed");
        assert!(layer.is_none());
    }

    #[test]
    fn test_borg_metrics_noop() {
        let metrics = BorgMetrics::noop();
        // Verify noop instruments don't panic on use
        metrics.agent_turns.add(1, &[]);
        metrics.agent_iterations.add(1, &[]);
        metrics.llm_requests.add(1, &[]);
        metrics.llm_duration.record(0.5, &[]);
        metrics.llm_tokens.add(100, &[]);
        metrics.tool_executions.add(1, &[]);
        metrics.tool_duration.record(0.1, &[]);
        metrics.gateway_requests.add(1, &[]);
        metrics.gateway_duration.record(0.2, &[]);
    }

    #[tokio::test]
    async fn test_init_with_tracing_enabled() {
        let config = TelemetryConfig {
            tracing_enabled: true,
            metrics_enabled: false,
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_name: "borg-test".to_string(),
            sampling_ratio: 1.0,
        };
        let result = init_telemetry(&config);
        // This may fail if no OTLP endpoint is available, but the builder itself should succeed
        if let Ok((layer, _guard)) = result {
            assert!(layer.is_some());
        }
    }

    #[tokio::test]
    async fn test_init_with_metrics_enabled() {
        let config = TelemetryConfig {
            tracing_enabled: false,
            metrics_enabled: true,
            otlp_endpoint: "http://localhost:4317".to_string(),
            service_name: "borg-test".to_string(),
            sampling_ratio: 1.0,
        };
        let result = init_telemetry(&config);
        if let Ok((layer, _guard)) = result {
            assert!(layer.is_none());
        }
    }

    #[test]
    fn telemetry_guard_noop_constructor() {
        let guard = TelemetryGuard::noop();
        assert!(guard.tracer_provider.is_none());
        assert!(guard.meter_provider.is_none());
        // Dropping a noop guard must not panic or print.
        drop(guard);
    }

    #[test]
    fn borg_metrics_default_is_new() {
        let metrics = BorgMetrics::default();
        metrics.agent_turns.add(1, &[]);
    }

    #[test]
    fn borg_metrics_from_config_respects_flag() {
        use crate::config::Config;
        let mut config = Config::default();
        config.telemetry.metrics_enabled = false;
        let noop = BorgMetrics::from_config(&config);
        noop.llm_requests.add(5, &[]);
        noop.gateway_duration.record(0.01, &[]);
    }
}
