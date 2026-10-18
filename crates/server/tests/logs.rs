//! Logs exported over OTLP to a fake collector.
//!
//! In a binary of its own, since telemetry installs a global subscriber.

mod common;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{body::Bytes, extract::State, routing::post};
use common::Flaky;
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, Interval, Logs, Mapping, Otlp, Server, Telemetry, TracesExport,
};
use hylla_server::{App, telemetry};
use object_store::{ObjectStore, memory::InMemory};
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::{AnyValue, any_value::Value},
    logs::v1::{LogRecord, SeverityNumber},
};
use prost::Message;
use tower::ServiceExt;

const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";

/// The exported records, with their scope's name.
#[derive(Clone, Default)]
struct Collector {
    records: Arc<Mutex<Vec<(String, LogRecord)>>>,
}

async fn collect(State(collector): State<Collector>, body: Bytes) -> StatusCode {
    let request = ExportLogsServiceRequest::decode(body).unwrap();
    let mut records = collector.records.lock().unwrap();
    for resource in request.resource_logs {
        for scope in resource.scope_logs {
            let name = scope.scope.map(|scope| scope.name).unwrap_or_default();
            records.extend(
                scope
                    .log_records
                    .into_iter()
                    .map(|record| (name.clone(), record)),
            );
        }
    }
    StatusCode::OK
}

async fn start_collector() -> (SocketAddr, Collector) {
    let collector = Collector::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/v1/logs", post(collect))
        // Spans are exported too, and dropped.
        .route("/v1/traces", post(async || StatusCode::OK))
        .with_state(collector.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, collector)
}

fn text(value: &Option<AnyValue>) -> String {
    match value.as_ref().and_then(|value| value.value.as_ref()) {
        Some(Value::StringValue(value)) => value.clone(),
        other => format!("{other:?}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// r[verify telemetry.logs]
// r[verify telemetry.logs.export]
#[tokio::test(flavor = "multi_thread")]
async fn logs_are_exported_over_otlp_with_trace_ids() {
    let (collector_addr, collector) = start_collector().await;
    // An invalid level is refused, before anything is installed.
    let invalid = telemetry::init(&Telemetry {
        logs: Logs {
            level: "hylla_server=loud".into(),
            ..Logs::default()
        },
        ..Telemetry::default()
    });
    assert!(invalid.is_err());

    let telemetry = telemetry::init(&Telemetry {
        otlp: Otlp {
            endpoint: Some(format!("http://{collector_addr}")),
            ..Otlp::default()
        },
        traces: TracesExport {
            enabled: true,
            ..TracesExport::default()
        },
        logs: Logs {
            export: true,
            ..Logs::default()
        },
        ..Telemetry::default()
    })
    .unwrap();

    let store = Flaky::new(Arc::new(InMemory::new()));
    store.fail(true);
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
                refresh: None,
            },
        )]),
        cache: Cache {
            chunk_size_kib: 1,
            metadata_ttl: Interval::from_secs(3600),
            ..Cache::default()
        },
        auth: HashMap::new(),
        telemetry: Default::default(),
        serve_backends: None,
        cluster: None,
    };
    let stores = HashMap::from([("assets".to_string(), store as Arc<dyn ObjectStore>)]);
    let app = App::new(&config, stores).await.unwrap();

    // The backend failing is logged as a warning, within the request's trace.
    let request = Request::builder()
        .uri("/app.css")
        .header(header::HOST, "cache.example.com")
        .header("traceparent", format!("00-{TRACE_ID}-b7ad6b7169203331-01"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app
        .listener("127.0.0.1:8080".parse().unwrap())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // Below the level, so neither logged nor exported.
    tracing::debug!("not exported");

    tokio::task::spawn_blocking(move || telemetry.flush())
        .await
        .unwrap();
    let records = collector.records.lock().unwrap().clone();
    let (scope, warning) = records
        .iter()
        .find(|(_, record)| text(&record.body).starts_with("metadata: "))
        .unwrap_or_else(|| panic!("no warning in {records:#?}"));
    assert_eq!(scope, "hylla_server::app");
    assert_eq!(warning.severity_number, SeverityNumber::Warn as i32);
    assert_eq!(hex(&warning.trace_id), TRACE_ID);
    let backend = warning
        .attributes
        .iter()
        .find(|kv| kv.key == "backend")
        .unwrap();
    assert_eq!(text(&backend.value), "assets");

    assert!(
        records
            .iter()
            .all(|(_, record)| text(&record.body) != "not exported")
    );
    // What the exporters log isn't exported.
    for (scope, _) in &records {
        for excluded in ["opentelemetry", "hyper", "h2", "tonic", "reqwest"] {
            assert!(!scope.starts_with(excluded), "{scope}");
        }
    }
}
