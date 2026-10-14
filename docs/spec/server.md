# Server

The server answers requests for objects from the cache, fetching what it misses from the
request's backend.

## Requests

r[server.hosts]
A request MUST be served by the server on its listen address whose server names contain
the request's `Host`, compared case-insensitively and without a port. Requests for other
hosts MUST be answered with 404.

r[server.routes]
A request MUST be routed by the server's mappings to a backend and an object key: the
rewritten path without its leading `/`, percent-decoded. Requests no mapping matches, and
requests for missing objects, MUST be answered with 404.

r[server.methods]
Only `GET` and `HEAD` MUST be accepted; other methods MUST be answered with 405 and an
`Allow` header.

r[server.get]
A `GET` MUST be answered with the object's content, its `Content-Length`, `Content-Type`
(`application/octet-stream` when unknown), `ETag`, `Last-Modified` and `Accept-Ranges:
bytes`.

r[server.head]
A `HEAD` MUST be answered with the headers of a `GET` and no body.

r[server.range]
A `Range` header with a single byte range (`a-b`, `a-` or `-n`) MUST be answered with 206,
the bytes of that range and a `Content-Range`. A range starting after the object MUST be
answered with 416. Other `Range` headers MUST be ignored, serving the whole object.

r[server.conditional]
A request whose `If-None-Match` contains the object's ETag, or `*`, MUST be answered with
304 and no body.

## Cache

r[cache.chunks]
Objects MUST be cached in chunks of the configured size, keyed by backend, object key,
ETag and chunk index, so a changed object misses instead of serving stale data. A chunk
missing from the cache MUST be fetched with a range request matching the ETag.

r[cache.metadata]
Object metadata (size, ETag, last modified and content type) MUST be cached with the
chunks, in memory and on disk when there is a disk tier, so it survives restarts. Metadata
older than the configured TTL MUST be fetched again.

r[cache.stale-if-error]
When metadata past its TTL can't be fetched again because the backend fails, the cached
metadata MUST still be used if it is younger than the TTL plus the configured
stale-if-error window, so cached objects stay available during backend outages, also
after a restart.

r[cache.not-found]
When the backend reports an object missing, its cached metadata MUST be dropped and the
request answered with 404, never with stale data.

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
