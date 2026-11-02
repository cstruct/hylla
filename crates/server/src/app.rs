use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use hylla_config::{Config, Router as MappingRouter};
use object_store::path::Path as ObjectPath;
use percent_encoding::percent_decode_str;

use anyhow::Context;
use hylla_config::Refresh;
use opentelemetry::KeyValue;
use tracing::{Instrument, field::Empty};

use crate::auth::{self, CompiledAuth, Verifiers};

use crate::{
    RefreshReport,
    backend::Stores,
    cache::CacheEvent,
    cache::{ObjectCache, ObjectMeta, ReadError},
    cluster::{self, Cluster},
    control::{self, ControlSettings, MappingEntry},
    listen,
    metrics::{self, METRICS},
    range::{self, RequestedRange},
    refresh,
    site::{self, Site},
    spans, telemetry,
    upstream::{self, Upstream},
};

/// The shared state of all listeners.
pub struct App {
    pub(crate) cache: Arc<ObjectCache>,
    pub(crate) stores: Stores,
    /// Whether public requests continue their callers' traces.
    follow_callers: bool,
    /// The cluster this node shares with, if any.
    pub(crate) cluster: Option<Arc<Cluster>>,
    /// The backends this node serves to other nodes, if any.
    pub(crate) upstream: Option<Upstream>,
    /// The control plane's settings, if it's configured.
    pub(crate) control: Option<Arc<ControlSettings>>,
    /// Every server's mappings, for the control plane.
    pub(crate) mappings: Vec<MappingEntry>,
    /// The type of each backend, such as `s3`, by backend name.
    pub(crate) backend_kinds: BTreeMap<String, &'static str>,
    /// The cache's events, until the cluster starts sharing them.
    cluster_events: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<CacheEvent>>>,
    /// Observable instruments, kept alive while the app is.
    _gauges: Vec<opentelemetry::metrics::ObservableGauge<u64>>,
    /// The refresh settings of the backends that have any, by backend name.
    refresh: BTreeMap<String, Refresh>,
    /// The servers of each listen address.
    listeners: BTreeMap<SocketAddr, Vec<Arc<Server>>>,
    /// The limits of every listener.
    pub(crate) limits: listen::Limits,
    /// The address of the health checks' listener, if any.
    pub(crate) health: Option<SocketAddr>,
    /// Set once every listener is serving.
    serving: std::sync::atomic::AtomicBool,
    /// Set when shutdown starts, telling the listeners to finish.
    stopping: tokio::sync::watch::Sender<bool>,
}

/// A configured server, with its mappings compiled.
struct Server {
    /// The server's name in the config, for metrics.
    name: String,
    /// The path prefix of each mapping, by mapping index, for metrics.
    prefixes: Vec<String>,
    /// The `Cache-Control` of each mapping's objects without one, by mapping index.
    cache_controls: Vec<Option<String>>,
    /// The exact host names it answers to, lowercased.
    names: Vec<String>,
    /// The host names its wildcards match one label in front of, lowercased.
    wildcards: Vec<String>,
    router: MappingRouter,
    /// The auth of each mapping, by mapping index.
    auth: Vec<Option<CompiledAuth>>,
    /// The site of each mapping, by mapping index.
    sites: Vec<Option<Site>>,
}

/// The server a request's host matched.
struct Matched<'a> {
    server: &'a Server,
    /// The host, as the request gave it, with its port.
    host: String,
    /// The label a wildcard server name matched, lowercased.
    label: Option<String>,
}

impl App {
    /// Builds the app from a validated config and the backends' object stores.
    pub async fn new(config: &Config, stores: Stores) -> anyhow::Result<Arc<Self>> {
        let verifiers = Verifiers::new(&config.auth)?;
        let mut listeners = BTreeMap::<SocketAddr, Vec<Arc<Server>>>::new();
        let mut servers = config.servers.iter().collect::<Vec<_>>();
        servers.sort_by_key(|(name, _)| *name);
        for (name, server) in servers.iter().copied() {
            let router = MappingRouter::new(server).map_err(|(mapping, rewrite, error)| {
                anyhow::anyhow!("server `{name}`, mapping {mapping}, rewrite {rewrite}: {error}")
            })?;
            let auth = server
                .mappings
                .iter()
                .enumerate()
                .map(|(index, mapping)| {
                    mapping
                        .auth
                        .as_ref()
                        .map(|auth| verifiers.mapping(auth))
                        .transpose()
                        .with_context(|| format!("server `{name}`, mapping {index}"))
                })
                .collect::<anyhow::Result<_>>()?;
            listeners
                .entry(server.listen)
                .or_default()
                .push(Arc::new(Server {
                    name: name.clone(),
                    prefixes: server
                        .mappings
                        .iter()
                        .map(|mapping| mapping.path_prefix.clone())
                        .collect(),
                    cache_controls: server
                        .mappings
                        .iter()
                        .map(|mapping| mapping.cache_control.clone())
                        .collect(),
                    names: server
                        .server_names
                        .iter()
                        .filter(|name| hylla_config::wildcard_suffix(name).is_none())
                        .map(|name| name.to_lowercase())
                        .collect(),
                    wildcards: server
                        .server_names
                        .iter()
                        .filter_map(|name| hylla_config::wildcard_suffix(name))
                        .map(str::to_lowercase)
                        .collect(),
                    router,
                    auth,
                    sites: server
                        .mappings
                        .iter()
                        .map(|mapping| mapping.site.clone().map(Site::new))
                        .collect(),
                }));
        }
        let refresh = config
            .backends
            .iter()
            .filter_map(|(name, backend)| Some((name.clone(), backend.refresh()?.clone())))
            .collect();
        let cache = Arc::new(ObjectCache::new(&config.cache).await?);
        let (cluster, cluster_events) = match &config.cluster {
            Some(cluster) => {
                let (sender, receiver) = tokio::sync::mpsc::channel(cluster::QUEUE);
                cache.report_to(sender);
                (Some(Arc::new(Cluster::new(cluster)?)), Some(receiver))
            }
            None => (None, None),
        };
        let control = config
            .control
            .as_ref()
            .map(ControlSettings::new)
            .transpose()?
            .map(Arc::new);
        let mut mappings = Vec::new();
        for (name, server) in &servers {
            for (index, mapping) in server.mappings.iter().enumerate() {
                mappings.push(MappingEntry {
                    server: (*name).clone(),
                    index,
                    path_prefix: mapping.path_prefix.clone(),
                    backend: mapping.backend.clone(),
                    writable: mapping.writable,
                    rewrites: !mapping.rewrites.is_empty(),
                    site: mapping.site.is_some(),
                });
            }
        }
        let backend_kinds = config
            .backends
            .iter()
            .map(|(name, backend)| {
                let kind = match backend {
                    hylla_config::Backend::S3 { .. } => "s3",
                    hylla_config::Backend::AzureBlob { .. } => "azure_blob",
                    hylla_config::Backend::Hylla { .. } => "hylla",
                };
                (name.clone(), kind)
            })
            .collect();
        let upstream = config
            .serve_backends
            .as_ref()
            .map(Upstream::new)
            .transpose()?;
        let mut gauges = Vec::new();
        if let Some(cluster) = &cluster {
            let cluster = Arc::downgrade(cluster);
            gauges.push(
                metrics::meter()
                    .u64_observable_gauge("hylla.cluster.members")
                    .with_description("Live nodes of the cluster, this one included")
                    .with_callback(move |observer| {
                        if let Some(cluster) = cluster.upgrade() {
                            observer.observe(cluster.members().len() as u64, &[]);
                        }
                    })
                    .build(),
            );
        }
        Ok(Arc::new(Self {
            cache,
            stores,
            follow_callers: config.telemetry.traces.follow_callers,
            _gauges: gauges,
            cluster,
            upstream,
            control,
            mappings,
            backend_kinds,
            cluster_events: std::sync::Mutex::new(cluster_events),
            refresh,
            listeners,
            limits: listen::Limits::new(&config.http),
            health: config.health.as_ref().map(|health| health.listen),
            serving: false.into(),
            stopping: tokio::sync::watch::Sender::new(false),
        }))
    }

    /// Tells the listeners to stop.
    pub(crate) fn shutdown(&self) -> listen::Shutdown {
        listen::Shutdown::new(&self.stopping)
    }

    /// Starts shutting down: listeners stop accepting connections, finish the requests in
    /// flight, and the node reports itself not ready.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
    }

    /// Marks every listener as serving.
    pub(crate) fn mark_serving(&self) {
        self.serving
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // r[impl health.ready]
    /// Whether the node serves requests: every listener started, and it isn't shutting
    /// down.
    pub fn ready(&self) -> bool {
        self.serving.load(std::sync::atomic::Ordering::Relaxed) && !*self.stopping.borrow()
    }

    /// The addresses to listen on.
    pub fn listen_addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.listeners.keys().copied()
    }

    /// The HTTP service of the listener on `listen`.
    pub fn listener(self: &Arc<Self>, listen: SocketAddr) -> axum::Router {
        axum::Router::new().fallback(handle).with_state(Listener {
            app: self.clone(),
            listen,
        })
    }

    /// Runs refresh rule `rule` of `backend` once.
    pub async fn refresh(&self, backend: &str, rule: usize) -> anyhow::Result<RefreshReport> {
        let rule_index = rule;
        let refresh = self
            .refresh
            .get(backend)
            .with_context(|| format!("backend `{backend}` has no refresh"))?;
        let rule = refresh
            .rules
            .get(rule)
            .with_context(|| format!("backend `{backend}` has no refresh rule {rule}"))?;
        let store = self
            .stores
            .get(backend)
            .with_context(|| format!("backend `{backend}` has no store"))?;
        // r[impl telemetry.traces.spans]
        let span = tracing::info_span!(
            "refresh",
            otel.status_code = Empty,
            hylla.backend = backend,
            hylla.rule = rule_index,
            hylla.listed = Empty,
            hylla.unchanged = Empty,
            hylla.changed = Empty,
            hylla.removed = Empty,
            hylla.preloaded = Empty,
            hylla.skipped = Empty,
            hylla.failed = Empty,
        );
        let report = refresh::run(&self.cache, backend, store.clone(), refresh, rule)
            .instrument(span.clone())
            .await
            .inspect_err(|_| spans::record_error(&span))?;
        span.record("hylla.listed", report.listed);
        // r[impl telemetry.metrics.refresh]
        let counts = [
            ("unchanged", report.unchanged),
            ("changed", report.changed),
            ("removed", report.removed),
            ("preloaded", report.preloaded),
            ("skipped", report.skipped),
            ("failed", report.failed),
        ];
        for (result, count) in counts {
            span.record(format!("hylla.{result}").as_str(), count);
        }
        for (result, count) in counts.into_iter().filter(|(_, count)| *count > 0) {
            METRICS.refresh_objects.add(
                count,
                &[
                    KeyValue::new("hylla.backend", backend.to_string()),
                    KeyValue::new("hylla.rule", rule_index as i64),
                    KeyValue::new("hylla.result", result),
                ],
            );
        }
        Ok(report)
    }

    /// Starts sharing with the cluster, if one is configured: its listener, gossip and the
    /// sharing of cache events.
    pub async fn start_cluster(
        self: &Arc<Self>,
    ) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
        let Some(cluster) = &self.cluster else {
            return Ok(Vec::new());
        };
        let events = self
            .cluster_events
            .lock()
            .expect("not poisoned")
            .take()
            .context("the cluster was already started")?;
        cluster::start(self, cluster, events).await
    }

    /// The health checks' service.
    pub fn health_router(self: &Arc<Self>) -> axum::Router {
        crate::health::router(self)
    }

    /// The control plane's service, if it's configured.
    pub fn control_router(self: &Arc<Self>) -> Option<axum::Router> {
        self.control
            .as_ref()
            .map(|settings| control::router(self, settings))
    }

    /// Starts the control plane, if configured.
    pub async fn start_control(
        self: &Arc<Self>,
    ) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
        match &self.control {
            Some(settings) => Ok(vec![control::start(self, settings).await?]),
            None => Ok(Vec::new()),
        }
    }

    /// Starts serving backends to other nodes, if configured.
    pub async fn start_upstream(
        self: &Arc<Self>,
    ) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
        match &self.upstream {
            Some(serving) => Ok(vec![upstream::start(self, serving).await?]),
            None => Ok(Vec::new()),
        }
    }

    /// The addresses of the live nodes of the cluster, this one included, or none without a
    /// cluster.
    pub fn cluster_members(&self) -> Vec<SocketAddr> {
        self.cluster
            .as_ref()
            .map(|cluster| cluster.members())
            .unwrap_or_default()
    }

    // r[impl cluster.refresh]
    /// Whether this node runs refresh rule `rule` of `backend`: always without a cluster,
    /// and when it owns the rule in one.
    pub fn owns_refresh_rule(&self, backend: &str, rule: usize) -> bool {
        self.cluster
            .as_ref()
            .is_none_or(|cluster| cluster.owns(&format!("refresh\n{backend}\n{rule}")))
    }

    // r[impl refresh.schedule]
    /// Runs every enabled refresh rule now and then on its interval, until the returned
    /// tasks are aborted.
    pub fn spawn_refresh(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = Vec::new();
        for (backend, refresh) in &self.refresh {
            for (index, rule) in refresh.rules.iter().enumerate() {
                if !rule.mode.revalidates() && !rule.mode.preloads() {
                    continue;
                }
                let app = self.clone();
                let backend = backend.clone();
                let every = rule.every.duration();
                // In a cluster, let membership settle first, so nodes don't all run every
                // rule at startup.
                let settle = self
                    .cluster
                    .as_ref()
                    .map(|cluster| cluster.settle_time())
                    .unwrap_or_default();
                tasks.push(tokio::spawn(async move {
                    tokio::time::sleep(settle).await;
                    let mut interval = tokio::time::interval(every);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        interval.tick().await;
                        if !app.owns_refresh_rule(&backend, index) {
                            continue;
                        }
                        match app.refresh(&backend, index).await {
                            Ok(report) => {
                                tracing::info!(backend, rule = index, ?report, "refreshed")
                            }
                            Err(error) => {
                                tracing::warn!(backend, rule = index, "refresh failed: {error:#}")
                            }
                        }
                    }
                }));
            }
        }
        tasks
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.cache.close().await
    }

    // r[impl server.hosts]
    // r[impl site.hosts.wildcard]
    // r[impl site.hosts.precedence]
    fn server(&self, listen: SocketAddr, request: &Request) -> Option<Matched<'_>> {
        // HTTP/2 requests carry their host in `:authority`, which ends up in the URI.
        let given = match request.headers().get(header::HOST) {
            Some(host) => host.to_str().ok()?,
            None => {
                let authority = request.uri().authority()?.as_str();
                authority.rsplit('@').next().unwrap_or(authority)
            }
        };
        // Strip the port, keeping bracketed IPv6 addresses intact.
        let host = match given.rsplit_once(':') {
            Some((name, port)) if !port.contains(']') => name,
            _ => given,
        }
        .to_lowercase();
        let servers = self.listeners.get(&listen)?;
        let (server, label) = match servers.iter().find(|server| server.names.contains(&host)) {
            Some(server) => (server, None),
            None => {
                // Wildcards match exactly one label in front of their name.
                let (label, rest) = host.split_once('.')?;
                if label.is_empty() {
                    return None;
                }
                let server = servers
                    .iter()
                    .find(|server| server.wildcards.iter().any(|suffix| suffix == rest))?;
                (server, Some(label.to_string()))
            }
        };
        Some(Matched {
            server,
            host: given.to_string(),
            label,
        })
    }
}

#[derive(Clone)]
struct Listener {
    app: Arc<App>,
    listen: SocketAddr,
}

/// What served a request, for its metrics.
#[derive(Default)]
struct RequestLabels {
    server: Option<String>,
    mapping: Option<String>,
}

// r[impl telemetry.metrics.requests]
// r[impl telemetry.traces.requests]
async fn handle(State(listener): State<Listener>, request: Request) -> Response {
    let started = std::time::Instant::now();
    // Anyone can make up methods, so other ones share one name and series.
    let method = match *request.method() {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        _ => "_OTHER",
    };
    // The span lasts until the body is sent, which holds it.
    let span = tracing::info_span!(
        "request",
        otel.name = method,
        otel.kind = "server",
        otel.status_code = Empty,
        http.request.method = method,
        http.response.status_code = Empty,
        url.path = Empty,
        hylla.server = Empty,
        hylla.mapping = Empty,
    );
    // r[impl telemetry.traces.propagation]
    if listener.app.follow_callers {
        spans::continue_trace(&span, request.headers());
    }
    if telemetry::record_paths() {
        span.record("url.path", request.uri().path());
    }
    let mut labels = RequestLabels::default();
    let response = respond(&listener, request, &mut labels)
        .instrument(span.clone())
        .await;
    let status = response.status();
    span.record("http.response.status_code", status.as_u16());
    if status.is_server_error() {
        spans::record_error(&span);
    }
    let mut attributes = vec![
        KeyValue::new("http.request.method", method),
        KeyValue::new("http.response.status_code", i64::from(status.as_u16())),
    ];
    if let Some(server) = labels.server {
        span.record("hylla.server", server.as_str());
        attributes.push(KeyValue::new("hylla.server", server));
    }
    if let Some(mapping) = labels.mapping {
        spans::rename(&span, format!("{method} {mapping}"));
        span.record("hylla.mapping", mapping.as_str());
        attributes.push(KeyValue::new("hylla.mapping", mapping));
    }
    // Recorded once the body is sent, or dropped, as when the client goes away.
    let recorded = Recorded {
        started,
        attributes,
    };
    response.map(|body| {
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            let _recorded = &recorded;
            chunk
        }))
    })
}

/// Records a request's duration when dropped, with its body.
struct Recorded {
    started: std::time::Instant,
    attributes: Vec<KeyValue>,
}

impl Drop for Recorded {
    fn drop(&mut self) {
        METRICS
            .requests
            .record(metrics::seconds(self.started.elapsed()), &self.attributes);
    }
}

async fn respond(listener: &Listener, request: Request, labels: &mut RequestLabels) -> Response {
    // r[impl server.methods]
    let head = match *request.method() {
        Method::GET => false,
        Method::HEAD => true,
        _ => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, "GET, HEAD")],
            )
                .into_response();
        }
    };
    let app = &listener.app;
    let Some(Matched {
        server,
        host,
        label,
    }) = app.server(listener.listen, &request)
    else {
        return (StatusCode::NOT_FOUND, "unknown host\n").into_response();
    };
    labels.server = Some(server.name.clone());
    // r[impl server.routes.decoded]
    // Decoded before routing, so `/%74enant` or `/tenant%2Fx` can't route past the mapping
    // its key falls under, and its auth.
    let Ok(path) = percent_decode_str(request.uri().path()).decode_utf8() else {
        return (StatusCode::BAD_REQUEST, "path isn't UTF-8\n").into_response();
    };
    // r[impl server.routes]
    let Some(route) = server.router.route(&path) else {
        return (StatusCode::NOT_FOUND, "no mapping\n").into_response();
    };
    labels.mapping = server.prefixes.get(route.mapping).cloned();
    let auth = server.auth.get(route.mapping).and_then(Option::as_ref);
    let private = auth.is_some_and(|auth| auth.private);

    if let Some(site) = server.sites.get(route.mapping).and_then(Option::as_ref) {
        let prefix = server.prefixes[route.mapping].trim_end_matches('/');
        let document = route.path.strip_prefix(prefix).unwrap_or(&route.path);
        let response = serve_site(
            app,
            site,
            route.backend,
            SiteRequest {
                host: &host,
                label: label.as_deref(),
                document,
                headers: request.headers(),
                uri: request.uri(),
                head,
                auth,
                cache_control: server
                    .cache_controls
                    .get(route.mapping)
                    .and_then(Option::as_deref),
            },
        )
        .await;
        return mark_private(response, private);
    }

    let key = route.path.trim_start_matches('/');
    if let Some(refused) = check_auth(auth, request.headers(), key).await {
        return mark_private(refused, private);
    }

    let request_headers = request.headers();
    let response = async {
        let Some(store) = app.stores.get(route.backend) else {
            return (StatusCode::BAD_GATEWAY, "backend unavailable\n").into_response();
        };
        if key.is_empty() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let path = ObjectPath::from(key);
        let cache_control = server
            .cache_controls
            .get(route.mapping)
            .and_then(Option::as_deref);
        serve_object(
            app,
            route.backend,
            store,
            path,
            request_headers,
            head,
            cache_control,
        )
        .await
    }
    .await;
    mark_private(response, private)
}

// r[impl auth.mappings]
/// Checks a request's token against `auth`, for backend key `key`, answering with the
/// refusal when it doesn't pass.
async fn check_auth(
    auth: Option<&CompiledAuth>,
    headers: &HeaderMap,
    key: &str,
) -> Option<Response> {
    let refusal = auth?.check(headers, &format!("/{key}")).await.err()?;
    match &refusal {
        auth::Refusal::InvalidToken(reason) => {
            tracing::debug!(reason, "refused an invalid token");
        }
        auth::Refusal::KeysUnavailable(reason) => {
            tracing::debug!(reason, "refused a token, without keys to check it with");
        }
        _ => {}
    }
    METRICS
        .auth_refusals
        .add(1, &[KeyValue::new("hylla.reason", refusal.reason())]);
    let (status, headers) = auth::refusal_response(&refusal);
    Some((status, headers).into_response())
}

/// A request to a site.
struct SiteRequest<'a> {
    /// The host, as the request gave it, with its port.
    host: &'a str,
    /// The label a wildcard server name matched.
    label: Option<&'a str>,
    /// The request's path within the mapping.
    document: &'a str,
    headers: &'a HeaderMap,
    uri: &'a http::Uri,
    head: bool,
    auth: Option<&'a CompiledAuth>,
    /// The mapping's `Cache-Control` of objects without one.
    cache_control: Option<&'a str>,
}

/// Serves a request to a site from the build it chooses, redirecting requests through an
/// alias in their host to the build's own host.
async fn serve_site(
    app: &Arc<App>,
    site: &Site,
    backend: &str,
    request: SiteRequest<'_>,
) -> Response {
    let Some(store) = app.stores.get(backend) else {
        return (StatusCode::BAD_GATEWAY, "backend unavailable\n").into_response();
    };
    let build = match site.build(app, backend, store, request.label).await {
        Ok(build) => build,
        Err(failure) => return failure.into_response(),
    };
    let headers = request.headers;
    // r[impl site.auth]
    let key = site.key(&build.sha, request.document);
    // r[impl site.redirect.missing-build]
    // Redirected without checking the build exists: its host answers 404 if it doesn't.
    if let Some(alias) = &build.alias
        && build.from_host
        && site.config().redirect
    {
        // Only to tokens allowed into the build too, so they can't learn where aliases point.
        let alias_key = format!("{}{alias}", site.config().aliases);
        for key in [&alias_key, &key] {
            if let Some(refused) = check_auth(request.auth, headers, key).await {
                return refused;
            }
        }
        let path_and_query = request
            .uri
            .path_and_query()
            .map_or("/", |path| path.as_str());
        return site::redirect(request.host, path_and_query, &build.sha);
    }
    if let Some(refused) = check_auth(request.auth, headers, &key).await {
        return refused;
    }
    // r[impl site.conditional]
    // Served as any other object, so conditional and range requests go by its own ETag.
    let serve = |key: &str| {
        serve_object(
            app,
            backend,
            store,
            ObjectPath::from(key),
            headers,
            request.head,
            request.cache_control,
        )
    };
    let mut response = serve(&key).await;
    // r[impl site.fallback.missing]
    if response.status() == StatusCode::NOT_FOUND
        && site::is_page_load(headers)
        && let Some(fallback) = site.fallback_key(&build.sha)
        && check_auth(request.auth, headers, &fallback).await.is_none()
    {
        response = serve(&fallback).await;
    }
    // r[impl site.alias-cache-control]
    if build.alias.is_some() {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header_value(&site.config().alias_cache_control),
        );
    }
    response
}

// r[impl auth.private]
/// Marks a response of an authenticated mapping as private, so shared caches don't serve it
/// to others.
fn mark_private(mut response: Response, private: bool) -> Response {
    if private {
        let headers = response.headers_mut();
        // The object's other directives still hold, such as how long it may be kept.
        let others = headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|directive| {
                        !directive.is_empty()
                            && !directive.eq_ignore_ascii_case("public")
                            && !directive.eq_ignore_ascii_case("private")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let value = if others.is_empty() {
            "private".to_string()
        } else {
            format!("private, {others}")
        };
        headers.insert(header::CACHE_CONTROL, header_value(&value));
    }
    response
}

pub(crate) async fn serve_object(
    app: &Arc<App>,
    backend: &str,
    store: &Arc<dyn object_store::ObjectStore>,
    path: ObjectPath,
    request_headers: &HeaderMap,
    head: bool,
    cache_control: Option<&str>,
) -> Response {
    let serve = || {
        serve_version(
            app,
            backend,
            store,
            path.clone(),
            request_headers,
            head,
            cache_control,
        )
    };
    match serve().await {
        Ok(response) => response,
        // r[impl server.changed]
        // Its metadata is forgotten, so it's fetched anew.
        Err(changed) => {
            tracing::debug!(backend, %path, "the object changed, serving it again: {changed:#}");
            serve().await.unwrap_or_else(|error| {
                tracing::warn!(backend, %path, "the object keeps changing: {error:#}");
                StatusCode::BAD_GATEWAY.into_response()
            })
        }
    }
}

/// Serves the version of an object whose metadata is cached, or fails when it changed
/// before anything was sent.
async fn serve_version(
    app: &Arc<App>,
    backend: &str,
    store: &Arc<dyn object_store::ObjectStore>,
    path: ObjectPath,
    request_headers: &HeaderMap,
    head: bool,
    cache_control: Option<&str>,
) -> Result<Response, anyhow::Error> {
    let route_backend = backend;

    let meta = match app
        .cache
        .metadata(route_backend, store.clone(), &path)
        .await
    {
        Ok(found) => found,
        Err(ReadError::NotFound) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(ReadError::Backend(error) | ReadError::Changed(error)) => {
            tracing::warn!(backend = route_backend, %path, "metadata: {error:#}");
            return Ok(StatusCode::BAD_GATEWAY.into_response());
        }
    };
    let (meta, age) = meta;
    let mut headers = object_headers(&meta);
    // r[impl server.cache-control]
    if !headers.contains_key(header::CACHE_CONTROL)
        && let Some(cache_control) = cache_control
    {
        headers.insert(header::CACHE_CONTROL, header_value(cache_control));
    }
    headers.insert(header::AGE, HeaderValue::from(age.as_secs()));

    // r[impl server.if-match]
    if let Some(if_match) = request_headers.get(header::IF_MATCH) {
        let matches = if_match.to_str().is_ok_and(|value| {
            value.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*" || meta.etag.as_deref() == Some(tag)
            })
        });
        if !matches {
            return Ok((StatusCode::PRECONDITION_FAILED, headers).into_response());
        }
    }

    // r[impl server.conditional]
    // Weak comparison, as RFC 9110 has it: proxies that compress responses weaken ETags.
    let weak = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    if let Some(if_none_match) = request_headers.get(header::IF_NONE_MATCH)
        && if_none_match.to_str().is_ok_and(|value| {
            value.split(',').any(|tag| {
                tag.trim() == "*"
                    || meta
                        .etag
                        .as_deref()
                        .is_some_and(|etag| weak(tag) == weak(etag))
            })
        })
    {
        return Ok((StatusCode::NOT_MODIFIED, headers).into_response());
    }

    // r[impl server.if-range]
    let if_range_matches = request_headers
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|if_range| {
            let if_range = if_range.trim();
            if if_range.starts_with('"') || if_range.starts_with("W/") {
                // Strong comparison: weak ETags never match.
                !if_range.starts_with("W/") && meta.etag.as_deref() == Some(if_range)
            } else {
                headers
                    .get(header::LAST_MODIFIED)
                    .is_some_and(|modified| modified.as_bytes() == if_range.as_bytes())
            }
        });
    let range = range::parse(
        request_headers
            .get(header::RANGE)
            .filter(|_| if_range_matches)
            .and_then(|value| value.to_str().ok()),
        meta.size,
    );
    let (status, range) = match range {
        RequestedRange::Full => (StatusCode::OK, 0..meta.size),
        RequestedRange::Partial(range) => {
            let content_range = format!("bytes {}-{}/{}", range.start, range.end - 1, meta.size);
            headers.insert(header::CONTENT_RANGE, header_value(&content_range));
            (StatusCode::PARTIAL_CONTENT, range)
        }
        RequestedRange::Unsatisfiable => {
            headers.insert(
                header::CONTENT_RANGE,
                header_value(&format!("bytes */{}", meta.size)),
            );
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response());
        }
    };
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(range.end - range.start),
    );
    // r[impl server.head]
    if head || range.is_empty() {
        return Ok((status, headers).into_response());
    }

    // r[impl server.get]
    let chunk_size = app.cache.chunk_size();
    let chunks = range.start / chunk_size..range.end.div_ceil(chunk_size);
    let first = chunks.start;
    let reader = Arc::new(ChunkReader {
        app: app.clone(),
        store: store.clone(),
        path: path.clone(),
        meta: meta.clone(),
        backend: route_backend.to_string(),
        range: range.clone(),
    });
    // Chunks read while the body is sent belong to the request's span too.
    let span = tracing::Span::current();
    let read = move |index: u64| {
        let reader = reader.clone();
        async move { reader.read(index).await }.instrument(span.clone())
    };

    // Read the first chunk before answering, so failures can still be reported as 502.
    let first_bytes = match read.clone()(first).await {
        Ok(bytes) => bytes,
        Err(ReadError::NotFound) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(ReadError::Changed(error)) => return Err(error),
        Err(ReadError::Backend(error)) => {
            tracing::warn!(backend = route_backend, %path, "chunk {first}: {error:#}");
            return Ok(StatusCode::BAD_GATEWAY.into_response());
        }
    };
    // r[impl server.read-ahead]
    let rest = stream::iter(chunks.skip(1))
        .map(read)
        .buffered(READ_AHEAD)
        .map(|chunk| {
            chunk.map_err(|error| match error {
                ReadError::NotFound => std::io::Error::other("object disappeared"),
                ReadError::Backend(error) | ReadError::Changed(error) => {
                    std::io::Error::other(format!("{error:#}"))
                }
            })
        });
    let body = stream::once(async move { Ok(first_bytes) }).chain(rest);
    Ok((status, headers, Body::from_stream(body)).into_response())
}

/// How many chunks after the one being sent are read at the same time.
const READ_AHEAD: usize = 4;

/// Reads the chunks of a response, trimmed to the requested range.
struct ChunkReader {
    app: Arc<App>,
    store: Arc<dyn object_store::ObjectStore>,
    path: ObjectPath,
    meta: ObjectMeta,
    backend: String,
    range: std::ops::Range<u64>,
}

impl ChunkReader {
    async fn read(&self, index: u64) -> Result<Bytes, ReadError> {
        let bytes = self
            .app
            .cache
            .chunk(
                &self.backend,
                self.store.clone(),
                &self.path,
                &self.meta,
                index,
            )
            .await?;
        let chunk_start = index * self.app.cache.chunk_size();
        let len = bytes.len() as u64;
        let from = self.range.start.saturating_sub(chunk_start).min(len) as usize;
        let to = (self.range.end - chunk_start).min(len) as usize;
        let bytes = bytes.slice(from..to);
        METRICS.served_bytes.add(
            bytes.len() as u64,
            &[KeyValue::new("hylla.backend", self.backend.clone())],
        );
        Ok(bytes)
    }
}

fn object_headers(meta: &ObjectMeta) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(etag) = &meta.etag {
        headers.insert(header::ETAG, header_value(etag));
    }
    headers.insert(
        header::LAST_MODIFIED,
        header_value(&httpdate::fmt_http_date(meta.last_modified)),
    );
    let content_type = meta
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    headers.insert(header::CONTENT_TYPE, header_value(content_type));
    let stored = &meta.headers;
    for (name, value) in [
        (header::CACHE_CONTROL, &stored.cache_control),
        (header::CONTENT_ENCODING, &stored.content_encoding),
        (header::CONTENT_DISPOSITION, &stored.content_disposition),
        (header::CONTENT_LANGUAGE, &stored.content_language),
    ] {
        if let Some(value) = value
            .as_deref()
            .and_then(|value| HeaderValue::from_str(value).ok())
        {
            headers.insert(name, value);
        }
    }
    headers
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""))
}
