# Serving backends

A hylla node can serve its backends to other hylla nodes, which use it as a `hylla`
backend. It answers them through its cache, so it fetches each object from its own backend
once for every node using it, as an origin shield. Nodes using it don't share their caches
with it, or with each other through it.

r[upstream.tls]
The listener MUST only accept TLS connections from nodes with a certificate issued by the
configured CA for the configured client name. Nodes MUST only connect to a node whose
certificate is issued by their configured CA for the host of its URL.

r[upstream.backends]
Only the backends listed MUST be served. Requests for any other backend MUST be refused
with 403.

r[upstream.objects]
`GET` and `HEAD` of `/backends/{backend}/objects/{key}` MUST answer as servers do, from the
cache and fetched from the backend on miss, with single byte ranges, `If-None-Match` and
`If-Match`.

r[upstream.list]
`GET` of `/backends/{backend}/list`, with an optional `prefix`, MUST list the backend's
objects under the prefix as JSON, with their key, size, ETag and last modification time,
from the backend itself.

r[backend.hylla.reads]
A `hylla` backend MUST read objects and their metadata with `GET` and `HEAD` of their
objects path, passing ranges, `If-Match` and `If-None-Match` on, and MUST report a missing
object, a failed precondition and an unchanged object as such.

r[backend.hylla.read-only]
A `hylla` backend MUST refuse writes, deletes and copies as not supported.
