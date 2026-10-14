//! The metrics hylla records, with the global meter provider.
//!
//! Attributes are kept low-cardinality: servers, mappings by path prefix, backends and
//! outcomes, never object keys or request paths.

use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram, Meter},
};

/// The meter the instruments are created with, from the global provider at first use.
pub fn meter() -> Meter {
    global::meter("hylla")
}

pub struct Metrics {
    pub requests: Histogram<f64>,
    pub served_bytes: Counter<u64>,
    pub chunks: Counter<u64>,
    pub metadata: Counter<u64>,
    pub backend_requests: Histogram<f64>,
    pub backend_bytes: Counter<u64>,
    pub refresh_objects: Counter<u64>,
    pub auth_refusals: Counter<u64>,
    pub cluster_updates: Counter<u64>,
    pub cluster_pulled_chunks: Counter<u64>,
    pub cluster_gossip_failures: Counter<u64>,
}

// r[impl telemetry.metrics.instruments]
/// The instruments, created from the global meter provider at first use, so telemetry must
/// be set up before the app records anything.
pub static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let meter = meter();
    Metrics {
        requests: meter
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_description("Duration of HTTP requests")
            .build(),
        served_bytes: meter
            .u64_counter("hylla.served.bytes")
            .with_unit("By")
            .with_description("Object bytes sent to clients")
            .build(),
        chunks: meter
            .u64_counter("hylla.cache.chunks")
            .with_description("Chunk lookups, by whether they hit the cache")
            .build(),
        metadata: meter
            .u64_counter("hylla.cache.metadata")
            .with_description("Metadata lookups, by how they were answered")
            .build(),
        backend_requests: meter
            .f64_histogram("hylla.backend.request.duration")
            .with_unit("s")
            .with_description("Duration of requests to backends")
            .build(),
        backend_bytes: meter
            .u64_counter("hylla.backend.bytes")
            .with_unit("By")
            .with_description("Chunk bytes fetched from backends")
            .build(),
        refresh_objects: meter
            .u64_counter("hylla.refresh.objects")
            .with_description("Objects handled by refresh runs, by outcome")
            .build(),
        auth_refusals: meter
            .u64_counter("hylla.auth.refusals")
            .with_description("Requests refused by auth, by reason")
            .build(),
        cluster_updates: meter
            .u64_counter("hylla.cluster.updates")
            .with_description("Metadata and preload updates sent to and received from nodes")
            .build(),
        cluster_pulled_chunks: meter
            .u64_counter("hylla.cluster.pulled_chunks")
            .with_description("Chunks of preloaded objects pulled, by where from")
            .build(),
        cluster_gossip_failures: meter
            .u64_counter("hylla.cluster.gossip.failures")
            .with_description("Failed gossip exchanges with other nodes")
            .build(),
    }
});

/// Records how a request to a backend went.
pub fn backend_request(
    backend: &str,
    operation: &'static str,
    started: Instant,
    outcome: &'static str,
) {
    METRICS.backend_requests.record(
        seconds(started.elapsed()),
        &[
            KeyValue::new("hylla.backend", backend.to_string()),
            KeyValue::new("hylla.operation", operation),
            KeyValue::new("hylla.outcome", outcome),
        ],
    );
}

/// The outcome of a backend request, for [`backend_request`].
pub fn outcome<T>(result: &object_store::Result<T>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(object_store::Error::NotFound { .. }) => "not_found",
        Err(object_store::Error::Precondition { .. }) => "precondition_failed",
        Err(object_store::Error::NotModified { .. }) => "not_modified",
        Err(_) => "error",
    }
}

pub fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}
