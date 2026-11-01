# Origin shield

Edge nodes near their clients read a bucket through a shield node next to it, so the
bucket serves each object once, to the shield, rather than once per edge.

- [shield/](shield): reads the bucket, and serves it to edges with `serve_backends`.
- [edge/](edge): reads it from the shield as a `hylla` backend.

The edges don't share their caches with the shield; they read from it as from object
storage, and it answers through its own cache. Edges are read only: their mappings can't be
writable.

Both sides use mutual TLS, with certificates from a CA you run for this:

- The shield's certificate is issued for the host in the edges' `url`, here
  `shield.internal`.
- The edges' certificates are issued for the shield's `client_name`, here `hylla.edge`.
- Both have the server and client auth usages.

The shield serves every object of its listed backends to any node with a certificate for
`client_name`, without its mappings' `auth`: put JWT auth on the edges' mappings.

Certificates and keys are read again when their files change, so cert-manager can rotate
them without a restart. So is the CA.
