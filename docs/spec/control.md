# Control plane

An optional listener with a web UI and a JSON API, for the people running hylla: the status
of each backend, the mappings, and browsing their objects, changing those of writable
mappings. Public listeners never change objects.

r[control.auth]
Every request to the control plane MUST be authenticated with HTTP Basic auth, as a
configured user whose password matches its argon2 hash, and refused with 401 and a
`WWW-Authenticate` challenge otherwise.

r[control.same-origin]
A request that changes anything, by any method but `GET`, `HEAD` and `OPTIONS`, MUST be
refused with 403 when its `Sec-Fetch-Site` is neither `same-origin` nor `none`, or, without
it, when its `Origin` isn't the control plane's host, since browsers send Basic credentials
with requests other sites make.

r[control.backends]
The control plane MUST report, for each backend, its type, its last request and its last
failed request since startup, with when it was made, the operation, its outcome, how long
it took and, for failures, the error.

r[control.browse]
The control plane MUST list the objects and folders under a prefix of any mapping without
rewrites, relative to its path prefix. Mappings with rewrites MUST be refused with 409,
since their backend keys don't map back to their paths.

r[control.writes]
The control plane MUST upload, delete and move the objects of writable mappings only,
refusing others with 403, and only under the mapping's path prefix: paths leaving it, such
as with `..` segments, MUST be refused with 400. Uploads larger than the configured limit
MUST be refused with 413.

r[control.invalidate]
After an object is changed through the control plane, its cached metadata MUST be
forgotten, so the next request reads it from the backend.

r[control.api]
The JSON API MUST serve the backends at `/v1/backends`, the mappings at `/v1/mappings`, a
mapping's listing at `/v1/mappings/{server}/{index}/list`, uploads with `PUT` and deletes
with `DELETE` of `/v1/mappings/{server}/{index}/objects/{path}`, moves with `POST` of
`{"from", "to"}` to `/v1/mappings/{server}/{index}/move`, and uploads of form files with
`POST` to `/v1/mappings/{server}/{index}/upload`.

r[control.ui]
The UI MUST be rendered on the server and work without JavaScript, its changes being forms
posted to the server. When the hydration bundle is embedded, the UI MUST be hydrated.

r[control.hash-password]
`hylla hash-password` MUST print an argon2id hash of the password read from stdin, with a
random salt, as a PHC string, and refuse an empty password.
