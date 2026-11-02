# Objects behind Konklav

Konklav decides who reads what, by the actions its roles grant callers, down to single
objects:

- `/object/{object_id}/...` requires an access token from Konklav whose caller holds
  `object:read` at the scope `object` with the id from the path, such as `o-1` for
  `/object/o-1/report.pdf`.
- `/reports/...` requires `report:read` anywhere in the token's tenant.
- `/public/...` needs no token.

In Konklav:

1. Declare the actions `object:read` and `report:read`, and the scope kind `object` with
   `"registered": true`, in its catalog, and grant them through roles.
2. Record each object as a registered scope, `PUT /v1/registered-scopes/object/{id}` with
   its tenant, before serving it, so Konklav refuses callers from other tenants.
3. Create a service principal for hylla with a secret key credential. Put the credential's
   id in `key_id`, and its secret in the secrets file.

`relying_url` is Konklav's relying surface, `subject_url` its subject-facing one, where hylla
logs in. Set `issuer` and `audience` to what Konklav's access tokens for hylla carry.

```bash
curl -H "Authorization: Bearer $TOKEN" https://files.example.com/object/o-1/report.pdf
```

Requests without a token get 401 with `WWW-Authenticate: Bearer`, tokens Konklav finds
invalid 401 with `error="invalid_token"`, and callers without the actions, or paths no rule
matches, 403. When Konklav asks for a fresh sign-in, the answer is 401 with
`error="insufficient_user_authentication"`. When Konklav can't be reached, or doesn't decide
within `timeout`, requests get 503: nothing is served without a decision.
