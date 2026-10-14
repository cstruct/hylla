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
        backend: archive
backends:
  assets:
    type: s3                  # s3, swift or azure_blob
    bucket: assets
    region: eu-north-1
  archive:
    type: swift
    auth_url: https://swift.example.com/v3
    container: archive
    username: hylla
```

```yaml
# secrets.yaml
backends:
  assets:
    access_key_id: AKIA...
    secret_access_key: ...
  archive:
    password: ...
```

Any public value can be overridden with environment variables, such as
`HYLLA__SERVERS__PUBLIC__LISTEN=0.0.0.0:9090`. See [crates/config](crates/config).

The behaviour is specified in [docs/spec](docs/spec) and tracked with
[tracey](https://github.com/bearcove/tracey).

```bash
tracey query status
```
