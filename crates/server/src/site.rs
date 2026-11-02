//! Sites: frontend builds served by commit SHA, or through aliases naming one.

use std::{collections::HashMap, sync::Arc};

use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use object_store::path::Path as ObjectPath;

use crate::{
    app::App,
    cache::{ObjectMeta, ReadError},
};

/// The largest alias, in bytes: a SHA and some whitespace.
const MAX_ALIAS_BYTES: u64 = 1024;

/// The most aliases whose SHA is kept, beyond which they're all read again.
const MAX_CACHED_ALIASES: usize = 10_000;

/// A mapping's site, with the SHAs its aliases point at.
pub(crate) struct Site {
    config: hylla_config::Site,
    /// The SHA each alias pointed at, by alias key, with the ETag it was read at.
    shas: std::sync::Mutex<HashMap<String, (String, String)>>,
}

/// The build a request is served from.
pub(crate) struct Build {
    pub(crate) sha: String,
    /// The alias it was chosen through, if any.
    pub(crate) alias: Option<String>,
    /// Whether the alias came from the request's host, rather than the site's `alias`.
    pub(crate) from_host: bool,
}

impl Site {
    pub(crate) fn new(config: hylla_config::Site) -> Self {
        Self {
            config,
            shas: Default::default(),
        }
    }

    pub(crate) fn config(&self) -> &hylla_config::Site {
        &self.config
    }

    // r[impl site.label]
    /// Chooses the build of a request by the site's `alias`, or by the label its host's
    /// wildcard matched, answering with the response to send instead when there's none.
    pub(crate) async fn build(
        &self,
        app: &Arc<App>,
        backend: &str,
        store: &Arc<dyn object_store::ObjectStore>,
        label: Option<&str>,
    ) -> Result<Build, Failure> {
        let (name, from_host) = match (&self.config.alias, label) {
            (Some(alias), _) => (alias.as_str(), false),
            (None, Some(label)) if hylla_config::is_sha(label) => {
                return Ok(Build {
                    sha: label.to_string(),
                    alias: None,
                    from_host: true,
                });
            }
            (None, Some(label)) => (label, true),
            (None, None) => return Err(Failure::NotFound),
        };
        // r[impl site.alias-names]
        if !hylla_config::is_alias_name(name) {
            return Err(Failure::NotFound);
        }
        let sha = self.resolve(app, backend, store, name).await?;
        Ok(Build {
            sha,
            alias: Some(name.to_string()),
            from_host,
        })
    }

    // r[impl site.alias-object]
    // r[impl site.alias-stale]
    /// The SHA alias `name` points at, read through the cache, so it's checked against its
    /// ETag after the metadata TTL, served stale through outages, and forgotten when changed
    /// through a control plane.
    async fn resolve(
        &self,
        app: &Arc<App>,
        backend: &str,
        store: &Arc<dyn object_store::ObjectStore>,
        name: &str,
    ) -> Result<String, Failure> {
        let key = format!("{}{name}", self.config.aliases);
        let path = ObjectPath::from(key.as_str());
        // The alias may change between reading its metadata and its body, once.
        for _ in 0..2 {
            let meta = match app.cache.metadata(backend, store.clone(), &path).await {
                Ok((meta, _)) => meta,
                // r[impl site.alias-missing]
                Err(ReadError::NotFound) => return Err(Failure::NotFound),
                Err(ReadError::Backend(error) | ReadError::Changed(error)) => {
                    tracing::warn!(backend, alias = name, "alias metadata: {error:#}");
                    return Err(Failure::BadGateway);
                }
            };
            // r[impl site.alias-cached]
            if let Some(etag) = &meta.etag
                && let Some((cached_etag, sha)) = self.shas.lock().expect("not poisoned").get(&key)
                && cached_etag == etag
            {
                return Ok(sha.clone());
            }
            match self.read(app, backend, store, &path, &meta).await {
                Ok(sha) => {
                    if let Some(etag) = meta.etag {
                        let mut shas = self.shas.lock().expect("not poisoned");
                        if shas.len() >= MAX_CACHED_ALIASES && !shas.contains_key(&key) {
                            shas.clear();
                        }
                        shas.insert(key, (etag, sha.clone()));
                    }
                    return Ok(sha);
                }
                Err(Read::Changed) => continue,
                Err(Read::Failed(response)) => return Err(response),
            }
        }
        tracing::warn!(backend, alias = name, "the alias keeps changing");
        Err(Failure::BadGateway)
    }

    // r[impl site.alias-invalid]
    /// Reads the SHA in the body of the alias at `path`.
    async fn read(
        &self,
        app: &Arc<App>,
        backend: &str,
        store: &Arc<dyn object_store::ObjectStore>,
        path: &ObjectPath,
        meta: &ObjectMeta,
    ) -> Result<String, Read> {
        let invalid = |reason: &str| {
            tracing::warn!(backend, alias = %path, "invalid alias: {reason}");
            Read::Failed(Failure::BadGateway)
        };
        if meta.size > MAX_ALIAS_BYTES {
            return Err(invalid("it's larger than 1 KiB"));
        }
        let mut body = Vec::new();
        for index in 0..app.cache.chunk_count(meta.size) {
            match app
                .cache
                .chunk(backend, store.clone(), path, meta, index)
                .await
            {
                Ok(bytes) => body.extend_from_slice(&bytes),
                Err(ReadError::Changed(_)) => return Err(Read::Changed),
                Err(ReadError::NotFound) => return Err(Read::Changed),
                Err(ReadError::Backend(error)) => {
                    tracing::warn!(backend, alias = %path, "alias body: {error:#}");
                    return Err(Read::Failed(Failure::BadGateway));
                }
            }
        }
        let sha = std::str::from_utf8(&body)
            .map(str::trim)
            .map_err(|_| invalid("it isn't UTF-8"))?;
        if !hylla_config::is_sha(sha) {
            return Err(invalid("its body isn't a SHA"));
        }
        Ok(sha.to_string())
    }

    // r[impl site.keys]
    /// The backend key of `document` of build `sha`, a path within the mapping, with the
    /// index appended to directories.
    pub(crate) fn key(&self, sha: &str, document: &str) -> String {
        let document = document.trim_start_matches('/');
        let index = if document.is_empty() || document.ends_with('/') {
            self.config.index.as_str()
        } else {
            ""
        };
        format!("{}{sha}/{document}{index}", self.config.builds)
    }

    /// The backend key of the fallback of build `sha`, if the site has one.
    pub(crate) fn fallback_key(&self, sha: &str) -> Option<String> {
        let fallback = self.config.fallback.as_deref()?;
        Some(format!("{}{sha}/{fallback}", self.config.builds))
    }
}

/// Why a request's build couldn't be chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// There's no such alias, or it isn't a valid name.
    NotFound,
    /// The alias couldn't be read, or isn't a SHA.
    BadGateway,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadGateway => StatusCode::BAD_GATEWAY,
        }
        .into_response()
    }
}

/// Why an alias couldn't be read.
enum Read {
    /// It changed or went away while it was read.
    Changed,
    Failed(Failure),
}

// r[impl site.redirect]
// r[impl site.redirect.no-store]
/// A redirect of a request to `host`, whose first label is an alias, to the same path and
/// query on the host of build `sha`, keeping the rest of the host and its port.
pub(crate) fn redirect(host: &str, path_and_query: &str, sha: &str) -> Response {
    let rest = host.split_once('.').map_or("", |(_, rest)| rest);
    // Scheme-relative, so it works behind a proxy terminating TLS.
    let location = format!("//{sha}.{rest}{path_and_query}");
    let Ok(location) = HeaderValue::from_str(&location) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    (
        StatusCode::TEMPORARY_REDIRECT,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response()
}

// r[impl site.fallback]
/// Whether a request loads a page, rather than a script, image or other resource of one.
pub(crate) fn is_page_load(headers: &HeaderMap) -> bool {
    match headers.get("sec-fetch-mode") {
        Some(mode) => mode.as_bytes().eq_ignore_ascii_case(b"navigate"),
        None => headers
            .get_all(header::ACCEPT)
            .iter()
            .filter_map(|accept| accept.to_str().ok())
            .flat_map(|accept| accept.split(','))
            .any(|media| {
                media
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/html"))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(fallback: Option<&str>) -> Site {
        Site::new(hylla_config::Site {
            builds: "builds/".into(),
            aliases: "aliases/".into(),
            alias: None,
            index: "index.html".into(),
            fallback: fallback.map(Into::into),
            redirect: true,
            alias_cache_control: "no-cache".into(),
        })
    }

    // r[verify site.keys]
    #[test]
    fn keys_are_under_the_build_with_indexes_for_directories() {
        let site = site(Some("index.html"));
        assert_eq!(site.key("abc", "/app.js"), "builds/abc/app.js");
        assert_eq!(site.key("abc", "/"), "builds/abc/index.html");
        assert_eq!(site.key("abc", ""), "builds/abc/index.html");
        assert_eq!(site.key("abc", "/docs/"), "builds/abc/docs/index.html");
        assert_eq!(site.fallback_key("abc").unwrap(), "builds/abc/index.html");
    }

    // r[verify site.fallback]
    #[test]
    fn page_loads_are_navigations_or_html_without_fetch_metadata() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            pairs
                .iter()
                .map(|(name, value)| {
                    (
                        http::HeaderName::from_static(name),
                        HeaderValue::from_static(value),
                    )
                })
                .collect::<HeaderMap>()
        };
        assert!(is_page_load(&headers(&[("sec-fetch-mode", "navigate")])));
        assert!(is_page_load(&headers(&[(
            "accept",
            "text/html,application/xhtml+xml;q=0.9"
        )])));
        assert!(!is_page_load(&headers(&[
            ("sec-fetch-mode", "no-cors"),
            ("accept", "text/html")
        ])));
        assert!(!is_page_load(&headers(&[("accept", "*/*")])));
        assert!(!is_page_load(&headers(&[])));
    }
}
