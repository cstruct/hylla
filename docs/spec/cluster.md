# Cluster

Several hylla nodes can share revalidations and preloads, so each object is fetched from a
backend once per cluster rather than once per node. Every node holds everything; nodes don't
split the cache between them.

## Membership

r[cluster.membership]
Nodes MUST gossip their member lists, with a heartbeat per node, over the cluster listener.
A node whose heartbeat hasn't advanced for the failure timeout MUST be considered gone,
counting from when the node it was learned from last saw it advance, not from when it was
learned of. Members at this node's address, or at another member's address with an earlier
start, MUST be dropped as earlier runs.

r[cluster.seeds]
A node MUST gossip with its seeds as well as the live nodes, resolving seed names again on
every round, so a DNS name returning every node lets nodes find each other.

r[cluster.gossip-timeout]
A node MUST advance its heartbeat every gossip round, and gossip with each node and resolve
each seed on its own, within a timeout shorter than the failure timeout, so a node or seed
that doesn't answer can't delay gossip with the others until they consider each other gone.
A seed that doesn't resolve MUST keep the addresses it last resolved to.

r[cluster.identity]
A node's identity MUST include when it started, so a restarted node joins as a new member.

r[cluster.auth]
Every request between nodes MUST be signed with the shared cluster key, covering its method,
path and query, a timestamp and its body, and requests with a missing, invalid or more than
a minute old signature MUST be refused. The key itself MUST NOT be sent.

r[cluster.clock-skew]
A request refused because its signature's timestamp is too far from now MUST be warned
about, at most once a minute, naming how far the clocks are apart.

r[cluster.auth.responses]
Every answer to a request between nodes MUST be signed with the cluster key too, covering
the request's signature, its status and its body, and answers with a missing or invalid
signature MUST be refused, so no one on the network path can add members or serve chunks,
with TLS or without.

r[cluster.connection-limits]
The cluster listener MUST accept at most 64 connections from one address at a time,
refusing more, so one client can't take the connections the nodes need.

r[cluster.tls]
With TLS configured, the cluster listener MUST only accept TLS connections, and nodes MUST
only accept each other, connecting or connected to, with a certificate issued by the
cluster CA for the configured server name, whatever address they are at. Requests MUST be
signed either way.

r[cluster.tls.rotate]
A node's certificate and key MUST be read again when their files change, for new
connections, keeping the ones in use when the new files can't be read or don't match.

## Sharing

r[cluster.ownership]
Every live node MUST agree which node owns a key, by rendezvous hashing over the live nodes,
so ownership moves when the owner leaves.

r[cluster.refresh]
A refresh rule MUST only run on the node that owns it.

r[cluster.share-metadata]
Metadata a node fetches from a backend MUST be pushed to the other live nodes, unless
sharing metadata is turned off.

r[cluster.apply-metadata]
A node MUST apply metadata from another node unless its own is at least as new, keeping the
original fetch time, so it expires as if it had been fetched locally.

r[cluster.share-preload]
Objects a node preloads MUST be announced to the other live nodes, unless sharing preloads
is turned off.

r[cluster.share-forgets]
Objects changed through a node's control plane MUST be announced to the other live nodes,
whatever sharing is turned off, so they forget their metadata too.

r[cluster.apply-forgets]
A node MUST forget the metadata of objects another node announces as changed, after applying
the metadata in the same update, so metadata fetched before the change doesn't stay.

r[cluster.forget-tombstones]
A node MUST remember when it last forgot an object's metadata, and MUST NOT apply metadata
from another node fetched before then, so a slow or replayed update can't bring back a
changed or deleted object.

r[cluster.forget-retries]
Forgets that fail to reach a live node MUST be sent to it again, up to three attempts in all,
with backoff.

r[cluster.reannounce]
Preload runs MUST announce unchanged objects too, so nodes that joined later catch up on the
next run, pulling only the chunks they don't have.

r[cluster.pull]
A node MUST pull the chunks of an announced object from the node that preloaded it, falling
back to the backend for what that node can't provide.

r[cluster.queue]
The updates waiting to be shared with other nodes, and the announced objects waiting to be
pulled, MUST each be held in a queue of bounded size, dropping and counting what doesn't
fit. An object MUST be queued for pulling once while it's queued or being pulled.

r[cluster.chunks]
A node MUST serve other nodes chunks from its cache only, never fetching them from a backend
for them.
