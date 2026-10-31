# A cluster in Kubernetes

Three hylla nodes sharing their work, as a StatefulSet with a disk cache per pod. This is a
starting point to adapt, not a tested deployment.

- [config.styx](config.styx): a cluster with mutual TLS, a disk cache, health checks and
  JSON logs.
- [secrets.styx](secrets.styx): the cluster key and the backend's credentials.
- [hylla.yaml](hylla.yaml): the services, the StatefulSet, a PodDisruptionBudget and a
  cert-manager Certificate.

```bash
kubectl create namespace hylla
kubectl -n hylla create configmap hylla-config --from-file=config.styx
kubectl -n hylla create secret generic hylla-secrets --from-file=secrets.styx
kubectl -n hylla apply -f hylla.yaml
```

Things to know:

- Each pod advertises its own address through `HYLLA__CLUSTER__ADVERTISE`, from its pod IP.
  The `advertise` in `config.styx` is only a placeholder.
- The headless `hylla-cluster` service publishes pods that aren't ready, so nodes can find
  each other while they start.
- `/readyz` returns 503 once a pod starts shutting down, so it leaves the `hylla` service
  before its connections close. `terminationGracePeriodSeconds` must be longer than
  `http.shutdown_timeout`, or the pod is killed before its cache is written to disk.
- All nodes must run the same hylla version: nodes sign their answers to each other, and
  refuse nodes that don't.
- Keep the nodes' clocks in sync; signatures more than a minute off are refused.
- The cluster key and credentials can only come from `secrets.styx`, not the environment.

See [Clustering](../../docs/guide/clustering.md) and
[Running in production](../../docs/guide/production.md).
