# Minimal

One server in front of one S3 compatible bucket, with a memory-only cache. This is the
config the [quickstart](../../README.md#quickstart) runs.

- [config.styx](config.styx): the server and the backend.
- [secrets.styx](secrets.styx): the backend's credentials.

```bash
hylla --config config.styx --secrets secrets.styx
curl http://localhost:8080/app.css
```

For AWS S3, drop `endpoint`, set `region` to the bucket's region, and put real credentials
in `secrets.styx`.
