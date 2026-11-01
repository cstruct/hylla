# Control plane, with Konklav

A control plane where people sign in through an OpenID provider and scripts with Basic
auth. See [the control plane guide](../../docs/guide/control-plane.md) for what it shows and
the JSON API.

1. Hash a password for the script user, and put the hash in `secrets.styx`:

   ```bash
   hylla hash-password
   ```

2. Register hylla as a client at the provider, with the redirect URI
   `https://hylla-control.example.com/auth/callback` and the post-logout redirect URI
   `https://hylla-control.example.com/`. With Konklav, give the client the `openid`,
   `email` and `groups` scopes and `visible_groups` such as `hylla-*`, and the people who
   should get in the global role `hylla-admin`.

3. Run hylla, and open `https://hylla-control.example.com` through the proxy in front of
   `127.0.0.1:9090`.

Scripts upload with the JSON API:

```bash
curl -u deploy -T app.css https://hylla-control.example.com/v1/mappings/public/0/objects/css/app.css
```
