//! Sharing revalidations and preloads between hylla nodes.
//!
//! Nodes find each other by gossiping their member lists over the cluster listener, joining
//! through seeds. Each refresh rule runs on one node, its owner by rendezvous hashing over
//! the live nodes. Metadata a node fetches from a backend is pushed to the others, and what
//! the owner of a rule preloads is announced to the others, which pull the chunks from it
//! rather than from the backend. Objects changed through a node's control plane are
//! announced too, so the others forget their metadata. Everything between nodes is signed with the cluster key,
//! and, with TLS configured, sent over mutual TLS.

mod sign;

use std::{
    collections::{HashMap, HashSet},
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

/// The most cache events waiting to be shared with other nodes. More are dropped and counted.
pub(crate) const QUEUE: usize = 10_000;

/// The most announced objects waiting to be pulled from other nodes. More are dropped and
/// counted, and fetched from the backend when requested.
const PULL_QUEUE: usize = 1_000;

/// How many announced objects are pulled at once.
const PULLS: usize = 4;

/// This node's view of the cluster.
pub struct Cluster {
    id: String,
    addr: SocketAddr,
    listen: SocketAddr,
    seeds: Vec<String>,
    gossip_interval: Duration,
    failure_timeout: Duration,
    /// How long a gossip exchange or a seed lookup may take.
    gossip_timeout: Duration,
    share: ClusterShare,
    signer: Signer,
    client: reqwest::Client,
    /// `https` with TLS configured, `http` otherwise.
    scheme: &'static str,
    /// The listener's TLS configuration, if TLS is configured.
    tls: Option<Arc<rustls::ServerConfig>>,
    heartbeat: AtomicU64,
    /// When requests refused for clock skew were last warned about, in Unix seconds.
    skew_warned: AtomicU64,
    /// The other nodes, by id.
    members: Mutex<HashMap<String, Member>>,
    /// The nodes a gossip exchange is in flight with, so a slow one isn't asked again
    /// before it answered.
    gossiping: Mutex<HashSet<SocketAddr>>,
    /// What each seed last resolved to, used while it doesn't resolve.
    seed_addrs: Mutex<HashMap<String, Seed>>,
    /// Announced objects waiting to be pulled, from the node that has them.
    pulls: mpsc::Sender<Pull>,
    pull_queue: Mutex<Option<mpsc::Receiver<Pull>>>,
    /// The objects queued or being pulled, by backend, key and ETag, so each is pulled once.
    pulling: Mutex<HashSet<(String, String, Option<String>)>>,
}

/// An announced object to pull from the node that has it.
struct Pull {
    origin: SocketAddr,
    shared: SharedMeta,
    /// The span of the updates announcing it, which its pull belongs to.
    span: Span,
}

/// What a seed resolved to.
#[derive(Default)]
struct Seed {
    /// The addresses it last resolved to.
    addrs: Vec<SocketAddr>,
    /// Whether it failed to resolve since, which is logged once.
    failing: bool,
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
    /// Objects whose metadata to forget; missing from nodes that predate it.
    #[serde(default)]
    forgotten: Vec<WireKey>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireKey {
    backend: String,
    key: String,
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
        let gossip_interval = config.gossip_interval.duration();
        let failure_timeout = config.failure_timeout.duration();
        // Long enough for a busy node to answer, short enough for several exchanges to fail
        // before the failure timeout, so one node that doesn't answer can't hold up the rest.
        let gossip_timeout = (failure_timeout / 4)
            .max(gossip_interval)
            .min(failure_timeout / 2);
        let client = reqwest::Client::builder().connect_timeout(gossip_timeout);
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
        let (pulls, pull_queue) = mpsc::channel(PULL_QUEUE);
        Ok(Self {
            // r[impl cluster.identity]
            // The start time makes a restarted node a new member, rather than one whose
            // heartbeat went backwards.
            id: format!("{addr}-{started}"),
            addr,
            listen: config.listen,
            seeds: config.seeds.clone(),
            gossip_interval,
            failure_timeout,
            gossip_timeout,
            share: config.share.clone(),
            signer: Signer::new(&config.key),
            client: client.build()?,
            scheme: if tls.is_some() { "https" } else { "http" },
            tls,
            heartbeat: AtomicU64::new(0),
            skew_warned: AtomicU64::new(0),
            members: Mutex::default(),
            gossiping: Mutex::default(),
            seed_addrs: Mutex::default(),
            pulls,
            pull_queue: Mutex::new(Some(pull_queue)),
            pulling: Mutex::default(),
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
    // r[impl cluster.gossip-timeout]
    /// Gossips with the live nodes and the seeds, resolving seed names again each time.
    ///
    /// The heartbeat advances every round, and each exchange runs on its own with a timeout,
    /// so a node or seed that doesn't answer doesn't slow gossip with the others.
    async fn gossip_round(self: &Arc<Self>) {
        self.heartbeat.fetch_add(1, Ordering::Relaxed);
        let mut targets = self
            .live_peers()
            .into_iter()
            .map(|(_, addr)| addr)
            .collect::<Vec<_>>();
        targets.extend(self.resolve_seeds().await);
        targets.sort();
        targets.dedup();
        targets.retain(|addr| *addr != self.addr && *addr != self.listen);

        let body = serde_json::to_vec(&self.view()).expect("serializable");
        for addr in targets {
            if !self.gossiping.lock().expect("not poisoned").insert(addr) {
                continue;
            }
            let cluster = self.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let response = cluster
                    .post(addr, "/cluster/gossip", body, cluster.gossip_timeout)
                    .await;
                cluster
                    .gossiping
                    .lock()
                    .expect("not poisoned")
                    .remove(&addr);
                match response.and_then(|bytes| Ok(serde_json::from_slice::<Gossip>(&bytes)?)) {
                    Ok(gossip) => cluster.merge(gossip),
                    Err(error) => {
                        METRICS.cluster_gossip_failures.add(1, &[]);
                        tracing::debug!(%addr, "gossip failed: {error:#}");
                    }
                }
            });
        }
    }

    /// The seeds' addresses, resolved at the same time, each within the gossip timeout. A
    /// seed that doesn't resolve keeps the addresses it last resolved to.
    async fn resolve_seeds(&self) -> Vec<SocketAddr> {
        let lookups = self.seeds.iter().map(|seed| async move {
            let lookup =
                tokio::time::timeout(self.gossip_timeout, tokio::net::lookup_host(seed.as_str()));
            let resolved = match lookup.await {
                Ok(Ok(addrs)) => Ok(addrs.collect::<Vec<_>>()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("timed out".to_string()),
            };
            (seed, resolved)
        });
        let resolved = join_all(lookups).await;
        let mut seeds = self.seed_addrs.lock().expect("not poisoned");
        let mut addrs = Vec::new();
        for (seed, resolved) in resolved {
            let state = seeds.entry(seed.clone()).or_default();
            match resolved {
                Ok(resolved) => {
                    state.addrs = resolved;
                    state.failing = false;
                }
                // Logged once until it resolves again, rather than every round.
                Err(error) if !state.failing => {
                    tracing::warn!(seed, "resolving a seed: {error}");
                    state.failing = true;
                }
                Err(_) => {}
            }
            addrs.extend(state.addrs.iter().copied());
        }
        addrs
    }

    /// Posts `body` to another node, returning its answer once its signature is checked.
    async fn post(
        &self,
        addr: SocketAddr,
        path: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> anyhow::Result<Bytes> {
        let signature = self.signer.sign("POST", path, &body);
        let response = self
            .client
            .post(format!("{}://{addr}{path}", self.scheme))
            .header(sign::HEADER, &signature)
            .header("content-type", "application/json")
            .headers(spans::trace_headers())
            .timeout(timeout)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            bail!("{} answered {}", addr, status);
        }
        let response_signature = response
            .headers()
            .get(sign::RESPONSE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await?;
        self.check_response(addr, &signature, response_signature, status, &bytes)?;
        Ok(bytes)
    }

    // r[impl cluster.auth.responses]
    /// Checks the signature of another node's answer to the request signed `request`.
    fn check_response(
        &self,
        addr: SocketAddr,
        request: &str,
        signature: Option<String>,
        status: StatusCode,
        body: &[u8],
    ) -> anyhow::Result<()> {
        let valid = signature.is_some_and(|signature| {
            self.signer
                .verify_response(&signature, request, status.as_u16(), body)
        });
        if !valid {
            bail!("{addr} answered without a valid signature");
        }
        Ok(())
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
            .header(sign::HEADER, &signature)
            .headers(spans::trace_headers())
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = response.status();
        if status != StatusCode::OK && status != StatusCode::NOT_FOUND {
            bail!("{addr} answered {status}");
        }
        let response_signature = response
            .headers()
            .get(sign::RESPONSE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await?;
        self.check_response(addr, &signature, response_signature, status, &bytes)?;
        Ok((status == StatusCode::OK).then_some(bytes))
    }

    // r[impl cluster.share-metadata]
    // r[impl cluster.share-preload]
    // r[impl cluster.share-forgets]
    /// Pushes batches of cache events to the live nodes.
    async fn broadcast(&self, mut events: mpsc::Receiver<CacheEvent>) {
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
                forgotten: Vec::new(),
            };
            for event in batch {
                match event {
                    CacheEvent::Fetched(shared) if self.share.metadata => {
                        updates.fetched.push(WireMeta::from(&shared));
                    }
                    CacheEvent::Preloaded(shared) if self.share.preload => {
                        updates.preloaded.push(WireMeta::from(&shared));
                    }
                    // Forgets are shared whatever is turned off: other nodes cache what
                    // they fetch themselves too.
                    CacheEvent::Forgotten { backend, key } => {
                        updates.forgotten.push(WireKey { backend, key });
                    }
                    _ => {}
                }
            }
            if updates.fetched.is_empty()
                && updates.preloaded.is_empty()
                && updates.forgotten.is_empty()
            {
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
                hylla.forgotten = updates.forgotten.len(),
                hylla.peers = peers.len(),
            );
            join_all(peers.into_iter().map(|(_, addr)| {
                let body = body.clone();
                async move {
                    let timeout = self.failure_timeout / 2;
                    if let Err(error) = self.post(addr, "/cluster/updates", body, timeout).await {
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
        ("forgotten", updates.forgotten.len()),
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
    events: mpsc::Receiver<CacheEvent>,
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
    let pull_queue = cluster
        .pull_queue
        .lock()
        .expect("not poisoned")
        .take()
        .context("the cluster was already started")?;
    let serving = tokio::spawn(crate::listen::serve(
        listener,
        router,
        cluster.tls.clone(),
        app.limits.clone(),
        app.shutdown(),
        "cluster",
    ));
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
        tokio::spawn(pull_queued(app.clone(), cluster.clone(), pull_queue)),
    ])
}

#[allow(
    clippy::result_large_err,
    reason = "the error is the response, as in axum"
)]
/// Verifies a request's signature, returning it, or answering 401 when it's missing or
/// invalid.
fn verify(
    cluster: &Cluster,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<String, Response> {
    let path_and_query = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    let signature = headers
        .get(sign::HEADER)
        .and_then(|value| value.to_str().ok());
    if let Some(signature) = signature
        && cluster
            .signer
            .verify(signature, method.as_str(), path_and_query, body)
    {
        return Ok(signature.to_string());
    }
    // r[impl cluster.clock-skew]
    // Refused for a reason that's easy to miss otherwise: warned about, once a minute.
    if let Some(skew) = signature.and_then(sign::skew) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = cluster.skew_warned.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 60
            && cluster
                .skew_warned
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                skew_secs = skew,
                "refused a request from another node: its clock is {skew}s off this one's; \
                 keep the nodes' clocks in sync, such as with NTP"
            );
        }
    }
    Err(StatusCode::UNAUTHORIZED.into_response())
}

// r[impl cluster.auth.responses]
/// An answer to the request signed `request`, signed in turn.
fn signed(cluster: &Cluster, request: &str, status: StatusCode, body: Bytes) -> Response {
    let signature = cluster
        .signer
        .sign_response(request, status.as_u16(), &body);
    (
        status,
        [
            (sign::RESPONSE_HEADER, signature),
            ("content-type", "application/json".to_string()),
        ],
        body,
    )
        .into_response()
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
    let request = match verify(cluster, &method, &uri, &headers, &body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match serde_json::from_slice::<Gossip>(&body) {
        Ok(gossip) => cluster.merge(gossip),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    }
    let view = serde_json::to_vec(&cluster.view()).expect("serializable");
    signed(cluster, &request, StatusCode::OK, view.into())
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
    let request = match verify(&cluster, &method, &uri, &headers, &body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let Ok(mut updates) = serde_json::from_slice::<Updates>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    record_updates("received", &updates, 1);
    let forgotten = std::mem::take(&mut updates.forgotten);
    if cluster.share.metadata {
        for wire in updates.fetched {
            app.cache.apply_metadata(wire.into()).await;
        }
    }
    if cluster.share.preload {
        for wire in updates.preloaded {
            let shared = SharedMeta::from(wire);
            app.cache.apply_metadata(shared.clone()).await;
            cluster.queue_pull(updates.origin, shared);
        }
    }
    // r[impl cluster.apply-forgets]
    // Last, so metadata fetched before a change in the same batch doesn't come back.
    for wire in forgotten {
        app.cache
            .forget(&wire.backend, &ObjectPath::from(wire.key.as_str()));
    }
    signed(&cluster, &request, StatusCode::NO_CONTENT, Bytes::new())
}

impl Cluster {
    // r[impl cluster.queue]
    /// Queues pulling an announced object, unless it's queued or being pulled already, or
    /// the queue is full.
    fn queue_pull(&self, origin: SocketAddr, shared: SharedMeta) {
        let id = (
            shared.backend.clone(),
            shared.key.clone(),
            shared.meta.etag.clone(),
        );
        if !self
            .pulling
            .lock()
            .expect("not poisoned")
            .insert(id.clone())
        {
            return;
        }
        let pull = Pull {
            origin,
            shared,
            span: Span::current(),
        };
        if self.pulls.try_send(pull).is_err() {
            self.pulling.lock().expect("not poisoned").remove(&id);
            METRICS
                .cluster_dropped
                .add(1, &[KeyValue::new("hylla.kind", "pull")]);
        }
    }
}

/// Pulls queued objects, a few at a time, until the queue closes.
async fn pull_queued(app: Arc<App>, cluster: Arc<Cluster>, mut queue: mpsc::Receiver<Pull>) {
    let pulls = Arc::new(Semaphore::new(PULLS));
    while let Some(Pull {
        origin,
        shared,
        span: parent,
    }) = queue.recv().await
    {
        let Ok(permit) = pulls.clone().acquire_owned().await else {
            return;
        };
        let app = app.clone();
        let cluster = cluster.clone();
        let span = tracing::info_span!(
            parent: &parent,
            "cluster.pull",
            hylla.backend = shared.backend,
            hylla.object = Empty,
        );
        spans::record_object(&span, &shared.key);
        tokio::spawn(
            async move {
                let id = (
                    shared.backend.clone(),
                    shared.key.clone(),
                    shared.meta.etag.clone(),
                );
                pull(&app, &cluster, origin, shared).await;
                cluster.pulling.lock().expect("not poisoned").remove(&id);
                drop(permit);
            }
            .instrument(span),
        );
    }
}

// r[impl cluster.pull]
/// Pulls the chunks of an object another node preloaded from it, falling back to the
/// backend for chunks it can't provide.
async fn pull(app: &App, cluster: &Cluster, origin: SocketAddr, shared: SharedMeta) {
    // r[impl cache.no-etag]
    let Some(etag) = shared.meta.etag.clone() else {
        return;
    };
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
    let request = match verify(cluster, &method, &uri, &headers, b"") {
        Ok(request) => request,
        Err(response) => return response,
    };
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
        Some(bytes) => signed(cluster, &request, StatusCode::OK, bytes),
        None => signed(cluster, &request, StatusCode::NOT_FOUND, Bytes::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use hylla_config::{ClusterShare, Interval};

    use super::{Cluster, PULL_QUEUE};
    use crate::cache::{ObjectMeta, SharedMeta};

    fn shared(key: &str, etag: &str) -> SharedMeta {
        SharedMeta {
            backend: "assets".into(),
            key: key.into(),
            meta: ObjectMeta {
                size: 1,
                etag: Some(etag.into()),
                last_modified: SystemTime::UNIX_EPOCH,
                content_type: None,
            },
            fetched: SystemTime::now(),
        }
    }

    // r[verify cluster.queue]
    #[test]
    fn announced_objects_are_queued_once_and_within_the_limit() {
        let cluster = Cluster::new(&hylla_config::Cluster {
            listen: "127.0.0.1:7946".parse().unwrap(),
            advertise: None,
            seeds: Vec::new(),
            gossip_interval: Interval::from_secs(1),
            failure_timeout: Interval::from_secs(10),
            key: "a cluster key of at least thirty-two characters".into(),
            tls: None,
            share: ClusterShare::default(),
        })
        .unwrap();
        assert_eq!(cluster.gossip_timeout, Duration::from_millis(2500));
        let origin = "127.0.0.1:7947".parse().unwrap();
        cluster.queue_pull(origin, shared("a", "1"));
        // The same version again is pulled once; another version is pulled too.
        cluster.queue_pull(origin, shared("a", "1"));
        cluster.queue_pull(origin, shared("a", "2"));
        for index in 0..PULL_QUEUE {
            cluster.queue_pull(origin, shared(&format!("more/{index}"), "1"));
        }
        let mut queue = cluster.pull_queue.lock().unwrap().take().unwrap();
        let mut queued = Vec::new();
        while let Ok(pull) = queue.try_recv() {
            queued.push((pull.shared.key, pull.shared.meta.etag.unwrap()));
        }
        assert_eq!(queued.len(), PULL_QUEUE);
        assert_eq!(queued[0], ("a".to_string(), "1".to_string()));
        assert_eq!(queued[1], ("a".to_string(), "2".to_string()));
        // What didn't fit isn't marked as being pulled, so a later announcement queues it.
        assert_eq!(cluster.pulling.lock().unwrap().len(), PULL_QUEUE);
    }
}
