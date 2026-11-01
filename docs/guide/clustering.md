# Clustering

hylla nodes can share their work in two ways:

- **A cluster**: nodes in one place, behind one load balancer, share revalidations and
  preloads, so the backend serves each object once per cluster rather than once per node.
  Every node still holds everything it serves; nodes don't split the cache between them.
- **An origin shield**: nodes elsewhere read a backend through a node next to it, as
  their backend.

## A cluster

```styx
cluster {
  listen 0.0.0.0:7946      // private network only
  advertise 10.0.0.5:7946  // how the others reach this node
  seeds (hylla-cluster.hylla.svc.cluster.local:7946)
}
```

```styx
// secrets.styx
cluster {
  key replace-with-a-long-random-string  // the same on every node, at least 32 characters
}
```

What the nodes share:

- **Membership**: nodes gossip who's alive through their seeds and each other. A DNS name
  returning every node, such as a Kubernetes headless service, works as a seed; it's
  resolved again every round. A node unheard of for `failure_timeout` (10 seconds) is
  considered gone.
- **Refresh rules**: each rule runs on one node, chosen by hashing over the live nodes, so
  it moves when that node leaves.
- **Metadata**: what a node fetches from a backend is pushed to the others, so they don't
  each ask.
- **Preloads**: what a node preloads is announced, and the others pull its chunks from that
  node rather than from the backend.
- **Changes**: objects changed through a node's control plane are forgotten by every node.

Things to know:

- **Everything between nodes is signed** with the shared key: requests and answers both.
  So all nodes must run a hylla version that signs answers.
- **Signatures carry a timestamp**, and are refused when it's more than a minute off. Keep
  the nodes' clocks in sync, such as with NTP. hylla warns when it refuses a node for its
  clock.
- **Ownership of refresh rules is best-effort, not a lock.** While nodes join, leave or
  can't reach each other, a rule can run on two nodes, or on none until the failure timeout
  passes. That only costs backend requests, since refreshing is safe to repeat.
- **What's shared is bounded**: when other nodes are slow to take updates, or many objects
  are announced at once, what doesn't fit in the queues is dropped and counted in
  `hylla.cluster.dropped`. Nodes then fetch from the backend themselves.
- `share.metadata false` or `share.preload false` turns either off.
- **Forgets are retried**: a node that misses one because it didn't answer is sent it again,
  three tries in all. A node also refuses metadata from others fetched before it last forgot
  the object, so a late update can't bring back a changed object.
- **Restarted nodes replace their earlier runs**: a node at the same address with a later
  start replaces the one before, at once rather than after the failure timeout.
- **The cluster listener takes at most 64 connections from one address**, so one client
  can't take the connections the nodes need. Keep it on a private network all the same.

### Mutual TLS

Without `tls`, nodes talk plain HTTP, authenticated by the key but readable by anyone on the
path. With it, they only accept each other's certificates:

```styx
cluster {
  listen 0.0.0.0:7946
  advertise 10.0.0.5:7946
  seeds (hylla-cluster.hylla.svc.cluster.local:7946)
  tls {
    ca /etc/hylla/tls/ca.crt
    cert /etc/hylla/tls/tls.crt
    key /etc/hylla/tls/tls.key
    server_name hylla.cluster
  }
}
```

- Every node's certificate must be issued by `ca` for `server_name` as a DNS name, with
  both the server and client auth usages, since a node is both.
- Nodes are reached by address, which changes as they come and go, so certificates are
  checked for `server_name`, not for addresses.
- Use a CA for the cluster only: any certificate it issues for `server_name` is a node.
- The certificate and key are read again when their files change, so cert-manager can
  rotate them without a restart. The CA is read at startup.
- All nodes need `tls` on or off together.

[examples/cluster-kubernetes](../../examples/cluster-kubernetes) runs a cluster with mutual
TLS in Kubernetes.

## Origin shield

A node can serve its backends to other hylla nodes, which use it as a `hylla` backend. It
answers them through its cache, so it fetches each object from its own backend once for all
of them. They read from it as from object storage, without sharing their caches with it.
This is always over mutual TLS.

```styx
// The shield, serving its `assets` backend.
serve_backends {
  listen 0.0.0.0:7947
  backends (assets)
  tls {
    ca /etc/hylla/tls/ca.crt
    cert /etc/hylla/tls/tls.crt  // issued for shield.internal
    key /etc/hylla/tls/tls.key
    client_name hylla.edge
  }
}
```

```styx
// An edge using it.
backends {
  assets {
    type hylla
    url https://shield.internal:7947
    backend assets
    tls {
      ca /etc/hylla/tls/ca.crt
      cert /etc/hylla/tls/tls.crt  // issued for hylla.edge
      key /etc/hylla/tls/tls.key
    }
  }
}
```

- The shield's certificate must be issued for the host in the edges' `url`, and the edges'
  certificates for its `client_name`, by the CA each side trusts.
- Only the listed backends are served; others are refused.
- Refresh rules on the edges list through the shield, in one response, within the
  backend's `timeouts.request`.
- A `hylla` backend is read only, so mappings to it can't be writable.

[examples/origin-shield](../../examples/origin-shield) has both sides.
