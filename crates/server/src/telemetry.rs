//! Exporting telemetry over OTLP.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, ensure};
use hylla_config::{Otlp, OtlpProtocol};
use opentelemetry::{KeyValue, global, trace::TracerProvider};
use opentelemetry_otlp::{
    MetricExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::{Sampler, SdkTracerProvider},
};
use tracing_subscriber::{
    EnvFilter, Layer, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt,
};

/// The telemetry providers, flushed and shut down by [`Telemetry::shutdown`].
pub struct Telemetry {
    meter_provider: Option<SdkMeterProvider>,
    tracer_provider: Option<SdkTracerProvider>,
}

/// Whether spans record request and object paths.
static RECORD_PATHS: AtomicBool = AtomicBool::new(false);

pub(crate) fn record_paths() -> bool {
    RECORD_PATHS.load(Ordering::Relaxed)
}

// r[impl telemetry.init]
/// Sets up exporting telemetry as configured, installing the global providers and the
/// tracing subscriber, which logs to stderr filtered by `RUST_LOG`.
///
/// Must run within a Tokio runtime, which gRPC exporters need, and before the app is built,
/// so its instruments use the installed meter provider.
pub fn init(config: &hylla_config::Telemetry) -> anyhow::Result<Telemetry> {
    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes(
            config
                .resource
                .iter()
                .map(|attribute| KeyValue::new(attribute.key.clone(), attribute.value.clone())),
        )
        .build();

    // r[impl telemetry.metrics]
    let meter_provider = if config.metrics.enabled {
        let exporter = metric_exporter(&config.otlp, config.metrics.endpoint.as_deref())
            .context("creating the OTLP metric exporter")?;
        let reader = PeriodicReader::builder(exporter)
            .with_interval(config.metrics.interval.duration())
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_reader(reader)
            .build();
        global::set_meter_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    // r[impl telemetry.traces]
    let tracer_provider = if config.traces.enabled {
        let ratio = config.traces.sample_ratio;
        ensure!(
            (0.0..=1.0).contains(&ratio),
            "the trace sample ratio must be from 0 to 1, not {ratio}"
        );
        let exporter = span_exporter(&config.otlp, config.traces.endpoint.as_deref())
            .context("creating the OTLP span exporter")?;
        let provider = SdkTracerProvider::builder()
            .with_resource(resource)
            .with_sampler(sampler(ratio))
            .with_batch_exporter(exporter)
            .build();
        global::set_tracer_provider(provider.clone());
        // r[impl telemetry.traces.propagation]
        global::set_text_map_propagator(TraceContextPropagator::new());
        RECORD_PATHS.store(config.traces.record_paths, Ordering::Relaxed);
        Some(provider)
    } else {
        None
    };

    let logs = tracing_subscriber::fmt::layer()
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    // Spans are filtered on their own, so a quieter log level doesn't lose them.
    let spans = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("hylla"))
            .with_location(false)
            .with_threads(false)
            .with_target(false)
            .with_tracked_inactivity(false)
            .with_filter(Targets::new().with_target("hylla_server", tracing::Level::INFO))
    });
    tracing_subscriber::registry()
        .with(logs)
        .with(spans)
        .try_init()
        .context("installing the tracing subscriber")?;

    Ok(Telemetry {
        meter_provider,
        tracer_provider,
    })
}

// r[impl telemetry.traces.sampling]
/// Samples `ratio` of the traces hylla starts, and follows the caller's decision for the
/// rest.
fn sampler(ratio: f64) -> Sampler {
    Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
}

impl Telemetry {
    /// Exports what's pending now.
    pub fn flush(&self) {
        if let Some(provider) = &self.tracer_provider
            && let Err(error) = provider.force_flush()
        {
            tracing::warn!("flushing traces failed: {error}");
        }
        if let Some(provider) = &self.meter_provider
            && let Err(error) = provider.force_flush()
        {
            tracing::warn!("flushing metrics failed: {error}");
        }
    }

    /// Exports what's pending and stops exporting.
    pub fn shutdown(&self) {
        if let Some(provider) = &self.tracer_provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!("shutting down traces failed: {error}");
        }
        if let Some(provider) = &self.meter_provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!("shutting down metrics failed: {error}");
        }
    }
}

// r[impl telemetry.endpoints]
/// The endpoint for a signal: the signal's own, or over HTTP the OTLP base endpoint with the
/// signal's path appended. `None` leaves it to the `OTEL_EXPORTER_OTLP_*` environment.
fn endpoint(otlp: &Otlp, signal: Option<&str>, path: &str) -> Option<String> {
    signal.map(String::from).or_else(|| {
        otlp.endpoint.as_ref().map(|base| match otlp.protocol {
            OtlpProtocol::HttpProtobuf => format!("{}{path}", base.trim_end_matches('/')),
            OtlpProtocol::Grpc => base.clone(),
        })
    })
}

fn metric_exporter(otlp: &Otlp, signal_endpoint: Option<&str>) -> anyhow::Result<MetricExporter> {
    let endpoint = endpoint(otlp, signal_endpoint, "/v1/metrics");
    Ok(match otlp.protocol {
        OtlpProtocol::HttpProtobuf => {
            over_http(MetricExporter::builder().with_http(), otlp, endpoint).build()?
        }
        OtlpProtocol::Grpc => {
            over_grpc(MetricExporter::builder().with_tonic(), otlp, endpoint)?.build()?
        }
    })
}

fn span_exporter(otlp: &Otlp, signal_endpoint: Option<&str>) -> anyhow::Result<SpanExporter> {
    let endpoint = endpoint(otlp, signal_endpoint, "/v1/traces");
    Ok(match otlp.protocol {
        OtlpProtocol::HttpProtobuf => {
            over_http(SpanExporter::builder().with_http(), otlp, endpoint).build()?
        }
        OtlpProtocol::Grpc => {
            over_grpc(SpanExporter::builder().with_tonic(), otlp, endpoint)?.build()?
        }
    })
}

/// Configures an exporter over HTTP.
fn over_http<B: WithExportConfig + WithHttpConfig>(
    mut builder: B,
    otlp: &Otlp,
    endpoint: Option<String>,
) -> B {
    builder = builder
        .with_protocol(Protocol::HttpBinary)
        .with_timeout(otlp.timeout.duration());
    if !otlp.headers.is_empty() {
        builder = builder.with_headers(otlp.headers.clone());
    }
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    builder
}

/// Configures an exporter over gRPC.
fn over_grpc<B: WithExportConfig + WithTonicConfig>(
    mut builder: B,
    otlp: &Otlp,
    endpoint: Option<String>,
) -> anyhow::Result<B> {
    builder = builder
        .with_protocol(Protocol::Grpc)
        .with_timeout(otlp.timeout.duration());
    if !otlp.headers.is_empty() {
        builder = builder.with_metadata(metadata(&otlp.headers)?);
    }
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    Ok(builder)
}

fn metadata(
    headers: &HashMap<String, String>,
) -> anyhow::Result<opentelemetry_otlp::tonic_types::metadata::MetadataMap> {
    let mut map = http::HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            http::HeaderName::try_from(name.as_str())
                .with_context(|| format!("header `{name}`"))?,
            http::HeaderValue::try_from(value.as_str())
                .with_context(|| format!("header `{name}`"))?,
        );
    }
    Ok(opentelemetry_otlp::tonic_types::metadata::MetadataMap::from_headers(map))
}

#[cfg(test)]
mod tests {
    use hylla_config::{Otlp, OtlpProtocol};

    use opentelemetry::{
        Context,
        trace::{SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId, TraceState},
    };
    use opentelemetry_sdk::trace::{SamplingDecision, ShouldSample};

    use super::{endpoint, sampler};

    // r[verify telemetry.traces.sampling]
    #[test]
    fn sampling_follows_the_ratio_and_the_caller() {
        let decide = |ratio, parent: Option<TraceFlags>| {
            let trace_id = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
            let parent = parent.map(|flags| {
                Context::new().with_remote_span_context(SpanContext::new(
                    trace_id,
                    SpanId::from_hex("b7ad6b7169203331").unwrap(),
                    flags,
                    true,
                    TraceState::default(),
                ))
            });
            sampler(ratio)
                .should_sample(
                    parent.as_ref(),
                    trace_id,
                    "request",
                    &SpanKind::Server,
                    &[],
                    &[],
                )
                .decision
        };
        assert_eq!(decide(1.0, None), SamplingDecision::RecordAndSample);
        assert_eq!(decide(0.0, None), SamplingDecision::Drop);
        assert_eq!(
            decide(0.0, Some(TraceFlags::SAMPLED)),
            SamplingDecision::RecordAndSample
        );
        assert_eq!(
            decide(1.0, Some(TraceFlags::default())),
            SamplingDecision::Drop
        );
    }

    // r[verify telemetry.endpoints]
    #[test]
    fn signal_endpoints_override_the_base() {
        let mut otlp = Otlp::default();
        assert_eq!(endpoint(&otlp, None, "/v1/metrics"), None);
        otlp.endpoint = Some("http://collector:4318/".into());
        assert_eq!(
            endpoint(&otlp, None, "/v1/metrics").as_deref(),
            Some("http://collector:4318/v1/metrics")
        );
        assert_eq!(
            endpoint(
                &otlp,
                Some("http://prometheus:9090/api/v1/otlp/v1/metrics"),
                "/v1/metrics"
            )
            .as_deref(),
            Some("http://prometheus:9090/api/v1/otlp/v1/metrics")
        );
        otlp.protocol = OtlpProtocol::Grpc;
        otlp.endpoint = Some("http://collector:4317".into());
        assert_eq!(
            endpoint(&otlp, None, "/v1/metrics").as_deref(),
            Some("http://collector:4317")
        );
    }
}
