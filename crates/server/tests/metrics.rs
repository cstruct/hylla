//! Metrics exported over OTLP to a fake collector.
//!
//! In a binary of its own, since telemetry installs a global meter provider.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{body::Bytes, extract::State, http::HeaderMap, routing::post};
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Cluster, Config, Interval, Mapping, MetricsExport, Otlp, Refresh, RefreshMode,
    RefreshRule, ResourceAttribute, Server, Telemetry,
};
use hylla_server::{App, telemetry};
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::{AnyValue, KeyValue, any_value::Value},
    metrics::v1::{Metric, metric::Data, number_data_point},
};
use prost::Message;
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Collector {
    requests: Arc<Mutex<Vec<(HeaderMap, ExportMetricsServiceRequest)>>>,
}

async fn collect(
    State(collector): State<Collector>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let request = ExportMetricsServiceRequest::decode(body).unwrap();
    collector.requests.lock().unwrap().push((headers, request));
    StatusCode::OK
}

async fn start_collector() -> (SocketAddr, Collector) {
    let collector = Collector::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/v1/metrics", post(collect))
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

/// The data points of a metric, as their attributes and value (a sum's value or a
/// histogram's count).
fn points(metric: &Metric) -> Vec<(HashMap<String, String>, f64)> {
    match metric.data.as_ref().unwrap() {
        Data::Sum(sum) => sum
            .data_points
            .iter()
            .map(|point| (attributes(&point.attributes), number(&point.value)))
            .collect(),
        Data::Histogram(histogram) => histogram
            .data_points
            .iter()
            .map(|point| (attributes(&point.attributes), point.count as f64))
            .collect(),
        Data::Gauge(gauge) => gauge
            .data_points
            .iter()
            .map(|point| (attributes(&point.attributes), number(&point.value)))
            .collect(),
        other => panic!("unexpected metric data {other:?}"),
    }
}

fn number(value: &Option<number_data_point::Value>) -> f64 {
    match value {
        Some(number_data_point::Value::AsInt(value)) => *value as f64,
        Some(number_data_point::Value::AsDouble(value)) => *value,
        None => 0.0,
    }
}

/// The value of the data point of `name` whose attributes include `matching`.
fn value(metrics: &HashMap<String, Metric>, name: &str, matching: &[(&str, &str)]) -> f64 {
    let metric = metrics
        .get(name)
        .unwrap_or_else(|| panic!("no metric `{name}` in {:?}", metrics.keys()));
    points(metric)
        .into_iter()
        .filter(|(attributes, _)| {
            matching
                .iter()
                .all(|(key, value)| attributes.get(*key).map(String::as_str) == Some(*value))
        })
        .map(|(_, value)| value)
        .sum()
}

async fn request(app: &Arc<App>, host: &str, path: &str, range: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri(path).header(header::HOST, host);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
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

// r[verify telemetry.init]
// r[verify telemetry.metrics]
// r[verify telemetry.metrics.instruments]
// r[verify telemetry.metrics.requests]
// r[verify telemetry.metrics.cache]
// r[verify telemetry.metrics.foyer]
// r[verify telemetry.metrics.refresh]
// r[verify telemetry.metrics.cluster]
// r[verify config.telemetry]
// r[verify config.telemetry.otlp]
// r[verify config.telemetry.metrics]
#[tokio::test(flavor = "multi_thread")]
async fn metrics_are_exported_over_otlp() {
    let (collector_addr, collector) = start_collector().await;
    let telemetry = telemetry::init(&Telemetry {
        service_name: "hylla".into(),
        resource: vec![ResourceAttribute {
            key: "deployment.environment".into(),
            value: "test".into(),
        }],
        otlp: Otlp {
            endpoint: Some(format!("http://{collector_addr}")),
            headers: HashMap::from([("x-api-key".into(), "secret".into())]),
            ..Otlp::default()
        },
        metrics: MetricsExport {
            enabled: true,
            endpoint: None,
            interval: Interval::from_secs(3600),
        },
        ..Telemetry::default()
    })
    .unwrap();

    let store = Arc::new(InMemory::new());
    store
        .put(&Path::from("app.css"), vec![7u8; 3000].into())
        .await
        .unwrap();
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
        // A cluster that isn't started, of only this node.
        cluster: Some(Cluster {
            listen: "127.0.0.1:7946".parse().unwrap(),
            advertise: None,
            seeds: Vec::new(),
            gossip_interval: Interval::from_secs(1),
            failure_timeout: Interval::from_secs(10),
            key: "a cluster key of at least thirty-two characters".into(),
            share: Default::default(),
        }),
    };
    let stores = HashMap::from([("assets".to_string(), store as Arc<dyn ObjectStore>)]);
    let app = App::new(&config, stores).await.unwrap();

    // A miss fetching all three chunks, then a hit, a range and an unknown host.
    assert_eq!(
        request(&app, "cache.example.com", "/app.css", None).await,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "cache.example.com", "/app.css", None).await,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "cache.example.com", "/app.css", Some("bytes=0-99")).await,
        StatusCode::PARTIAL_CONTENT
    );
    assert_eq!(
        request(&app, "other.example.com", "/app.css", None).await,
        StatusCode::NOT_FOUND
    );

    // A refresh finds the object cached and unchanged.
    let report = app.refresh("assets", 0).await.unwrap();
    assert_eq!(report.unchanged, 1);

    tokio::task::spawn_blocking(move || telemetry.flush())
        .await
        .unwrap();

    let requests = collector.requests.lock().unwrap().clone();
    assert!(!requests.is_empty(), "nothing was exported");
    let (headers, _) = &requests[0];
    assert_eq!(headers["x-api-key"], "secret");
    assert_eq!(headers["content-type"], "application/x-protobuf");

    let mut metrics = HashMap::new();
    for (_, request) in &requests {
        for resource in &request.resource_metrics {
            let resource_attributes = attributes(&resource.resource.as_ref().unwrap().attributes);
            assert_eq!(resource_attributes["service.name"], "hylla");
            assert_eq!(resource_attributes["deployment.environment"], "test");
            for scope in &resource.scope_metrics {
                for metric in &scope.metrics {
                    metrics.insert(metric.name.clone(), metric.clone());
                }
            }
        }
    }

    let requests = |status: &str| {
        value(
            &metrics,
            "http.server.request.duration",
            &[
                ("http.response.status_code", status),
                ("http.request.method", "GET"),
            ],
        )
    };
    assert_eq!(requests("200"), 2.0);
    assert_eq!(requests("206"), 1.0);
    assert_eq!(requests("404"), 1.0);
    assert_eq!(
        value(
            &metrics,
            "http.server.request.duration",
            &[("hylla.server", "public"), ("hylla.mapping", "/")]
        ),
        3.0
    );

    let chunks = |result| {
        value(
            &metrics,
            "hylla.cache.chunks",
            &[("hylla.backend", "assets"), ("hylla.result", result)],
        )
    };
    assert_eq!(chunks("miss"), 3.0);
    assert_eq!(chunks("hit"), 4.0);
    assert_eq!(
        value(
            &metrics,
            "hylla.cache.metadata",
            &[("hylla.result", "fetched")]
        ),
        1.0
    );
    assert_eq!(
        value(
            &metrics,
            "hylla.cache.metadata",
            &[("hylla.result", "fresh")]
        ),
        2.0
    );
    assert_eq!(
        value(
            &metrics,
            "hylla.backend.request.duration",
            &[("hylla.operation", "head"), ("hylla.outcome", "ok")]
        ),
        1.0
    );
    assert_eq!(
        value(
            &metrics,
            "hylla.backend.request.duration",
            &[("hylla.operation", "get"), ("hylla.outcome", "ok")]
        ),
        3.0
    );
    assert_eq!(value(&metrics, "hylla.backend.bytes", &[]), 3000.0);
    assert_eq!(value(&metrics, "hylla.served.bytes", &[]), 6100.0);
    assert_eq!(
        value(
            &metrics,
            "hylla.refresh.objects",
            &[("hylla.rule", "0"), ("hylla.result", "unchanged")]
        ),
        1.0
    );
    assert_eq!(
        value(
            &metrics,
            "hylla.backend.request.duration",
            &[("hylla.operation", "list")]
        ),
        1.0
    );
    assert_eq!(value(&metrics, "hylla.cluster.members", &[]), 1.0);
    // foyer reports its own metrics too.
    assert!(
        metrics.keys().any(|name| name.starts_with("foyer")),
        "no foyer metrics in {:?}",
        metrics.keys()
    );
}
