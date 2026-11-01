# CDN in front of S3

A cache in front of a website's assets in S3, set up for production:

- **Rewrites** map public paths to bucket keys: `/assets/app.css` is `app.css`, and `/` is
  `pages/index.html`.
- **Refresh rules** revalidate cached objects every few minutes, keep the front page
  preloaded, and warm the cache with CSS and JavaScript.
- **`stale_while_revalidate`** serves cached objects past their TTL without waiting on S3,
  and **`stale_if_error`** keeps serving them for a week while S3 fails.
- **A disk cache** with `write_policy on_insertion`, so a restart keeps what's cached.
- **Health checks** on port 8081 and **metrics** over OTLP.

Mount a volume at `/var/cache/hylla` for the disk cache, writable by hylla's user. See
[Running in production](../../docs/guide/production.md).

Objects must have ETags, as S3 gives them, to be cached; see
[Troubleshooting](../../docs/guide/troubleshooting.md#nothing-is-cached).
