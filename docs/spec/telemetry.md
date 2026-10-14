# Telemetry

hylla exports telemetry over OTLP, to any collector or backend that accepts it, such as an
OpenTelemetry Collector, Prometheus or Quickwit.

## Exporting

r[telemetry.init]
Telemetry MUST be set up from the config before the app is built, installing the providers
of the enabled signals globally, and MUST be flushed and shut down when hylla stops.

r[telemetry.endpoints]
A signal MUST be exported to its own endpoint when configured, used as is, and otherwise to
the OTLP endpoint, with the signal's path (such as `/v1/metrics`) appended over HTTP. Without
either, the standard `OTEL_EXPORTER_OTLP_*` environment variables MUST apply.

## Metrics

r[telemetry.metrics]
When metrics are enabled, they MUST be exported periodically over OTLP, with the configured
service name and resource attributes, and the configured headers.

r[telemetry.metrics.instruments]
Metric attributes MUST be low-cardinality: servers, mappings by path prefix, backends,
operations and outcomes, never object keys or request paths.

r[telemetry.metrics.requests]
The duration of every request MUST be recorded as `http.server.request.duration`, with its
method, status code, and the server and mapping that served it, when any.

r[telemetry.metrics.cache]
Chunk lookups MUST be counted by whether they hit the cache, and metadata lookups by how they
were answered, along with the duration and outcome of every backend request and the bytes
fetched from backends and served to clients.

r[telemetry.metrics.foyer]
foyer's own cache metrics MUST be exported along with hylla's.

r[telemetry.metrics.refresh]
The objects refresh runs handle MUST be counted by backend, rule and outcome.

r[telemetry.metrics.cluster]
The live members of the cluster MUST be reported, and updates sent and received, chunks pulled
from other nodes or backends, and failed gossip exchanges counted.

## Traces

r[telemetry.traces]
When traces are enabled, spans MUST be exported over OTLP in batches, with the configured
service name, resource attributes and headers, and logging MUST NOT filter them away.

r[telemetry.traces.sampling]
The configured share of traces hylla starts MUST be sampled, and traces continued from a
caller MUST follow the caller's sampling decision.

r[telemetry.traces.propagation]
Requests MUST continue the trace their W3C `traceparent` header names, if any.

r[telemetry.traces.requests]
Every request MUST be a server span named by its method and mapping, recording its status
code, server and mapping, failed when it answers with a server error, and lasting until its
body is sent.

r[telemetry.traces.spans]
Metadata and chunk lookups, background revalidations and refresh runs MUST be spans, and so
MUST every backend request, as a client span recording its outcome and failed when the
backend fails.

r[telemetry.traces.paths]
Request and object paths MUST only be recorded in spans when configured to.

r[telemetry.traces.cluster]
Sharing updates with other nodes, receiving them, pulling preloaded objects and serving
chunks to other nodes MUST be spans of one trace, carried between nodes in `traceparent`
headers. Gossip MUST NOT be traced.

## Logs

r[telemetry.logs]
Logs MUST be written to stderr, filtered by the configured level, or by `RUST_LOG` when set,
and an invalid level MUST be refused.

r[telemetry.logs.json]
In the JSON format, every event MUST be one JSON object on a line of its own, with its time,
level, target, message and fields, the spans it's in, and the trace and span IDs when traced.

r[telemetry.logs.export]
When exported, logs MUST be sent over OTLP in batches with the trace context of the span they
were logged in, filtered by the same level, leaving out what the exporters themselves log.
