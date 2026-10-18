mod common;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::body::{Body, to_bytes};
use base64::Engine;
use common::{Flaky, Pki};
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Cluster, ClusterShare, ClusterTls, Config, Control, ControlUser, Interval,
    Mapping, Refresh, RefreshMode, RefreshRule, Server,
};
use hylla_server::App;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use tower::ServiceExt;

const KEY: &str = "a cluster key of at least thirty-two characters";

fn content() -> Vec<u8> {
    (0..3000u32).map(|i| (i % 251) as u8).collect()
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A shared backend with `app.css`, and a store over it per node, so each node's backend
/// can fail and count requests on its own.
async fn backend() -> Arc<InMemory> {
    let memory = Arc::new(InMemory::new());
    memory
        .put(&Path::from("app.css"), content().into())
        .await
        .unwrap();
    memory
}

struct Node {
    app: Arc<App>,
    addr: SocketAddr,
    store: Arc<Flaky>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn node(memory: &Arc<InMemory>, seeds: Vec<String>, key: &str) -> Node {
    node_with(memory, seeds, key, None, None).await
}

async fn node_with_tls(
    memory: &Arc<InMemory>,
    seeds: Vec<String>,
    key: &str,
    tls: Option<ClusterTls>,
) -> Node {
    node_with(memory, seeds, key, tls, None).await
}

/// A node whose mapping is writable through its control plane, by `admin`.
async fn writable_node(memory: &Arc<InMemory>, seeds: Vec<String>) -> Node {
    let control = Control {
        listen: "127.0.0.1:9090".parse().unwrap(),
        users: HashMap::from([(
            "admin".into(),
            ControlUser {
                password_hash: hash(PASSWORD),
            },
        )]),
        max_upload_mib: 1,
        oidc: None,
    };
    node_with(memory, seeds, KEY, None, Some(control)).await
}

const PASSWORD: &str = "correct horse battery staple";

fn hash(password: &str) -> String {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let salt = SaltString::encode_b64(b"a salt for tests").unwrap();
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

async fn node_with(
    memory: &Arc<InMemory>,
    seeds: Vec<String>,
    key: &str,
    tls: Option<ClusterTls>,
    control: Option<Control>,
) -> Node {
    let addr = free_addr();
    let store = Flaky::new(memory.clone());
    let config = Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: "127.0.0.1:8080".parse().unwrap(),
                server_names: vec!["cache.example.com".into()],
                mappings: vec![Mapping {
                    path_prefix: "/".into(),
                    backend: "assets".into(),
                    rewrites: Vec::new(),
                    writable: control.is_some(),
                    auth: None,
                }],
            },
        )]),
        backends: HashMap::from([(
            "assets".into(),
            Backend::S3 {
                bucket: "assets".into(),
                region: "eu-north-1".into(),
                endpoint: None,
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
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
            },
        )]),
        cache: Cache {
            chunk_size_kib: 1,
            metadata_ttl: Interval::from_secs(3600),
            ..Cache::default()
        },
        auth: HashMap::new(),
        telemetry: Default::default(),
        control,
        serve_backends: None,
        cluster: Some(Cluster {
            listen: addr,
            advertise: None,
            seeds,
            gossip_interval: Interval(Duration::from_millis(50)),
            failure_timeout: Interval(Duration::from_millis(500)),
            key: key.into(),
            tls,
            share: ClusterShare::default(),
        }),
    };
    config.validate().unwrap();
    let stores = HashMap::from([("assets".to_string(), store.clone() as Arc<dyn ObjectStore>)]);
    let app = App::new(&config, stores).await.unwrap();
    let tasks = app.start_cluster().await.unwrap();
    Node {
        app,
        addr,
        store,
        tasks,
    }
}

async fn eventually(what: &str, mut condition: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn request(app: &Arc<App>, method: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "cache.example.com")
        .body(Body::empty())
        .unwrap();
    let response = app
        .listener("127.0.0.1:8080".parse().unwrap())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    // A body can fail midway, when a chunk can't be fetched.
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .map(|body| body.to_vec())
        .unwrap_or_default();
    (status, body)
}

async fn three_nodes(memory: &Arc<InMemory>) -> (Node, Node, Node) {
    let a = node(memory, Vec::new(), KEY).await;
    let b = node(memory, vec![a.addr.to_string()], KEY).await;
    // A DNS name works as a seed too.
    let c = node(memory, vec![format!("localhost:{}", a.addr.port())], KEY).await;
    for node in [&a, &b, &c] {
        eventually("all nodes to join", async || {
            node.app.cluster_members().len() == 3
        })
        .await;
    }
    (a, b, c)
}

// r[verify cluster.membership]
// r[verify cluster.seeds]
// r[verify config.cluster.seeds]
#[tokio::test]
async fn nodes_join_through_seeds() {
    let memory = backend().await;
    let (a, b, c) = three_nodes(&memory).await;
    let mut expected = vec![a.addr, b.addr, c.addr];
    expected.sort();
    for node in [&a, &b, &c] {
        assert_eq!(node.app.cluster_members(), expected);
    }
}

// r[verify cluster.ownership]
// r[verify cluster.refresh]
// r[verify cluster.identity]
#[tokio::test]
async fn rules_have_one_owner_which_moves_when_it_leaves() {
    let memory = backend().await;
    let (a, b, c) = three_nodes(&memory).await;
    let owners = |nodes: &[&Node], rule| {
        nodes
            .iter()
            .filter(|node| node.app.owns_refresh_rule("assets", rule))
            .count()
    };
    for rule in 0..20 {
        assert_eq!(owners(&[&a, &b, &c], rule), 1, "rule {rule}");
    }
    // Rules spread over the nodes.
    assert!((0..20).any(|rule| c.app.owns_refresh_rule("assets", rule)));

    drop(c);
    for node in [&a, &b] {
        eventually("the gone node to be dropped", async || {
            node.app.cluster_members().len() == 2
        })
        .await;
    }
    for rule in 0..20 {
        assert_eq!(owners(&[&a, &b], rule), 1, "rule {rule}");
    }
}

// r[verify cluster.share-metadata]
// r[verify cluster.apply-metadata]
// r[verify config.cluster.share]
#[tokio::test]
async fn fetched_metadata_is_shared() {
    let memory = backend().await;
    let a = node(&memory, Vec::new(), KEY).await;
    let b = node(&memory, vec![a.addr.to_string()], KEY).await;
    eventually("both nodes to join", async || {
        a.app.cluster_members().len() == 2
    })
    .await;
    b.store.fail(true);

    assert_eq!(request(&a.app, "GET", "/app.css").await.1, content());
    // Give the push time to arrive; asking B earlier would make it ask its backend.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // B answers HEAD, which needs only metadata, without its backend.
    assert_eq!(request(&b.app, "HEAD", "/app.css").await.0, StatusCode::OK);
    assert_eq!(b.store.requests(), 0, "B never asked its backend");
}

// r[verify cluster.share-preload]
// r[verify cluster.pull]
// r[verify cluster.chunks]
#[tokio::test]
async fn preloads_are_pulled_from_the_node_that_preloaded() {
    let memory = backend().await;
    let a = node(&memory, Vec::new(), KEY).await;
    let b = node(&memory, vec![a.addr.to_string()], KEY).await;
    eventually("both nodes to join", async || {
        b.app.cluster_members().len() == 2
    })
    .await;
    let (owner, other) = if a.app.owns_refresh_rule("assets", 0) {
        (&a, &b)
    } else {
        (&b, &a)
    };
    other.store.fail(true);

    let report = owner.app.refresh("assets", 0).await.unwrap();
    assert_eq!(report.preloaded, 1);
    // Give the announcement and the pull time to finish; asking the other node earlier
    // would make it fetch what it lacks from its backend.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The other node pulled the chunks from the owner, without its backend.
    assert_eq!(request(&other.app, "GET", "/app.css").await.1, content());
    assert_eq!(
        other.store.requests(),
        0,
        "the other node never asked its backend"
    );
}

/// Changes `/app.css` through `app`'s control plane, signed in as `admin`.
async fn change(app: &Arc<App>, method: &str, body: &'static str) {
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("admin:{PASSWORD}"));
    let request = Request::builder()
        .method(method)
        .uri("/v1/mappings/public/0/objects/app.css")
        .header(header::HOST, "127.0.0.1:9090")
        .header(header::AUTHORIZATION, format!("Basic {credentials}"))
        .body(Body::from(body))
        .unwrap();
    let response = app
        .control_router()
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "{method}");
}

// r[verify cluster.share-forgets]
// r[verify cluster.apply-forgets]
// r[verify control.invalidate]
#[tokio::test]
async fn objects_changed_through_the_control_plane_are_forgotten_by_every_node() {
    let memory = backend().await;
    let a = writable_node(&memory, Vec::new()).await;
    let b = writable_node(&memory, vec![a.addr.to_string()]).await;
    for node in [&a, &b] {
        eventually("both nodes to join", async || {
            node.app.cluster_members().len() == 2
        })
        .await;
    }
    // B caches app.css, metadata and chunks, for an hour.
    assert_eq!(request(&b.app, "GET", "/app.css").await.1, content());

    // Overwritten through A, B serves the new content rather than what it cached.
    change(&a.app, "PUT", "changed").await;
    eventually("B to forget the overwritten object", async || {
        request(&b.app, "GET", "/app.css").await.1 == b"changed"
    })
    .await;

    // Deleted through A, B no longer serves it.
    change(&a.app, "DELETE", "").await;
    eventually("B to forget the deleted object", async || {
        request(&b.app, "GET", "/app.css").await.0 == StatusCode::NOT_FOUND
    })
    .await;
}

// r[verify cluster.auth]
// r[verify config.cluster.key]
#[tokio::test]
async fn nodes_without_the_key_are_refused() {
    let memory = backend().await;
    let a = node(&memory, Vec::new(), KEY).await;
    let intruder = node(
        &memory,
        vec![a.addr.to_string()],
        "another key of at least thirty-two characters",
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://{}/cluster/gossip", a.addr))
        .body(r#"{"members":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.app.cluster_members(), vec![a.addr]);
    assert_eq!(intruder.app.cluster_members(), vec![intruder.addr]);
}

// r[verify cluster.reannounce]
// r[verify cluster.pull]
#[tokio::test]
async fn nodes_joining_later_catch_up_on_the_next_preload() {
    let memory = backend().await;
    let a = node(&memory, Vec::new(), KEY).await;
    // Alone, A owns the rule and preloads.
    assert_eq!(a.app.refresh("assets", 0).await.unwrap().preloaded, 1);

    let b = node(&memory, vec![a.addr.to_string()], KEY).await;
    eventually("both nodes to join", async || {
        a.app.cluster_members().len() == 2
    })
    .await;
    b.store.fail(true);

    // A's next run finds app.css unchanged, and announces it again.
    let report = a.app.refresh("assets", 0).await.unwrap();
    assert_eq!((report.preloaded, report.unchanged), (0, 1));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(request(&b.app, "GET", "/app.css").await.1, content());
    assert_eq!(b.store.requests(), 0, "B pulled from A, not its backend");
}

/// The name the nodes' certificates are issued for.
const SERVER_NAME: &str = "hylla.cluster";

/// TLS settings for a node of a cluster with `pki`'s CA, with a certificate for `name`.
fn cluster_tls(pki: &Pki, name: &str) -> ClusterTls {
    let issued = pki.issue(name);
    ClusterTls {
        ca: pki.ca(),
        cert: issued.cert,
        key: issued.key,
        server_name: SERVER_NAME.into(),
    }
}

// r[verify cluster.tls]
// r[verify config.cluster.tls]
#[tokio::test]
async fn nodes_share_over_mutual_tls() {
    let memory = backend().await;
    let pki = Pki::new("share");
    let a = node_with_tls(
        &memory,
        Vec::new(),
        KEY,
        Some(cluster_tls(&pki, SERVER_NAME)),
    )
    .await;
    let b = node_with_tls(
        &memory,
        vec![a.addr.to_string()],
        KEY,
        Some(cluster_tls(&pki, SERVER_NAME)),
    )
    .await;
    for node in [&a, &b] {
        eventually("both nodes to join", async || {
            node.app.cluster_members().len() == 2
        })
        .await;
    }

    // Preloads are announced and their chunks pulled over TLS too.
    let (owner, other) = if a.app.owns_refresh_rule("assets", 0) {
        (&a, &b)
    } else {
        (&b, &a)
    };
    other.store.fail(true);
    assert_eq!(owner.app.refresh("assets", 0).await.unwrap().preloaded, 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(request(&other.app, "GET", "/app.css").await.1, content());
    assert_eq!(other.store.requests(), 0, "the chunks came from the owner");

    // The listener doesn't speak plain HTTP.
    let plain = reqwest::Client::new()
        .post(format!("http://{}/cluster/gossip", a.addr))
        .body(r#"{"members":[]}"#)
        .send()
        .await;
    assert!(plain.is_err(), "{plain:?}");
}

// r[verify cluster.tls]
#[tokio::test]
async fn nodes_without_a_certificate_for_the_cluster_are_refused() {
    let memory = backend().await;
    let pki = Pki::new("refuse");
    let other = Pki::new("refuse-other");
    let a = node_with_tls(
        &memory,
        Vec::new(),
        KEY,
        Some(cluster_tls(&pki, SERVER_NAME)),
    )
    .await;
    let seeds = vec![a.addr.to_string()];
    let intruders = [
        // Issued by another CA.
        node_with_tls(
            &memory,
            seeds.clone(),
            KEY,
            Some(cluster_tls(&other, SERVER_NAME)),
        )
        .await,
        // Issued by the cluster CA, but for something else: refused connecting, and refused
        // when connected to.
        node_with_tls(
            &memory,
            seeds.clone(),
            KEY,
            Some(cluster_tls(&pki, "cache.example.com")),
        )
        .await,
        // Without TLS.
        node(&memory, seeds, KEY).await,
    ];

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(a.app.cluster_members(), vec![a.addr]);
    for intruder in &intruders {
        assert_eq!(intruder.app.cluster_members(), vec![intruder.addr]);
    }
}
