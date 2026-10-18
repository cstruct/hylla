//! Serving backends to other hylla nodes, which use this node as a `hylla` backend.
//!
//! Objects are answered through the cache, as servers answer them, so this node fetches
//! each object from its backend once for every node using it. Only the configured backends
//! are served, over mutual TLS with nodes holding a certificate for the client name.

use std::{collections::BTreeSet, net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::TryStreamExt;
use hylla_config::ServeBackends;
use object_store::{ObjectStore, path::Path as ObjectPath};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, field::Empty};

use crate::{App, app::serve_object, spans};

/// The settings of serving backends, with the listener's TLS configuration loaded.
pub struct Upstream {
    listen: SocketAddr,
    backends: BTreeSet<String>,
    tls: Arc<rustls::ServerConfig>,
}

impl Upstream {
    // r[impl upstream.tls]
    pub fn new(config: &ServeBackends) -> anyhow::Result<Self> {
        let files = crate::tls::Files {
            ca: &config.tls.ca,
            cert: &config.tls.cert,
            key: &config.tls.key,
        };
        let tls = crate::tls::server_config(&files, &config.tls.client_name)
            .context("setting up TLS for serving backends")?;
        Ok(Self {
            listen: config.listen,
            backends: config.backends.iter().cloned().collect(),
            tls,
        })
    }
}

/// A listing of a backend's objects, as sent to the nodes using it.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Listing {
    pub objects: Vec<Listed>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Listed {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified_millis: i64,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    prefix: Option<String>,
}

/// Starts the listener serving `upstream`'s backends of `app`.
pub async fn start(
    app: &Arc<App>,
    upstream: &Upstream,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(upstream.listen)
        .await
        .with_context(|| format!("binding the serve_backends listener on {}", upstream.listen))?;
    tracing::info!(listen = %upstream.listen, backends = ?upstream.backends, "serving backends");
    let router = Router::new()
        .route("/backends/{backend}/objects/{*key}", get(object))
        .route("/backends/{backend}/list", get(list))
        .with_state(app.clone());
    Ok(tokio::spawn(crate::tls::serve(
        listener,
        router,
        upstream.tls.clone(),
        "serve_backends",
    )))
}

// r[impl upstream.backends]
/// The store of `backend`, if it's served, or the response refusing it.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response, as in axum"
)]
fn served(app: &App, backend: &str) -> Result<Arc<dyn ObjectStore>, Response> {
    let served = app
        .upstream
        .as_ref()
        .is_some_and(|upstream| upstream.backends.contains(backend));
    if !served {
        return Err((StatusCode::FORBIDDEN, "backend not served\n").into_response());
    }
    app.stores
        .get(backend)
        .cloned()
        .ok_or_else(|| (StatusCode::BAD_GATEWAY, "backend unavailable\n").into_response())
}

/// A key as sent by a `hylla` backend: the object path in its encoded form, so it maps to
/// the same object here.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response, as in axum"
)]
fn object_path(key: &str) -> Result<ObjectPath, Response> {
    ObjectPath::parse(key).map_err(|_| (StatusCode::BAD_REQUEST, "invalid key\n").into_response())
}

// r[impl upstream.objects]
async fn object(
    State(app): State<Arc<App>>,
    method: Method,
    Path((backend, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let span = tracing::info_span!(
        "upstream",
        otel.name = "upstream.object",
        otel.kind = "server",
        hylla.backend = backend,
        hylla.object = Empty,
    );
    spans::record_object(&span, &key);
    spans::continue_trace(&span, &headers);
    async {
        let store = match served(&app, &backend) {
            Ok(store) => store,
            Err(response) => return response,
        };
        let path = match object_path(&key) {
            Ok(path) => path,
            Err(response) => return response,
        };
        serve_object(
            &app,
            &backend,
            &store,
            path,
            &headers,
            method == Method::HEAD,
        )
        .await
    }
    .instrument(span)
    .await
}

// r[impl upstream.list]
async fn list(
    State(app): State<Arc<App>>,
    Path(backend): Path<String>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let span = tracing::info_span!(
        "upstream",
        otel.name = "upstream.list",
        otel.kind = "server",
        hylla.backend = backend,
    );
    spans::continue_trace(&span, &headers);
    async {
        let store = match served(&app, &backend) {
            Ok(store) => store,
            Err(response) => return response,
        };
        let prefix = match query.prefix.as_deref().filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => match object_path(prefix) {
                Ok(prefix) => Some(prefix),
                Err(response) => return response,
            },
            None => None,
        };
        let listed = spans::backend_call(
            &backend,
            "list",
            prefix.as_ref(),
            store.list(prefix.as_ref()).try_collect::<Vec<_>>(),
        )
        .await;
        match listed {
            Ok(objects) => Json(Listing {
                objects: objects
                    .into_iter()
                    .map(|meta| Listed {
                        key: meta.location.to_string(),
                        size: meta.size,
                        etag: meta.e_tag,
                        last_modified_millis: meta.last_modified.timestamp_millis(),
                    })
                    .collect(),
            })
            .into_response(),
            Err(error) => {
                tracing::warn!(backend, "listing for another node failed: {error}");
                StatusCode::BAD_GATEWAY.into_response()
            }
        }
    }
    .instrument(span)
    .await
}
