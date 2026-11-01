# hylla

hylla is a caching layer in front of object storage. Clients read and write objects
through hylla, which keeps frequently used objects in a hybrid RAM and disk cache.

This is the initial scope; requirements are refined as the design takes shape.

## Backends

r[backend.s3]
hylla MUST support Amazon S3 compatible object storage as a backend.

r[backend.azure-blob]
hylla MUST support Azure Blob Storage as a backend.

r[backend.hylla]
hylla MUST support another hylla node serving backends as a backend, read only.

r[backend.timeouts]
Requests to a backend MUST fail when connecting takes longer than the backend's connect
timeout, or the whole request longer than its request timeout, so a backend that hangs can't
hold requests or refresh runs forever.

r[backend.retries]
Requests to S3 and Azure Blob Storage that fail with a server error, or without an answer,
MUST be retried at most the backend's `retries` times, with backoff, and not once the request
timeout has passed since the first attempt, so an outage is noticed, and stale objects
served, within about the request timeout.

## Cache

r[cache.ram]
hylla MUST cache objects in RAM.

r[cache.disk]
hylla MUST cache objects on local disk.

## Configuration

r[config.ordna]
hylla MUST load its configuration with ordna.
