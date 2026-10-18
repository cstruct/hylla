//! Sharing revalidations and preloads between hylla nodes.
//!
//! Nodes find each other by gossiping their member lists over the cluster listener, joining
//! through seeds. Each refresh rule runs on one node, its owner by rendezvous hashing over
//! the live nodes. Metadata a node fetches from a backend is pushed to the others, and what
//! the owner of a rule preloads is announced to the others, which pull the chunks from it
//! rather than from the backend. Everything between nodes is signed with the cluster key,
//! and, with TLS configured, sent over mutual TLS.

mod sign;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::future::join_all;
use hylla_config::ClusterShare;
use object_store::path::Path as ObjectPath;
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc};
use tracing::{Instrument, Span, field::Empty};

use crate::{
    App,
    cache::{CacheEvent, ChunkKey, ObjectMeta, SharedMeta},
    metrics::METRICS,
    spans,
};

use self::sign::Signer;

/// This node's view of the cluster.
pub struct Cluster {
    id: String,
    addr: SocketAddr,
    listen: SocketAddr,
    seeds: Vec<String>,
    gossip_interval: Duration,
    failure_timeout: Duration,
    share: ClusterShare,
    signer: Signer,
    client: reqwest::Client,
    /// `https` with TLS configured, `http` otherwise.
    scheme: &'static str,
    /// The listener's TLS configuration, if TLS is configured.
    tls: Option<Arc<rustls::ServerConfig>>,
    heartbeat: AtomicU64,
    /// The other nodes, by id.
    members: Mutex<HashMap<String, Member>>,
    /// Limits how many preloaded objects are pulled from other nodes at once.
    pulls: Semaphore,
}

struct Member {
    addr: SocketAddr,
    heartbeat: u64,
    /// When its heartbeat last advanced.
    last_seen: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemberInfo {
    id: String,
    addr: SocketAddr,
    heartbeat: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Gossip {
    members: Vec<MemberInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Updates {
    /// The node that has the preloaded objects.
    origin: SocketAddr,
    fetched: Vec<WireMeta>,
    preloaded: Vec<WireMeta>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMeta {
    backend: String,
    key: String,
    size: u64,
    etag: Option<String>,
    last_modified_millis: u64,
    content_type: Option<String>,
    fetched_millis: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChunkQuery {
    backend: String,
    key: String,
    etag: String,
    index: u64,
}

impl Cluster {
    pub fn new(config: &hylla_config::Cluster) -> anyhow::Result<Self> {
        let addr = config.advertise.unwrap_or(config.listen);
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let client = reqwest::Client::builder().connect_timeout(config.failure_timeout.duration());
        let (client, tls) = match &config.tls {
            // r[impl cluster.tls]
            Some(tls) => {
                let files = crate::tls::Files {
                    ca: &tls.ca,
                    cert: &tls.cert,
                    key: &tls.key,
                };
                let server = crate::tls::server_config(&files, &tls.server_name)
                    .context("setting up cluster TLS")?;
                let client = client.use_preconfigured_tls(
                    crate::tls::client_config(&files, Some(&tls.server_name))
                        .context("setting up cluster TLS")?,
                );
                (client, Some(server))
            }
            None => (client, None),
        };
        Ok(Self {
            // r[impl cluster.identity]
            // The start time makes a restarted node a new member, rather than one whose
            // heartbeat went backwards.
            id: format!("{addr}-{started}"),
            addr,
            listen: config.listen,
            seeds: config.seeds.clone(),
            gossip_interval: config.gossip_interval.duration(),
            failure_timeout: config.failure_timeout.duration(),
            share: config.share.clone(),
            signer: Signer::new(&config.key),
            client: client.build()?,
            scheme: if tls.is_some() { "https" } else { "http" },
            tls,
            heartbeat: AtomicU64::new(0),
            members: Mutex::default(),
            pulls: Semaphore::new(4),
        })
    }

    /// How long membership takes to settle after starting.
    pub fn settle_time(&self) -> Duration {
        self.gossip_interval * 3
    }

    /// The addresses of the live nodes, this one included, sorted.
    pub fn members(&self) -> Vec<SocketAddr> {
        let mut members = self
            .live_peers()
            .into_iter()
            .map(|(_, addr)| addr)
            .chain([self.addr])
            .collect::<Vec<_>>();
        members.sort();
        members
    }

    fn live_peers(&self) -> Vec<(String, SocketAddr)> {
        let members = self.members.lock().expect("not poisoned");
        members
            .iter()
            .filter(|(_, member)| member.last_seen.elapsed() < self.failure_timeout)
            .map(|(id, member)| (id.clone(), member.addr))
            .collect()
    }

    // r[impl cluster.ownership]
    /// Whether this node owns `key`: it has the highest score of the live nodes, by
    /// rendezvous hashing, so every node agrees without coordinating.
    pub fn owns(&self, key: &str) -> bool {
        let score = |id: &str| {
            let digest = aws_lc_rs::digest::digest(
                &aws_lc_rs::digest::SHA256,
                format!("{id}\n{key}").as_bytes(),
            );
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&digest.as_ref()[..8]);
            (u64::from_be_bytes(bytes), id.to_string())
        };
        let own = score(&self.id);
        self.live_peers().iter().all(|(id, _)| score(id) < own)
    }

    fn view(&self) -> Gossip {
        let mut members = vec![MemberInfo {
            id: self.id.clone(),
            addr: self.addr,
            heartbeat: self.heartbeat.load(Ordering::Relaxed),
        }];
        let known = self.members.lock().expect("not poisoned");
        members.extend(
            known
                .iter()
                .filter(|(_, member)| member.last_seen.elapsed() < self.failure_timeout)
                .map(|(id, member)| MemberInfo {
                    id: id.clone(),
                    addr: member.addr,
                    heartbeat: member.heartbeat,
                }),
        );
        Gossip { members }
    }

    // r[impl cluster.membership]
    /// Merges another node's view: members whose heartbeat advanced are seen now.
    fn merge(&self, gossip: Gossip) {
        let mut members = self.members.lock().expect("not poisoned");
        for info in gossip.members {
            if info.id == self.id {
                continue;
            }
            match members.get_mut(&info.id) {
                Some(member) if info.heartbeat <= member.heartbeat => {}
                Some(member) => {
                    member.addr = info.addr;
                    member.heartbeat = info.heartbeat;
                    member.last_seen = Instant::now();
                }
                None => {
                    members.insert(
                        info.id,
                        Member {
                            addr: info.addr,
                            heartbeat: info.heartbeat,
                            last_seen: Instant::now(),
                        },
                    );
                }
            }
        }
        // Forget nodes long gone, so their ids don't pile up.
        let forget_after = self.failure_timeout * 3;
        members.retain(|_, member| member.last_seen.elapsed() < forget_after);
    }

    // r[impl cluster.seeds]
    /// Gossips with the live nodes and the seeds, resolving seed names again each time.
    async fn gossip_round(&self) {
        self.heartbeat.fetch_add(1, Ordering::Relaxed);
        let mut targets = self
            .live_peers()
            .into_iter()
            .map(|(_, addr)| addr)
            .collect::<Vec<_>>();
        for seed in &self.seeds {
            match tokio::net::lookup_host(seed.as_str()).await {
                Ok(addrs) => targets.extend(addrs),
                Err(error) => tracing::debug!(seed, "resolving a seed: {error}"),
            }
        }
        targets.sort();
        targets.dedup();
        targets.retain(|addr| *addr != self.addr && *addr != self.listen);

        let body = serde_json::to_vec(&self.view()).expect("serializable");
        let responses = join_all(targets.into_iter().map(|addr| {
            let body = body.clone();
            async move { (addr, self.post(addr, "/cluster/gossip", body).await) }
        }))
        .await;
        for (addr, response) in responses {
            match response.and_then(|bytes| Ok(serde_json::from_slice::<Gossip>(&bytes)?)) {
                Ok(gossip) => self.merge(gossip),
                Err(error) => {
                    METRICS.cluster_gossip_failures.add(1, &[]);
                    tracing::debug!(%addr, "gossip failed: {error:#}");
                }
            }
        }
    }

    async fn post(&self, addr: SocketAddr, path: &str, body: Vec<u8>) -> anyhow::Result<Bytes> {
        let signature = self.signer.sign("POST", path, &body);
        let response = self
            .client
            .post(format!("{}://{addr}{path}", self.scheme))
            .header(sign::HEADER, signature)
            .header("content-type", "application/json")
            .headers(spans::trace_headers())
            .timeout(self.failure_timeout)
            .body(body)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("{} answered {}", addr, response.status());
        }
        Ok(response.bytes().await?)
    }

    /// A chunk from another node's cache, if it has it.
    async fn fetch_chunk(
        &self,
        addr: SocketAddr,
        chunk: &ChunkKey,
    ) -> anyhow::Result<Option<Bytes>> {
        let query = serde_urlencoded::to_string(ChunkQuery {
            backend: chunk.backend.clone(),
            key: chunk.key.clone(),
            etag: chunk.etag.clone(),
            index: chunk.index,
        })?;
        let path_and_query = format!("/cluster/chunk?{query}");
        let signature = self.signer.sign("GET", &path_and_query, b"");
        let response = self
            .client
            .get(format!("{}://{addr}{path_and_query}", self.scheme))
            .header(sign::HEADER, signature)
            .headers(spans::trace_headers())
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        match response.status() {
            StatusCode::OK => Ok(Some(response.bytes().await?)),
            StatusCode::NOT_FOUND => Ok(None),
            status => bail!("{addr} answered {status}"),
        }
    }

    // r[impl cluster.share-metadata]
    // r[impl cluster.share-preload]
    /// Pushes batches of cache events to the live nodes.
    async fn broadcast(&self, mut events: mpsc::UnboundedReceiver<CacheEvent>) {
        while let Some(event) = events.recv().await {
            let mut batch = vec![event];
            let deadline = tokio::time::sleep(Duration::from_millis(100));
            tokio::pin!(deadline);
            while batch.len() < 1000 {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(event) => batch.push(event),
                        None => break,
                    },
                    () = &mut deadline => break,
                }
            }
            let mut updates = Updates {
                origin: self.addr,
                fetched: Vec::new(),
                preloaded: Vec::new(),
            };
            for event in batch {
                match event {
                    CacheEvent::Fetched(shared) if self.share.metadata => {
                        updates.fetched.push(WireMeta::from(&shared));
                    }
                    CacheEvent::Preloaded(shared) if self.share.preload => {
                        updates.preloaded.push(WireMeta::from(&shared));
                    }
                    _ => {}
                }
            }
            if updates.fetched.is_empty() && updates.preloaded.is_empty() {
                continue;
            }
            let body = serde_json::to_vec(&updates).expect("serializable");
            let peers = self.live_peers();
            // r[impl telemetry.metrics.cluster]
            record_updates("sent", &updates, peers.len() as u64);
            // r[impl telemetry.traces.cluster]
            let span = tracing::info_span!(
                "cluster.share",
                otel.kind = "producer",
                otel.status_code = Empty,
                hylla.fetched = updates.fetched.len(),
                hylla.preloaded = updates.preloaded.len(),
                hylla.peers = peers.len(),
            );
            join_all(peers.into_iter().map(|(_, addr)| {
                let body = body.clone();
                async move {
                    if let Err(error) = self.post(addr, "/cluster/updates", body).await {
                        spans::record_error(&Span::current());
                        tracing::warn!(%addr, "sharing updates failed: {error:#}");
                    }
                }
            }))
            .instrument(span)
            .await;
        }
    }
}

/// Counts the updates in a batch sent to `peers` nodes, or received.
fn record_updates(direction: &'static str, updates: &Updates, peers: u64) {
    for (kind, count) in [
        ("fetched", updates.fetched.len()),
        ("preloaded", updates.preloaded.len()),
    ] {
        if count > 0 {
            METRICS.cluster_updates.add(
                count as u64 * peers,
                &[
                    KeyValue::new("hylla.direction", direction),
                    KeyValue::new("hylla.kind", kind),
                ],
            );
        }
    }
}

impl From<&SharedMeta> for WireMeta {
    fn from(shared: &SharedMeta) -> Self {
        Self {
            backend: shared.backend.clone(),
            key: shared.key.clone(),
            size: shared.meta.size,
            etag: shared.meta.etag.clone(),
            last_modified_millis: millis(shared.meta.last_modified),
            content_type: shared.meta.content_type.clone(),
            fetched_millis: millis(shared.fetched),
        }
    }
}

impl From<WireMeta> for SharedMeta {
    fn from(wire: WireMeta) -> Self {
        Self {
            backend: wire.backend,
            key: wire.key,
            meta: ObjectMeta {
                size: wire.size,
                etag: wire.etag,
                last_modified: from_millis(wire.last_modified_millis),
                content_type: wire.content_type,
            },
            fetched: from_millis(wire.fetched_millis),
        }
    }
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn from_millis(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

/// Starts the cluster listener and the gossip and broadcast tasks of `app`'s cluster.
pub async fn start(
    app: &Arc<App>,
    cluster: &Arc<Cluster>,
    events: mpsc::UnboundedReceiver<CacheEvent>,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let listener = tokio::net::TcpListener::bind(cluster.listen)
        .await
        .with_context(|| format!("binding the cluster listener on {}", cluster.listen))?;
    tracing::info!(listen = %cluster.listen, id = cluster.id, "cluster listening");
    let router = Router::new()
        .route("/cluster/gossip", post(gossip))
        .route("/cluster/updates", post(updates))
        .route("/cluster/chunk", get(chunk))
        .with_state(app.clone());

    let gossiping = cluster.clone();
    let broadcasting = cluster.clone();
    let serving = match &cluster.tls {
        Some(tls) => tokio::spawn(crate::tls::serve(listener, router, tls.clone(), "cluster")),
        None => tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!("cluster listener failed: {error}");
            }
        }),
    };
    Ok(vec![
        serving,
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(gossiping.gossip_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                gossiping.gossip_round().await;
            }
        }),
        tokio::spawn(async move { broadcasting.broadcast(events).await }),
    ])
}

/// Verifies a request's signature, answering 401 when it's missing or invalid.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response, as in axum"
)]
fn verify(
    cluster: &Cluster,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), Response> {
    let path_and_query = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    let valid = headers
        .get(sign::HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|signature| {
            cluster
                .signer
                .verify(signature, method.as_str(), path_and_query, body)
        });
    if valid {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED.into_response())
    }
}

#[allow(
    clippy::result_large_err,
    reason = "the error is the response, as in axum"
)]
fn cluster_of(app: &App) -> Result<&Arc<Cluster>, Response> {
    app.cluster
        .as_ref()
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

async fn gossip(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let cluster = match cluster_of(&app) {
        Ok(cluster) => cluster,
        Err(response) => return response,
    };
    if let Err(response) = verify(cluster, &method, &uri, &headers, &body) {
        return response;
    }
    match serde_json::from_slice::<Gossip>(&body) {
        Ok(gossip) => cluster.merge(gossip),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    }
    let view = serde_json::to_vec(&cluster.view()).expect("serializable");
    ([("content-type", "application/json")], view).into_response()
}

/// A span of a request from another node, continuing its trace.
fn peer_span(name: &'static str, headers: &HeaderMap) -> Span {
    let span = tracing::info_span!(
        "peer",
        otel.name = name,
        otel.kind = "server",
        hylla.result = Empty,
    );
    spans::continue_trace(&span, headers);
    span
}

async fn updates(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let span = peer_span("cluster.updates", &headers);
    receive_updates(app, method, uri, headers, body)
        .instrument(span)
        .await
}

async fn receive_updates(
    app: Arc<App>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let cluster = match cluster_of(&app) {
        Ok(cluster) => cluster.clone(),
        Err(response) => return response,
    };
    if let Err(response) = verify(&cluster, &method, &uri, &headers, &body) {
        return response;
    }
    let Ok(updates) = serde_json::from_slice::<Updates>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    record_updates("received", &updates, 1);
    if cluster.share.metadata {
        for wire in updates.fetched {
            app.cache.apply_metadata(wire.into()).await;
        }
    }
    if cluster.share.preload {
        for wire in updates.preloaded {
            let shared = SharedMeta::from(wire);
            app.cache.apply_metadata(shared.clone()).await;
            let app = app.clone();
            let cluster = cluster.clone();
            let origin = updates.origin;
            let span = tracing::info_span!(
                "cluster.pull",
                hylla.backend = shared.backend,
                hylla.object = Empty,
            );
            spans::record_object(&span, &shared.key);
            tokio::spawn(
                async move { pull(&app, &cluster, origin, shared).await }.instrument(span),
            );
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

// r[impl cluster.pull]
/// Pulls the chunks of an object another node preloaded from it, falling back to the
/// backend for chunks it can't provide.
async fn pull(app: &App, cluster: &Cluster, origin: SocketAddr, shared: SharedMeta) {
    let Ok(_permit) = cluster.pulls.acquire().await else {
        return;
    };
    let etag = shared.meta.etag.clone().unwrap_or_default();
    for index in 0..app.cache.chunk_count(shared.meta.size) {
        let key = ChunkKey {
            backend: shared.backend.clone(),
            key: shared.key.clone(),
            etag: etag.clone(),
            index,
        };
        if app.cache.cached_chunk(&key).await.is_some() {
            continue;
        }
        match cluster.fetch_chunk(origin, &key).await {
            Ok(Some(bytes)) => {
                METRICS
                    .cluster_pulled_chunks
                    .add(1, &[KeyValue::new("hylla.source", "peer")]);
                app.cache.insert_chunk(key, bytes);
            }
            outcome => {
                if let Err(error) = outcome {
                    tracing::warn!(%origin, key = shared.key, "pulling a chunk failed: {error:#}");
                }
                // The origin can't help: fetch what's missing from the backend instead.
                let Some(store) = app.stores.get(&shared.backend) else {
                    return;
                };
                let path = ObjectPath::from(shared.key.as_str());
                METRICS
                    .cluster_pulled_chunks
                    .add(1, &[KeyValue::new("hylla.source", "backend")]);
                if let Err(error) = app
                    .cache
                    .preload(&shared.backend, store.clone(), &path, &shared.meta)
                    .await
                {
                    tracing::warn!(
                        key = shared.key,
                        "preloading from the backend failed: {error:?}"
                    );
                }
                return;
            }
        }
    }
}

// r[impl cluster.chunks]
async fn chunk(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let span = peer_span("cluster.chunk", &headers);
    serve_chunk(app, method, uri, headers)
        .instrument(span)
        .await
}

async fn serve_chunk(app: Arc<App>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let cluster = match cluster_of(&app) {
        Ok(cluster) => cluster,
        Err(response) => return response,
    };
    if let Err(response) = verify(cluster, &method, &uri, &headers, b"") {
        return response;
    }
    let Ok(query) = serde_urlencoded::from_str::<ChunkQuery>(uri.query().unwrap_or_default())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let key = ChunkKey {
        backend: query.backend,
        key: query.key,
        etag: query.etag,
        index: query.index,
    };
    // Only what's cached: other nodes fall back to the backend themselves.
    let cached = app.cache.cached_chunk(&key).await;
    Span::current().record(
        "hylla.result",
        if cached.is_some() { "hit" } else { "miss" },
    );
    match cached {
        Some(bytes) => bytes.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
