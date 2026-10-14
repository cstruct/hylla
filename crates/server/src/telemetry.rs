//! Exporting telemetry over OTLP.

use std::collections::HashMap;

use anyhow::Context;
use hylla_config::{Otlp, OtlpProtocol};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{
    MetricExporter, Protocol, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
};

/// The telemetry providers, flushed and shut down by [`Telemetry::shutdown`].
pub struct Telemetry {
    meter_provider: Option<SdkMeterProvider>,
}

// r[impl telemetry.init]
/// Sets up exporting telemetry as configured, installing the global providers.
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
            .with_resource(resource)
            .with_reader(reader)
            .build();
        global::set_meter_provider(provider.clone());
        Some(provider)
    } else {
        None
    };
    Ok(Telemetry { meter_provider })
}

impl Telemetry {
    /// Exports what's pending now.
    pub fn flush(&self) {
        if let Some(provider) = &self.meter_provider
            && let Err(error) = provider.force_flush()
        {
            tracing::warn!("flushing metrics failed: {error}");
        }
    }

    /// Exports what's pending and stops exporting.
    pub fn shutdown(&self) {
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
    let timeout = otlp.timeout.duration();
    Ok(match otlp.protocol {
        OtlpProtocol::HttpProtobuf => {
            let mut builder = MetricExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_timeout(timeout);
            if !otlp.headers.is_empty() {
                builder = builder.with_headers(otlp.headers.clone());
            }
            if let Some(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build()?
        }
        OtlpProtocol::Grpc => {
            let mut builder = MetricExporter::builder()
                .with_tonic()
                .with_protocol(Protocol::Grpc)
                .with_timeout(timeout);
            if !otlp.headers.is_empty() {
                builder = builder.with_metadata(metadata(&otlp.headers)?);
            }
            if let Some(endpoint) = endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build()?
        }
    })
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

    use super::endpoint;

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
