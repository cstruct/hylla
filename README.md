# hylla

A caching layer in front of object storage (S3, Swift and Azure Blob Storage), with a
hybrid RAM and disk cache. Configured with [ordna](../ordna).

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
      - path_prefix: /
        backend: assets
backends:
  assets:
    type: s3                  # s3 (swift and azure_blob are configurable but not served yet)
    bucket: assets
    region: eu-north-1
    endpoint: http://localhost:9000   # optional, for S3 compatible stores
cache:                        # optional, every setting has a default
  chunk_size_kib: 1024        # objects are cached in chunks of this size
  metadata_ttl_seconds: 30    # how long size, ETag and content type are cached
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
backends:
  assets:
    access_key_id: AKIA...
    secret_access_key: ...
```

Any public value can be overridden with environment variables, such as
`HYLLA__SERVERS__PUBLIC__LISTEN=0.0.0.0:9090`. See [crates/config](crates/config).

## Running

```bash
cargo run --release -p hylla-server -- --config config.yaml --secrets secrets.yaml
```

Requests are matched to a server by their `Host` header:

```bash
curl -H "Host: cache.example.com" http://localhost:8080/assets/app.css
```

GET and HEAD are supported, including single byte ranges and `If-None-Match`. The cache is a
[foyer](https://github.com/foyer-rs/foyer) hybrid cache: objects are stored in chunks keyed
by their ETag, in memory and, when configured, on disk, where they survive restarts.

The behaviour is specified in [docs/spec](docs/spec) and tracked with
[tracey](https://github.com/bearcove/tracey).

```bash
tracey query status
```
