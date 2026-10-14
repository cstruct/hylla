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
Object metadata (size, ETag, last modified and content type) MUST be cached in memory for
the configured TTL, after which it MUST be fetched again.
