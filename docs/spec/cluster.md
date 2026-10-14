# Cluster

Several hylla nodes can share revalidations and preloads, so each object is fetched from a
backend once per cluster rather than once per node. Every node holds everything; nodes don't
split the cache between them.

## Membership

r[cluster.membership]
Nodes MUST gossip their member lists, with a heartbeat per node, over the cluster listener.
A node whose heartbeat hasn't advanced for the failure timeout MUST be considered gone.

r[cluster.seeds]
A node MUST gossip with its seeds as well as the live nodes, resolving seed names again on
every round, so a DNS name returning every node lets nodes find each other.

r[cluster.identity]
A node's identity MUST include when it started, so a restarted node joins as a new member.

r[cluster.auth]
Every request between nodes MUST be signed with the shared cluster key, covering its method,
path and query, a timestamp and its body, and requests with a missing, invalid or more than
a minute old signature MUST be refused. The key itself MUST NOT be sent.

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

r[cluster.reannounce]
Preload runs MUST announce unchanged objects too, so nodes that joined later catch up on the
next run, pulling only the chunks they don't have.

r[cluster.pull]
A node MUST pull the chunks of an announced object from the node that preloaded it, falling
back to the backend for what that node can't provide.

r[cluster.chunks]
A node MUST serve other nodes chunks from its cache only, never fetching them from a backend
for them.
