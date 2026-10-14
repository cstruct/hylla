# Configuration

hylla's configuration consists of servers, which accept requests, and backends, which
hold the objects.

```yaml
servers:
  public:
    listen: 0.0.0.0:8080
    server_names: [cache.example.com]
    mappings:
      - path_prefix: /assets
        backend: assets
        rewrites:
          - pattern: ^/assets/(.*)$
            replace: /$1
backends:
  assets:
    type: s3
    bucket: assets
    region: eu-north-1
```

## Sources

r[config.sources]
The configuration MUST be loaded from a public YAML config file, an optional YAML secrets
file and `HYLLA__` prefixed environment variables, in that order of precedence (lowest
first).

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
A backend MUST be one of `s3`, `swift` or `azure_blob`, selected by its `type` field, with
the fields of that type.

r[config.backends.secrets]
Backend credentials MUST be secret, so they can only be set by the secrets file.
