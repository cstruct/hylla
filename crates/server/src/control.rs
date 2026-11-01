//! The control plane: a web UI, rendered with Leptos, and a JSON API, showing the status of
//! each backend and browsing the mappings, changing the objects of writable ones.
//!
//! Everything is behind HTTP Basic auth, and requests that change anything are refused when
//! a browser says they come from another site, since browsers send Basic credentials along
//! with any request to the control plane.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::Context;
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{any, get, post, put},
};
use base64::Engine;
use futures::{Stream, StreamExt, TryStreamExt};
use http::{HeaderMap, Method, StatusCode, header};
use hylla_control::{
    BackendInfo, CallInfo, ControlPlane, Listing, MappingInfo, ObjectEntry, Viewer,
};
use leptos::prelude::{LeptosOptions, provide_context};
use leptos_axum::{LeptosRoutes, generate_route_list, handle_server_fns_with_context};
use object_store::{
    ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart, path::Path as ObjectPath,
};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::{
    App,
    oidc::{Oidc, SignInError},
    spans, status,
};

/// The UI's hydration bundle, as `mise run ui` builds it. Without it, the UI is served
/// rendered on the server only, which works without JavaScript.
#[derive(rust_embed::RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../target/site/pkg"]
#[allow_missing = true]
struct Bundle;

/// How long a verified password is remembered, so argon2 doesn't run for every request.
const VERIFIED_FOR: Duration = Duration::from_secs(600);
/// The log target of audit events.
pub(crate) const AUDIT_TARGET: &str = "hylla::audit";
/// How many failed sign-ins a client address may have within `FAILURE_WINDOW`.
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How many client addresses' failures are remembered.
const MAX_FAILING_CLIENTS: usize = 10_000;
/// How many passwords are checked at a time, each with argon2's memory.
const HASHING: usize = 4;

/// The control plane's settings, with its users' password hashes checked.
pub struct ControlSettings {
    listen: SocketAddr,
    /// The PHC strings of the users' argon2 password hashes, by user name.
    users: HashMap<String, String>,
    max_upload: usize,
    /// The SHA-256 of `Authorization` headers verified, when, and for which user.
    verified: Mutex<HashMap<Vec<u8>, (Instant, String)>>,
    /// Signing in through an OpenID provider, if configured.
    oidc: Option<Oidc>,
    /// Failed sign-ins by client address: since when, and how many.
    failures: Mutex<HashMap<Option<IpAddr>, (Instant, u32)>>,
    /// Held while checking a password.
    hashing: Semaphore,
}

/// What checking an `Authorization` header found.
enum Checked {
    User(String),
    Refused,
    /// Too many failed sign-ins from the client: try again after this long.
    Limited(Duration),
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
            oidc: config.oidc.as_ref().map(Oidc::new).transpose()?,
            failures: Mutex::default(),
            hashing: Semaphore::new(HASHING),
        })
    }

    // r[impl control.auth-limits]
    /// Checks an `Authorization` header from `client`, unless the client failed too often.
    async fn check(&self, authorization: &str, client: Option<IpAddr>) -> Checked {
        let limited = {
            let failures = self.failures.lock().expect("not poisoned");
            failures.get(&client).and_then(|(since, count)| {
                let left = FAILURE_WINDOW.checked_sub(since.elapsed())?;
                (*count >= MAX_FAILURES).then_some(left)
            })
        };
        if let Some(left) = limited {
            return Checked::Limited(left);
        }
        if let Some(user) = self.verify(authorization).await {
            return Checked::User(user);
        }
        let user = basic_user(authorization).unwrap_or_default();
        tracing::warn!(
            target: AUDIT_TARGET,
            user,
            client = client.map(|client| client.to_string()),
            "refused a sign-in: wrong user name or password"
        );
        let mut failures = self.failures.lock().expect("not poisoned");
        if failures.len() >= MAX_FAILING_CLIENTS {
            failures.retain(|_, (since, _)| since.elapsed() < FAILURE_WINDOW);
        }
        if failures.len() < MAX_FAILING_CLIENTS {
            let entry = failures.entry(client).or_insert((Instant::now(), 0));
            if entry.0.elapsed() >= FAILURE_WINDOW {
                *entry = (Instant::now(), 0);
            }
            entry.1 += 1;
        }
        Checked::Refused
    }

    // r[impl control.auth]
    /// The user whose name and password an `Authorization` header holds, if any.
    async fn verify(&self, authorization: &str) -> Option<String> {
        let key = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, authorization.as_bytes())
            .as_ref()
            .to_vec();
        {
            let verified = self.verified.lock().expect("not poisoned");
            if let Some((_, user)) = verified
                .get(&key)
                .filter(|(at, _)| at.elapsed() < VERIFIED_FOR)
            {
                return Some(user.clone());
            }
        }
        let credentials = authorization
            .strip_prefix("Basic ")
            .and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .ok()
            })
            .and_then(|decoded| String::from_utf8(decoded).ok())?;
        let (user, password) = credentials.split_once(':')?;
        let hash = self.users.get(user)?.clone();
        let password = password.to_string();
        // argon2 takes a while and much memory: on blocking threads, a few at a time, so
        // guessing passwords can't stall the threads serving requests.
        let _hashing = self.hashing.acquire().await.ok()?;
        let valid = tokio::task::spawn_blocking(move || {
            PasswordHash::new(&hash).is_ok_and(|hash| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &hash)
                    .is_ok()
            })
        })
        .await
        .unwrap_or(false);
        if !valid {
            return None;
        }
        let mut verified = self.verified.lock().expect("not poisoned");
        verified.retain(|_, (at, _)| at.elapsed() < VERIFIED_FOR);
        verified.insert(key, (Instant::now(), user.to_string()));
        Some(user.to_string())
    }
}

/// The user name in a Basic `Authorization` header, for logging.
fn basic_user(authorization: &str) -> Option<String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(authorization.strip_prefix("Basic ")?.trim())
        .ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    Some(credentials.split_once(':')?.0.to_string())
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
    /// An upload larger than `max_upload_mib`.
    TooLarge,
    /// Reading an upload from the client failed.
    Upload(String),
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
            Self::TooLarge => write!(f, "the upload is larger than `max_upload_mib`"),
            Self::Upload(error) => write!(f, "reading the upload failed: {error}"),
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
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Upload(_) => StatusCode::BAD_REQUEST,
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

tokio::task_local! {
    /// Who the request being handled is from, for the audit log.
    static VIEWER: Viewer;
}

// r[impl control.audit]
/// Logs a change made through the control plane: who made it, how they signed in, what
/// they changed, and whether it worked.
fn audit(
    action: &'static str,
    server: &str,
    index: usize,
    path: &str,
    to: Option<&str>,
    size: Option<usize>,
    result: &Result<(), ControlError>,
) {
    let (user, signed_in_with) = VIEWER
        .try_with(|viewer| {
            let with = if viewer.signed_in_with_provider {
                "oidc"
            } else {
                "basic"
            };
            (viewer.name.clone(), with)
        })
        .unwrap_or_else(|_| (String::new(), "none"));
    match result {
        Ok(()) => tracing::info!(
            target: AUDIT_TARGET,
            action,
            user,
            signed_in_with,
            server,
            mapping = index,
            path,
            to,
            size,
            "changed an object"
        ),
        Err(error) => tracing::warn!(
            target: AUDIT_TARGET,
            action,
            user,
            signed_in_with,
            server,
            mapping = index,
            path,
            to,
            size,
            %error,
            "failed to change an object"
        ),
    }
}

/// Uploads are sent to backends in parts of this size once they're larger.
const PART_SIZE: usize = 8 * 1024 * 1024;

/// How many parts of an upload are sent at once.
const PARTS_IN_FLIGHT: usize = 4;

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

    // r[impl control.upload-streaming]
    /// Uploads `body` as an object: in one request when it's small, and otherwise in parts
    /// as it arrives, so uploads are never held in memory whole.
    async fn put<E: std::fmt::Display>(
        &self,
        server: &str,
        index: usize,
        path: &str,
        body: impl Stream<Item = Result<Bytes, E>> + Unpin,
    ) -> Result<(), ControlError> {
        let mut size = 0;
        let result = self.put_counted(server, index, path, body, &mut size).await;
        audit("upload", server, index, path, None, Some(size), &result);
        result
    }

    async fn put_counted<E: std::fmt::Display>(
        &self,
        server: &str,
        index: usize,
        path: &str,
        body: impl Stream<Item = Result<Bytes, E>> + Unpin,
        counted: &mut usize,
    ) -> Result<(), ControlError> {
        let mapping = self.writable(server, index)?;
        let key = mapping.key(path)?;
        let store = self.store(mapping)?;
        let max = self
            .app
            .control
            .as_ref()
            .map_or(usize::MAX, |settings| settings.max_upload);
        let mut body = body;
        let mut buffered = Vec::new();
        let (mut size, mut buffered_size) = (0usize, 0usize);
        let mut upload: Option<WriteMultipart> = None;
        let result = async {
            while let Some(bytes) = body.next().await {
                let bytes = bytes.map_err(|error| ControlError::Upload(error.to_string()))?;
                size += bytes.len();
                *counted = size;
                if size > max {
                    return Err(ControlError::TooLarge);
                }
                if let Some(upload) = &mut upload {
                    upload.wait_for_capacity(PARTS_IN_FLIGHT).await?;
                    upload.put(bytes);
                    continue;
                }
                buffered_size += bytes.len();
                buffered.push(bytes);
                if buffered_size >= PART_SIZE {
                    let started = spans::backend_call(
                        &mapping.backend,
                        "put_multipart",
                        Some(&key),
                        store.put_multipart(&key),
                    )
                    .await?;
                    let mut started = WriteMultipart::new_with_chunk_size(started, PART_SIZE);
                    for bytes in buffered.drain(..) {
                        started.put(bytes);
                    }
                    upload = Some(started);
                }
            }
            Ok(())
        }
        .await;
        match (result, upload) {
            (Ok(()), Some(upload)) => {
                spans::backend_call(&mapping.backend, "put", Some(&key), upload.finish()).await?;
            }
            (Ok(()), None) => {
                let payload = PutPayload::from_iter(buffered);
                spans::backend_call(
                    &mapping.backend,
                    "put",
                    Some(&key),
                    store.put(&key, payload),
                )
                .await?;
            }
            (Err(error), upload) => {
                if let Some(upload) = upload
                    && let Err(abort) = upload.abort().await
                {
                    tracing::warn!(%key, "aborting an upload failed: {abort}");
                }
                return Err(error);
            }
        }
        // r[impl control.invalidate]
        self.app.cache.forget_everywhere(&mapping.backend, &key);
        Ok(())
    }

    async fn delete(&self, server: &str, index: usize, path: &str) -> Result<(), ControlError> {
        let result = self.delete_object(server, index, path).await;
        audit("delete", server, index, path, None, None, &result);
        result
    }

    async fn delete_object(
        &self,
        server: &str,
        index: usize,
        path: &str,
    ) -> Result<(), ControlError> {
        let mapping = self.writable(server, index)?;
        let key = mapping.key(path)?;
        let store = self.store(mapping)?;
        spans::backend_call(&mapping.backend, "delete", Some(&key), store.delete(&key)).await?;
        self.app.cache.forget_everywhere(&mapping.backend, &key);
        Ok(())
    }

    async fn rename(
        &self,
        server: &str,
        index: usize,
        from: &str,
        to: &str,
    ) -> Result<(), ControlError> {
        let result = self.rename_object(server, index, from, to).await;
        audit("move", server, index, from, Some(to), None, &result);
        result
    }

    async fn rename_object(
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
        self.app.cache.forget_everywhere(&mapping.backend, &from);
        self.app.cache.forget_everywhere(&mapping.backend, &to);
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
    // Signing in through the provider needs no sign-in.
    let auth = Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .with_state(settings.clone());
    ui.merge(api)
        .route("/pkg/{*file}", get(bundle_file))
        .layer(middleware::from_fn_with_state(
            settings.clone(),
            authenticate,
        ))
        .merge(auth)
        .layer(middleware::map_response(security_headers))
}

// r[impl control.headers]
/// Keeps other sites from framing the control plane, which would let them trick someone
/// signed in into clicking its buttons, and browsers from guessing content types or sending
/// its URLs elsewhere.
async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        headers.insert(name, http::HeaderValue::from_static(value));
    }
    response
}

/// The value of a request's cookie.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}

/// A `Set-Cookie` value, HTTP-only and same-site, secure when the control plane is reached
/// over HTTPS.
fn set_cookie(name: &str, value: &str, path: &str, max_age: Duration, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{name}={value}; Path={path}; Max-Age={}; HttpOnly; SameSite=Lax{secure}",
        max_age.as_secs()
    )
}

#[derive(Deserialize)]
struct LoginQuery {
    #[serde(default)]
    return_to: String,
}

async fn login(
    State(settings): State<Arc<ControlSettings>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let Some(oidc) = &settings.oidc else {
        return (
            StatusCode::NOT_FOUND,
            "signing in through a provider isn't configured\n",
        )
            .into_response();
    };
    match oidc.start(&query.return_to).await {
        Ok((url, state)) => (
            [(
                header::SET_COOKIE,
                set_cookie(
                    oidc.login_cookie(),
                    &state,
                    "/auth",
                    Duration::from_secs(600),
                    oidc.secure_cookies(),
                ),
            )],
            Redirect::to(&url),
        )
            .into_response(),
        // r[impl control.oidc.errors]
        // Details are for the logs: whoever asks hasn't signed in.
        Err(error) => {
            tracing::warn!("starting a sign-in failed: {error:#}");
            (
                StatusCode::BAD_GATEWAY,
                "the provider can't be reached; the control plane's logs say why\n",
            )
                .into_response()
        }
    }
}

async fn callback(
    State(settings): State<Arc<ControlSettings>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(oidc) = &settings.oidc else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let secure = oidc.secure_cookies();
    let forget_login = set_cookie(oidc.login_cookie(), "", "/auth", Duration::ZERO, secure);
    match oidc
        .finish(&query, cookie(&headers, oidc.login_cookie()))
        .await
    {
        Ok((session, return_to)) => {
            let ttl = oidc.session_ttl();
            let mut response = Redirect::to(&return_to).into_response();
            let cookies = response.headers_mut();
            cookies.append(header::SET_COOKIE, header_value(&forget_login));
            cookies.append(
                header::SET_COOKIE,
                header_value(&set_cookie(
                    oidc.session_cookie(),
                    &session,
                    "/",
                    ttl,
                    secure,
                )),
            );
            response
        }
        Err(error) => {
            let status = match &error {
                SignInError::UnknownState => StatusCode::BAD_REQUEST,
                SignInError::Provider(_) | SignInError::NotAllowed(_) => StatusCode::FORBIDDEN,
                SignInError::Failed(_) => StatusCode::BAD_GATEWAY,
            };
            tracing::warn!("a sign-in failed: {error}");
            let body = match &error {
                SignInError::Failed(_) => {
                    "signing in failed; the control plane's logs say why\n".to_string()
                }
                error => format!("{error}\n"),
            };
            (status, [(header::SET_COOKIE, forget_login)], body).into_response()
        }
    }
}

async fn logout(State(settings): State<Arc<ControlSettings>>, headers: HeaderMap) -> Response {
    if cross_site(&headers) {
        return (StatusCode::FORBIDDEN, "cross-site requests are refused\n").into_response();
    }
    let Some(oidc) = &settings.oidc else {
        return Redirect::to("/").into_response();
    };
    let to = oidc.end(cookie(&headers, oidc.session_cookie())).await;
    (
        [(
            header::SET_COOKIE,
            set_cookie(
                oidc.session_cookie(),
                "",
                "/",
                Duration::ZERO,
                oidc.secure_cookies(),
            ),
        )],
        Redirect::to(&to),
    )
        .into_response()
}

fn header_value(value: &str) -> http::HeaderValue {
    http::HeaderValue::from_str(value).expect("cookies are header values")
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
    Ok(tokio::spawn(crate::listen::serve(
        listener,
        router,
        None,
        app.limits.clone(),
        app.shutdown(),
        "control plane",
    )))
}

async fn authenticate(
    State(settings): State<Arc<ControlSettings>>,
    mut request: Request,
    next: Next,
) -> Response {
    // r[impl control.oidc.session]
    let session = settings.oidc.as_ref().and_then(|oidc| {
        let cookie = cookie(request.headers(), oidc.session_cookie())?;
        oidc.session(cookie)
    });
    let viewer = match session {
        Some(session) => Some(Viewer {
            name: session.name,
            signed_in_with_provider: true,
        }),
        None => {
            let authorization = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok());
            let client = request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| addr.ip());
            match authorization {
                Some(authorization) => match settings.check(authorization, client).await {
                    Checked::User(name) => Some(Viewer {
                        name,
                        signed_in_with_provider: false,
                    }),
                    Checked::Refused => None,
                    Checked::Limited(left) => {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(header::RETRY_AFTER, (left.as_secs() + 1).to_string())],
                            "too many failed sign-ins, try again later\n",
                        )
                            .into_response();
                    }
                },
                None => None,
            }
        }
    };
    let Some(viewer) = viewer else {
        // Browsers loading a page sign in through the provider; anything else, such as
        // scripts, with Basic auth.
        let page = request.method() == Method::GET
            && request
                .headers()
                .get(header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|accept| accept.contains("text/html"));
        if settings.oidc.is_some() && page {
            let return_to = request
                .uri()
                .path_and_query()
                .map_or("/", |path| path.as_str());
            let query = serde_urlencoded::to_string([("return_to", return_to)]).unwrap_or_default();
            return Redirect::to(&format!("/auth/login?{query}")).into_response();
        }
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"hylla\", charset=\"UTF-8\"",
            )],
            "sign in to the control plane\n",
        )
            .into_response();
    };
    request.extensions_mut().insert(viewer.clone());
    let changes = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if changes && cross_site(request.headers()) {
        return (StatusCode::FORBIDDEN, "cross-site requests are refused\n").into_response();
    }
    VIEWER.scope(viewer, next.run(request)).await
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
    body: axum::body::Body,
) -> Result<StatusCode, ControlError> {
    plane
        .put(&server, index, &path, body.into_data_stream())
        .await?;
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
        let path = format!("{}{name}", query.prefix);
        plane
            .put(&server, index, &path, field)
            .await
            .map_err(IntoResponse::into_response)?;
    }
    if query.redirect.is_some() {
        let prefix = serde_urlencoded::to_string([("prefix", &query.prefix)]).unwrap_or_default();
        return Ok(Redirect::to(&format!("/mappings/{server}/{index}?{prefix}")).into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
