# Server

The server answers requests for objects from the cache, fetching what it misses from the
request's backend.

## Requests

r[server.hosts]
A request MUST be served by the server on its listen address whose server names contain
the request's `Host`, or without one, the host of its URI, as HTTP/2's `:authority` is,
compared case-insensitively and without a port. Requests for other
hosts MUST be answered with 404.

r[server.routes]
A request MUST be routed by the server's mappings to a backend and an object key: the
rewritten path without its leading `/`. Requests no mapping matches, and requests for
missing objects, MUST be answered with 404.

r[server.routes.decoded]
A request's path MUST be percent-decoded before it is matched to a mapping, rewritten or
checked against auth, so an encoded path such as `/%74enant` or `/tenant%2Fx` reaches the
mapping its key falls under. Paths that don't decode to UTF-8 MUST be answered with 400.

r[server.methods]
Only `GET` and `HEAD` MUST be accepted; other methods MUST be answered with 405 and an
`Allow` header.

r[server.get]
A `GET` MUST be answered with the object's content, its `Content-Length`, `Content-Type`
(`application/octet-stream` when unknown), `ETag`, `Last-Modified` and `Accept-Ranges:
bytes`.

r[server.object-headers]
Responses for an object MUST carry the `Cache-Control`, `Content-Encoding`,
`Content-Disposition` and `Content-Language` its backend stored with it, cached with its
metadata, also through a `hylla` backend.

r[server.cache-control]
An object without a stored `Cache-Control` MUST be answered with its mapping's
`cache_control`, when set. Responses for objects MUST carry an `Age`: the seconds since the
object's metadata was fetched from its backend.

r[server.head]
A `HEAD` MUST be answered with the headers of a `GET` and no body.

r[server.range]
A `Range` header with a single byte range (`a-b`, `a-` or `-n`) MUST be answered with 206,
the bytes of that range and a `Content-Range`. A range starting after the object MUST be
answered with 416. Other `Range` headers MUST be ignored, serving the whole object.

r[server.if-range]
A `Range` with an `If-Range` that is neither the object's ETag, compared strongly, nor its
`Last-Modified` MUST be ignored, serving the whole object, so a resumed download isn't
spliced from two versions.

r[server.read-ahead]
While an object is sent, up to 4 of the chunks after the one being sent MUST be read at the
same time, so a cold object isn't fetched one chunk round-trip at a time.

r[server.changed]
When a chunk can't be fetched because the object changed since its metadata was cached, the
request MUST be served again with the metadata fetched anew, rather than failing, as long
as nothing of the response was sent.

r[server.conditional]
A request whose `If-None-Match` contains the object's ETag, compared weakly, so `W/"x"`
matches `"x"`, or `*`, also for an object without an ETag, MUST be answered with 304 and no
body.

r[server.if-match]
A request with an `If-Match` that contains neither the object's ETag nor `*` MUST be
answered with 412 and no body, so a reader can't mix parts of different versions.

r[server.shutdown]
On SIGINT or SIGTERM, hylla MUST stop accepting connections and finish the requests in
flight, closing the connections still open once the shutdown timeout passes, then write the
memory tier of the cache to disk and exit successfully.

r[server.sighup]
On SIGHUP, hylla MUST log that it doesn't reload its config, and keep running, rather than
being ended by it, as a reflexive `kill -HUP` or systemd's `ExecReload` would.

r[server.limits]
Every listener MUST close a connection whose request headers don't arrive within the header
read timeout, or that sends and receives nothing for the idle timeout unless it's `0s`, while
no request on it is in flight, and
MUST have at most `max_connections` connections open, leaving further ones waiting to be
accepted.

## Health

r[health.live]
The health listener MUST answer `GET /healthz` with 200 while hylla runs, also while it
shuts down.

r[health.ready]
The health listener MUST answer `GET /readyz` with 200 once every listener is serving, and
with 503 before that and once shutdown started, so load balancers stop sending requests
before connections are closed.

r[health.check]
`hylla healthcheck ADDRESS` MUST succeed when the health listener at `ADDRESS` answers
`/healthz`, or with `--ready`, `/readyz`, with 200 within a few seconds, and fail otherwise,
so containers without curl can be checked.

## Cache

r[cache.chunks]
Objects MUST be cached in chunks of the configured size, keyed by backend, object key,
ETag and chunk index, so a changed object misses instead of serving stale data. A chunk
missing from the cache MUST be fetched with a range request matching the ETag.

r[cache.no-etag]
The chunks of an object its backend gives no ETag MUST NOT be cached, preloaded or pulled
from other nodes, and MUST be read from the backend for every request, since chunks of its
versions couldn't be told apart.

r[cache.metadata]
Object metadata (size, ETag, last modified and content type) MUST be cached with the
chunks, in memory and on disk when there is a disk tier, so it survives restarts. Metadata
older than the configured TTL MUST be fetched again.

r[cache.stale-while-revalidate]
Metadata past its TTL but within the configured stale-while-revalidate window MUST be used
without waiting on the backend, while it is revalidated in the background.

r[cache.revalidation-debounce]
An object MUST have at most one background revalidation running, shared by all requests
for it. After a background revalidation fails, the object MUST NOT be revalidated in the
background again until a back-off of the metadata TTL, at least a second, has passed.

r[cache.coalesce]
Concurrent requests for an object whose metadata must be fetched from the backend MUST share
one fetch.

r[cache.failure-backoff]
After fetching an object's metadata fails, requests within a back-off of the metadata TTL,
at least a second, MUST be answered from metadata within the stale-if-error window without
asking the backend again. Failures MUST be forgotten once their back-off passed.

r[cache.stale-if-error]
When metadata past its TTL can't be fetched again because the backend fails, the cached
metadata MUST still be used if it is younger than the TTL plus the configured
stale-if-error window, so cached objects stay available during backend outages, also
after a restart.

r[cache.not-found]
When the backend reports an object missing, its cached metadata MUST be dropped and the
request answered with 404, never with stale data. The object MUST then be answered with 404
without asking the backend for the `not_found_ttl`, unless it's changed through this node's
control plane or another node's.

r[cache.changed]
Cached metadata MUST only be dropped by a failed chunk fetch when the object changed (its
ETag no longer matches) or went away, not on other failures such as outages.

## Refresh

Refresh rules keep cached objects fresh, and the cache warm, in the background.

r[refresh.schedule]
Every enabled refresh rule MUST run at startup and then on its interval.

r[refresh.run]
A run MUST compare the objects a rule covers to the cache, processing at most the
backend's `concurrency` objects at the same time, and report what it did.

r[refresh.list]
A prefix rule MUST list the backend once per run, comparing each listed object's ETag and
size to its cached metadata, so unchanged objects need no further requests.

r[refresh.list-batches]
A run MUST handle what it lists a batch at a time as the listing arrives, rather than
holding a whole listing in memory, with the preload caps applying across batches.

r[refresh.revalidate]
Revalidating MUST mark the metadata of unchanged cached objects as fetched now, and fetch
the metadata of changed cached objects again. Objects that aren't cached MUST be left
alone.

r[refresh.preload]
Preloading MUST fetch objects that aren't cached, or changed, into the cache: their
metadata and every chunk.

r[refresh.caps]
Preloading MUST stop at `max_objects` objects and `max_mib` of data per run, taking objects
in listing order, and report the objects it skipped.

r[refresh.removed]
A key rule whose object went away MUST drop its cached metadata.
