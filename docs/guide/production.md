# Running in production

## Installing

- **Binaries** for Linux (x86_64 and aarch64, glibc 2.35 or later) and macOS (Apple
  silicon) are on the [releases](https://github.com/cstruct/hylla/releases) page. Check them
  against the release's `SHA256SUMS`.
- **Docker images** for `linux/amd64` and `linux/arm64` are on Docker Hub as
  [`cstruct/hylla`](https://hub.docker.com/r/cstruct/hylla), tagged `latest` and by version,
  such as `0.1.0` and `0.1`.
- **From source**: `cargo build --release -p hylla-server` builds `target/release/hylla`.
  Run `mise run ui` first to embed the control plane UI's hydration bundle, which the
  releases and images have.

## Docker

The image runs `hylla --config /etc/hylla/config.styx` as the distroless `nonroot` user
(65532), without a shell:

```bash
docker run --stop-timeout 30 -p 8080:8080 -p 8081:8081 \
  -v ./config.styx:/etc/hylla/config.styx:ro \
  -v ./secrets.styx:/etc/hylla/secrets.styx:ro \
  -v hylla-cache:/var/cache/hylla \
  cstruct/hylla --config /etc/hylla/config.styx --secrets /etc/hylla/secrets.styx
```

A disk cache at `/var/cache/hylla` must be writable by user 65532. A new named volume is.

`--stop-timeout 30` gives hylla longer to stop than its `http.shutdown_timeout` of 20
seconds, so it writes its memory cache to disk before Docker kills it, which it does after 10
seconds by default. With Compose, set `stop_grace_period: 30s`.

## Sizing the cache

- **Memory**: `cache.memory.capacity_mib` holds the hottest chunks. The process needs some
  room beyond it, so give the container a memory limit a few hundred MiB above it.
- **Disk**: `cache.disk.capacity_mib` holds everything else, and survives restarts. Give the
  volume a little more than the capacity.
- **Chunks**: objects are cached in `chunk_size_kib` chunks (1 MiB by default), and a range
  request reads only the chunks it covers. Smaller chunks waste less on small ranges; larger
  ones mean fewer backend requests for large objects. A chunk must fit in a disk block
  (`block_size_mib`, 16 MiB by default).
- `write_policy on_insertion` writes chunks to disk as soon as they're cached, so a restart
  keeps more of the cache, at the cost of disk writes. The default, `on_eviction`, writes
  them when they leave memory.

## Freshness

hylla trusts an object's metadata (size, ETag, content type) for `metadata_ttl`, 30 seconds
by default, and then checks it with the backend before serving it. Chunks are cached by ETag,
so a changed object is fetched again rather than served stale.

- `stale_while_revalidate` serves past the TTL without waiting, checking in the background.
- `stale_if_error` keeps serving cached objects past the TTL while the backend fails, for a
  day by default. A failed request to the backend is retried `timeouts.retries` times (2),
  within `timeouts.request` (30 seconds), before the cached object is served. For a second
  after that, or the metadata TTL if longer, the cached object is served without asking the
  backend again.
- Requests for the same object waiting on the backend share one request to it.
- Missing objects are remembered for `not_found_ttl` (10 seconds), so requests for them
  don't all reach the backend.
- Objects keep the `Cache-Control`, `Content-Encoding`, `Content-Disposition` and
  `Content-Language` they were stored with. A mapping's `cache_control` sets
  `Cache-Control` for objects without one, and responses carry an `Age`.
- Refresh rules check cached objects on a schedule, so they rarely need checking when
  they're requested, and preload objects before anyone asks for them.

## Health checks and shutdown

With a `health` listener, point load balancers and orchestrators at it:

- `/healthz` answers 200 while hylla runs, also while it shuts down. Use it for liveness.
- `/readyz` answers 200 once every listener serves, and 503 once shutdown starts. Use it for
  readiness, so a node leaves the load balancer before its connections close.

On SIGTERM or Ctrl-C, hylla stops accepting connections, gives requests in flight
`http.shutdown_timeout` (20 seconds by default) to finish, closes the connections still
open, and writes its memory cache to disk. Give it longer than the shutdown timeout to stop,
such as Kubernetes' `terminationGracePeriodSeconds`, or it's killed before the cache is
written.

## Limits

The `http` section applies to every listener:

- `header_read_timeout` (10 seconds) closes connections that don't send their headers in
  time.
- `idle_timeout` (60 seconds) closes connections that send and receive nothing, while no
  request on them is in flight.
- `max_connections` (10000 per listener) leaves further connections waiting to be accepted.

Backends' `timeouts` (5 seconds to connect, 30 seconds per request) keep a backend that
hangs from holding requests forever.

## TLS

hylla's public listeners speak plain HTTP; put a load balancer or proxy that terminates TLS
in front of them. Between hylla nodes, mutual TLS is built in: see [Clustering](clustering.md).

## Kubernetes

[examples/cluster-kubernetes](../../examples/cluster-kubernetes) is a starting point: a
StatefulSet with a disk cache per pod, probes on the health listener, a
PodDisruptionBudget, and certificates from cert-manager.

## Upgrading

- hylla is versioned as one. Read the release notes before upgrading.
- The nodes of a cluster must be upgraded together when the protocol between them changes,
  as it did when nodes started signing their answers. Nodes that don't speak the same
  protocol refuse each other.
- The disk cache's format comes from [foyer](https://github.com/foyer-rs/foyer). If a new
  version can't read it, delete the cache directory: it's a cache, and fills again.
