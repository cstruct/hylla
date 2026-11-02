# Frontend builds per commit

Static frontend builds served by commit, with aliases CI moves without a config reload:

- **Previews** on `*.preview.example.com`. `<sha>.preview.example.com` serves that build, and
  `pr1873.preview.example.com` redirects to the SHA host of the build `pr1873` points at, so
  a browser stays on one build while the alias moves.
- **The live site** on `www.example.com`, serving whatever `live` points at, answered with
  `Cache-Control: no-cache` since the same URLs serve another build once it moves.
- **Single page apps**: page loads of paths a build doesn't have get its `index.html`.
- **A private writable mapping** for CI, on `127.0.0.1:8090`, and the control plane on
  `127.0.0.1:9090`, whose writes every node picks up at once.

The bucket holds builds under `builds/<sha>/`, and aliases under `aliases/<name>`, each an
object whose body is the SHA it points at:

```bash
# Upload a build, with a long Cache-Control, since a build never changes.
aws s3 sync dist/ "s3://frontend/builds/$GITHUB_SHA/" \
  --cache-control "public, max-age=31536000, immutable"

# Point the PR's alias at it, through the control plane, so it moves at once.
curl --fail -u "ci:$HYLLA_CI_PASSWORD" -X PUT --data "$GITHUB_SHA" \
  "http://127.0.0.1:9090/v1/mappings/ci/0/objects/aliases/pr$PR_NUMBER"
```

Writing an alias straight to the bucket works too: it's picked up within `metadata_ttl`.

Wildcard hosts need a wildcard DNS record and certificate for `*.preview.example.com`.
Each SHA host is its own origin, so storage and cookies without a `Domain` don't carry
over from one preview build to the next. See
[sites](../../docs/guide/configuration.md#sites).
