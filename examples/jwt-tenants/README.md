# Files per tenant, behind JWTs

Each tenant reads only its own files, with a JWT from your identity provider:

- `/tenants/...` requires an `Authorization: Bearer` token signed with one of the keys of
  `https://id.example.com`, current by its `exp` and `nbf`, issued by it for the audience
  `files.example.com`.
- `paths ("/tenants/${tenant_id}")` limits each token to keys under its `tenant_id` claim.
  A token for `acme` gets 403 for `/tenants/other/...`.
- `/public/...` needs no token.

Replace `issuer` with your provider's, and `algorithm` with the one it signs with: one of
RS256 to RS512, PS256 to PS512, ES256, ES384 or EdDSA. Shared secret algorithms aren't
supported. hylla reads the provider's keys through its discovery document, and reads them
again when it rotates them. For a provider without one, set `jwks_uri` to where it publishes
its keys; for a fixed key, set `public_key`, PEM encoded.

A token's claims can be lists too: with `paths ("/teams/${groups}")`, a token with
`"groups": ["red", "blue"]` reads under `/teams/red` and `/teams/blue`.

```bash
curl -H "Authorization: Bearer $TOKEN" https://files.example.com/tenants/acme/report.pdf
```

Requests without a token get 401 with `WWW-Authenticate: Bearer`, invalid tokens 401 with
`error="invalid_token"`, and tokens for another tenant 403. Paths with `.` or `..` segments
get 400.
