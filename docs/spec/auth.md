# Auth

Mappings can require requests to carry a JWT, checked against a configured verifier, and
optionally limit tokens to backend paths derived from their claims.

## Tokens

r[auth.mappings]
Requests for a mapping with `auth` MUST be checked before anything is fetched or served;
requests for mappings without it MUST be served without checks.

r[auth.bearer]
The token MUST be read from an `Authorization: Bearer` header; requests without one MUST
be refused.

r[auth.keys]
Each verifier's public key MUST be parsed at startup, failing startup when it doesn't
parse for the verifier's algorithm.

r[auth.validate]
A token MUST be signed with the verifier's algorithm and key, MUST have an `exp` claim that
hasn't passed, and MUST NOT have an `nbf` claim that hasn't been reached, both within the
verifier's leeway. When the verifier has an issuer or audience, the token MUST have that
`iss` or `aud`. Other algorithms, including `none` and shared secret algorithms, MUST be
refused.

## Paths

r[auth.paths]
When a mapping's auth has path templates, a request MUST be refused unless its backend path
(after rewrites, decoded) is under the prefix one of the templates renders to with the
token's claims.

r[auth.claim-values]
A template MUST only render when every claim it uses is a string or number forming a single,
non-empty path segment that isn't `.` or `..`, so claims can't widen the prefix.

r[auth.dot-segments]
Requests for a mapping with auth whose backend path has `.` or `..` segments MUST be refused
with 400.

## Responses

r[auth.responses]
A request without a token, or with an invalid one, MUST be answered with 401 and
`WWW-Authenticate: Bearer` (with `error="invalid_token"` for invalid tokens). A valid token
that doesn't allow the path MUST be answered with 403 and
`WWW-Authenticate: Bearer error="insufficient_scope"`.

r[auth.private]
Responses of a mapping with auth MUST be marked `Cache-Control: private`, unless its auth
turns that off.
