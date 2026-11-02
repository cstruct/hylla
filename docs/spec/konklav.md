# Konklav

Mappings can leave authorization to [Konklav](https://github.com/cstruct/konklav): rules
match request paths, capturing segments, and name the actions a request needs, optionally at
a scope whose id is a capture, such as `object:read` at the `object` named by
`/object/{object_id}/*`. Konklav decides whether the caller's access token holds them, through
its relying SDK.

## Provider

r[konklav.optional]
Without a `konklav` section, hylla MUST NOT connect to Konklav, and requests MUST be served
as without this feature.

r[konklav.client]
With a `konklav` section, hylla MUST start Konklav's relying client at startup, logging in as
the configured service principal and following Konklav's change feed. Startup MUST NOT wait
for Konklav or fail when it can't be reached.

## Rules

r[konklav.mappings]
Requests for a mapping with `konklav` MUST be decided by Konklav before anything is fetched
or served; requests for mappings without it MUST NOT be.

r[konklav.patterns]
A rule's `path` MUST match the request path, percent-decoded, segment by segment: a literal
segment matches itself, `{name}` matches any one non-empty segment and captures it as `name`,
and a final `*` matches the rest of the path, including nothing. So `/object/{object_id}/*`
matches `/object/o-1`, `/object/o-1/` and `/object/o-1/a/b`, capturing `o-1`.

r[konklav.first-match]
The first of a mapping's rules whose path matches a request MUST decide it. A request no rule
matches MUST be refused with 403, so a path the rules forgot isn't served.

r[konklav.dot-segments]
Requests for a mapping with `konklav` whose path has `.` or `..` segments MUST be refused
with 400.

## Requirements

r[konklav.requirements]
A rule with a `scope` MUST require its `actions` at the scope whose kind is the scope's
`kind` and whose id is the value of the capture its `id` names, verbatim. A rule without a
`scope` MUST require its actions tenant-wide in the token's tenant.

r[konklav.tenancy]
The owner of a scope MUST be left to Konklav: the token's tenant, or for a registered scope
kind, the tenant Konklav recorded for the scope, so a caller can't reach another tenant's
object by naming its id.

## Decisions

r[konklav.bearer]
The access token MUST be read from an `Authorization: Bearer` header; requests without one
MUST be refused, without asking Konklav.

r[konklav.decide]
Decisions MUST come from Konklav's relying client, which verifies the token against
Konklav's keys, its `iss` and `aud` against the configured `issuer` and `audience`, and
evaluates the requirements.

r[konklav.cache]
hylla MUST NOT cache decisions itself. The relying client decides from entitlement sets and
registered scopes it caches, which Konklav's change feed invalidates within about a second,
and asks Konklav's check API for every decision while those caches can't be trusted.
Requests MUST be decided even when the object they ask for is cached.

r[konklav.obligations]
An allow MUST only be served when none of its obligations has attributes, since hylla can't
narrow what it serves to them, such as a share's date range; otherwise the request MUST be
refused with 403.

r[konklav.fail-closed]
When the relying client fails, or doesn't decide within the provider's `timeout`, the
request MUST be refused with 503 and not served, from the cache or otherwise.

## Responses

r[konklav.responses]
A request without a token, or with a token Konklav denies as `invalid_token`, MUST be
answered with 401 and `WWW-Authenticate: Bearer` (with `error="invalid_token"` for invalid
tokens). One Konklav asks to step up MUST be answered with 401 and
`WWW-Authenticate: Bearer error="insufficient_user_authentication"`, with `max_age` when the
step-up limits the authentication's age. Any other denial MUST be answered with 403 and
`WWW-Authenticate: Bearer error="insufficient_scope"`.

r[konklav.private]
Responses of a mapping with `konklav` MUST be marked `Cache-Control: private`, unless its
`konklav` turns that off, as for `auth`.

## With auth

r[konklav.with-auth]
A mapping with both `auth` and `konklav` MUST check `auth` first, and only ask Konklav about
requests `auth` allows, both reading the same bearer token. Its responses MUST be private
when either asks for it.
