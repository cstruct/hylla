# Examples

Configs for common setups, each with a `config.styx`, a `secrets.styx` to fill in, and a
README. Every one is loaded and validated by the tests, so they stay current.

| Example | What it shows |
|---|---|
| [minimal](minimal) | One server in front of one bucket: the quickstart's config |
| [cdn](cdn) | A website's assets: rewrites, refresh rules, a disk cache, stale content while S3 is down, health checks and metrics |
| [jwt-tenants](jwt-tenants) | Files per tenant behind JWTs, each tenant limited to its own prefix |
| [konklav](konklav) | Objects behind Konklav, which decides who holds `object:read` on each one |
| [control-plane](control-plane) | The web UI and JSON API, with Basic auth and sign-in through Konklav |
| [cluster-kubernetes](cluster-kubernetes) | Three nodes sharing their work in Kubernetes, with manifests |
| [origin-shield](origin-shield) | Edge nodes reading a bucket through a shield node next to it |
| [reference](reference) | Most settings in one annotated config |
