# Observability

hylla exports metrics, traces and logs over OTLP, to any collector or backend that accepts
it, such as an OpenTelemetry Collector, Prometheus or Quickwit. Each signal is off until
enabled, except logs to stderr.

```styx
telemetry {
  service_name hylla
  resource ({key deployment.environment, value production})
  otlp {
    endpoint http://collector:4318  // or the OTEL_EXPORTER_OTLP_* variables
    protocol http_protobuf          // or grpc
  }
  metrics {enabled true, interval 30s}
  traces {enabled true, sample_ratio 0.1}
  logs {format json, export true}
}
```

Every signal carries hylla's version as `service.version`, which it also logs at startup.

Over HTTP, the signal's path, such as `/v1/metrics`, is added to `otlp.endpoint`. Each
signal can go elsewhere with its own `endpoint`, used as is, such as Prometheus' OTLP
receiver at `http://prometheus:9090/api/v1/otlp/v1/metrics`. API keys go in `otlp.headers`,
in the secrets file.

For a single run, `--otlp-endpoint`, `--metrics` and `--traces` turn things on without
changing the config.

## Metrics

Attributes never include object keys or request paths, so cardinality stays low.

| Metric | Attributes | |
|---|---|---|
| `http.server.request.duration` | method (`_OTHER` for other than `GET` and `HEAD`), status code, `hylla.server`, `hylla.mapping` (its prefix) | Every request, until its body is sent |
| `hylla.served.bytes` | `hylla.backend` | Object bytes sent to clients |
| `hylla.cache.chunks` | `hylla.backend`, `hylla.result`: `hit`, `miss`, `error`, `uncacheable` | Chunk lookups |
| `hylla.cache.metadata` | `hylla.backend`, `hylla.result`: `fresh`, `fetched`, `stale_while_revalidate`, `stale_if_error`, `not_found`, `error` | Metadata lookups |
| `hylla.backend.request.duration` | `hylla.backend`, `hylla.operation`, `hylla.outcome` | Requests to backends |
| `hylla.backend.bytes` | `hylla.backend` | Chunk bytes fetched from backends |
| `hylla.refresh.objects` | `hylla.backend`, `hylla.rule`, `hylla.result` | Objects refresh runs handled, by what they did |
| `hylla.auth.refusals` | `hylla.reason`: `missing_token`, `invalid_token`, `forbidden`, `dot_segments`, `keys_unavailable`, and for Konklav, `no_rule`, `step_up`, `decision_unavailable` | Requests refused by auth or Konklav |
| `hylla.cluster.members` | | Live nodes, this one included |
| `hylla.cluster.updates` | `hylla.direction`, `hylla.kind` | Metadata, preload and forget updates sent and received |
| `hylla.cluster.pulled_chunks` | `hylla.source`: `peer` or `backend` | Chunks of preloaded objects pulled |
| `hylla.cluster.gossip.failures` | | Failed gossip exchanges |
| `hylla.cluster.dropped` | `hylla.kind` | Updates and pulls dropped because their queue was full |

[foyer](https://github.com/foyer-rs/foyer)'s own cache metrics are exported too. Prometheus
turns dots into underscores, and adds `_total` to counters: `hylla_cache_chunks_total`.

Worth watching:

- The hit ratio: `hylla.cache.chunks` with `hit`, over all chunk lookups.
- Backend errors: `hylla.backend.request.duration` with outcome `error`, and
  `hylla.cache.metadata` with `stale_if_error`, which means clients got cached objects
  while the backend failed.
- `hylla.cluster.members` below the number of nodes you run.

## Traces

Every request is a server span named by its method and mapping, such as `GET /assets`,
failed when it answers with a server error. Inside it are spans for metadata and chunk
lookups, and a client span for every backend request. Refresh runs, background
revalidations, and sharing between cluster nodes are traced too, across nodes. Gossip
isn't.

- With `follow_callers`, requests with a W3C `traceparent` header continue the caller's
  trace, and follow its sampling decision. It's off by default, since anyone can send one.
  Requests between nodes continue their traces either way. Of the traces hylla starts,
  `sample_ratio` are sampled.
- Paths aren't recorded in spans unless `record_paths` is on, since they can tell more than
  they should, such as who a tenant is.

## Logs

Logs go to stderr, filtered by `logs.level` (`info` by default) or `RUST_LOG`, such as
`info,hylla_server=debug`. In the `json` format, each event is one JSON object on a line,
with the trace and span IDs when it's traced. With `export`, logs are sent over OTLP too.

Targets worth knowing:

- `hylla::audit`: changes made through the control plane, and failed sign-ins, see
  [Control plane](control-plane.md#safety). They're kept at `info` whatever `logs.level`
  says, unless it names the target, such as `warn,hylla::audit=off`.
- `hylla_server`: everything else hylla logs: listeners starting, backend failures, refresh
  runs, cluster problems.
