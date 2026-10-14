use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::body::{Body, to_bytes};
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, DiskCache, Interval, Mapping, MemoryCache, Refresh, RefreshMode,
    RefreshRule, Rewrite, Server, WritePolicy,
};
use hylla_server::App;
use object_store::{
    Attribute, Attributes, ObjectStore, ObjectStoreExt, PutOptions, memory::InMemory, path::Path,
};
use tower::ServiceExt;

const LISTEN: &str = "127.0.0.1:8080";

fn listen() -> SocketAddr {
    LISTEN.parse().unwrap()
}

fn mapping(path_prefix: &str, rewrites: &[(&str, &str)]) -> Mapping {
    Mapping {
        path_prefix: path_prefix.into(),
        backend: "assets".into(),
        rewrites: rewrites
            .iter()
            .map(|(pattern, replace)| Rewrite {
                pattern: (*pattern).into(),
                replace: (*replace).into(),
            })
            .collect(),
    }
}

/// A config with a `public` server mapping `/assets` (rewritten to the bucket root) and an
/// `internal` server on the same address mapping everything, both to the `assets` backend.
fn config(cache: Cache) -> Config {
    config_with_refresh(cache, None)
}

fn config_with_refresh(cache: Cache, refresh: Option<Refresh>) -> Config {
    let server = |names: &[&str], mappings| Server {
        listen: listen(),
        server_names: names.iter().map(|name| (*name).into()).collect(),
        mappings,
    };
    Config {
        servers: HashMap::from([
            (
                "public".into(),
                server(
                    &["cache.example.com"],
                    vec![mapping("/assets", &[(r"^/assets/(.*)$", "/$1")])],
                ),
            ),
            (
                "internal".into(),
                server(&["cache.internal"], vec![mapping("/", &[])]),
            ),
        ]),
        backends: HashMap::from([(
            "assets".into(),
            Backend::S3 {
                bucket: "assets".into(),
                region: "eu-north-1".into(),
                endpoint: None,
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
                refresh,
            },
        )]),
        cache,
    }
}

/// A small cache: 1 KiB chunks, so test objects span several chunks.
fn small_cache(metadata_ttl_seconds: u64) -> Cache {
    Cache {
        chunk_size_kib: 1,
        metadata_ttl: Interval::from_secs(metadata_ttl_seconds),
        ..Cache::default()
    }
}

/// 3000 bytes, spanning three chunks, with distinct bytes so ranges are checkable.
fn content() -> Vec<u8> {
    (0..3000u32).map(|i| (i % 251) as u8).collect()
}

async fn put(store: &InMemory, key: &str, data: Vec<u8>) {
    let attributes = Attributes::from_iter([(
        Attribute::ContentType,
        object_store::AttributeValue::from("text/css"),
    )]);
    store
        .put_opts(
            &Path::from(key),
            data.into(),
            PutOptions {
                attributes,
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

async fn app(cache: Cache) -> (Arc<App>, Arc<InMemory>) {
    let store = Arc::new(InMemory::new());
    put(&store, "app.css", content()).await;
    let stores = HashMap::from([("assets".to_string(), store.clone() as Arc<dyn ObjectStore>)]);
    (App::new(&config(cache), stores).await.unwrap(), store)
}

struct Response {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

async fn request(
    app: &Arc<App>,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .listener(listen())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Response {
        status,
        headers,
        body,
    }
}

async fn get(app: &Arc<App>, path: &str) -> Response {
    request(app, "GET", "cache.example.com", path, &[]).await
}

fn header<'a>(response: &'a Response, name: header::HeaderName) -> &'a str {
    response.headers[name].to_str().unwrap()
}

// r[verify server.get]
// r[verify backend.s3]
// r[verify cache.chunks]
#[tokio::test]
async fn serves_whole_objects() {
    let (app, _) = app(small_cache(60)).await;
    let response = get(&app, "/assets/app.css").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, content());
    assert_eq!(header(&response, header::CONTENT_LENGTH), "3000");
    assert_eq!(header(&response, header::CONTENT_TYPE), "text/css");
    assert_eq!(header(&response, header::ACCEPT_RANGES), "bytes");
    assert!(response.headers.contains_key(header::ETAG));
    assert!(response.headers.contains_key(header::LAST_MODIFIED));
}

// r[verify server.range]
#[tokio::test]
async fn serves_ranges_across_chunks() {
    let (app, _) = app(small_cache(60)).await;
    let range = |range: &'static str| {
        let app = app.clone();
        async move {
            request(
                &app,
                "GET",
                "cache.example.com",
                "/assets/app.css",
                &[("range", range)],
            )
            .await
        }
    };

    let response = range("bytes=1000-2100").await;
    assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.body, content()[1000..2101]);
    assert_eq!(
        header(&response, header::CONTENT_RANGE),
        "bytes 1000-2100/3000"
    );
    assert_eq!(header(&response, header::CONTENT_LENGTH), "1101");

    let response = range("bytes=-10").await;
    assert_eq!(response.body, content()[2990..]);
    assert_eq!(
        header(&response, header::CONTENT_RANGE),
        "bytes 2990-2999/3000"
    );

    let response = range("bytes=3000-").await;
    assert_eq!(response.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(header(&response, header::CONTENT_RANGE), "bytes */3000");
}

// r[verify server.head]
#[tokio::test]
async fn head_requests_have_no_body() {
    let (app, _) = app(small_cache(60)).await;
    let response = request(&app, "HEAD", "cache.example.com", "/assets/app.css", &[]).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(header(&response, header::CONTENT_LENGTH), "3000");
    assert!(response.body.is_empty());
}

// r[verify server.hosts]
// r[verify server.routes]
// r[verify config.mappings.rewrites]
#[tokio::test]
async fn routes_by_host_and_mapping() {
    let (app, _) = app(small_cache(60)).await;
    // The public server rewrites /assets/app.css to app.css; ports and case are ignored.
    let response = request(
        &app,
        "GET",
        "Cache.Example.com:8080",
        "/assets/app.css",
        &[],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    // The internal server on the same address maps the bucket root.
    let response = request(&app, "GET", "cache.internal", "/app.css", &[]).await;
    assert_eq!(response.body, content());
    // The public server has no mapping for /app.css.
    assert_eq!(get(&app, "/app.css").await.status, StatusCode::NOT_FOUND);
    let response = request(&app, "GET", "other.example.com", "/assets/app.css", &[]).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&app, "/assets/missing.css").await.status,
        StatusCode::NOT_FOUND
    );
}

// r[verify server.methods]
#[tokio::test]
async fn only_get_and_head_are_allowed() {
    let (app, _) = app(small_cache(60)).await;
    let response = request(&app, "POST", "cache.example.com", "/assets/app.css", &[]).await;
    assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header(&response, header::ALLOW), "GET, HEAD");
}

// r[verify server.conditional]
#[tokio::test]
async fn matching_etags_are_not_modified() {
    let (app, _) = app(small_cache(60)).await;
    let etag = header(&get(&app, "/assets/app.css").await, header::ETAG).to_string();
    let response = request(
        &app,
        "GET",
        "cache.example.com",
        "/assets/app.css",
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_MODIFIED);
    assert!(response.body.is_empty());
}

// r[verify cache.ram]
// r[verify cache.metadata]
#[tokio::test]
async fn cached_objects_are_served_without_the_backend() {
    let (app, store) = app(small_cache(60)).await;
    assert_eq!(get(&app, "/assets/app.css").await.status, StatusCode::OK);
    store.delete(&Path::from("app.css")).await.unwrap();
    // Metadata and chunks are cached, so the object is still served.
    let response = get(&app, "/assets/app.css").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, content());
}

// r[verify cache.metadata]
// r[verify cache.chunks]
// r[verify cache.not-found]
#[tokio::test]
async fn changed_objects_are_refetched_once_metadata_expires() {
    let (app, store) = app(small_cache(0)).await;
    assert_eq!(get(&app, "/assets/app.css").await.body, content());

    put(&store, "app.css", b"body { color: red }".to_vec()).await;
    let response = get(&app, "/assets/app.css").await;
    assert_eq!(response.body, b"body { color: red }");

    store.delete(&Path::from("app.css")).await.unwrap();
    assert_eq!(
        get(&app, "/assets/app.css").await.status,
        StatusCode::NOT_FOUND
    );
}

// r[verify cache.disk]
#[tokio::test]
async fn serves_through_a_disk_tier() {
    let dir = std::env::temp_dir().join(format!("hylla-server-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = Cache {
        chunk_size_kib: 1,
        metadata_ttl: Interval::from_secs(60),
        stale_while_revalidate: Interval::from_secs(0),
        stale_if_error: Interval::from_secs(0),
        write_policy: WritePolicy::OnInsertion,
        memory: MemoryCache { capacity_mib: 1 },
        disk: Some(DiskCache {
            path: dir.clone(),
            capacity_mib: 16,
            block_size_mib: 1,
        }),
    };
    let (app, store) = app(cache).await;
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
    store.delete(&Path::from("app.css")).await.unwrap();
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
    app.close().await.unwrap();
    assert!(
        dir.read_dir().unwrap().next().is_some(),
        "the disk tier holds files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A store that fails every request while `failing` is set, as during an outage.
#[derive(Debug)]
struct Flaky {
    inner: Arc<InMemory>,
    failing: std::sync::atomic::AtomicBool,
    requests: std::sync::atomic::AtomicUsize,
}

impl Flaky {
    fn new(inner: Arc<InMemory>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            failing: false.into(),
            requests: 0.into(),
        })
    }

    fn fail(&self, failing: bool) {
        self.failing
            .store(failing, std::sync::atomic::Ordering::Relaxed);
    }

    fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn check(&self) -> object_store::Result<()> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.failing.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(object_store::Error::Generic {
                store: "flaky",
                source: "the backend is down".into(),
            });
        }
        Ok(())
    }
}

impl std::fmt::Display for Flaky {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Flaky({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Flaky {
    async fn put_opts(
        &self,
        location: &Path,
        payload: object_store::PutPayload,
        options: PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.check()?;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        use futures::StreamExt;
        if let Err(error) = self.check() {
            return futures::stream::once(async { Err(error) }).boxed();
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

async fn flaky_app(cache: Cache, store: Arc<Flaky>) -> Arc<App> {
    let stores = HashMap::from([("assets".to_string(), store as Arc<dyn ObjectStore>)]);
    App::new(&config(cache), stores).await.unwrap()
}

fn disk_cache(
    dir: &std::path::Path,
    metadata_ttl_seconds: u64,
    stale_if_error_seconds: u64,
) -> Cache {
    Cache {
        chunk_size_kib: 1,
        metadata_ttl: Interval::from_secs(metadata_ttl_seconds),
        stale_while_revalidate: Interval::from_secs(0),
        stale_if_error: Interval::from_secs(stale_if_error_seconds),
        write_policy: WritePolicy::OnEviction,
        memory: MemoryCache { capacity_mib: 1 },
        disk: Some(DiskCache {
            path: dir.to_path_buf(),
            capacity_mib: 16,
            block_size_mib: 1,
        }),
    }
}

// r[verify cache.stale-if-error]
// r[verify cache.disk]
// r[verify config.cache.stale-if-error]
#[tokio::test]
async fn serves_cached_objects_after_a_restart_while_the_backend_is_down() {
    let dir = std::env::temp_dir().join(format!("hylla-server-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let memory = Arc::new(InMemory::new());
    put(&memory, "app.css", content()).await;
    let store = Flaky::new(memory);

    // Fill the cache, and flush it to disk on close.
    let app = flaky_app(disk_cache(&dir, 0, 3600), store.clone()).await;
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
    app.close().await.unwrap();
    drop(app);

    // Restart with the backend down: metadata and chunks come from disk. The metadata is
    // past its TTL of 0, but within the stale-if-error window.
    store.fail(true);
    let app = flaky_app(disk_cache(&dir, 0, 3600), store.clone()).await;
    let response = get(&app, "/assets/app.css").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, content());
    let response = request(
        &app,
        "GET",
        "cache.example.com",
        "/assets/app.css",
        &[("range", "bytes=1000-1099")],
    )
    .await;
    assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.body, content()[1000..1100]);
    app.close().await.unwrap();
    drop(app);

    // Without a stale-if-error window, an expired object isn't served while the backend fails.
    let app = flaky_app(disk_cache(&dir, 0, 0), store.clone()).await;
    assert_eq!(
        get(&app, "/assets/app.css").await.status,
        StatusCode::BAD_GATEWAY
    );
    app.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// r[verify cache.changed]
// r[verify cache.stale-if-error]
#[tokio::test]
async fn outages_keep_cached_chunks_servable() {
    let memory = Arc::new(InMemory::new());
    put(&memory, "app.css", content()).await;
    let store = Flaky::new(memory);
    let cache = Cache {
        metadata_ttl: Interval::from_secs(0),
        stale_if_error: Interval::from_secs(3600),
        ..small_cache(0)
    };
    let app = flaky_app(cache, store.clone()).await;
    let range = |range: &'static str| {
        let app = app.clone();
        async move {
            request(
                &app,
                "GET",
                "cache.example.com",
                "/assets/app.css",
                &[("range", range)],
            )
            .await
        }
    };

    // Cache the first chunk only, then take the backend down.
    assert_eq!(range("bytes=0-99").await.body, content()[..100]);
    store.fail(true);
    assert_eq!(
        range("bytes=0-99").await.status,
        StatusCode::PARTIAL_CONTENT
    );
    // A chunk that isn't cached can't be served...
    assert_eq!(
        range("bytes=2000-2099").await.status,
        StatusCode::BAD_GATEWAY
    );
    // ...but failing to fetch it doesn't make the cached ones unservable.
    let response = range("bytes=0-99").await;
    assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.body, content()[..100]);

    // Once the backend is back, everything is served again.
    store.fail(false);
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
}

fn rule(prefix: Option<&str>, key: Option<&str>, mode: RefreshMode) -> RefreshRule {
    RefreshRule {
        prefix: prefix.map(Into::into),
        key: key.map(Into::into),
        every: Interval::from_secs(3600),
        mode,
        max_objects: None,
        max_mib: None,
    }
}

async fn refresh_app(cache: Cache, store: Arc<Flaky>, rules: Vec<RefreshRule>) -> Arc<App> {
    let refresh = Refresh {
        concurrency: 2,
        rules,
    };
    let stores = HashMap::from([("assets".to_string(), store as Arc<dyn ObjectStore>)]);
    App::new(&config_with_refresh(cache, Some(refresh)), stores)
        .await
        .unwrap()
}

/// A store with `app.css` holding [`content`].
async fn flaky_store() -> (Arc<InMemory>, Arc<Flaky>) {
    let memory = Arc::new(InMemory::new());
    put(&memory, "app.css", content()).await;
    let store = Flaky::new(memory.clone());
    (memory, store)
}

// r[verify refresh.revalidate]
// r[verify refresh.list]
// r[verify refresh.run]
#[tokio::test]
async fn revalidation_picks_up_changes_before_the_ttl() {
    let (memory, store) = flaky_store().await;
    let app = refresh_app(
        small_cache(3600),
        store,
        vec![rule(Some(""), None, RefreshMode::Revalidate)],
    )
    .await;
    assert_eq!(get(&app, "/assets/app.css").await.body, content());

    put(&memory, "app.css", b"body { color: red }".to_vec()).await;
    // Within the TTL, the cached version is still served...
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
    // ...until a revalidation notices the change.
    let report = app.refresh("assets", 0).await.unwrap();
    assert_eq!((report.listed, report.changed), (1, 1));
    assert_eq!(
        get(&app, "/assets/app.css").await.body,
        b"body { color: red }"
    );
}

// r[verify refresh.revalidate]
// r[verify config.refresh.modes]
#[tokio::test]
async fn revalidation_keeps_unchanged_objects_fresh_and_ignores_uncached_ones() {
    let (memory, store) = flaky_store().await;
    put(&memory, "other.css", content()).await;
    let cache = Cache {
        stale_if_error: Interval::from_secs(0),
        ..small_cache(1)
    };
    let app = refresh_app(
        cache,
        store.clone(),
        vec![rule(Some(""), None, RefreshMode::Revalidate)],
    )
    .await;
    assert_eq!(get(&app, "/assets/app.css").await.status, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // The metadata expired, but revalidation finds it unchanged and marks it fresh. The
    // object that isn't cached is left alone.
    let requests = store.requests();
    let report = app.refresh("assets", 0).await.unwrap();
    assert_eq!(
        (report.listed, report.unchanged, report.preloaded),
        (2, 1, 0)
    );
    assert_eq!(
        store.requests(),
        requests + 1,
        "an unchanged object needs no requests beyond the listing"
    );

    store.fail(true);
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
    assert_eq!(
        get(&app, "/assets/other.css").await.status,
        StatusCode::BAD_GATEWAY
    );
}

// r[verify refresh.preload]
// r[verify refresh.caps]
#[tokio::test]
async fn preloading_fills_the_cache_within_its_caps() {
    let (memory, store) = flaky_store().await;
    for key in ["b.css", "c.css"] {
        put(&memory, key, content()).await;
    }
    put(&memory, "big/blob", vec![7; 2 * 1024 * 1024]).await;
    let preload = |prefix, max_objects, max_mib| RefreshRule {
        max_objects,
        max_mib,
        ..rule(Some(prefix), None, RefreshMode::Preload)
    };
    let app = refresh_app(
        small_cache(3600),
        store.clone(),
        vec![preload("", Some(2), None), preload("big/", None, Some(1))],
    )
    .await;

    // Objects are listed in key order: app.css and b.css fit the cap of 2.
    let report = app.refresh("assets", 0).await.unwrap();
    assert_eq!((report.listed, report.preloaded, report.skipped), (4, 2, 2));
    assert_eq!(report.preloaded_bytes, 6000);
    // The 2 MiB object doesn't fit 1 MiB.
    let report = app.refresh("assets", 1).await.unwrap();
    assert_eq!((report.listed, report.preloaded, report.skipped), (1, 0, 1));

    store.fail(true);
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
    assert_eq!(get(&app, "/assets/b.css").await.body, content());
    assert_eq!(
        get(&app, "/assets/c.css").await.status,
        StatusCode::BAD_GATEWAY
    );
}

// r[verify refresh.preload]
// r[verify refresh.revalidate]
#[tokio::test]
async fn refreshing_both_preloads_changed_objects() {
    let (memory, store) = flaky_store().await;
    let rules = vec![RefreshRule {
        max_objects: Some(10),
        ..rule(Some(""), None, RefreshMode::Both)
    }];
    let app = refresh_app(small_cache(3600), store.clone(), rules).await;
    assert_eq!(app.refresh("assets", 0).await.unwrap().preloaded, 1);
    // Unchanged, so only revalidated the second time.
    let report = app.refresh("assets", 0).await.unwrap();
    assert_eq!((report.unchanged, report.preloaded), (1, 0));

    put(&memory, "app.css", b"body { color: red }".to_vec()).await;
    assert_eq!(app.refresh("assets", 0).await.unwrap().preloaded, 1);
    store.fail(true);
    assert_eq!(
        get(&app, "/assets/app.css").await.body,
        b"body { color: red }"
    );
}

// r[verify refresh.removed]
#[tokio::test]
async fn key_rules_drop_deleted_objects() {
    let (memory, store) = flaky_store().await;
    let rules = vec![
        rule(None, Some("app.css"), RefreshMode::Revalidate),
        rule(None, Some("app.css"), RefreshMode::Disabled),
    ];
    let app = refresh_app(small_cache(3600), store, rules).await;
    assert_eq!(get(&app, "/assets/app.css").await.status, StatusCode::OK);
    assert_eq!(app.refresh("assets", 1).await.unwrap(), Default::default());
    assert_eq!(app.refresh("assets", 0).await.unwrap().unchanged, 1);

    memory.delete(&Path::from("app.css")).await.unwrap();
    assert_eq!(app.refresh("assets", 0).await.unwrap().removed, 1);
    assert_eq!(
        get(&app, "/assets/app.css").await.status,
        StatusCode::NOT_FOUND
    );
}

// r[verify refresh.schedule]
#[tokio::test]
async fn scheduled_rules_run_at_startup() {
    let (_, store) = flaky_store().await;
    let rules = vec![RefreshRule {
        max_objects: Some(10),
        ..rule(Some(""), None, RefreshMode::Preload)
    }];
    let app = refresh_app(small_cache(3600), store.clone(), rules).await;
    let tasks = app.spawn_refresh();
    // A listing, a metadata request and three chunks.
    for _ in 0..100 {
        if store.requests() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    for task in tasks {
        task.abort();
    }
    store.fail(true);
    assert_eq!(get(&app, "/assets/app.css").await.body, content());
}
