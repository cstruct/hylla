# Sites

A site serves static frontend builds from a backend, one build per commit. Each build lives
under its commit's SHA and never changes. Aliases, such as `pr1873` or `live`, are objects
naming the SHA they point at, so CI moves an alias by writing one small object, and hylla
picks it up without reloading its config.

```text
builds/3f2a…e1/index.html       a build, uploaded once by CI
builds/3f2a…e1/assets/app-4b1c.js
aliases/pr1873                  body: 3f2a…e1
aliases/live                    body: 9c07…d4
```

```styx
servers {
  previews {
    listen 0.0.0.0:8080
    server_names (*.preview.example.com)
    mappings ({path_prefix /, backend frontend, site {}})
  }
  live {
    listen 0.0.0.0:8080
    server_names (www.example.com)
    mappings ({path_prefix /, backend frontend, site {alias live}})
  }
}
```

`3f2a…e1.preview.example.com` serves that build. `pr1873.preview.example.com/settings`
redirects to `3f2a…e1.preview.example.com/settings`, so a browser stays on one build for the
whole session, even if `pr1873` moves while the page is open. `www.example.com` serves
whatever `live` points at, without redirecting.

## Hosts

r[site.hosts.wildcard]
A server name MAY be a wildcard, `*.` followed by a host name, matching hosts with exactly
one more label in front of that name, such as `pr1873.preview.example.com` for
`*.preview.example.com`, but neither `preview.example.com` nor `a.b.preview.example.com`.
That label, lowercased, MUST be the request's site label.

r[site.hosts.precedence]
A request's host MUST be matched to the server with that exact name on its listen address
before any wildcard, so `www.example.com` and `*.example.com` can share an address.

r[config.validate.wildcards]
A server name with `*` anywhere but as its whole first label, or a wildcard without at least
two labels after it, such as `*.com`, MUST be reported as invalid. Two servers with the same
wildcard, compared case-insensitively, on the same address MUST be reported as invalid.

## Configuration

r[config.site]
A mapping MAY have a `site` section, making it serve builds rather than mapping its paths
to keys. Every setting of it has a default.

r[config.site.prefixes]
`builds` MUST set the backend key prefix builds are stored under, defaulting to `builds/`,
and `aliases` the prefix aliases are stored under, defaulting to `aliases/`.

r[config.site.alias]
`alias` MAY name the alias the mapping always serves. Without it, the build MUST be chosen
by the request's site label.

r[config.site.index]
`index` MUST set the object served for paths ending with `/`, defaulting to `index.html`.

r[config.site.fallback]
`fallback` MAY set an object of the build, such as `index.html`, served for page loads of
paths the build doesn't have, for single page apps that route in the browser.

r[config.site.redirect]
`redirect` MUST select whether requests choosing an alias by their site label are
redirected to the build's SHA host, defaulting to `true`.

r[config.site.alias-cache-control]
`alias_cache_control` MUST set the `Cache-Control` of responses served through an alias
without redirecting, defaulting to `no-cache`.

r[config.validate.site]
A site mapping with rewrites, that is writable, or whose backend is a `hylla` backend MUST
be reported as invalid, and so MUST a site without `alias` on a server without a wildcard
name, an `alias` that isn't a valid alias name, prefixes that are empty, start with `/`,
don't end with `/` or overlap, and an `index` or `fallback` that's empty or starts with `/`.

## Choosing a build

r[site.label]
A site label of 40 or 64 lowercase hexadecimal characters MUST be taken as the SHA of a
build, without reading an alias. Any other label MUST be taken as the name of an alias.

r[site.alias-names]
Alias names MUST be 1 to 63 lowercase letters, digits and `-`, not starting or ending with
`-`, and not 40 or 64 hexadecimal characters, so every alias can be a host label and none
can be mistaken for a SHA. A request naming an invalid alias MUST be answered with 404,
without reading the backend.

r[site.alias-object]
An alias MUST be read from the object at the aliases prefix followed by its name, through
the cache like any other object, so it is checked against its ETag after `metadata_ttl`,
and is forgotten on every node when changed through a control plane. Its body, without
leading and trailing whitespace, MUST be the SHA it points at.

r[site.alias-cached]
The SHA an alias points at MUST be kept with the ETag it was read at, so requests don't
read the alias's body again until its ETag changes.

r[site.alias-missing]
A request through an alias that doesn't exist MUST be answered with 404.

r[site.alias-invalid]
An alias larger than 1 KiB, or whose body isn't a SHA, MUST be answered with 502, and why
logged, rather than served from a key built from it.

r[site.alias-stale]
While the backend can't be reached, an alias MUST be resolved as within the stale-if-error
window of `cache.stale-if-error`, so a site keeps being served through an outage.

## Serving a build

r[site.keys]
A request MUST be served the object at the builds prefix, the SHA, `/`, and the request's
path after the mapping's path prefix, without its leading `/`. A path ending with `/` MUST
have the site's `index` appended.

r[site.redirect]
A request choosing an alias by its site label, on a site with `redirect`, MUST be answered
with 307 to the same path and query on the host with the SHA in place of the label,
keeping the request's port, as a scheme-relative `Location` such as
`//3f2a…e1.preview.example.com/settings?tab=2`, so it works behind a proxy terminating TLS.

r[site.redirect.no-store]
Redirects to a SHA host MUST carry `Cache-Control: no-store`, so neither browsers nor
caches keep sending an alias to a build it no longer points at.

r[site.redirect.missing-build]
An alias MUST be redirected without checking that the build exists; requests for a missing
build MUST be answered with 404 on its SHA host.

r[site.fallback]
When the object for a request's path is missing, a site with a `fallback` MUST serve the
build's fallback object with 200 instead, for page loads only: requests with
`Sec-Fetch-Mode: navigate`, or without `Sec-Fetch-Mode` whose `Accept` contains
`text/html`. Other requests for missing objects, such as scripts and images, MUST be
answered with 404, so a missing asset doesn't load an HTML page in its place.

r[site.fallback.missing]
When the fallback object is missing too, the request MUST be answered with 404.

r[site.alias-cache-control]
Responses served through an alias without redirecting MUST carry the site's
`alias_cache_control` in place of the object's own `Cache-Control`, since the same URL
serves another build once the alias moves. Responses on a SHA host MUST carry the object's,
or the mapping's `cache_control`, as any other object's.

r[site.conditional]
Conditional and range requests MUST be answered as for any other object, by the ETag of the
object served, so an unchanged file is answered with 304 also after an alias moves to
another build.

r[site.browse]
The control plane MUST refuse to list a site mapping with 409, as for mappings with
rewrites. Builds and aliases MUST be written through a writable mapping without a site
covering the same backend keys, whose changes are forgotten on every node by
`cluster.share-forgets`.

## Open questions

These aren't requirements yet.

- **Moving the live site mid-session.** `www.example.com` can't redirect to a SHA host
  without the address bar showing it, so a page loaded before `live` moves can ask for an
  asset the new build doesn't have. A site could serve missing paths from the build an alias
  pointed at before, kept for a while, or CI could upload each build's assets to a shared
  prefix too.
- **Origins per build.** Each SHA host is its own origin, so `localStorage`, cookies set
  without a `Domain` and service workers don't carry over from one preview build to the
  next, and an OpenID provider needs a wildcard redirect URI to sign in on previews.
- **Preloading.** A refresh rule covers fixed keys, so it can't follow `live` to warm the
  build it points at. A rule could take an alias and preload its build when it moves.
- **Indexing.** Preview hosts could be answered with `X-Robots-Tag: noindex`.
- **Short SHAs.** Only full SHAs are accepted, since a backend can't look up a key by a
  prefix of it cheaply. CI can write an alias per short SHA if links need them.
- **Telemetry.** Requests could be counted by whether they were redirected, served
  through an alias or fallen back.
