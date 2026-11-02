//! Mappings leaving authorization to Konklav, against a fake Konklav whose change feed never
//! connects, so the relying client asks its check API about every request.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    response::{IntoResponse, Response as AxumResponse},
    routing::{get as route_get, post},
};
use http::{HeaderMap, Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, Interval, JwtAlgorithm, Konklav, KonklavRule, KonklavScope, Mapping,
    MappingAuth, MappingKonklav, Server, Site, Verifier,
};
use hylla_server::App;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use serde_json::{Value, json};
use tower::ServiceExt;

const PRIVATE_KEY: &str = include_str!("keys/es256-private.pem");
const PUBLIC_KEY: &str = include_str!("keys/es256-public.pem");

fn listen() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}

/// How the fake Konklav answers checks.
#[derive(Clone)]
enum Answer {
    Decision(Value),
    Fail,
    Hang,
}

/// A fake Konklav: logins succeed, the change feed is down, and checks are answered as told.
struct FakeKonklav {
    url: String,
    answer: Mutex<Answer>,
    /// The bodies of the logins and checks it got.
    logins: Mutex<Vec<Value>>,
    checks: Mutex<Vec<Value>>,
}

impl FakeKonklav {
    async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Arc::new(Self {
            url: format!("http://{}", listener.local_addr().unwrap()),
            answer: Mutex::new(Answer::Decision(json!({"decision": "allow"}))),
            logins: Mutex::default(),
            checks: Mutex::default(),
        });
        let router = Router::new()
            .route("/v1/login/secret-key", post(login))
            .route("/v1/check", post(check))
            .route(
                "/v1/events",
                route_get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/.well-known/jwks.json",
                route_get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            )
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        fake
    }

    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    fn checks(&self) -> Vec<Value> {
        self.checks.lock().unwrap().clone()
    }

    fn config(&self) -> Konklav {
        Konklav {
            relying_url: self.url.clone(),
            subject_url: self.url.clone(),
            key_id: "hylla-key".into(),
            secret: "hylla-secret".into(),
            issuer: "https://id.example.com".into(),
            audience: "files.example.com".into(),
            timeout: Interval(Duration::from_millis(300)),
        }
    }
}

async fn login(State(fake): State<Arc<FakeKonklav>>, Json(body): Json<Value>) -> Json<Value> {
    fake.logins.lock().unwrap().push(body);
    Json(json!({"access_token": "service-token", "expires_in": 3600}))
}

async fn check(
    State(fake): State<Arc<FakeKonklav>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> AxumResponse {
    assert_eq!(
        headers[header::AUTHORIZATION].to_str().unwrap(),
        "Bearer service-token"
    );
    fake.checks.lock().unwrap().push(body);
    let answer = fake.answer.lock().unwrap().clone();
    match answer {
        Answer::Decision(decision) => Json(decision).into_response(),
        Answer::Fail => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Answer::Hang => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn mapping(path_prefix: &str, konklav: Option<MappingKonklav>) -> Mapping {
    Mapping {
        path_prefix: path_prefix.into(),
        backend: "assets".into(),
        rewrites: Vec::new(),
        writable: false,
        cache_control: None,
        auth: None,
        konklav,
        site: None,
    }
}

fn rule(path: &str, actions: &[&str], scope: Option<(&str, &str)>) -> KonklavRule {
    KonklavRule {
        path: path.into(),
        actions: actions.iter().map(|action| (*action).into()).collect(),
        scope: scope.map(|(kind, id)| KonklavScope {
            kind: kind.into(),
            id: id.into(),
        }),
    }
}

fn rules(rules: Vec<KonklavRule>) -> Option<MappingKonklav> {
    Some(MappingKonklav {
        rules,
        private: true,
    })
}

/// The mappings most tests use: objects by id, tenant-wide reports, and public files.
fn mappings() -> Vec<Mapping> {
    vec![
        mapping(
            "/object",
            rules(vec![rule(
                "/object/{object_id}/*",
                &["object:read"],
                Some(("object", "object_id")),
            )]),
        ),
        mapping(
            "/reports",
            rules(vec![
                rule("/reports/secret", &["report:admin"], None),
                rule("/reports/*", &["report:read"], None),
            ]),
        ),
        mapping("/public", None),
    ]
}

async fn app(
    mappings: Vec<Mapping>,
    konklav: Option<Konklav>,
    verifiers: HashMap<String, Verifier>,
) -> Arc<App> {
    let store = Arc::new(InMemory::new());
    let build = "1111111111111111111111111111111111111111";
    store
        .put(
            &Path::from("aliases/live"),
            build.as_bytes().to_vec().into(),
        )
        .await
        .unwrap();
    for key in [
        "object/o-1/report.pdf",
        "object/o-2/report.pdf",
        "reports/q1.pdf",
        "reports/secret",
        "public/app.css",
        "builds/1111111111111111111111111111111111111111/index.html",
    ] {
        store
            .put(&Path::from(key), key.as_bytes().to_vec().into())
            .await
            .unwrap();
    }
    let config = Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: listen(),
                server_names: vec!["cache.example.com".into()],
                mappings,
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
        auth: verifiers,
        konklav,
        telemetry: Default::default(),
        control: None,
        serve_backends: None,
        health: None,
        http: Default::default(),
        cluster: None,
    };
    let stores = HashMap::from([("assets".to_string(), store as Arc<dyn ObjectStore>)]);
    App::new(&config, stores).await.unwrap()
}

struct Response {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

async fn get(app: &Arc<App>, path: &str, token: Option<&str>) -> Response {
    let mut request = Request::builder()
        .uri(path)
        .header(header::HOST, "cache.example.com");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
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

// r[verify konklav.client]
// r[verify konklav.mappings]
// r[verify konklav.requirements]
// r[verify konklav.tenancy]
// r[verify konklav.decide]
// r[verify konklav.private]
#[tokio::test]
async fn captures_become_scopes_konklav_decides() {
    let fake = FakeKonklav::start().await;
    let app = app(mappings(), Some(fake.config()), HashMap::new()).await;

    let response = get(&app, "/object/o-1/report.pdf", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"object/o-1/report.pdf");
    assert_eq!(response.header(header::CACHE_CONTROL), Some("private"));

    // hylla logged in as its service principal, and asked about the caller's token, at the
    // captured object, leaving its tenant to Konklav.
    assert_eq!(
        fake.logins.lock().unwrap()[0],
        json!({"key_id": "hylla-key", "secret": "hylla-secret"})
    );
    assert_eq!(
        fake.checks(),
        vec![json!({
            "token": "caller-token",
            "requirements": {
                "scopes": [{"kind": "object", "id": "o-1", "actions": ["object:read"]}],
            },
        })]
    );

    // Without a scope, the actions are required tenant-wide.
    let response = get(&app, "/reports/q1.pdf", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        fake.checks()[1]["requirements"],
        json!({"tenant": ["report:read"]})
    );

    // Mappings without `konklav` aren't decided by it.
    let response = get(&app, "/public/app.css", None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header(header::CACHE_CONTROL), None);
    assert_eq!(fake.checks().len(), 2);
}

// r[verify konklav.first-match]
// r[verify konklav.patterns]
#[tokio::test]
async fn the_first_matching_rule_decides_and_unmatched_paths_are_refused() {
    let fake = FakeKonklav::start().await;
    let app = app(mappings(), Some(fake.config()), HashMap::new()).await;

    let response = get(&app, "/reports/secret", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        fake.checks()[0]["requirements"],
        json!({"tenant": ["report:admin"]})
    );

    // `/object` itself has no object id, so no rule matches it: it's refused without asking.
    let response = get(&app, "/object", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(
        response.header(header::WWW_AUTHENTICATE),
        Some(r#"Bearer error="insufficient_scope""#)
    );
    assert_eq!(response.header(header::CACHE_CONTROL), Some("private"));
    assert_eq!(fake.checks().len(), 1);
}

// r[verify konklav.dot-segments]
#[tokio::test]
async fn dot_segments_are_refused() {
    let fake = FakeKonklav::start().await;
    let app = app(mappings(), Some(fake.config()), HashMap::new()).await;
    for path in ["/object/o-1/../o-2/report.pdf", "/object/./o-2/report.pdf"] {
        let response = get(&app, path, Some("caller-token")).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{path}");
    }
    assert!(fake.checks().is_empty());
}

// r[verify konklav.bearer]
// r[verify konklav.responses]
#[tokio::test]
async fn refusals_follow_konklavs_decisions() {
    let fake = FakeKonklav::start().await;
    let app = app(mappings(), Some(fake.config()), HashMap::new()).await;
    let path = "/object/o-2/report.pdf";

    let response = get(&app, path, None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(response.header(header::WWW_AUTHENTICATE), Some("Bearer"));
    assert!(fake.checks().is_empty());

    fake.answer(Answer::Decision(
        json!({"decision": "deny", "reason": "invalid_token"}),
    ));
    let response = get(&app, path, Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.header(header::WWW_AUTHENTICATE),
        Some(r#"Bearer error="invalid_token""#)
    );

    fake.answer(Answer::Decision(
        json!({"decision": "deny", "reason": "token_kind"}),
    ));
    let response = get(&app, path, Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(
        response.header(header::WWW_AUTHENTICATE),
        Some(r#"Bearer error="insufficient_scope""#)
    );
    assert_eq!(response.header(header::CACHE_CONTROL), Some("private"));

    fake.answer(Answer::Decision(json!({
        "decision": "step_up",
        "requirement": {"max_age_seconds": 300},
    })));
    let response = get(&app, path, Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.header(header::WWW_AUTHENTICATE),
        Some(r#"Bearer error="insufficient_user_authentication", max_age=300"#)
    );

    fake.answer(Answer::Decision(json!({
        "decision": "step_up",
        "requirement": {"min_methods": 2},
    })));
    let response = get(&app, path, Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.header(header::WWW_AUTHENTICATE),
        Some(r#"Bearer error="insufficient_user_authentication""#)
    );
}

// r[verify konklav.obligations]
#[tokio::test]
async fn allows_relying_on_share_attributes_are_refused() {
    let fake = FakeKonklav::start().await;
    let app = app(mappings(), Some(fake.config()), HashMap::new()).await;
    let obligation = |attributes: Value| {
        json!({
            "decision": "allow",
            "obligations": [{
                "share": "0193a2f0-0000-7000-8000-00000000000a",
                "tenant": "0193a2f0-0000-7000-8000-000000000001",
                "kind": "object",
                "id": "o-1",
                "attributes": attributes,
            }],
        })
    };

    fake.answer(Answer::Decision(obligation(json!({}))));
    let response = get(&app, "/object/o-1/report.pdf", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::OK);

    fake.answer(Answer::Decision(obligation(json!({
        "data_range": {"from": "2026-01-01", "until": "2026-06-30"},
    }))));
    let response = get(&app, "/object/o-1/report.pdf", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

// r[verify konklav.fail-closed]
// r[verify konklav.cache]
#[tokio::test]
async fn requests_konklav_cant_decide_are_refused_even_when_cached() {
    let fake = FakeKonklav::start().await;
    let app = app(mappings(), Some(fake.config()), HashMap::new()).await;
    let path = "/object/o-1/report.pdf";

    // Cached now.
    assert_eq!(
        get(&app, path, Some("caller-token")).await.status,
        StatusCode::OK
    );

    fake.answer(Answer::Fail);
    let response = get(&app, path, Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.body.is_empty());
    assert_eq!(response.header(header::CACHE_CONTROL), Some("private"));

    fake.answer(Answer::Hang);
    let started = std::time::Instant::now();
    let response = get(&app, path, Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(started.elapsed() < Duration::from_secs(5));

    // Every request was asked about, the cached one too.
    assert_eq!(fake.checks().len(), 3);
}

// r[verify konklav.client]
#[tokio::test]
async fn startup_doesnt_wait_for_konklav() {
    // Nothing listens there.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let mut konklav = FakeKonklav::start().await.config();
    konklav.relying_url = url.clone();
    konklav.subject_url = url;
    let app = app(mappings(), Some(konklav), HashMap::new()).await;

    let response = get(&app, "/object/o-1/report.pdf", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    let response = get(&app, "/public/app.css", None).await;
    assert_eq!(response.status, StatusCode::OK);
}

// r[verify konklav.optional]
#[tokio::test]
async fn without_konklav_mappings_are_served_as_before() {
    let app = app(vec![mapping("/public", None)], None, HashMap::new()).await;
    let response = get(&app, "/public/app.css", None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header(header::CACHE_CONTROL), None);
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn jwt() -> String {
    jsonwebtoken::encode(
        &Header::new(Algorithm::ES256),
        &json!({"sub": "acme", "exp": now() + 3600}),
        &EncodingKey::from_ec_pem(PRIVATE_KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}

// r[verify konklav.with-auth+2]
#[tokio::test]
async fn auth_is_checked_before_konklav_is_asked() {
    let fake = FakeKonklav::start().await;
    let mut both = mapping(
        "/object",
        Some(MappingKonklav {
            rules: vec![rule(
                "/object/{object_id}/*",
                &["object:read"],
                Some(("object", "object_id")),
            )],
            private: false,
        }),
    );
    both.auth = Some(MappingAuth {
        verifier: "partners".into(),
        paths: Vec::new(),
        private: true,
    });
    let verifiers = HashMap::from([(
        "partners".to_string(),
        Verifier {
            algorithm: JwtAlgorithm::Es256,
            public_key: Some(PUBLIC_KEY.into()),
            jwks_uri: None,
            issuer: None,
            audience: None,
            leeway: Interval::from_secs(30),
        },
    )]);
    let app = app(vec![both], Some(fake.config()), verifiers).await;
    let path = "/object/o-1/report.pdf";

    // A token `auth` refuses never reaches Konklav.
    let response = get(&app, path, Some("not-a-jwt")).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(fake.checks().is_empty());

    // One it allows is decided by Konklav, which gets the same token.
    let token = jwt();
    fake.answer(Answer::Decision(
        json!({"decision": "deny", "reason": "token_kind"}),
    ));
    let response = get(&app, path, Some(&token)).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(fake.checks()[0]["token"], json!(token));

    fake.answer(Answer::Decision(json!({"decision": "allow"})));
    let response = get(&app, path, Some(&token)).await;
    assert_eq!(response.status, StatusCode::OK);
    // Private, since `auth` asks for it, though `konklav` doesn't.
    assert_eq!(response.header(header::CACHE_CONTROL), Some("private"));
}

// r[verify konklav.sites]
#[tokio::test]
async fn sites_ask_konklav_before_choosing_a_build() {
    let fake = FakeKonklav::start().await;
    let mut site = mapping("/", rules(vec![rule("/*", &["site:read"], None)]));
    site.site = Some(Site {
        builds: "builds/".into(),
        aliases: "aliases/".into(),
        alias: Some("live".into()),
        index: "index.html".into(),
        fallback: None,
        redirect: true,
        alias_cache_control: "no-cache".into(),
    });
    let app = app(vec![site], Some(fake.config()), HashMap::new()).await;

    fake.answer(Answer::Decision(
        json!({"decision": "deny", "reason": "token_kind"}),
    ));
    let response = get(&app, "/", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(
        fake.checks(),
        vec![json!({
            "token": "caller-token",
            "requirements": {"tenant": ["site:read"]},
        })]
    );

    fake.answer(Answer::Decision(json!({"decision": "allow"})));
    let response = get(&app, "/", Some("caller-token")).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body,
        b"builds/1111111111111111111111111111111111111111/index.html"
    );
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some("private, no-cache")
    );

    let response = get(&app, "/", None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}
