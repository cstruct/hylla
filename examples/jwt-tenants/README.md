# Files per tenant, behind JWTs

Each tenant reads only its own files, with a JWT from your identity provider:

- `/tenants/...` requires an `Authorization: Bearer` token signed by the `customers`
  verifier's key, current by its `exp` and `nbf`, issued by `https://id.example.com` for the
  audience `files.example.com`.
- `paths ("/tenants/${tenant_id}")` limits each token to keys under its `tenant_id` claim.
  A token for `acme` gets 403 for `/tenants/other/...`.
- `/public/...` needs no token.

Replace `public_key` with your provider's key, PEM encoded, and `algorithm` with the one it
signs with: one of RS256 to RS512, PS256 to PS512, ES256, ES384 or EdDSA. Shared secret
algorithms aren't supported.

```bash
curl -H "Authorization: Bearer $TOKEN" https://files.example.com/tenants/acme/report.pdf
```

Requests without a token get 401 with `WWW-Authenticate: Bearer`, invalid tokens 401 with
`error="invalid_token"`, and tokens for another tenant 403. Paths with `.` or `..` segments
get 400.
