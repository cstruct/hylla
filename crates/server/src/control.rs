//! The control plane: a web UI, rendered with Leptos, and a JSON API, showing the status of
//! each backend and browsing the mappings, changing the objects of writable ones.
//!
//! Everything is behind HTTP Basic auth, and requests that change anything are refused when
//! a browser says they come from another site, since browsers send Basic credentials along
//! with any request to the control plane.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::Context;
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{any, get, post, put},
};
use base64::Engine;
use futures::TryStreamExt;
use http::{HeaderMap, Method, StatusCode, header};
use hylla_control::{BackendInfo, CallInfo, ControlPlane, Listing, MappingInfo, ObjectEntry};
use leptos::prelude::{LeptosOptions, provide_context};
use leptos_axum::{LeptosRoutes, generate_route_list, handle_server_fns_with_context};
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};
use serde::Deserialize;

use crate::{App, spans, status};

/// The UI's hydration bundle, as `mise run ui` builds it. Without it, the UI is served
/// rendered on the server only, which works without JavaScript.
#[derive(rust_embed::RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../target/site/pkg"]
#[allow_missing = true]
struct Bundle;

/// How long a verified password is remembered, so argon2 doesn't run for every request.
const VERIFIED_FOR: Duration = Duration::from_secs(600);

/// The control plane's settings, with its users' password hashes checked.
pub struct ControlSettings {
    listen: SocketAddr,
    /// The PHC strings of the users' argon2 password hashes, by user name.
    users: HashMap<String, String>,
    max_upload: usize,
    /// The SHA-256 of `Authorization` headers verified, and when.
    verified: Mutex<HashMap<Vec<u8>, Instant>>,
}

impl ControlSettings {
    pub fn new(config: &hylla_config::Control) -> anyhow::Result<Self> {
        for (name, user) in &config.users {
            PasswordHash::new(&user.password_hash)
                .map_err(|error| anyhow::anyhow!("{error}"))
                .with_context(|| format!("the password hash of control user `{name}`"))?;
        }
        Ok(Self {
            listen: config.listen,
            users: config
                .users
                .iter()
                .map(|(name, user)| (name.clone(), user.password_hash.clone()))
                .collect(),
            max_upload: usize::try_from(config.max_upload_mib.saturating_mul(1024 * 1024))
                .unwrap_or(usize::MAX),
            verified: Mutex::default(),
        })
    }

    // r[impl control.auth]
    /// Whether an `Authorization` header holds a user's name and password.
    fn check(&self, authorization: &str) -> bool {
        let key = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, authorization.as_bytes())
            .as_ref()
            .to_vec();
        {
            let verified = self.verified.lock().expect("not poisoned");
            if verified
                .get(&key)
                .is_some_and(|at| at.elapsed() < VERIFIED_FOR)
            {
                return true;
            }
        }
        let Some(credentials) = authorization
            .strip_prefix("Basic ")
            .and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .ok()
            })
            .and_then(|decoded| String::from_utf8(decoded).ok())
        else {
            return false;
        };
        let Some((user, password)) = credentials.split_once(':') else {
            return false;
        };
        let valid = self
            .users
            .get(user)
            .and_then(|hash| PasswordHash::new(hash).ok())
            .is_some_and(|hash| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &hash)
                    .is_ok()
            });
        if valid {
            let mut verified = self.verified.lock().expect("not poisoned");
            verified.retain(|_, at| at.elapsed() < VERIFIED_FOR);
            verified.insert(key, Instant::now());
        }
        valid
    }
}

/// A mapping of a server, for the control plane.
pub struct MappingEntry {
    pub server: String,
    pub index: usize,
    pub path_prefix: String,
    pub backend: String,
    pub writable: bool,
    pub rewrites: bool,
}

impl MappingEntry {
    fn info(&self) -> MappingInfo {
        MappingInfo {
            server: self.server.clone(),
            index: self.index,
            path_prefix: self.path_prefix.clone(),
            backend: self.backend.clone(),
            writable: self.writable,
        }
    }

    /// The backend key of the mapping's prefix, such as `assets` for `/assets`, or empty for
    /// `/`.
    fn prefix_key(&self) -> &str {
        self.path_prefix.trim_matches('/')
    }

    /// The backend key of `path`, relative to the mapping, refusing paths that would leave
    /// it, such as with `..`.
    fn key(&self, path: &str) -> Result<ObjectPath, ControlError> {
        if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
            return Err(ControlError::InvalidPath(path.to_string()));
        }
        let key = match self.prefix_key() {
            "" => path.to_string(),
            prefix => format!("{prefix}/{path}"),
        };
        ObjectPath::parse(&key).map_err(|_| ControlError::InvalidPath(path.to_string()))
    }

    /// `key` relative to the mapping.
    fn relative(&self, key: &str) -> String {
        match self.prefix_key() {
            "" => key.to_string(),
            prefix => key
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(key)
                .to_string(),
        }
    }
}

#[derive(Debug)]
enum ControlError {
    UnknownMapping,
    NotWritable,
    /// Mappings with rewrites can't be listed: their keys don't map back to their paths.
    Rewrites,
    InvalidPath(String),
    NotFound,
    Backend(object_store::Error),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownMapping => write!(f, "no such mapping"),
            Self::NotWritable => write!(f, "the mapping isn't writable"),
            Self::Rewrites => write!(
                f,
                "the mapping has rewrites, so its backend keys don't map back to its paths"
            ),
            Self::InvalidPath(path) => write!(f, "`{path}` isn't a path within the mapping"),
            Self::NotFound => write!(f, "no such object"),
            Self::Backend(error) => write!(f, "the backend failed: {error}"),
        }
    }
}

impl ControlError {
    fn status(&self) -> StatusCode {
        match self {
            Self::UnknownMapping | Self::NotFound => StatusCode::NOT_FOUND,
            Self::NotWritable => StatusCode::FORBIDDEN,
            Self::Rewrites => StatusCode::CONFLICT,
            Self::InvalidPath(_) => StatusCode::BAD_REQUEST,
            Self::Backend(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        (self.status(), format!("{self}\n")).into_response()
    }
}

impl From<object_store::Error> for ControlError {
    fn from(error: object_store::Error) -> Self {
        match error {
            object_store::Error::NotFound { .. } => Self::NotFound,
            error => Self::Backend(error),
        }
    }
}

/// What the control plane does, for both the UI and the JSON API.
struct Plane {
    app: Arc<App>,
}

impl Plane {
    fn mapping(&self, server: &str, index: usize) -> Result<&MappingEntry, ControlError> {
        self.app
            .mappings
            .iter()
            .find(|mapping| mapping.server == server && mapping.index == index)
            .ok_or(ControlError::UnknownMapping)
    }

    fn store(&self, mapping: &MappingEntry) -> Result<Arc<dyn ObjectStore>, ControlError> {
        self.app
            .stores
            .get(&mapping.backend)
            .cloned()
            .ok_or(ControlError::UnknownMapping)
    }

    // r[impl control.writes]
    fn writable(&self, server: &str, index: usize) -> Result<&MappingEntry, ControlError> {
        let mapping = self.mapping(server, index)?;
        if mapping.writable {
            Ok(mapping)
        } else {
            Err(ControlError::NotWritable)
        }
    }

    // r[impl control.browse]
    async fn list(
        &self,
        server: &str,
        index: usize,
        prefix: &str,
    ) -> Result<Listing, ControlError> {
        let mapping = self.mapping(server, index)?;
        if mapping.rewrites {
            return Err(ControlError::Rewrites);
        }
        if !(prefix.is_empty() || prefix.ends_with('/')) {
            return Err(ControlError::InvalidPath(prefix.to_string()));
        }
        let base = match prefix.trim_end_matches('/') {
            "" => match mapping.prefix_key() {
                "" => None,
                key => Some(
                    ObjectPath::parse(key)
                        .map_err(|_| ControlError::InvalidPath(prefix.to_string()))?,
                ),
            },
            folder => Some(mapping.key(folder)?),
        };
        let store = self.store(mapping)?;
        let listed = spans::backend_call(
            &mapping.backend,
            "list",
            base.as_ref(),
            store.list_with_delimiter(base.as_ref()),
        )
        .await;
        let (folders, objects) = match listed {
            Ok(listed) => (
                listed
                    .common_prefixes
                    .iter()
                    .map(|folder| format!("{}/", mapping.relative(folder.as_ref())))
                    .collect::<Vec<_>>(),
                listed.objects,
            ),
            // Stores that only list everything, such as other hylla nodes: group what's
            // deeper into folders here.
            Err(object_store::Error::NotSupported { .. }) => {
                let all = spans::backend_call(
                    &mapping.backend,
                    "list",
                    base.as_ref(),
                    store.list(base.as_ref()).try_collect::<Vec<_>>(),
                )
                .await?;
                let mut folders = Vec::new();
                let mut objects = Vec::new();
                for object in all {
                    let relative = mapping.relative(object.location.as_ref());
                    match relative
                        .strip_prefix(prefix)
                        .and_then(|rest| rest.split_once('/'))
                    {
                        Some((folder, _)) => folders.push(format!("{prefix}{folder}/")),
                        None => objects.push(object),
                    }
                }
                folders.sort();
                folders.dedup();
                (folders, objects)
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Listing {
            mapping: mapping.info(),
            prefix: prefix.to_string(),
            folders,
            objects: objects
                .into_iter()
                .map(|object| ObjectEntry {
                    path: mapping.relative(object.location.as_ref()),
                    size: object.size,
                    etag: object.e_tag,
                    last_modified_millis: object.last_modified.timestamp_millis(),
                })
                .collect(),
        })
    }

    async fn put(
        &self,
        server: &str,
        index: usize,
        path: &str,
        bytes: Bytes,
    ) -> Result<(), ControlError> {
        let mapping = self.writable(server, index)?;
        let key = mapping.key(path)?;
        let store = self.store(mapping)?;
        spans::backend_call(
            &mapping.backend,
            "put",
            Some(&key),
            store.put(&key, bytes.into()),
        )
        .await?;
        // r[impl control.invalidate]
        self.app.cache.forget(&mapping.backend, &key);
        Ok(())
    }

    async fn delete(&self, server: &str, index: usize, path: &str) -> Result<(), ControlError> {
        let mapping = self.writable(server, index)?;
        let key = mapping.key(path)?;
        let store = self.store(mapping)?;
        spans::backend_call(&mapping.backend, "delete", Some(&key), store.delete(&key)).await?;
        self.app.cache.forget(&mapping.backend, &key);
        Ok(())
    }

    async fn rename(
        &self,
        server: &str,
        index: usize,
        from: &str,
        to: &str,
    ) -> Result<(), ControlError> {
        let mapping = self.writable(server, index)?;
        let (from, to) = (mapping.key(from)?, mapping.key(to)?);
        let store = self.store(mapping)?;
        spans::backend_call(
            &mapping.backend,
            "rename",
            Some(&from),
            store.rename(&from, &to),
        )
        .await?;
        self.app.cache.forget(&mapping.backend, &from);
        self.app.cache.forget(&mapping.backend, &to);
        Ok(())
    }

    fn backends(&self) -> Vec<BackendInfo> {
        let info = |call: status::Call| CallInfo {
            at_millis: call
                .at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            operation: call.operation.to_string(),
            outcome: call.outcome.to_string(),
            duration_millis: call.duration.as_millis() as u64,
            error: call.error,
        };
        self.app
            .backend_kinds
            .iter()
            .map(|(name, kind)| {
                let status = status::of(name);
                BackendInfo {
                    name: name.clone(),
                    kind: (*kind).to_string(),
                    last: status.last.map(info),
                    last_error: status.last_error.map(info),
                }
            })
            .collect()
    }

    fn mappings(&self) -> Vec<MappingInfo> {
        self.app.mappings.iter().map(MappingEntry::info).collect()
    }
}

#[async_trait::async_trait]
impl ControlPlane for Plane {
    fn backends(&self) -> Vec<BackendInfo> {
        Plane::backends(self)
    }

    fn mappings(&self) -> Vec<MappingInfo> {
        Plane::mappings(self)
    }

    async fn list(&self, server: &str, index: usize, prefix: &str) -> Result<Listing, String> {
        Plane::list(self, server, index, prefix)
            .await
            .map_err(|error| error.to_string())
    }

    async fn delete(&self, server: &str, index: usize, path: &str) -> Result<(), String> {
        Plane::delete(self, server, index, path)
            .await
            .map_err(|error| error.to_string())
    }

    async fn rename(&self, server: &str, index: usize, from: &str, to: &str) -> Result<(), String> {
        Plane::rename(self, server, index, from, to)
            .await
            .map_err(|error| error.to_string())
    }
}

// r[impl control.ui]
/// The control plane's service.
pub fn router(app: &Arc<App>, settings: &Arc<ControlSettings>) -> Router {
    let plane = Arc::new(Plane { app: app.clone() });
    let shared: Arc<dyn ControlPlane> = plane.clone();
    let context = move || provide_context(shared.clone());
    let hydrate = Bundle::get("hylla.js").is_some();
    let options = LeptosOptions::builder()
        .output_name("hylla")
        .site_pkg_dir("pkg")
        .build();
    // Generated once: generating renders the app in a mode of its own, which a render
    // running alongside, such as another router's, can see.
    static ROUTES: std::sync::OnceLock<Vec<leptos_axum::AxumRouteListing>> =
        std::sync::OnceLock::new();
    let routes = ROUTES
        .get_or_init(|| generate_route_list(hylla_control::App))
        .clone();
    let ui = Router::new()
        .leptos_routes_with_context(&options, routes, context.clone(), {
            let options = options.clone();
            move || hylla_control::shell(options.clone(), hydrate)
        })
        .route(
            "/api/{*function}",
            any(move |request: Request| handle_server_fns_with_context(context.clone(), request)),
        )
        .with_state(options);
    // r[impl control.api]
    let api = Router::new()
        .route("/v1/backends", get(backends))
        .route("/v1/mappings", get(mappings))
        .route("/v1/mappings/{server}/{index}/list", get(list))
        .route(
            "/v1/mappings/{server}/{index}/objects/{*path}",
            put(upload).delete(delete),
        )
        .route("/v1/mappings/{server}/{index}/move", post(rename))
        .route("/v1/mappings/{server}/{index}/upload", post(upload_form))
        .layer(DefaultBodyLimit::max(settings.max_upload))
        .with_state(plane);
    ui.merge(api)
        .route("/pkg/{*file}", get(bundle_file))
        .layer(middleware::from_fn_with_state(
            settings.clone(),
            authenticate,
        ))
}

/// Starts the control plane's listener.
pub async fn start(
    app: &Arc<App>,
    settings: &Arc<ControlSettings>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(settings.listen)
        .await
        .with_context(|| format!("binding the control plane on {}", settings.listen))?;
    tracing::info!(listen = %settings.listen, "control plane listening");
    let router = router(app, settings);
    Ok(tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!("control plane failed: {error}");
        }
    }))
}

async fn authenticate(
    State(settings): State<Arc<ControlSettings>>,
    request: Request,
    next: Next,
) -> Response {
    let signed_in = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| settings.check(value));
    if !signed_in {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"hylla\", charset=\"UTF-8\"",
            )],
            "sign in to the control plane\n",
        )
            .into_response();
    }
    let changes = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if changes && cross_site(request.headers()) {
        return (StatusCode::FORBIDDEN, "cross-site requests are refused\n").into_response();
    }
    next.run(request).await
}

// r[impl control.same-origin]
/// Whether a browser says a request comes from another site. Tools send neither header.
fn cross_site(headers: &HeaderMap) -> bool {
    let text = |name| {
        headers
            .get(name)
            .and_then(|value: &http::HeaderValue| value.to_str().ok())
    };
    if let Some(site) = text(header::HeaderName::from_static("sec-fetch-site")) {
        return !matches!(site, "same-origin" | "none");
    }
    match (text(header::ORIGIN), text(header::HOST)) {
        (Some(origin), Some(host)) => {
            let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
            !origin_host.eq_ignore_ascii_case(host)
        }
        (Some(_), None) => true,
        (None, _) => false,
    }
}

async fn bundle_file(Path(file): Path<String>) -> Response {
    match Bundle::get(&file) {
        Some(content) => (
            [(
                header::CONTENT_TYPE,
                content.metadata.mimetype().to_string(),
            )],
            content.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn backends(State(plane): State<Arc<Plane>>) -> Json<Vec<BackendInfo>> {
    Json(plane.backends())
}

async fn mappings(State(plane): State<Arc<Plane>>) -> Json<Vec<MappingInfo>> {
    Json(plane.mappings())
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    prefix: String,
}

async fn list(
    State(plane): State<Arc<Plane>>,
    Path((server, index)): Path<(String, usize)>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Listing>, ControlError> {
    Ok(Json(plane.list(&server, index, &query.prefix).await?))
}

async fn upload(
    State(plane): State<Arc<Plane>>,
    Path((server, index, path)): Path<(String, usize, String)>,
    body: Bytes,
) -> Result<StatusCode, ControlError> {
    plane.put(&server, index, &path, body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete(
    State(plane): State<Arc<Plane>>,
    Path((server, index, path)): Path<(String, usize, String)>,
) -> Result<StatusCode, ControlError> {
    plane.delete(&server, index, &path).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Move {
    from: String,
    to: String,
}

async fn rename(
    State(plane): State<Arc<Plane>>,
    Path((server, index)): Path<(String, usize)>,
    Json(moved): Json<Move>,
) -> Result<StatusCode, ControlError> {
    plane.rename(&server, index, &moved.from, &moved.to).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UploadQuery {
    #[serde(default)]
    prefix: String,
    /// Whether to redirect to the UI's listing of the prefix, as the UI's form asks.
    #[serde(default)]
    redirect: Option<String>,
}

/// Uploads the files of a form under a prefix, by their names.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response, as in axum"
)]
async fn upload_form(
    State(plane): State<Arc<Plane>>,
    Path((server, index)): Path<(String, usize)>,
    Query(query): Query<UploadQuery>,
    mut form: Multipart,
) -> Result<Response, Response> {
    let bad_form = |error: axum::extract::multipart::MultipartError| {
        (StatusCode::BAD_REQUEST, format!("{error}\n")).into_response()
    };
    while let Some(field) = form.next_field().await.map_err(bad_form)? {
        // Only the file's name, wherever the browser says it came from.
        let Some(name) = field
            .file_name()
            .and_then(|name| name.rsplit(['/', '\\']).next())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        let bytes = field.bytes().await.map_err(bad_form)?;
        let path = format!("{}{name}", query.prefix);
        plane
            .put(&server, index, &path, bytes)
            .await
            .map_err(IntoResponse::into_response)?;
    }
    if query.redirect.is_some() {
        let prefix = serde_urlencoded::to_string([("prefix", &query.prefix)]).unwrap_or_default();
        return Ok(Redirect::to(&format!("/mappings/{server}/{index}?{prefix}")).into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
