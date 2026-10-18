//! Tracing spans: propagating trace context over HTTP, and the spans of backend requests.
//!
//! Spans are exported as OpenTelemetry spans when traces are enabled, with their `otel.*`
//! fields setting the span's name, kind and status.

use std::time::Instant;

use http::{HeaderMap, HeaderName, HeaderValue};
use object_store::path::Path as ObjectPath;
use opentelemetry::{
    Context, global,
    propagation::{Extractor, Injector},
    trace::TraceContextExt,
};
use tracing::{Instrument, Span, field::Empty};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::{metrics, telemetry};

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(HeaderName::as_str).collect()
    }
}

struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(key),
            HeaderValue::try_from(value.as_str()),
        ) {
            self.0.insert(name, value);
        }
    }
}

// r[impl telemetry.traces.propagation]
/// Continues the trace a request's `traceparent` header names, if any, in `span`.
pub fn continue_trace(span: &Span, headers: &HeaderMap) {
    let parent: Context =
        global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)));
    // Fails only without traces, when there's nothing to continue.
    let _ = span.set_parent(parent);
}

/// The trace context headers of the current span, for a request to another node.
pub fn trace_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    let context = Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderInjector(&mut headers));
    });
    headers
}

// r[impl telemetry.traces.paths]
/// Records the object `path` in `span`'s `hylla.object` field, when paths are recorded.
pub fn record_object(span: &Span, path: &str) {
    if telemetry::record_paths() {
        span.record("hylla.object", path);
    }
}

/// Renames `span`, which `otel.name` can't once it started.
pub fn rename(span: &Span, name: String) {
    span.context().span().update_name(name);
}

/// Marks `span` as failed.
pub fn record_error(span: &Span) {
    span.record("otel.status_code", "error");
}

// r[impl telemetry.traces.spans]
/// Runs a request to a backend in a span, recording its duration and outcome in the
/// metrics and the backend's status too.
pub async fn backend_call<T>(
    backend: &str,
    operation: &'static str,
    path: Option<&ObjectPath>,
    call: impl Future<Output = object_store::Result<T>>,
) -> object_store::Result<T> {
    let span = tracing::info_span!(
        "backend",
        otel.name = format!("backend {operation}"),
        otel.kind = "client",
        otel.status_code = Empty,
        hylla.backend = backend,
        hylla.operation = operation,
        hylla.outcome = Empty,
        hylla.object = Empty,
    );
    if let Some(path) = path {
        record_object(&span, path.as_ref());
    }
    let started = Instant::now();
    let at = std::time::SystemTime::now();
    let result = call.instrument(span.clone()).await;
    let outcome = metrics::outcome(&result);
    metrics::backend_request(backend, operation, started, outcome);
    crate::status::record(
        backend,
        crate::status::Call {
            at,
            operation,
            outcome,
            duration: started.elapsed(),
            error: match &result {
                Err(error) if outcome == "error" => Some(error.to_string()),
                _ => None,
            },
        },
    );
    span.record("hylla.outcome", outcome);
    if outcome == "error" {
        record_error(&span);
    }
    result
}
