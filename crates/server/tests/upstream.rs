mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::body::{Body, to_bytes};
use common::{Flaky, Pki};
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, HyllaTls, Interval, Mapping, Refresh, RefreshMode, RefreshRule,
    ServeBackends, ServeTls, Server,
};
use hylla_server::App;
use object_store::{GetOptions, ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use tower::ServiceExt;

/// The name downstream nodes' certificates are issued for.
const CLIENT_NAME: &str = "hylla.downstream";

const LISTEN: &str = "127.0.0.1:8080";

fn content() -> Vec<u8> {
    (0..3000u32).map(|i| (i % 251) as u8).collect()
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn s3() -> Backend {
    Backend::S3 {
        bucket: "assets".into(),
        region: "eu-north-1".into(),
        endpoint: None,
        access_key_id: "test".into(),
        secret_access_key: "test".into(),
        refresh: None,
    }
}

fn config(backends: HashMap<String, Backend>, serve: Option<ServeBackends>) -> Config {
    Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: LISTEN.parse().unwrap(),
                server_names: vec!["cache.example.com".into()],
                mappings: vec![Mapping {
                    path_prefix: "/".into(),
                    backend: "shield".into(),
                    rewrites: Vec::new(),
                    writable: false,
                    auth: None,
                }],
            },
        )]),
        backends,
        cache: Cache {
            chunk_size_kib: 1,
            metadata_ttl: Interval::from_secs(3600),
            ..Cache::default()
        },
        auth: HashMap::new(),
        telemetry: Default::default(),
        cluster: None,
        control: None,
        serve_backends: serve,
    }
}

/// A node serving its `assets` backend, but not `private`, to downstream nodes.
struct Upstream {
    addr: SocketAddr,
    store: Arc<Flaky>,
    _app: Arc<App>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// An upstream node whose certificate is issued by `pki` for `host`.
async fn upstream(pki: &Pki, host: &str) -> Upstream {
    let memory = Arc::new(InMemory::new());
    memory
        .put(&Path::from("app.css"), content().into())
        .await
        .unwrap();
    let store = Flaky::new(memory);
    let addr = free_addr();
    let issued = pki.issue(host);
    let config = config(
        HashMap::from([("shield".into(), s3()), ("private".into(), s3())]),
        Some(ServeBackends {
            listen: addr,
            backends: vec!["shield".into()],
            tls: ServeTls {
                ca: pki.ca(),
                cert: issued.cert,
                key: issued.key,
                client_name: CLIENT_NAME.into(),
            },
        }),
    );
    let stores = HashMap::from([
        ("shield".to_string(), store.clone() as Arc<dyn ObjectStore>),
        (
            "private".to_string(),
            Arc::new(InMemory::new()) as Arc<dyn ObjectStore>,
        ),
    ]);
    let app = App::new(&config, stores).await.unwrap();
    let tasks = app.start_upstream().await.unwrap();
    Upstream {
        addr,
        store,
        _app: app,
        tasks,
    }
}

/// A downstream node's config, with `shield` reading `backend` of `upstream` with a
/// certificate issued by `pki` for `name`.
fn downstream_config(upstream: &Upstream, pki: &Pki, name: &str, backend: &str) -> Config {
    let issued = pki.issue(name);
    let hylla = Backend::Hylla {
        url: format!("https://localhost:{}", upstream.addr.port()),
        backend: backend.into(),
        tls: HyllaTls {
            ca: pki.ca(),
            cert: issued.cert,
            key: issued.key,
        },
        refresh: Some(Refresh {
            concurrency: 2,
            rules: vec![RefreshRule {
                prefix: Some(String::new()),
                key: None,
                every: Interval::from_secs(3600),
                mode: RefreshMode::Preload,
                max_objects: Some(10),
                max_mib: None,
            }],
        }),
    };
    config(HashMap::from([("shield".into(), hylla)]), None)
}

async fn downstream(upstream: &Upstream, pki: &Pki) -> Arc<App> {
    let config = downstream_config(upstream, pki, CLIENT_NAME, "shield");
    let stores = hylla_server::backend::stores(&config).unwrap();
    App::new(&config, stores).await.unwrap()
}

/// The `shield` store of a downstream node's config.
fn store(config: &Config) -> Arc<dyn ObjectStore> {
    hylla_server::backend::stores(config).unwrap()["shield"].clone()
}

async fn request(
    app: &Arc<App>,
    method: &str,
    path: &str,
    range: Option<&str>,
) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "cache.example.com");
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    let response = app
        .listener(LISTEN.parse().unwrap())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default();
    (status, headers, body)
}

// r[verify backend.hylla]
// r[verify backend.hylla.reads]
// r[verify upstream.objects]
// r[verify upstream.tls]
// r[verify config.backends.hylla]
// r[verify config.serve-backends]
#[tokio::test]
async fn nodes_read_through_another_nodes_cache() {
    let pki = Pki::new("upstream-read");
    let upstream = upstream(&pki, "localhost").await;
    let a = downstream(&upstream, &pki).await;
    let b = downstream(&upstream, &pki).await;

    let (status, headers, body) = request(&a, "GET", "/app.css", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, content());
    assert_eq!(headers[header::CONTENT_LENGTH], "3000");
    let fetched = upstream.store.requests();
    assert!(fetched > 0);

    // Another node reads it from the upstream node's cache, so its backend isn't asked.
    let (status, _, body) = request(&b, "GET", "/app.css", Some("bytes=1000-1999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, content()[1000..2000]);
    let (status, headers, _) = request(&b, "HEAD", "/app.css", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], "3000");
    assert_eq!(
        upstream.store.requests(),
        fetched,
        "the upstream node fetched the object once"
    );
    assert_eq!(
        request(&b, "GET", "/missing.css", None).await.0,
        StatusCode::NOT_FOUND
    );
}

// r[verify upstream.list]
#[tokio::test]
async fn refresh_rules_list_through_another_node() {
    let pki = Pki::new("upstream-list");
    let upstream = upstream(&pki, "localhost").await;
    let app = downstream(&upstream, &pki).await;
    let report = app.refresh("shield", 0).await.unwrap();
    assert_eq!((report.listed, report.preloaded), (1, 1));
}

// r[verify backend.hylla.reads]
// r[verify backend.hylla.read-only]
// r[verify server.if-match]
#[tokio::test]
async fn preconditions_and_writes_are_reported_as_object_store_errors() {
    let pki = Pki::new("upstream-errors");
    let upstream = upstream(&pki, "localhost").await;
    let store = store(&downstream_config(&upstream, &pki, CLIENT_NAME, "shield"));
    let path = Path::from("app.css");

    let head = store.head(&path).await.unwrap();
    assert_eq!(head.size, 3000);
    let etag = head.e_tag.clone().unwrap();
    let options = |if_match: Option<String>, if_none_match: Option<String>| GetOptions {
        if_match,
        if_none_match,
        range: Some((0..10).into()),
        ..Default::default()
    };
    let bytes = store
        .get_opts(&path, options(Some(etag.clone()), None))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes, content()[..10]);
    assert!(matches!(
        store
            .get_opts(&path, options(Some("\"another\"".into()), None))
            .await,
        Err(object_store::Error::Precondition { .. })
    ));
    assert!(matches!(
        store.get_opts(&path, options(None, Some(etag))).await,
        Err(object_store::Error::NotModified { .. })
    ));
    assert!(matches!(
        store.head(&Path::from("missing.css")).await,
        Err(object_store::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.put(&path, b"x".to_vec().into()).await,
        Err(object_store::Error::NotSupported { .. })
    ));
    assert!(matches!(
        store.delete(&path).await,
        Err(object_store::Error::NotSupported { .. })
    ));
}

// r[verify upstream.backends]
#[tokio::test]
async fn only_the_listed_backends_are_served() {
    let pki = Pki::new("upstream-backends");
    let upstream = upstream(&pki, "localhost").await;
    let store = store(&downstream_config(&upstream, &pki, CLIENT_NAME, "private"));
    let error = store.head(&Path::from("app.css")).await.unwrap_err();
    assert!(error.to_string().contains("403"), "{error}");
}

// r[verify upstream.tls]
#[tokio::test]
async fn nodes_without_the_right_certificates_are_refused() {
    let pki = Pki::new("upstream-refuse");
    let other = Pki::new("upstream-refuse-other");
    let upstream = upstream(&pki, "localhost").await;
    let path = Path::from("app.css");

    // A client certificate issued for another name, or by another CA.
    for (pki, name) in [(&pki, "cache.example.com"), (&other, CLIENT_NAME)] {
        let store = store(&downstream_config(&upstream, pki, name, "shield"));
        assert!(store.head(&path).await.is_err(), "{name}");
    }
    // An upstream node whose certificate isn't for the host it's reached at.
    let elsewhere = self::upstream(&pki, "elsewhere.example.com").await;
    let store = store(&downstream_config(&elsewhere, &pki, CLIENT_NAME, "shield"));
    assert!(store.head(&path).await.is_err());
    assert_eq!(upstream.store.requests() + elsewhere.store.requests(), 0);

    // And plain HTTP isn't served.
    let plain = reqwest::Client::new()
        .get(format!(
            "http://{}/backends/shield/objects/app.css",
            upstream.addr
        ))
        .send()
        .await;
    assert!(plain.is_err(), "{plain:?}");
}
