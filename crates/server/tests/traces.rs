//! Traces exported over OTLP to a fake collector.
//!
//! In a binary of its own, since telemetry installs a global tracer provider and subscriber.

mod common;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{body::Bytes, extract::State, routing::post};
use common::Flaky;
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Cluster, ClusterShare, Config, Interval, Mapping, Otlp, Refresh, RefreshMode,
    RefreshRule, Server, Telemetry, TracesExport,
};
use hylla_server::{App, telemetry};
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use opentelemetry_proto::tonic::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{AnyValue, KeyValue, any_value::Value},
    trace::v1::{Span, span::SpanKind, status::StatusCode as SpanStatus},
};
use prost::Message;
use tower::ServiceExt;

const KEY: &str = "a cluster key of at least thirty-two characters";
const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
const PARENT_ID: &str = "b7ad6b7169203331";

#[derive(Clone, Default)]
struct Collector {
    spans: Arc<Mutex<Vec<Span>>>,
}

async fn collect(State(collector): State<Collector>, body: Bytes) -> StatusCode {
    let request = ExportTraceServiceRequest::decode(body).unwrap();
    let mut spans = collector.spans.lock().unwrap();
    for resource in request.resource_spans {
        let attributes = attributes(&resource.resource.unwrap().attributes);
        assert_eq!(attributes["service.name"], "hylla");
        for scope in resource.scope_spans {
            spans.extend(scope.spans);
        }
    }
    StatusCode::OK
}

async fn start_collector() -> (SocketAddr, Collector) {
    let collector = Collector::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/v1/traces", post(collect))
        .with_state(collector.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, collector)
}

fn text(value: &Option<AnyValue>) -> String {
    match value.as_ref().and_then(|value| value.value.as_ref()) {
        Some(Value::StringValue(value)) => value.clone(),
        Some(Value::IntValue(value)) => value.to_string(),
        other => format!("{other:?}"),
    }
}

fn attributes(attributes: &[KeyValue]) -> HashMap<String, String> {
    attributes
        .iter()
        .map(|kv| (kv.key.clone(), text(&kv.value)))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The exported spans, flushing what's pending first.
async fn spans(telemetry: &Arc<telemetry::Telemetry>, collector: &Collector) -> Vec<Span> {
    let telemetry = telemetry.clone();
    tokio::task::spawn_blocking(move || telemetry.flush())
        .await
        .unwrap();
    collector.spans.lock().unwrap().clone()
}

fn named<'a>(spans: &'a [Span], name: &str) -> Vec<&'a Span> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn children<'a>(spans: &'a [Span], parent: &Span) -> Vec<&'a Span> {
    spans
        .iter()
        .filter(|span| span.parent_span_id == parent.span_id)
        .collect()
}

fn attribute(span: &Span, key: &str) -> Option<String> {
    attributes(&span.attributes).remove(key)
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn config(cluster: Option<Cluster>) -> Config {
    Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: "127.0.0.1:8080".parse().unwrap(),
                server_names: vec!["cache.example.com".into()],
                mappings: vec![Mapping {
                    path_prefix: "/".into(),
                    backend: "assets".into(),
                    rewrites: Vec::new(),
                    writable: false,
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
                    concurrency: 1,
                    rules: vec![RefreshRule {
                        prefix: Some(String::new()),
                        key: None,
                        every: Interval::from_secs(3600),
                        mode: RefreshMode::Both,
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
        control: None,
        serve_backends: None,
        cluster,
    }
}

async fn request(app: &Arc<App>, path: &str, traceparent: Option<&str>) -> StatusCode {
    let mut request = Request::builder()
        .uri(path)
        .header(header::HOST, "cache.example.com");
    if let Some(traceparent) = traceparent {
        request = request.header("traceparent", traceparent);
    }
    let response = app
        .listener("127.0.0.1:8080".parse().unwrap())
        .oneshot(request.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    status
}

async fn backend() -> Arc<InMemory> {
    let memory = Arc::new(InMemory::new());
    memory
        .put(&Path::from("app.css"), vec![7u8; 3000].into())
        .await
        .unwrap();
    memory
}

// r[verify telemetry.traces]
// r[verify telemetry.traces.requests]
// r[verify telemetry.traces.spans]
// r[verify telemetry.traces.paths]
// r[verify telemetry.traces.propagation]
// r[verify telemetry.traces.cluster]
// r[verify config.telemetry.traces]
#[tokio::test(flavor = "multi_thread")]
async fn traces_are_exported_over_otlp() {
    let (collector_addr, collector) = start_collector().await;
    let telemetry = Arc::new(
        telemetry::init(&Telemetry {
            otlp: Otlp {
                endpoint: Some(format!("http://{collector_addr}")),
                ..Otlp::default()
            },
            traces: TracesExport {
                enabled: true,
                record_paths: true,
                ..TracesExport::default()
            },
            ..Telemetry::default()
        })
        .unwrap(),
    );

    // A request continuing a caller's trace: its metadata and chunks, from the backend.
    let memory = backend().await;
    let store = Flaky::new(memory.clone());
    let stores = HashMap::from([("assets".to_string(), store.clone() as Arc<dyn ObjectStore>)]);
    let app = App::new(&config(None), stores).await.unwrap();
    let traceparent = format!("00-{TRACE_ID}-{PARENT_ID}-01");
    assert_eq!(
        request(&app, "/app.css", Some(&traceparent)).await,
        StatusCode::OK
    );

    let spans = spans(&telemetry, &collector).await;
    let requests = named(&spans, "GET /");
    assert_eq!(
        requests.len(),
        1,
        "{:#?}",
        spans
            .iter()
            .map(|span| (&span.name, attributes(&span.attributes)))
            .collect::<Vec<_>>()
    );
    let request_span = requests[0];
    assert_eq!(hex(&request_span.trace_id), TRACE_ID);
    assert_eq!(hex(&request_span.parent_span_id), PARENT_ID);
    assert_eq!(request_span.kind, SpanKind::Server as i32);
    assert_eq!(
        attribute(request_span, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(
        attribute(request_span, "url.path").as_deref(),
        Some("/app.css")
    );
    assert_eq!(
        attribute(request_span, "hylla.server").as_deref(),
        Some("public")
    );

    let steps = children(&spans, request_span);
    let metadata = steps.iter().find(|span| span.name == "metadata").unwrap();
    assert_eq!(
        attribute(metadata, "hylla.result").as_deref(),
        Some("fetched")
    );
    assert_eq!(
        attribute(metadata, "hylla.object").as_deref(),
        Some("app.css")
    );
    let head = children(&spans, metadata);
    assert_eq!(head.len(), 1);
    assert_eq!(head[0].name, "backend head");
    assert_eq!(head[0].kind, SpanKind::Client as i32);
    assert_eq!(attribute(head[0], "hylla.outcome").as_deref(), Some("ok"));
    let chunks = steps
        .iter()
        .filter(|span| span.name == "chunk")
        .collect::<Vec<_>>();
    assert_eq!(chunks.len(), 3);
    for chunk in chunks {
        assert_eq!(attribute(chunk, "hylla.result").as_deref(), Some("miss"));
        let get = children(&spans, chunk);
        assert_eq!(get.len(), 1);
        assert_eq!(get[0].name, "backend get");
        assert_eq!(hex(&get[0].trace_id), TRACE_ID);
    }

    // A backend failing makes a failed request, in a trace of its own.
    store.fail(true);
    assert_eq!(
        request(&app, "/other.css", None).await,
        StatusCode::BAD_GATEWAY
    );
    store.fail(false);
    let spans = self::spans(&telemetry, &collector).await;
    let failed = named(&spans, "GET /")
        .into_iter()
        .find(|span| attribute(span, "http.response.status_code").as_deref() == Some("502"))
        .unwrap();
    assert_ne!(hex(&failed.trace_id), TRACE_ID);
    assert!(failed.parent_span_id.is_empty());
    assert_eq!(
        failed.status.as_ref().unwrap().code,
        SpanStatus::Error as i32
    );

    // A refresh run, with its listing.
    let report = app.refresh("assets", 0).await.unwrap();
    assert_eq!(report.unchanged, 1);
    let spans = self::spans(&telemetry, &collector).await;
    let refresh = named(&spans, "refresh");
    assert_eq!(refresh.len(), 1);
    assert_eq!(
        attribute(refresh[0], "hylla.unchanged").as_deref(),
        Some("1")
    );
    assert!(
        children(&spans, refresh[0])
            .iter()
            .any(|span| span.name == "backend list")
    );

    // Between nodes, a preload's announcement, the pull and the chunks pulled are one trace.
    let (a_addr, b_addr) = (free_addr(), free_addr());
    let node = |addr, seeds| {
        let memory = memory.clone();
        async move {
            let stores = HashMap::from([(
                "assets".to_string(),
                Flaky::new(memory) as Arc<dyn ObjectStore>,
            )]);
            let app = App::new(
                &config(Some(Cluster {
                    listen: addr,
                    advertise: None,
                    seeds,
                    gossip_interval: Interval(Duration::from_millis(50)),
                    failure_timeout: Interval(Duration::from_millis(500)),
                    key: KEY.into(),
                    tls: None,
                    share: ClusterShare::default(),
                })),
                stores,
            )
            .await
            .unwrap();
            let tasks = app.start_cluster().await.unwrap();
            (app, tasks)
        }
    };
    let (a, a_tasks) = node(a_addr, Vec::new()).await;
    let (b, b_tasks) = node(b_addr, vec![a_addr.to_string()]).await;
    for _ in 0..200 {
        if a.cluster_members().len() == 2 && b.cluster_members().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let owner = if a.owns_refresh_rule("assets", 0) {
        &a
    } else {
        &b
    };
    assert_eq!(owner.refresh("assets", 0).await.unwrap().preloaded, 1);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let spans = self::spans(&telemetry, &collector).await;
    let share = named(&spans, "cluster.share")
        .into_iter()
        .find(|span| attribute(span, "hylla.preloaded").as_deref() == Some("1"))
        .unwrap();
    assert_eq!(share.kind, SpanKind::Producer as i32);
    let received = children(&spans, share);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].name, "cluster.updates");
    assert_eq!(received[0].kind, SpanKind::Server as i32);
    let pulls = children(&spans, received[0]);
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0].name, "cluster.pull");
    let served = children(&spans, pulls[0]);
    assert_eq!(served.len(), 3, "a chunk span per chunk");
    for chunk in served {
        assert_eq!(chunk.name, "cluster.chunk");
        assert_eq!(chunk.trace_id, share.trace_id);
        assert_eq!(attribute(chunk, "hylla.result").as_deref(), Some("hit"));
    }
    // Gossip isn't traced.
    assert!(spans.iter().all(|span| !span.name.contains("gossip")));

    for task in a_tasks.iter().chain(&b_tasks) {
        task.abort();
    }
}
