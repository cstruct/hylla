use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::body::{Body, to_bytes};
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, DiskCache, Mapping, MemoryCache, Rewrite, Server, WritePolicy,
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
            },
        )]),
        cache,
    }
}

/// A small cache: 1 KiB chunks, so test objects span several chunks.
fn small_cache(metadata_ttl_seconds: u64) -> Cache {
    Cache {
        chunk_size_kib: 1,
        metadata_ttl_seconds,
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
        metadata_ttl_seconds: 60,
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
