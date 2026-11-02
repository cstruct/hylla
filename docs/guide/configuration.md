# Configuration

hylla is configured with a config file, a secrets file, environment variables and command
line flags. This page explains how they combine, and lists every setting.

## Files and precedence

The config is written in [Styx](https://github.com/bearcove/styx). A section is a name
followed by `{...}`, a list is `(...)`, and entries are separated by newlines or commas:

```styx
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com cdn.example.com)
    mappings ({path_prefix /, backend assets})
  }
}
```

Values come from four places. Each one overrides the ones before it:

1. **The config file**, `--config`. Without the flag, `config.styx` in the working
   directory is read if there is one. A file given with `--config` must exist.
2. **The secrets file**, `--secrets`. Credentials, keys and password hashes can only come
   from here, so the config file can be shared and committed without them.
3. **Environment variables**, `HYLLA__` followed by the path to a setting, with `__`
   between its parts, such as `HYLLA__CACHE__METADATA_TTL=5m` or
   `HYLLA__SERVERS__PUBLIC__LISTEN=0.0.0.0:9090`. Secrets can't be set this way.
4. **Command line flags**, for a few settings: `--log-level`, `--log-format`,
   `--otlp-endpoint`, `--metrics`, `--traces` and `--cache-memory-mib`. `hylla --help`
   lists them.

Durations are written like `90s`, `15m`, `1h 30m` or `1d`.

To see the config hylla would run with, and where each value comes from:

```bash
hylla config show --config config.styx --secrets secrets.styx
hylla config explain cache.metadata_ttl
```

Secrets are shown as `"<redacted>"`. A config that doesn't load is shown as far as it got,
with the required fields that nothing set marked `"<missing>"`, before the command fails
with every problem it found. hylla checks the same things at startup and refuses to start
on any of them.

## Settings

Settings marked **secret** can only be set in the secrets file. Others have the default
shown, or are required when there's none.

### `servers`

Servers by name. A request goes to the server on the address it came in on whose
`server_names` contain its `Host` header, compared without case or port; other hosts get
404. Several servers can share a listen address.

A server name can be a wildcard, such as `*.preview.example.com`, matching exactly one label
in front of it: `pr1873.preview.example.com`, but not `preview.example.com` or
`a.b.preview.example.com`. Exact names win over wildcards, so `www.example.com` and
`*.example.com` can share an address. [Sites](#sites) choose their build by that label.

| Setting | Default | |
|---|---|---|
| `listen` | required | The address and port, such as `0.0.0.0:8080` |
| `server_names` | required | The host names it answers to. The first is its public host |
| `mappings` | required | Which paths go to which backend, in order: the first match wins |

Each mapping:

| Setting | Default | |
|---|---|---|
| `path_prefix` | required | Such as `/assets`. It matches `/assets` and `/assets/...`, not `/assets2` |
| `backend` | required | The name of a backend in `backends` |
| `rewrites` | none | Regex replacements applied in order to the path, each `{pattern ..., replace ...}`. The replacement can use `$1` or `${name}` |
| `cache_control` | none | The `Cache-Control` of objects stored without one, such as `public, max-age=300`. Objects keep their own `Cache-Control`, `Content-Encoding`, `Content-Disposition` and `Content-Language` |
| `writable` | `false` | Whether the [control plane](control-plane.md) may change its objects. Writable mappings can't have rewrites |
| `auth` | none | Requires a JWT, see below |
| `site` | none | Serves frontend builds by commit, see [sites](#sites) |

The path is percent-decoded before it's matched, rewritten and checked. The backend key is
the rewritten path without its leading `/`: without rewrites, `/assets/app.css` is the key
`assets/app.css`.

A mapping's `auth`:

| Setting | Default | |
|---|---|---|
| `verifier` | required | The name of a verifier in `auth` |
| `paths` | none | Backend path prefixes a token may read, with claims filled in, such as `/tenants/${tenant_id}`. A list claim allows a prefix for each item: `/teams/${groups}` allows every group's. Without any, a valid token reads anything the mapping serves |
| `private` | `true` | Whether responses are marked `Cache-Control: private` |

#### Sites

A mapping with a `site` serves static frontend builds, one per commit, rather than mapping
paths to keys. CI uploads each build under its commit's full SHA, and points aliases such as
`pr1873` or `live` at builds by writing the SHA into a small object:

```text
builds/3f2a…e1/index.html       a build, never changed once uploaded
builds/3f2a…e1/assets/app-4b1c.js
aliases/pr1873                  body: 3f2a…e1
aliases/live                    body: 9c07…d4
```

On a server with a wildcard name, the label in front of it chooses the build:
`3f2a…e1.preview.example.com` serves that build, and `pr1873.preview.example.com/settings`
redirects with 307 to `//3f2a…e1.preview.example.com/settings`, so a browser stays on one
build for the whole session while the alias moves. A site with `alias` always serves that
alias, without redirecting, as for the live site.

Aliases are objects, read through the cache like any other, so moving one needs no config
reload: it's picked up within `metadata_ttl`, or at once on every node when written through
a writable mapping of the [control plane](control-plane.md). Builds and aliases are written
through such a mapping, or straight to the bucket; site mappings themselves can't be
writable or listed.

| Setting | Default | |
|---|---|---|
| `builds` | `builds/` | The key prefix builds are stored under |
| `aliases` | `aliases/` | The key prefix aliases are stored under |
| `alias` | none | The alias always served, such as `live`. Without it, the server needs a wildcard name |
| `index` | `index.html` | The object served for paths ending with `/` |
| `fallback` | none | The object served for page loads of paths the build doesn't have, such as `index.html` for single page apps. Scripts, images and other requests still get 404 |
| `redirect` | `true` | Whether aliases in the host redirect to the build's SHA host |
| `alias_cache_control` | `no-cache` | The `Cache-Control` of responses served through an alias without redirecting, replacing the object's own |

SHAs are 40 or 64 lowercase hexadecimal characters. Alias names are 1 to 63 lowercase
letters, digits and `-`, so each can be a host label. Upload builds with a long
`Cache-Control`, such as `public, max-age=31536000, immutable`: SHA hosts serve it as is.
[examples/sites](../../examples/sites) puts it together.

```styx
servers {
  previews {
    listen 0.0.0.0:8080
    server_names (*.preview.example.com)
    mappings ({path_prefix /, backend frontend, site {fallback index.html}})
  }
  live {
    listen 0.0.0.0:8080
    server_names (www.example.com)
    mappings ({path_prefix /, backend frontend, site {alias live, fallback index.html}})
  }
}
```

### `auth`

JWT verifiers by name, used by mappings' `auth`. Tokens come in an
`Authorization: Bearer` header, and must have an `exp` that hasn't passed.

| Setting | Default | |
|---|---|---|
| `algorithm` | required | One of `RS256`, `RS384`, `RS512`, `PS256`, `PS384`, `PS512`, `ES256`, `ES384` or `EdDSA` |
| `public_key` | none | The PEM encoded public key |
| `jwks_uri` | none | Where the provider publishes its keys, for providers without a discovery document |
| `issuer` | none | When set, tokens must have this `iss`. Without `public_key` or `jwks_uri`, keys are read through `{issuer}/.well-known/openid-configuration` |
| `audience` | none | When set, tokens must have this `aud`. Without it, any token signed with the key is accepted, so hylla warns |
| `leeway` | `30s` | Clock skew allowed for `exp` and `nbf` |

A verifier needs one of `public_key`, `jwks_uri` or `issuer`, which must be HTTPS URLs, or
HTTP ones to this host. Redirects aren't followed. With an OpenID provider, the
`issuer` is enough:

```styx
auth {
  customers {
    algorithm ES256
    issuer https://id.example.com
    audience files.example.com
  }
}
```

Keys from a provider are read when the first token needs them, and read again after an
hour, or when a token names a key they don't have, as after the provider rotated its keys,
but not more than once every 30 seconds. While the provider can't be reached, the keys it
had are still used. Until they've been read once, requests are answered with 503.

### `backends`

Backends by name, each with a `type`.

**`s3`**, Amazon S3 or an S3 compatible store:

| Setting | Default | |
|---|---|---|
| `bucket` | required | |
| `region` | required | Such as `eu-north-1`. Any value works for most S3 compatible stores |
| `endpoint` | AWS | The store's URL, such as `http://127.0.0.1:7070`. `http://` is allowed |
| `access_key_id` | required, secret | |
| `secret_access_key` | required, secret | |

**`azure_blob`**, Azure Blob Storage:

| Setting | Default | |
|---|---|---|
| `account` | required | |
| `container` | required | |
| `endpoint` | `https://{account}.blob.core.windows.net` | Such as `http://127.0.0.1:10000/devstoreaccount1` for Azurite |
| `account_key` | required, secret | |

**`hylla`**, another hylla node serving one of its backends with `serve_backends`, over
mutual TLS. See [Clustering](clustering.md#origin-shield).

| Setting | Default | |
|---|---|---|
| `url` | required | The node's `https://` URL, such as `https://shield.internal:7947` |
| `backend` | required | The backend's name on that node |
| `tls.ca` | required | The PEM file of the CA the node's certificate is issued by |
| `tls.cert`, `tls.key` | required | This node's certificate and key, issued for the other node's `client_name` |

Every type also takes:

| Setting | Default | |
|---|---|---|
| `timeouts.connect` | `5s` | How long connecting may take |
| `timeouts.request` | `30s` | How long a whole request may take. A chunk must arrive within it, and so must a listing from a `hylla` backend |
| `timeouts.retries` | `2` | How many times requests that failed with a server error, or without an answer, are retried, with backoff, within the request timeout. S3 and Azure only |
| `refresh` | none | Background refresh, see below |

A backend's `refresh` keeps cached objects current, and the cache warm:

| Setting | Default | |
|---|---|---|
| `concurrency` | `4` | Objects refreshed at the same time |
| `rules` | required | What to refresh, and how often |

Each rule:

| Setting | Default | |
|---|---|---|
| `prefix` or `key` | one required | A directory-like key prefix such as `css/` (`""` for everything), or a single key |
| `every` | required | How often, such as `15m`. The first run is at startup |
| `mode` | `revalidate` | `revalidate` checks cached objects, `preload` fetches objects that aren't cached, `both` does both, and `disabled` nothing |
| `max_objects`, `max_mib` | none | Caps on what one run preloads; preloading needs at least one |

### `cache`

| Setting | Default | |
|---|---|---|
| `chunk_size_kib` | `1024` | Objects are cached in chunks of this size |
| `metadata_ttl` | `30s` | How long an object's size, ETag and content type are trusted before they're checked again |
| `stale_while_revalidate` | `0s` | Past the TTL, how long to keep serving while checking in the background. `0s` checks before serving |
| `stale_if_error` | `1d` | Past the TTL, how long to keep serving while the backend fails. `0s` disables it |
| `not_found_ttl` | `10s` | How long an object the backend said is missing gets 404 without asking it again. Changing it through the control plane ends it. `0s` disables it |
| `write_policy` | `on_eviction` | When chunks are written to disk: when they leave memory, or `on_insertion`, as soon as they're cached |
| `memory.capacity_mib` | `256` | |
| `disk` | none | Without it, only memory is used |
| `disk.path` | required | The directory, such as `/var/cache/hylla` |
| `disk.capacity_mib` | required | At least 4 blocks |
| `disk.block_size_mib` | `16` | A chunk must fit in one |

Objects are cached by their ETag. Objects whose backend gives no ETag are never cached,
since their versions couldn't be told apart.

### `health`

Optional health checks, on a listener of their own: `/healthz` answers 200 while hylla
runs, and `/readyz` 200 while it serves, 503 before that and once it's shutting down.

| Setting | Default | |
|---|---|---|
| `listen` | required | Such as `0.0.0.0:8081` |

### `http`

Limits of every listener: servers, the control plane, the cluster, `serve_backends` and the
health checks.

| Setting | Default | |
|---|---|---|
| `header_read_timeout` | `10s` | How long a client may take to send a request's headers |
| `idle_timeout` | `60s` | Closes connections that send and receive nothing for this long, while no request on them is in flight. `0s` disables it |
| `max_connections` | `10000` | Per listener. More wait to be accepted |
| `shutdown_timeout` | `20s` | On SIGTERM, how long requests in flight may take to finish |

### `cluster`

Sharing with other hylla nodes. See [Clustering](clustering.md).

| Setting | Default | |
|---|---|---|
| `listen` | required | The cluster listener, such as `0.0.0.0:7946`. Keep it on a private network |
| `advertise` | `listen` | How the other nodes reach this one. Required when `listen` is `0.0.0.0` or `[::]` |
| `seeds` | none | Nodes to join through, as `host:port`. Names are resolved every round, so a DNS name returning every node works |
| `gossip_interval` | `1s` | |
| `failure_timeout` | `10s` | How long a node may go unheard before it's considered gone |
| `key` | required, secret | Shared by the nodes, at least 32 characters |
| `previous_keys` | none, secret | Keys still accepted from other nodes while `key` is changed, see [Clustering](clustering.md#changing-the-key) |
| `tls` | none | Mutual TLS: `ca`, `cert` and `key` files, and the `server_name` every node's certificate is issued for |
| `share.metadata` | `true` | Push metadata fetched from backends to the other nodes |
| `share.preload` | `true` | Run each refresh rule on one node, and pull what it preloads from it |

### `serve_backends`

Serving backends to other hylla nodes, which use them as `hylla` backends.

| Setting | Default | |
|---|---|---|
| `listen` | required | Such as `0.0.0.0:7947` |
| `backends` | required | The names of the backends served; others are refused |
| `tls.ca` | required | The CA other nodes' certificates must be issued by |
| `tls.cert`, `tls.key` | required | This node's certificate, issued for the host other nodes reach it at |
| `tls.client_name` | required | The name other nodes' certificates must be issued for |

### `control`

The control plane. See [Control plane](control-plane.md).

| Setting | Default | |
|---|---|---|
| `listen` | required | Such as `127.0.0.1:9090`. Keep it private |
| `users` | none | Users signing in with Basic auth, each with a secret `password_hash` from `hylla hash-password` |
| `max_upload_mib` | `256` | The largest upload |
| `oidc` | none | Signing in through an OpenID provider |
| `oidc.issuer` | required | The provider's issuer URL |
| `oidc.client_id` | required | |
| `oidc.client_secret` | none, secret | For a confidential client. Without it, hylla is a public client, protected by PKCE |
| `oidc.url` | required | The control plane's URL as browsers reach it |
| `oidc.groups` | required | People in at least one of these groups get in |
| `oidc.groups_claim` | `groups` | The claim listing a person's groups, in the ID token or the user info |
| `oidc.scopes` | `(email groups)` | Asked for besides `openid` |
| `oidc.session_ttl` | `8h` | How long a sign-in lasts |

### `telemetry`

Exporting over OTLP. See [Observability](observability.md).

| Setting | Default | |
|---|---|---|
| `service_name` | `hylla` | |
| `resource` | none | More resource attributes, as `({key ..., value ...})` |
| `otlp.endpoint` | `OTEL_EXPORTER_OTLP_*` | The collector's base URL, such as `http://collector:4318` |
| `otlp.protocol` | `http_protobuf` | Or `grpc` |
| `otlp.headers` | none, secret | Sent with every export, such as an API key |
| `otlp.timeout` | `10s` | |
| `metrics.enabled` | `false` | |
| `metrics.endpoint` | `otlp.endpoint` | The full URL for metrics |
| `metrics.interval` | `30s` | |
| `traces.enabled` | `false` | |
| `traces.endpoint` | `otlp.endpoint` | The full URL for traces |
| `traces.sample_ratio` | `1` | The share of traces hylla starts that are sampled |
| `traces.follow_callers` | `false` | Whether public requests continue the trace in their `traceparent` header, and its sampling. Anyone can send one, so turn it on only behind a proxy that sets it |
| `traces.record_paths` | `false` | Whether request and object paths go in spans |
| `logs.level` | `info` | `tracing` filter directives, such as `info,hylla_server=debug`. `RUST_LOG` overrides it |
| `logs.format` | `text` | Or `json` |
| `logs.export` | `false` | Whether to export logs over OTLP too |
| `logs.endpoint` | `otlp.endpoint` | The full URL for logs |
