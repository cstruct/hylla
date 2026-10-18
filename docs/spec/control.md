# Control plane

An optional listener with a web UI and a JSON API, for the people running hylla: the status
of each backend, the mappings, and browsing their objects, changing those of writable
mappings. Public listeners never change objects.

r[control.auth]
Every request to the control plane but signing in MUST be authenticated, with a session
from signing in through the OpenID provider or with HTTP Basic auth as a configured user
whose password matches its argon2 hash. Otherwise, a browser loading a page MUST be sent to
sign in through the provider when one is configured, coming back to the page, and anything
else MUST be refused with 401 and a `WWW-Authenticate` challenge.

## Signing in through an OpenID provider

r[control.oidc.login]
`/auth/login` MUST redirect to the provider's authorization endpoint for the authorization
code flow, with the client id, the redirect URI `{url}/auth/callback`, the `openid` scope
and the configured ones, and a fresh state, nonce and S256 PKCE challenge. The state MUST
be set in an HTTP-only cookie binding the sign-in to the browser, which MUST finish it
within ten minutes.

r[control.oidc.issuer]
The provider's discovery document MUST name the configured issuer.

r[control.oidc.callback]
`/auth/callback` MUST refuse with 400 a state that isn't the browser's sign-in, and with 403
an error from the provider. It MUST exchange the code with the PKCE verifier, with the client
secret when one is configured, and start a session for an ID token that checks out,
redirecting to the page the sign-in started from, if it's one of the control plane's.

r[control.oidc.id-token]
An ID token MUST be signed asymmetrically by one of the provider's keys, issued by the
provider for the client id, current, and carry the sign-in's nonce.

r[control.oidc.groups]
Only users in one of the configured groups MUST get a session, by the configured claim of
the ID token or, when the ID token has no such claim, of the provider's user info. Others
MUST be refused with 403.

r[control.oidc.session]
A session MUST be named by a random HTTP-only, same-site cookie, secure when the control
plane is reached over HTTPS, and last the configured time.

r[control.oidc.logout]
`POST /auth/logout` MUST end the session, clear its cookie, and redirect to the provider's
end session endpoint, when it has one, with the ID token as a hint and `{url}/` to return
to. Cross-site requests to it MUST be refused.

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
