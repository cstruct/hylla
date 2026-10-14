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

use crate::auth::{self, CompiledAuth, Verifiers};

use crate::{
    RefreshReport,
    backend::Stores,
    cache::{ObjectCache, ObjectMeta, ReadError},
    range::{self, RequestedRange},
    refresh,
};

/// The shared state of all listeners.
pub struct App {
    cache: Arc<ObjectCache>,
    stores: Stores,
    /// The refresh settings of the backends that have any, by backend name.
    refresh: BTreeMap<String, Refresh>,
    /// The servers of each listen address.
    listeners: BTreeMap<SocketAddr, Vec<Arc<Server>>>,
}

/// A configured server, with its mappings compiled.
struct Server {
    names: Vec<String>,
    router: MappingRouter,
    /// The auth of each mapping, by mapping index.
    auth: Vec<Option<CompiledAuth>>,
}

impl App {
    /// Builds the app from a validated config and the backends' object stores.
    pub async fn new(config: &Config, stores: Stores) -> anyhow::Result<Arc<Self>> {
        let verifiers = Verifiers::new(&config.auth)?;
        let mut listeners = BTreeMap::<SocketAddr, Vec<Arc<Server>>>::new();
        let mut servers = config.servers.iter().collect::<Vec<_>>();
        servers.sort_by_key(|(name, _)| *name);
        for (name, server) in servers {
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
                    names: server
                        .server_names
                        .iter()
                        .map(|name| name.to_lowercase())
                        .collect(),
                    router,
                    auth,
                }));
        }
        let refresh = config
            .backends
            .iter()
            .filter_map(|(name, backend)| Some((name.clone(), backend.refresh()?.clone())))
            .collect();
        Ok(Arc::new(Self {
            cache: Arc::new(ObjectCache::new(&config.cache).await?),
            stores,
            refresh,
            listeners,
        }))
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
        refresh::run(&self.cache, backend, store.clone(), refresh, rule).await
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
                tasks.push(tokio::spawn(async move {
                    let mut interval = tokio::time::interval(every);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        interval.tick().await;
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
    fn server(&self, listen: SocketAddr, headers: &HeaderMap) -> Option<&Server> {
        let host = headers.get(header::HOST)?.to_str().ok()?;
        // Strip the port, keeping bracketed IPv6 addresses intact.
        let host = match host.rsplit_once(':') {
            Some((name, port)) if !port.contains(']') => name,
            _ => host,
        }
        .to_lowercase();
        self.listeners
            .get(&listen)?
            .iter()
            .find(|server| server.names.contains(&host))
            .map(Arc::as_ref)
    }
}

#[derive(Clone)]
struct Listener {
    app: Arc<App>,
    listen: SocketAddr,
}

async fn handle(State(listener): State<Listener>, request: Request) -> Response {
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
    let Some(server) = app.server(listener.listen, request.headers()) else {
        return (StatusCode::NOT_FOUND, "unknown host\n").into_response();
    };
    // r[impl server.routes]
    let Some(route) = server.router.route(request.uri().path()) else {
        return (StatusCode::NOT_FOUND, "no mapping\n").into_response();
    };
    let key = percent_decode_str(route.path.trim_start_matches('/')).decode_utf8_lossy();

    // r[impl auth.mappings]
    let auth = server.auth.get(route.mapping).and_then(Option::as_ref);
    let private = auth.is_some_and(|auth| auth.private);
    if let Some(auth) = auth
        && let Err(refusal) = auth.check(request.headers(), &format!("/{key}"))
    {
        if let auth::Refusal::InvalidToken(reason) = &refusal {
            tracing::debug!(reason, "refused an invalid token");
        }
        let (status, headers) = auth::refusal_response(&refusal);
        return mark_private((status, headers).into_response(), private);
    }

    let request_headers = request.headers();
    let response = async {
        let Some(store) = app.stores.get(route.backend) else {
            return (StatusCode::BAD_GATEWAY, "backend unavailable\n").into_response();
        };
        if key.is_empty() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let path = ObjectPath::from(key.as_ref());
        serve_object(app, route.backend, store, path, request_headers, head).await
    }
    .await;
    mark_private(response, private)
}

// r[impl auth.private]
/// Marks a response of an authenticated mapping as private, so shared caches don't serve it
/// to others.
fn mark_private(mut response: Response, private: bool) -> Response {
    if private {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    }
    response
}

async fn serve_object(
    app: &Arc<App>,
    backend: &str,
    store: &Arc<dyn object_store::ObjectStore>,
    path: ObjectPath,
    request_headers: &HeaderMap,
    head: bool,
) -> Response {
    let route_backend = backend;

    let meta = match app
        .cache
        .metadata(route_backend, store.clone(), &path)
        .await
    {
        Ok(meta) => meta,
        Err(ReadError::NotFound) => return StatusCode::NOT_FOUND.into_response(),
        Err(ReadError::Backend(error)) => {
            tracing::warn!(backend = route_backend, %path, "metadata: {error:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let mut headers = object_headers(&meta);

    // r[impl server.conditional]
    if let (Some(etag), Some(if_none_match)) =
        (&meta.etag, request_headers.get(header::IF_NONE_MATCH))
        && if_none_match.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|tag| tag.trim() == etag || tag.trim() == "*")
        })
    {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }

    let range = range::parse(
        request_headers
            .get(header::RANGE)
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
            return (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response();
        }
    };
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(range.end - range.start),
    );
    // r[impl server.head]
    if head || range.is_empty() {
        return (status, headers).into_response();
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
    let read = move |index: u64| {
        let reader = reader.clone();
        async move { reader.read(index).await }
    };

    // Read the first chunk before answering, so failures can still be reported as 502.
    let first_bytes = match read.clone()(first).await {
        Ok(bytes) => bytes,
        Err(ReadError::NotFound) => return StatusCode::NOT_FOUND.into_response(),
        Err(ReadError::Backend(error)) => {
            tracing::warn!(backend = route_backend, %path, "chunk {first}: {error:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let rest = stream::iter(chunks.skip(1)).then(read).map(|chunk| {
        chunk.map_err(|error| match error {
            ReadError::NotFound => std::io::Error::other("object disappeared"),
            ReadError::Backend(error) => std::io::Error::other(format!("{error:#}")),
        })
    });
    let body = stream::once(async move { Ok(first_bytes) }).chain(rest);
    (status, headers, Body::from_stream(body)).into_response()
}

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
        Ok(bytes.slice(from..to))
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
    headers
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""))
}
