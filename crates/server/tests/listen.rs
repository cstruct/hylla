//! The limits of listeners, finishing requests on shutdown, and the health checks, over
//! real connections.

mod common;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use common::Flaky;
use hylla_config::{Backend, Cache, Config, Health, Http, Interval, Mapping, Server};
use hylla_server::App;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
};

fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A running node serving `app.css` from a backend that can be slowed down.
struct Node {
    addr: SocketAddr,
    health: SocketAddr,
    backend: Arc<Flaky>,
    app: Arc<App>,
    stop: Option<oneshot::Sender<()>>,
    served: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Node {
    async fn start(http: Http) -> Self {
        let addr = free_port();
        let health = free_port();
        let inner = Arc::new(InMemory::new());
        inner
            .put(&Path::from("app.css"), b"body {}".to_vec().into())
            .await
            .unwrap();
        let backend = Flaky::new(inner);
        let config = Config {
            servers: HashMap::from([(
                "public".into(),
                Server {
                    listen: addr,
                    server_names: vec!["cache.test".into()],
                    mappings: vec![Mapping {
                        path_prefix: "/".into(),
                        backend: "assets".into(),
                        rewrites: Vec::new(),
                        writable: false,
                        cache_control: None,
                        auth: None,
                        site: None,
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
                    timeouts: Default::default(),
                    refresh: None,
                },
            )]),
            cache: Cache::default(),
            auth: HashMap::new(),
            telemetry: Default::default(),
            control: None,
            serve_backends: None,
            health: Some(Health { listen: health }),
            http,
            cluster: None,
        };
        let stores = HashMap::from([(
            "assets".to_string(),
            backend.clone() as Arc<dyn ObjectStore>,
        )]);
        let app = App::new(&config, stores).await.unwrap();
        let (stop, stopped) = oneshot::channel();
        let served = tokio::spawn(hylla_server::serve(app.clone(), async move {
            let _ = stopped.await;
        }));
        let node = Self {
            addr,
            health,
            backend,
            app,
            stop: Some(stop),
            served,
        };
        // Until the listeners are bound.
        for _ in 0..100 {
            if node.app.ready() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(node.app.ready());
        node
    }

    fn stop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.unwrap()
    }
}

fn http() -> Http {
    Http::default()
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: cache.test\r\n\r\n")
}

/// Reads one response: its status and body, by its `Content-Length`.
async fn response(stream: &mut TcpStream) -> Option<(u16, Vec<u8>)> {
    let mut read = Vec::new();
    let mut buf = [0; 4096];
    let header_end = loop {
        if let Some(end) = read.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        read.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&read[..header_end]).to_string();
    let status = head.split(' ').nth(1)?.parse().ok()?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = read[header_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Some((status, body))
}

/// Whether the other side closed `stream` within `within`.
async fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0; 64];
    matches!(
        tokio::time::timeout(within, stream.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

async fn health(addr: SocketAddr, path: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: health\r\n\r\n").as_bytes())
        .await
        .unwrap();
    response(&mut stream).await.unwrap().0
}

// r[verify server.limits]
// r[verify config.http]
#[tokio::test]
async fn headers_must_arrive_within_the_timeout() {
    let node = Node::start(Http {
        header_read_timeout: Interval::from_secs(1),
        ..http()
    })
    .await;
    let mut stream = node.connect().await;
    let started = Instant::now();
    // Headers that never end.
    stream
        .write_all(b"GET /app.css HTTP/1.1\r\nHost: cache.test\r\n")
        .await
        .unwrap();
    let mut buf = [0; 1024];
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        // hyper may answer 408 before closing.
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(read.is_ok(), "the connection stayed open");
    assert!(started.elapsed() >= Duration::from_millis(900));

    // Requests sent in time are served.
    let mut stream = node.connect().await;
    stream.write_all(get("/app.css").as_bytes()).await.unwrap();
    assert_eq!(
        response(&mut stream).await,
        Some((200, b"body {}".to_vec()))
    );
}

// r[verify server.limits]
#[tokio::test]
async fn requests_waiting_on_the_backend_are_not_idle() {
    let node = Node::start(Http {
        idle_timeout: Interval::from_secs(1),
        ..http()
    })
    .await;
    node.backend.delay(Duration::from_millis(2500));
    let mut stream = node.connect().await;
    stream.write_all(get("/app.css").as_bytes()).await.unwrap();
    let (status, body) = response(&mut stream).await.expect("not closed as idle");
    assert_eq!(status, 200);
    assert_eq!(body, b"body {}");
}

// r[verify server.limits]
#[tokio::test]
async fn idle_connections_are_closed() {
    let node = Node::start(Http {
        idle_timeout: Interval::from_secs(1),
        ..http()
    })
    .await;
    let mut stream = node.connect().await;
    stream.write_all(get("/app.css").as_bytes()).await.unwrap();
    assert_eq!(response(&mut stream).await.unwrap().0, 200);
    // Kept alive, until it's idle for the timeout.
    let started = Instant::now();
    assert!(closed_within(&mut stream, Duration::from_secs(5)).await);
    assert!(started.elapsed() >= Duration::from_millis(900));

    // Without a timeout, idle connections stay open.
    let node = Node::start(Http {
        idle_timeout: Interval::from_secs(0),
        ..http()
    })
    .await;
    let mut stream = node.connect().await;
    stream.write_all(get("/app.css").as_bytes()).await.unwrap();
    assert_eq!(response(&mut stream).await.unwrap().0, 200);
    assert!(!closed_within(&mut stream, Duration::from_millis(1500)).await);
}

// r[verify server.limits]
#[tokio::test]
async fn connections_beyond_the_limit_wait() {
    let node = Node::start(Http {
        max_connections: 1,
        ..http()
    })
    .await;
    let mut first = node.connect().await;
    first.write_all(get("/app.css").as_bytes()).await.unwrap();
    assert_eq!(response(&mut first).await.unwrap().0, 200);

    // Accepted by the OS, but not served while the first is open.
    let mut second = node.connect().await;
    second.write_all(get("/app.css").as_bytes()).await.unwrap();
    let waiting = tokio::time::timeout(Duration::from_millis(500), response(&mut second)).await;
    assert!(waiting.is_err(), "served beyond the limit");

    drop(first);
    let served = tokio::time::timeout(Duration::from_secs(5), response(&mut second)).await;
    assert_eq!(served.unwrap().unwrap().0, 200);
}

// r[verify server.shutdown]
// r[verify health.ready]
// r[verify health.live]
// r[verify config.health]
#[tokio::test]
async fn shutdown_finishes_requests_in_flight() {
    let mut node = Node::start(http()).await;
    assert_eq!(health(node.health, "/healthz").await, 200);
    assert_eq!(health(node.health, "/readyz").await, 200);

    // Each read takes a second, metadata and the chunk.
    node.backend.delay(Duration::from_millis(500));
    let mut stream = node.connect().await;
    stream.write_all(get("/app.css").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    node.stop();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Not ready while finishing, and not accepting connections.
    assert_eq!(health(node.health, "/readyz").await, 503);
    assert_eq!(health(node.health, "/healthz").await, 200);
    let refused = match TcpStream::connect(node.addr).await {
        Err(_) => true,
        Ok(mut late) => {
            let _ = late.write_all(get("/app.css").as_bytes()).await;
            response(&mut late).await.is_none()
        }
    };
    assert!(refused, "accepted a connection while shutting down");

    assert_eq!(
        response(&mut stream).await,
        Some((200, b"body {}".to_vec()))
    );
    let served = tokio::time::timeout(Duration::from_secs(5), node.served)
        .await
        .expect("serve returns once requests finished");
    served.unwrap().unwrap();
}

// r[verify server.shutdown]
#[tokio::test]
async fn shutdown_closes_connections_after_the_timeout() {
    let mut node = Node::start(Http {
        shutdown_timeout: Interval(Duration::from_millis(500)),
        ..http()
    })
    .await;
    node.backend.delay(Duration::from_secs(30));
    let mut stream = node.connect().await;
    stream.write_all(get("/app.css").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    node.stop();
    let served = tokio::time::timeout(Duration::from_secs(5), node.served)
        .await
        .expect("serve returns after the shutdown timeout");
    served.unwrap().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(closed_within(&mut stream, Duration::from_secs(2)).await);
}
