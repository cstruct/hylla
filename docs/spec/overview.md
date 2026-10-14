# hylla

hylla is a caching layer in front of object storage. Clients read and write objects
through hylla, which keeps frequently used objects in a hybrid RAM and disk cache.

This is the initial scope; requirements are refined as the design takes shape.

## Backends

r[backend.s3]
hylla MUST support Amazon S3 compatible object storage as a backend.

r[backend.azure-blob]
hylla MUST support Azure Blob Storage as a backend.

## Cache

r[cache.ram]
hylla MUST cache objects in RAM.

r[cache.disk]
hylla MUST cache objects on local disk.

## Configuration

r[config.ordna]
hylla MUST load its configuration with ordna.
