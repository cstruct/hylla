<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/hylla-mark-dark.svg">
    <img src="assets/hylla-mark.svg" alt="" width="112">
  </picture>
</p>

# hylla

A caching layer in front of object storage (S3 and Azure Blob Storage), with a
hybrid RAM and disk cache. Configured with [ordna](https://github.com/cstruct/ordna).

> **Note on LLM use:** large parts of this project were written with an LLM, under human
> direction. The design and review are human.

## Configuration

Servers accept requests and route them to backends by path prefix; the first matching
mapping wins. Backend credentials are secret and go in a separate secrets file.

```yaml
# config.yaml
servers:
  public:
    listen: 0.0.0.0:8080
    server_names: [cache.example.com, cdn.example.com]
    mappings:
      - path_prefix: /assets
        backend: assets
        rewrites:             # regex replacements, applied in order
          - pattern: ^/assets/(.*)$
            replace: /$1      # /assets/app.css → /app.css on the backend
      - path_prefix: /tenant
        backend: assets
        auth:                 # optional: require a JWT
          verifier: partners
          paths: ['/tenant/${sub}']   # optional: backend prefixes with claims filled in
          private: true       # Cache-Control: private (default)
      - path_prefix: /
        backend: assets
telemetry:                    # optional: export over OTLP
  service_name: hylla
  resource:
    - {key: deployment.environment, value: production}
  otlp:
    endpoint: http://collector:4318   # or the OTEL_EXPORTER_OTLP_* environment variables
    protocol: http_protobuf   # or grpc
  metrics:
    enabled: true
    endpoint: http://prometheus:9090/api/v1/otlp/v1/metrics   # optional, per signal
    interval: 30s
  traces:
    enabled: true
    endpoint: http://quickwit:7280/api/v1/otlp/v1/traces   # optional, per signal
    sample_ratio: 0.1   # callers' sampled traces are always continued
    record_paths: false   # paths can be sensitive
  logs:
    level: info   # or RUST_LOG
    format: json   # or text; JSON lines carry trace and span IDs
    export: true   # over OTLP too
    endpoint: http://quickwit:7280/api/v1/otlp/v1/logs   # optional, per signal
cluster:                      # optional: share with other hylla nodes
  listen: 0.0.0.0:7946        # private network only: gossip and data between nodes
  advertise: 10.0.0.5:7946    # how the others reach this node
  seeds: [hylla-headless.cache.svc.cluster.local:7946]   # addresses or DNS names
  share:
    metadata: true            # push revalidated metadata to the other nodes
    preload: true             # one node runs each refresh rule; the others pull from it
auth:                         # JWT verifiers, by name
  partners:
    algorithm: ES256          # RS256..512, PS256..512, ES256, ES384 or EdDSA
    public_key: |
      -----BEGIN PUBLIC KEY-----
      MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE...
      -----END PUBLIC KEY-----
    audience: hylla           # optional; required in tokens when set (so is issuer)
    leeway: 30s               # clock skew allowed for exp and nbf
backends:
  assets:
    type: s3                  # s3 or azure_blob
    bucket: assets
    region: eu-north-1
    endpoint: http://localhost:9000   # optional, for S3 compatible stores
    refresh:                  # optional background refresh of this backend's objects
      concurrency: 4          # objects refreshed at the same time
      rules:
        - prefix: css/        # backend keys; directory-like, '' for everything
          every: 15m
          mode: revalidate    # disabled, revalidate (default), preload or both
        - key: index.html     # or a single key
          every: 10m
          mode: both
          max_objects: 1      # preloading needs max_objects and/or max_mib
cache:                        # optional, every setting has a default
  chunk_size_kib: 1024        # objects are cached in chunks of this size
  metadata_ttl: 30s           # how long size, ETag and content type are cached
  stale_while_revalidate: 0s  # past the TTL, keep serving this long while revalidating in
                              # the background, so requests don't wait; 0s disables
  stale_if_error: 1d          # keep serving cached objects this long past their TTL
                              # while the backend fails; 0s disables
  write_policy: on_eviction   # or on_insertion: when chunks are written to disk
  memory:
    capacity_mib: 256
  disk:                       # optional; without it only memory is used
    path: /var/cache/hylla
    capacity_mib: 10240
    block_size_mib: 16        # foyer disk blocks; a chunk must fit in one
```

```yaml
# secrets.yaml
cluster:
  key: replace-with-a-long-random-string  # shared by the nodes, at least 32 characters
backends:
  assets:
    access_key_id: AKIA...
    secret_access_key: ...
```

Any public value can be overridden with environment variables, such as
`HYLLA__SERVERS__PUBLIC__LISTEN=0.0.0.0:9090`. See [crates/config](crates/config).

## Running

Download a binary for Linux (x86_64 or aarch64, glibc 2.35 or later) or macOS (Apple
silicon) from the [releases](https://github.com/cstruct/hylla/releases), checked against
their `SHA256SUMS`, or build it:

```bash
cargo run --release -p hylla-server -- --config config.yaml --secrets secrets.yaml
```

Requests are matched to a server by their `Host` header:

```bash
curl -H "Host: cache.example.com" http://localhost:8080/assets/app.css
```

GET and HEAD are supported, including single byte ranges and `If-None-Match`. Mappings
with `auth` require an `Authorization: Bearer` JWT signed with the verifier's key, current
by its `exp` and `nbf`, and with `paths`, under a prefix built from its claims. Refresh rules
revalidate cached objects, so changes are picked up without requests waiting on the
backend, and preload objects before anyone asks for them. The cache is a
[foyer](https://github.com/foyer-rs/foyer) hybrid cache: objects are stored in chunks keyed
by their ETag, in memory and, when configured, on disk, where they survive restarts.

With a `cluster`, nodes find each other by gossip through their seeds; a DNS name
returning every node, such as a Kubernetes headless service, works as a seed. Each refresh
rule runs on one node, metadata fetched from a backend is pushed to the other nodes, and the
others pull what a node preloads directly from it, so the backend serves it once per cluster.
Everything between nodes is signed with the shared key.

### Docker

Images for `linux/amd64` and `linux/arm64` are on Docker Hub as
[`cstruct/hylla`](https://hub.docker.com/r/cstruct/hylla): `latest` and versions such as
`0.1.0` and `0.1` from releases, and `edge` from `main`. The config is read from
`/etc/hylla/config.yaml`, and a disk cache at `/var/cache/hylla` can be kept on a volume:

```bash
docker run -p 8080:8080 \
  -v ./config.yaml:/etc/hylla/config.yaml:ro \
  -v ./secrets.yaml:/etc/hylla/secrets.yaml:ro \
  -v hylla-cache:/var/cache/hylla \
  cstruct/hylla --config /etc/hylla/config.yaml --secrets /etc/hylla/secrets.yaml
```

hylla runs as the distroless `nonroot` user (65532), so a directory mounted for the disk
cache must be writable by it. A new named volume is.

## Testing

```bash
cargo test
```

The end-to-end tests run the `hylla` binary against [VersityGW](https://github.com/versity/versitygw)
(S3) and [Azurite](https://github.com/Azure/Azurite) (Azure Blob Storage), and three nodes
sharing over a cluster. [mise](https://mise.jdx.dev) installs both tools, pinned in
[mise.toml](mise.toml):

```bash
mise run e2e
```

The behaviour is specified in [docs/spec](docs/spec) and tracked with
[tracey](https://github.com/bearcove/tracey).

```bash
tracey query status
```

## Developing with ordna

hylla's config is built on [ordna](https://github.com/cstruct/ordna), taken from crates.io.
To build against a checkout of ordna next to this one instead, while changing both, patch it
in with [.cargo/ordna-local.toml](.cargo/ordna-local.toml):

```bash
ln -s ordna-local.toml .cargo/config.toml
```

`.cargo/config.toml` is ignored by git. While it's enabled, Cargo.lock records the local
packages, so remove the link before committing changes to it.

## Releasing

hylla is versioned as one, by `version` in the workspace's [Cargo.toml](Cargo.toml). Bump
it, then tag the commit with the version:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The [release workflow](.github/workflows/release.yml) checks the tag against the version
and runs CI. It then builds the binaries, pushes the Docker image, and creates a GitHub
release with the binaries, their checksums and notes generated from the pull requests
merged since the last one.
