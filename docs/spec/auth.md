# Auth

Mappings can require requests to carry a JWT, checked against a configured verifier, and
optionally limit tokens to backend paths derived from their claims. A verifier's keys are a
configured public key, or read from its provider.

## Tokens

r[auth.mappings]
Requests for a mapping with `auth` MUST be checked before anything is fetched or served;
requests for mappings without it MUST be served without checks.

r[auth.bearer]
The token MUST be read from an `Authorization: Bearer` header; requests without one MUST
be refused.

r[auth.keys]
Each verifier's `public_key` MUST be parsed at startup, failing startup when it doesn't
parse for the verifier's algorithm.

r[auth.validate]
A token MUST be signed with the verifier's algorithm and key, MUST have an `exp` claim that
hasn't passed, and MUST NOT have an `nbf` claim that hasn't been reached, both within the
verifier's leeway. When the verifier has an issuer or audience, the token MUST have that
`iss` or `aud`. Other algorithms, including `none` and shared secret algorithms, MUST be
refused.

r[auth.audience-warning]
A verifier without an audience MUST be warned about at startup, since it accepts tokens its
issuer made for other services.

r[auth.issuer-warning]
A verifier reading its keys from a `jwks_uri` without an issuer MUST be warned about at
startup, since it accepts tokens from every issuer sharing those keys.

## Keys from a provider

r[auth.provider-keys]
A verifier without a `public_key` MUST read its keys from its `jwks_uri`, or without one,
from the `jwks_uri` in its issuer's discovery document, whose `issuer` MUST be the
verifier's. The keys MUST be read when a request first needs them, once for concurrent
requests, and read again after an hour, or when a token names a key they don't have, but
not more than once every 30 seconds.

r[auth.provider-https]
Discovery documents and keys MUST only be read over HTTPS, or HTTP to a loopback address,
including a `jwks_uri` a discovery document names, and redirects MUST NOT be followed, so
no one on the network can hand hylla keys.

r[auth.provider-limits]
Discovery documents, keys, tokens and user info MUST be read up to 1 MiB, refusing larger
answers without reading further.

r[auth.provider-down]
When the keys can't be read again, the keys read before MUST still be used. Requests MUST
NOT wait for a read in progress when the keys read before have the token's key. While no
keys have been read, requests MUST be answered with 503.

r[auth.provider-key-choice]
A token MUST be checked with the key its `kid` names, or without a `kid`, only when there is
just one key. Keys for another algorithm than the verifier's, by their `alg`, or for
another use than signing, by their `use`, MUST NOT be used.

## Paths

r[auth.paths]
When a mapping's auth has path templates, a request MUST be refused unless its backend path
(after rewrites, decoded) is under the prefix one of the templates renders to with the
token's claims.

r[auth.claim-values]
A template MUST only render when every claim it uses is a string or number forming a single,
non-empty path segment that isn't `.` or `..`, so claims can't widen the prefix.

r[auth.list-claims]
A claim that is a list MUST stand for each of its items that is a string or number forming
such a segment, so a template renders a prefix for every one of them, such as one per group
for `/teams/${groups}`, and a request under any of them is allowed.

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
turns that off, followed by the object's or mapping's other directives, without `public`.
