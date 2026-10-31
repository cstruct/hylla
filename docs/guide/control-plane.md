# Control plane

The optional control plane is a web UI and JSON API for the people running hylla:

- the last request and the last failure of each backend,
- the mappings, and browsing their objects by folder,
- uploading, deleting and moving the objects of mappings marked `writable`, under their
  path prefix only.

Public listeners never write. [examples/control-plane](../../examples/control-plane) is a
complete config.

```styx
// config.styx
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com)
    mappings (
      {path_prefix /uploads, backend assets, writable true}
      {path_prefix /, backend assets}
    )
  }
}
control {
  listen 127.0.0.1:9090  // keep it private
  max_upload_mib 256     // the default
}
```

The control plane listens on plain HTTP. Keep it on a private network, or put a proxy that
terminates TLS in front of it, since Basic auth credentials are sent as they are.

Writable mappings can't have rewrites, since their backend keys must map back to their
paths, and can't use a read-only `hylla` backend.

## Signing in

### With a password

Users in `users` sign in with HTTP Basic auth, as scripts do. Hash a password, read from
stdin, with:

```bash
hylla hash-password
```

and put the hash in the secrets file:

```styx
// secrets.styx
control {users {admin {password_hash "$argon2id$v=19$m=19456,t=2,p=1$..."}}}
```

### Through an OpenID provider

People can sign in through an OpenID provider, such as Konklav, with the authorization code
flow and PKCE, if they're in one of the allowed groups:

```styx
control {
  listen 127.0.0.1:9090
  oidc {
    issuer https://id.example.com
    client_id hylla
    url https://hylla-control.internal  // the provider redirects to {url}/auth/callback
    groups (hylla-admin)                // from the `groups` claim, or `groups_claim`
  }
}
```

```styx
// secrets.styx, for a confidential client
control {oidc {client_secret "..."}}
```

At the provider, register the client with:

- the redirect URI `{url}/auth/callback`,
- to sign out at the provider too, the post-logout redirect URI `{url}/`.

With Konklav, give the client the `openid`, `email` and `groups` scopes and
`visible_groups` such as `hylla-*`, and the people who should get in the global role
`hylla-admin`.

Groups come from the ID token's `groups` claim, or the one `groups_claim` names, and
otherwise from the provider's user info. Sign-ins last `session_ttl`, 8 hours by default.

Sessions are kept in memory on the node people signed in on. With several nodes behind one
control plane URL, the load balancer must send each browser to the same node (sticky
sessions), or signing in goes in circles. A restart signs everyone out of that node.

## Safety

- Changes from other sites are refused, by `Sec-Fetch-Site` and `Origin`, since browsers
  send Basic credentials along with them. Pages can't be framed by other sites.
- Uploads larger than `max_upload_mib` are refused with 413. Uploads are passed on to the
  backend as they arrive, in parts once they're large, rather than held in memory.
- Every change, and every refused or failed one, is logged with the `hylla::audit` target:
  who made it, how they signed in, the path, the size of uploads and the outcome.
- After a change, the node forgets the object's cached metadata, and tells the other live
  nodes of a cluster to forget it too.

## JSON API

Mappings are addressed by their server's name and their position in its `mappings`,
starting at 0: with the config above, `/uploads` is `public/0`. Paths are relative to the
mapping's prefix, so `public/0/objects/css/app.css` is `/uploads/css/app.css`.

| Request | |
|---|---|
| `GET /v1/backends` | Each backend's type, last request and last failure |
| `GET /v1/mappings` | Every mapping: its server, index, prefix, backend, and whether it's writable |
| `GET /v1/mappings/{server}/{index}/list?prefix=css/` | A folder's objects and subfolders |
| `PUT /v1/mappings/{server}/{index}/objects/{path}` | Uploads the body |
| `DELETE /v1/mappings/{server}/{index}/objects/{path}` | |
| `POST /v1/mappings/{server}/{index}/move` | Moves `{"from": ..., "to": ...}` |
| `POST /v1/mappings/{server}/{index}/upload?prefix=css/` | Uploads the files of a `multipart/form-data` form |

```bash
curl -u admin https://hylla-control.internal/v1/backends
curl -u admin -T app.css https://hylla-control.internal/v1/mappings/public/0/objects/css/app.css
curl -u admin -X DELETE https://hylla-control.internal/v1/mappings/public/0/objects/css/app.css
```

## The UI

The UI is built with [Leptos](https://leptos.dev) and rendered on the server, and works
without JavaScript. Releases and the Docker image embed its hydration bundle; for builds
from source, `mise run ui` builds it into `target/site/pkg` before `cargo build`.
