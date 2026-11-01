# Troubleshooting

## The config doesn't load

hylla refuses to start on a config with any problem, and lists them all. To see what it
would run with, where each value comes from, and what's missing:

```bash
hylla config show --config config.styx --secrets secrets.styx
hylla config explain servers.public
```

Without `--secrets`, every credential shows as missing: that's expected, since credentials
only come from the secrets file. They can't be set through `HYLLA__...` variables either.

## 404 `unknown host`

The request's `Host` header isn't one of the `server_names` of a server on the address it
came in on. A request to `http://127.0.0.1:8080/` has the host `127.0.0.1`, not
`localhost`. Add the name to `server_names`, or send the header:

```bash
curl -H "Host: cache.example.com" http://127.0.0.1:8080/app.css
```

## 404 `no mapping`, or 404 for an object that exists

- No mapping's `path_prefix` matches the path. Prefixes match whole segments: `/assets`
  matches `/assets/app.css`, not `/assets2/app.css`.
- The key isn't what you expect. Without rewrites, the key is the whole path without its
  leading `/`: `/assets/app.css` is the key `assets/app.css`, not `app.css`. Add a rewrite
  such as `{pattern "^/assets/(.*)$", replace /$1}` to drop the prefix.
- The first matching mapping wins: put more specific prefixes before `/`.

## 502

The backend failed: it refused hylla's credentials, couldn't be reached, or didn't answer
within `timeouts`. hylla logs the error, and the control plane shows each backend's last
failure. Cached objects are still served for `stale_if_error` (a day by default) past their
TTL.

## Nothing is cached

Objects whose backend gives no ETag are never cached, since their versions couldn't be told
apart: every request reads them from the backend, and `hylla.cache.chunks` counts them as
`uncacheable`. S3 and Azure always give ETags. Some S3 compatible stores don't for objects
that weren't uploaded through them, such as files copied into VersityGW's directory: upload
them through the S3 API instead.

## The log says foyer runs "in mock mode"

Without `cache.disk`, foyer, the cache library, logs that its disk tier does nothing. That's
expected: the cache is in memory only.

## 401 or 403 with JWTs

- 401 `Bearer`: the request has no `Authorization: Bearer` header.
- 401 `invalid_token`: the token isn't signed with the verifier's key and algorithm, has
  expired, isn't valid yet, has no `exp`, or lacks the configured `iss` or `aud`. hylla logs
  why at `debug`.
- 403 `insufficient_scope`: the token is valid, but the path isn't under a prefix its claims
  allow, such as `/tenants/${tenant_id}` with another tenant's id.
- 400: the path has `.` or `..` segments.
- 503: the verifier reads its keys from a provider, which hasn't answered yet. hylla warns
  when reading it fails, and tries again every 30 seconds. Check that the `issuer` or
  `jwks_uri` can be reached from hylla, and that the discovery document's `issuer` is the
  verifier's, character for character.

## Cluster nodes don't find each other

`hylla.cluster.members` says how many nodes a node sees. Check that:

- every node has the same `cluster.key`, the same hylla version, and `tls` on or off
  together,
- `advertise` is an address the others can reach, not `0.0.0.0`,
- the seeds resolve: hylla warns once when a seed doesn't, and keeps its last addresses,
- the clocks agree within a minute: hylla warns when it refuses a node for its clock,
- with TLS, every certificate is issued by the cluster CA for `server_name`, with the server
  and client auth usages.

## A refresh rule doesn't run

In a cluster, each rule runs on one node only, which logs `refreshed` with what it did. The
others don't run it. A rule with `mode disabled` never runs.

## Signing in to the control plane goes in circles

Sessions are kept on the node people signed in on. With several nodes behind one control
plane URL, the load balancer must send each browser to the same node.

## The disk cache can't be written

In the Docker image, hylla runs as user 65532, which must be able to write
`cache.disk.path`. A new named volume can; a directory from the host needs
`chown 65532:65532`, or in Kubernetes, `fsGroup: 65532`.

## Shutdown is cut short

hylla gives requests `http.shutdown_timeout` to finish before writing its cache to disk.
If whatever stops it, such as Kubernetes, kills it sooner, the memory cache isn't written.
Give it longer than the shutdown timeout, such as `terminationGracePeriodSeconds: 30` for
the default 20 seconds, or `docker run --stop-timeout 30`: Docker kills containers after 10
seconds by default.
