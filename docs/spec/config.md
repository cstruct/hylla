# Configuration

hylla's configuration consists of servers, which accept requests, and backends, which
hold the objects.

```styx
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com)
    mappings (
      {
        path_prefix /assets
        backend assets
        rewrites ({pattern "^/assets/(.*)$", replace /$1})
      }
    )
  }
}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
  }
}
```

## Sources

r[config.sources]
The configuration MUST be loaded from a public Styx config file, an optional Styx secrets
file, `HYLLA__` prefixed environment variables and command line flags, in that order of
precedence (lowest first).

r[config.file]
A config file given with `--config` MUST exist. Without `--config`, `config.styx` MUST be
read if it exists, and the configuration loaded without a config file otherwise.

r[config.validate]
A loaded configuration MUST be validated, and every problem found MUST be reported
together.

## Servers

r[config.servers]
Servers MUST be configured as a map from server names (identifiers) to server blocks.

r[config.server.listen]
A server MUST listen on an address and port, such as `0.0.0.0:8080`.

r[config.server.names]
A server MUST have a list of host names it answers to, matched against a request's `Host`
header. Its first name MUST be its public host, used for absolute URLs.

r[config.server.mappings]
A server MUST have a list of mappings from request path prefixes to backends.

r[config.validate.mappings]
A configuration without a server with at least one mapping MUST be reported as invalid,
since it would serve nothing.

r[config.validate.server-names]
A server without server names MUST be reported as invalid.

r[config.validate.unique-hosts]
Several servers answering to the same host name, compared case-insensitively, on the same
address MUST be reported as invalid. Servers MAY share an address with different host
names.

## Mappings

r[config.mappings.priority]
Mappings MAY overlap; a request MUST be routed by the first mapping that matches it.

r[config.mappings.prefix]
A mapping MUST match a request path equal to its path prefix or continuing after it with
a `/`, so `/assets` matches `/assets/app.css` but not `/assets2`.

r[config.mappings.rewrites]
A mapping MAY have a list of rewrites, each a regex pattern and a replacement that can
refer to capture groups. The rewrites MUST be applied in order to the request path, each
replacing the first match of its pattern in the result of the previous one, giving the
path requested from the backend. A rewrite whose pattern doesn't match MUST leave the path
as is.

r[config.mappings.writable]
A mapping MAY be `writable`, defaulting to not, letting the control plane change its
objects under its path prefix.

r[config.validate.writable]
A writable mapping with rewrites, or whose backend is a `hylla` backend, MUST be reported as
invalid.

r[config.validate.backend-refs]
A mapping using a backend that isn't configured MUST be reported as invalid.

r[config.validate.absolute-paths]
A mapping whose path prefix doesn't start with `/` MUST be reported as invalid.

r[config.validate.rewrites]
A rewrite whose pattern isn't a valid regex MUST be reported as invalid.

r[config.validate.replacements]
A rewrite whose replacement refers to a capture group its pattern doesn't have MUST be
reported as invalid. References follow the regex crate's syntax: `$$` is a literal `$`,
`${name}` refers to the group in the braces, and `$name` to the longest run of letters,
digits and underscores, so `$1a` refers to a group named `1a`.

## Backends

r[config.backends]
Backends MUST be configured as a map from backend names to backends.

r[config.backends.types]
A backend MUST be one of `s3`, `azure_blob` or `hylla`, selected by its `type` field, with
the fields of that type.

r[config.backends.hylla]
A `hylla` backend MUST configure the `url` of another hylla node serving backends, the name
of the `backend` on that node, and `tls` with the PEM files of the `ca` its certificate
must be issued by and of this node's `cert` and `key`.

r[config.validate.hylla-backends]
A `hylla` backend whose `url` isn't an `https://` URL with a host, with an empty `backend`,
or with an empty TLS file MUST be reported as invalid.

r[config.refresh]
A backend MAY have a `refresh` section with `rules` and a `concurrency`, defaulting to 4,
limiting how many objects are refreshed at the same time.

r[config.refresh.rules]
A refresh rule MUST cover either a backend key `prefix` (directory-like, empty for the
whole backend) or a single backend `key`, and run `every` interval.

r[config.refresh.modes]
A refresh rule's `mode` MUST be `disabled`, `revalidate` (the default), `preload` or
`both`. Preload rules MAY cap each run with `max_objects` and `max_mib`.

r[config.durations]
Durations, such as `every`, `metadata_ttl` and `stale_if_error`, MUST be written like
`90s`, `15m` or `1h 30m`.

r[config.validate.refresh]
A refresh with a `concurrency` of zero, and rules setting both or neither of `prefix` and
`key`, a key or prefix starting with `/`, an empty key, a zero interval, or preloading
without `max_objects` or `max_mib`, MUST be reported as invalid.

r[config.backends.secrets]
Backend credentials MUST be secret, so they can only be set by the secrets file.

## Cache

r[config.cache]
The cache MUST be configured in an optional `cache` section, every setting of which has a
default.

r[config.cache.chunks]
`chunk_size_kib` MUST set the size of the chunks objects are cached in, defaulting to
1024.

r[config.cache.metadata-ttl]
`metadata_ttl` MUST set how long object metadata is cached, defaulting to `30s`.

r[config.cache.stale-while-revalidate]
`stale_while_revalidate` MUST set how long past its TTL cached metadata is still served
while it is revalidated in the background, defaulting to `0s`, which disables it.

r[config.cache.stale-if-error]
`stale_if_error` MUST set how long past its TTL cached metadata may still be used when the
backend fails, defaulting to `1d`. `0s` MUST disable it.

r[config.cache.write-policy]
`write_policy` MUST select when chunks are written to disk: `on_eviction` from memory, the
default, or `on_insertion`.

r[config.cache.memory]
`memory.capacity_mib` MUST set the capacity of the memory tier, defaulting to 256.

r[config.cache.disk]
An optional `disk` section MUST enable the disk tier, stored in `path` with a capacity of
`capacity_mib` and blocks of `block_size_mib`, defaulting to 16.

r[config.validate.cache]
A chunk size or memory capacity of zero, a chunk size not smaller than the disk block
size, and a disk capacity of fewer than 4 blocks MUST be reported as invalid.

## Auth

r[config.auth.verifiers]
JWT verifiers MUST be configured in an optional top-level `auth` map from names to
verifiers, each with an `algorithm`, a PEM `public_key`, an optional `issuer` and
`audience`, and a `leeway` defaulting to `30s`.

r[config.auth.algorithms]
A verifier's `algorithm` MUST be one of `RS256`, `RS384`, `RS512`, `PS256`, `PS384`,
`PS512`, `ES256`, `ES384` or `EdDSA`.

r[config.auth.mappings]
A mapping MAY have an `auth` section naming its `verifier`, with optional `paths` and a
`private` flag defaulting to true.

r[config.auth.paths]
`paths` MUST be backend path prefixes starting with `/`, in which `${claim}` is replaced by
a token's claim and `$$` is a literal `$`, such as `/tenant/${sub}`.

r[config.validate.auth]
A mapping's auth naming a verifier that isn't configured, or with a path template that
doesn't parse, MUST be reported as invalid.

## Serving backends

r[config.serve-backends]
An optional `serve_backends` section MUST configure serving backends to other hylla nodes:
the `listen` address, the names of the `backends` served, and `tls` with the PEM files of
the `ca` other nodes' certificates must be issued by and of this node's `cert` and `key`,
and the `client_name` other nodes' certificates must be issued for.

r[config.validate.serve-backends]
`serve_backends` without backends, naming a backend that isn't configured, listening on the
address of a server or the cluster, with an empty TLS file, or with a `client_name` that
isn't a DNS name MUST be reported as invalid.

## Control plane

r[config.control]
An optional `control` section MUST configure the control plane: the `listen` address, the
`users`, and `max_upload_mib`, the largest upload, defaulting to 256.

r[config.control.users]
`users` MUST map user names to their `password_hash`, an argon2 hash in PHC form, which is
secret, so only the secrets file can set it.

r[config.control.oidc]
An optional `oidc` section in `control` MUST configure signing in through an OpenID
provider: its `issuer`, the `client_id` and an optional secret `client_secret`, the control
plane's own `url`, the `groups` allowed in, the `groups_claim` holding them, defaulting to
`groups`, the `scopes` asked for besides `openid`, defaulting to `email` and `groups`, and
the `session_ttl`, defaulting to `8h`.

r[config.validate.control]
A control plane without users or `oidc`, with a password hash that isn't an argon2 hash, a
zero `max_upload_mib`, listening on the address of a server, the cluster or
`serve_backends`, or with `oidc` whose `issuer` or `url` isn't a URL, without a client id or
groups, or with a zero session lifetime MUST be reported as invalid.

## Cluster

r[config.cluster]
An optional `cluster` section MUST configure sharing with other nodes: the `listen` address
of the cluster listener, an optional `advertise` address other nodes reach this one at, a
`gossip_interval` defaulting to `1s` and a `failure_timeout` defaulting to `10s`.

r[config.cluster.seeds]
`seeds` MUST list nodes to join through as `host:port`, where the host is an address or a
DNS name.

r[config.cluster.key]
`key` MUST be the secret shared by the nodes, so only the secrets file can set it.

r[config.cluster.tls]
An optional `tls` section in `cluster` MUST turn on mutual TLS between the nodes, from the
PEM files of the `ca` nodes' certificates must be issued by and of this node's `cert` and
`key`, and the `server_name` every node's certificate is issued for.

r[config.cluster.share]
`share` MUST turn sharing `metadata` and `preload`s on or off, both defaulting to on.

r[config.validate.cluster]
A `listen` address of `0.0.0.0` or `[::]` without an `advertise` address, an unspecified
`advertise` address, a key shorter than 32 characters, seeds that aren't `host:port`, a zero
gossip interval, a failure timeout not longer than the gossip interval, and TLS settings
with an empty file or a `server_name` that isn't a DNS name MUST be reported as invalid.

## Telemetry

r[config.telemetry]
An optional `telemetry` section MUST configure exporting telemetry: the `service_name`,
defaulting to `hylla`, and further `resource` attributes as a list of `key` and `value`.

r[config.telemetry.otlp]
`otlp` MUST configure where and how to export: an optional base `endpoint`, the `protocol`,
`http_protobuf` (the default) or `grpc`, secret `headers`, and a `timeout` defaulting to
`10s`.

r[config.telemetry.metrics]
`metrics` MUST configure exporting metrics: whether it's `enabled` (off by default), an
optional `endpoint` overriding the OTLP one, and the export `interval`, defaulting to `30s`.

r[config.telemetry.traces]
`traces` MUST configure exporting traces: whether it's `enabled` (off by default), an optional
`endpoint` overriding the OTLP one, the `sample_ratio` from 0 to 1, defaulting to 1, and
whether to `record_paths` in spans, off by default.

r[config.telemetry.logs]
`logs` MUST configure logging: the `level` as filter directives, defaulting to `info`, the
`format`, `text` (the default) or `json`, whether to `export` logs over OTLP (off by default),
and an optional `endpoint` overriding the OTLP one.

## Inspecting

r[config.show]
`hylla config show` MUST print the effective configuration as Styx, every value with a
comment naming the source it came from and the sources it overrides, or that it is a
default. Secret values MUST be redacted, and settings left out MUST NOT be shown.

r[config.show.missing]
When the configuration doesn't load, `hylla config show` and `hylla config explain` MUST
show it as far as it loaded, marking every required field no source set as missing, and
then fail with the reasons it doesn't load. An invalid configuration MUST be shown in full
before failing with its validation errors.

r[config.explain]
`hylla config explain <path>` MUST print the value of the field at the path, or of every
field under it, with the source it came from and the sources it overrides, highest
precedence first, saying when a known field is unset and failing for an unknown one.
Secret values MUST be redacted.

## Command line flags

r[config.flags]
Command line flags MUST override the config file and environment variables for the
telemetry settings most often changed for a single run, and the memory cache's capacity:
`--log-level`, `--log-format`, `--otlp-endpoint`, `--metrics`, `--traces` and
`--cache-memory-mib`.
