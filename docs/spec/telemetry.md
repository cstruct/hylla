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
