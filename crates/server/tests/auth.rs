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
    routing::get as route_get,
};
use base64::Engine;
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, Interval, JwtAlgorithm, Mapping, MappingAuth, Rewrite, Server, Verifier,
};
use hylla_server::App;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use serde_json::{Value, json};
use tower::ServiceExt;

const PRIVATE_KEY: &str = include_str!("keys/es256-private.pem");
const PUBLIC_KEY: &str = include_str!("keys/es256-public.pem");
const OTHER_PRIVATE_KEY: &str = include_str!("keys/other-private.pem");

fn listen() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}

fn verifier(issuer: Option<&str>, audience: Option<&str>) -> Verifier {
    Verifier {
        algorithm: JwtAlgorithm::Es256,
        public_key: Some(PUBLIC_KEY.into()),
        jwks_uri: None,
        issuer: issuer.map(Into::into),
        audience: audience.map(Into::into),
        leeway: Interval::from_secs(30),
    }
}

fn mapping(path_prefix: &str, rewrites: &[(&str, &str)], auth: Option<MappingAuth>) -> Mapping {
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
        writable: false,
        cache_control: None,
        auth,
        site: None,
    }
}

fn auth(verifier: &str, paths: &[&str]) -> Option<MappingAuth> {
    Some(MappingAuth {
        verifier: verifier.into(),
        paths: paths.iter().map(|path| (*path).into()).collect(),
        private: true,
    })
}

/// A server with these mappings, in front of objects under `tenant/acme/`,
/// `tenant/other/`, `tenant/42/` and `public/`.
async fn app(mappings: Vec<Mapping>, verifiers: HashMap<String, Verifier>) -> Arc<App> {
    let store = Arc::new(InMemory::new());
    for key in [
        "tenant/acme/report.pdf",
        "tenant/other/report.pdf",
        "tenant/42/report.pdf",
        "public/app.css",
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

fn verifiers(verifier: Verifier) -> HashMap<String, Verifier> {
    HashMap::from([("partners".to_string(), verifier)])
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn token(claims: Value) -> String {
    sign(claims, PRIVATE_KEY)
}

fn sign(claims: Value, private_key: &str) -> String {
    jsonwebtoken::encode(
        &Header::new(Algorithm::ES256),
        &claims,
        &EncodingKey::from_ec_pem(private_key.as_bytes()).unwrap(),
    )
    .unwrap()
}

/// A token valid for an hour, with `claims` added.
fn valid(mut claims: Value) -> String {
    claims["exp"] = json!(now() + 3600);
    token(claims)
}

struct Response {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

async fn get(app: &Arc<App>, path: &str, authorization: Option<&str>) -> Response {
    let mut request = Request::builder()
        .uri(path)
        .header(header::HOST, "cache.example.com");
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
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

async fn get_with(app: &Arc<App>, path: &str, token: &str) -> Response {
    get(app, path, Some(&format!("Bearer {token}"))).await
}

fn header(response: &Response, name: header::HeaderName) -> Option<&str> {
    response
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap())
}

// r[verify auth.mappings]
// r[verify auth.bearer]
// r[verify auth.validate]
// r[verify auth.responses]
// r[verify auth.private]
// r[verify config.auth.mappings]
// r[verify config.auth.verifiers]
// r[verify config.auth.algorithms]
#[tokio::test]
async fn tokens_must_be_signed_and_current() {
    let app = app(
        vec![mapping("/", &[], auth("partners", &[]))],
        verifiers(verifier(None, None)),
    )
    .await;
    let path = "/public/app.css";

    let response = get(&app, path, None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(header(&response, header::WWW_AUTHENTICATE), Some("Bearer"));
    assert_eq!(header(&response, header::CACHE_CONTROL), Some("private"));
    assert_eq!(
        get(&app, path, Some("Basic dXNlcjpwYXNz")).await.status,
        StatusCode::UNAUTHORIZED
    );

    let response = get_with(&app, path, &valid(json!({}))).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"public/app.css");
    assert_eq!(header(&response, header::CACHE_CONTROL), Some("private"));

    let invalid = |token: String| {
        let app = app.clone();
        async move {
            let response = get_with(&app, path, &token).await;
            assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{token}");
            assert_eq!(
                header(&response, header::WWW_AUTHENTICATE),
                Some(r#"Bearer error="invalid_token""#)
            );
        }
    };
    // Expired, beyond the leeway of 30 seconds.
    invalid(token(json!({ "exp": now() - 120 }))).await;
    // Not valid yet.
    invalid(token(json!({ "exp": now() + 3600, "nbf": now() + 120 }))).await;
    // Without an expiry.
    invalid(token(json!({ "sub": "acme" }))).await;
    // Signed with another key.
    invalid(sign(json!({ "exp": now() + 3600 }), OTHER_PRIVATE_KEY)).await;
    // Signed with HMAC, using the public key as the secret.
    invalid(
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &json!({ "exp": now() + 3600 }),
            &EncodingKey::from_secret(PUBLIC_KEY.as_bytes()),
        )
        .unwrap(),
    )
    .await;
    // Not signed at all.
    invalid(format!(
        "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{}.",
        base64_url(&json!({ "exp": now() + 3600 }).to_string())
    ))
    .await;
    invalid("garbage".into()).await;

    // Expired within the leeway.
    let response = get_with(&app, path, &token(json!({ "exp": now() - 10 }))).await;
    assert_eq!(response.status, StatusCode::OK);
}

// r[verify auth.validate]
#[tokio::test]
async fn issuers_and_audiences_are_checked_when_configured() {
    let app = app(
        vec![mapping("/", &[], auth("partners", &[]))],
        verifiers(verifier(Some("https://auth.example.com"), Some("hylla"))),
    )
    .await;
    let status = |claims| {
        let app = app.clone();
        async move {
            get_with(&app, "/public/app.css", &valid(claims))
                .await
                .status
        }
    };
    let good = json!({ "iss": "https://auth.example.com", "aud": "hylla" });
    // Configured claims are required, not only checked when present.
    assert_eq!(
        status(json!({ "aud": "hylla" })).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(status(good).await, StatusCode::OK);
    assert_eq!(
        status(json!({ "iss": "https://auth.example.com" })).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(json!({ "iss": "https://auth.example.com", "aud": "other" })).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(json!({ "iss": "https://evil.example.com", "aud": "hylla" })).await,
        StatusCode::UNAUTHORIZED
    );
}

// r[verify auth.paths]
// r[verify auth.claim-values]
#[tokio::test]
async fn path_templates_limit_tokens_to_their_claims() {
    let app = app(
        vec![
            // Templates match the backend path, after rewrites.
            mapping(
                "/files",
                &[(r"^/files/(.*)$", "/tenant/$1")],
                auth("partners", &["/tenant/${sub}"]),
            ),
            mapping("/", &[], auth("partners", &["/tenant/${sub}", "/public"])),
        ],
        verifiers(verifier(None, None)),
    )
    .await;
    let acme = valid(json!({ "sub": "acme" }));

    let response = get_with(&app, "/tenant/acme/report.pdf", &acme).await;
    assert_eq!(response.body, b"tenant/acme/report.pdf");
    assert_eq!(
        get_with(&app, "/files/acme/report.pdf", &acme).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get_with(&app, "/public/app.css", &acme).await.status,
        StatusCode::OK
    );

    let response = get_with(&app, "/tenant/other/report.pdf", &acme).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(
        header(&response, header::WWW_AUTHENTICATE),
        Some(r#"Bearer error="insufficient_scope""#)
    );
    assert_eq!(
        get_with(&app, "/files/other/report.pdf", &acme)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Numeric claims are used as text.
    let numeric = valid(json!({ "sub": 42 }));
    assert_eq!(
        get_with(&app, "/tenant/42/report.pdf", &numeric)
            .await
            .status,
        StatusCode::OK
    );

    // Missing claims, and claims that aren't a single segment, allow nothing they'd fill in.
    for claims in [
        json!({}),
        json!({ "sub": "../other" }),
        json!({ "sub": "acme/x" }),
        json!({ "sub": "" }),
    ] {
        let token = valid(claims.clone());
        assert_eq!(
            get_with(&app, "/tenant/other/report.pdf", &token)
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{claims}"
        );
    }
}

// r[verify auth.dot-segments]
#[tokio::test]
async fn dot_segments_are_refused() {
    let app = app(
        vec![mapping("/", &[], auth("partners", &["/tenant/${sub}"]))],
        verifiers(verifier(None, None)),
    )
    .await;
    let acme = valid(json!({ "sub": "acme" }));
    for path in [
        "/tenant/acme/../other/report.pdf",
        "/tenant/acme/%2e%2e/other/report.pdf",
        "/tenant/acme/./report.pdf",
    ] {
        assert_eq!(
            get_with(&app, path, &acme).await.status,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
}

// r[verify server.routes.decoded]
#[tokio::test]
async fn encoded_paths_are_routed_as_decoded() {
    let app = app(
        vec![
            mapping("/tenant", &[], auth("partners", &["/tenant/${sub}"])),
            mapping("/", &[], None),
        ],
        verifiers(verifier(None, None)),
    )
    .await;
    // Each decodes to /tenant/acme/report.pdf, so it must reach the mapping with auth, not
    // fall through to the public one.
    for path in [
        "/%74enant/acme/report.pdf",
        "/%54ENANT/acme/report.pdf",
        "/tenant%2Facme/report.pdf",
        "/tenant%2facme%2freport.pdf",
        "/public/..%2Ftenant/acme/report.pdf",
        "/public/%2e%2e/tenant/acme/report.pdf",
    ] {
        let response = get(&app, path, None).await;
        assert_ne!(response.status, StatusCode::OK, "{path}");
        assert_ne!(response.body, b"tenant/acme/report.pdf", "{path}");
    }
    for path in ["/%74enant/acme/report.pdf", "/tenant%2Facme/report.pdf"] {
        assert_eq!(
            get(&app, path, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
        let other = valid(json!({ "sub": "other" }));
        assert_eq!(
            get_with(&app, path, &other).await.status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
        let acme = valid(json!({ "sub": "acme" }));
        let response = get_with(&app, path, &acme).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert_eq!(response.body, b"tenant/acme/report.pdf");
    }
    // Encoded characters that aren't separators still reach public objects.
    let response = get(&app, "/public/app%2Ecss", None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"public/app.css");
    assert_eq!(
        get(&app, "/public/%FF.css", None).await.status,
        StatusCode::BAD_REQUEST
    );
}

// r[verify auth.private]
// r[verify config.auth.mappings]
#[tokio::test]
async fn private_can_be_turned_off_and_public_mappings_stay_public() {
    let mut no_private = auth("partners", &[]).unwrap();
    no_private.private = false;
    let app = app(
        vec![
            mapping("/tenant", &[], Some(no_private)),
            mapping("/public", &[], None),
        ],
        verifiers(verifier(None, None)),
    )
    .await;
    let response = get_with(&app, "/tenant/acme/report.pdf", &valid(json!({}))).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(header(&response, header::CACHE_CONTROL), None);

    let response = get(&app, "/public/app.css", None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(header(&response, header::CACHE_CONTROL), None);
}

// r[verify auth.keys]
#[tokio::test]
async fn invalid_public_keys_fail_at_startup() {
    let mut broken = verifier(None, None);
    broken.public_key = Some("-----BEGIN PUBLIC KEY-----\nnope\n-----END PUBLIC KEY-----\n".into());
    let config = Config {
        servers: HashMap::new(),
        backends: HashMap::new(),
        cache: Cache::default(),
        auth: verifiers(broken),
        telemetry: Default::default(),
        control: None,
        serve_backends: None,
        health: None,
        http: Default::default(),
        cluster: None,
    };
    let error = App::new(&config, HashMap::new()).await.err().unwrap();
    assert!(
        format!("{error:#}").contains("auth verifier `partners`"),
        "{error:#}"
    );
}

fn base64_url(data: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = data.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | (u32::from(*byte) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

// r[verify auth.list-claims]
#[tokio::test]
async fn list_claims_allow_any_of_their_items() {
    let app = app(
        vec![mapping("/", &[], auth("partners", &["/tenant/${tenants}"]))],
        verifiers(verifier(None, None)),
    )
    .await;
    let token = valid(json!({ "tenants": ["other", 42, "../acme", ["acme"]] }));
    for (path, status) in [
        ("/tenant/other/report.pdf", StatusCode::OK),
        ("/tenant/42/report.pdf", StatusCode::OK),
        ("/tenant/acme/report.pdf", StatusCode::FORBIDDEN),
        ("/public/app.css", StatusCode::FORBIDDEN),
    ] {
        assert_eq!(get_with(&app, path, &token).await.status, status, "{path}");
    }
}

/// A key a fake provider signs tokens with.
struct SigningKey {
    encoding: EncodingKey,
    jwk: Value,
}

impl SigningKey {
    fn new(kid: &str) -> Self {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let public = key.public_key_raw();
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        Self {
            encoding: EncodingKey::from_ec_pem(key.serialize_pem().as_bytes()).unwrap(),
            jwk: json!({
                "kty": "EC", "crv": "P-256", "kid": kid, "alg": "ES256", "use": "sig",
                "x": b64(&public[1..33]), "y": b64(&public[33..65]),
            }),
        }
    }

    /// A token valid for an hour, naming `kid` in its header.
    fn sign(&self, kid: Option<&str>, mut claims: Value) -> String {
        claims["exp"] = json!(now() + 3600);
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(Into::into);
        jsonwebtoken::encode(&header, &claims, &self.encoding).unwrap()
    }
}

#[derive(Default)]
struct KeysState {
    /// The issuer the discovery document says.
    issuer: String,
    /// The `jwks_uri` the discovery document says, instead of the provider's own.
    jwks_uri: Option<String>,
    keys: Vec<Value>,
    down: bool,
    discovery_reads: usize,
    jwks_reads: usize,
}

/// A fake provider publishing its keys, at `/jwks` and through its discovery document.
struct Keys {
    url: String,
    state: Arc<Mutex<KeysState>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Keys {
    async fn start(keys: Vec<Value>) -> Self {
        async fn discovery(
            State(state): State<Arc<Mutex<KeysState>>>,
        ) -> Result<Json<Value>, StatusCode> {
            let mut state = state.lock().unwrap();
            state.discovery_reads += 1;
            if state.down {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            let jwks_uri = state
                .jwks_uri
                .clone()
                .unwrap_or_else(|| format!("{}/jwks", state.issuer));
            Ok(Json(
                json!({ "issuer": state.issuer, "jwks_uri": jwks_uri }),
            ))
        }
        async fn jwks(
            State(state): State<Arc<Mutex<KeysState>>>,
        ) -> Result<Json<Value>, StatusCode> {
            let keys = {
                let mut state = state.lock().unwrap();
                state.jwks_reads += 1;
                if state.down {
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
                state.keys.clone()
            };
            // Slow, so concurrent requests overlap.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Json(json!({ "keys": keys })))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(KeysState {
            issuer: url.clone(),
            keys,
            ..KeysState::default()
        }));
        let router = Router::new()
            .route("/.well-known/openid-configuration", route_get(discovery))
            .route("/jwks", route_get(jwks))
            .route(
                "/moved",
                route_get(|| async { axum::response::Redirect::temporary("/jwks") }),
            )
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            url,
            state,
            _task: task,
        }
    }

    fn reads(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        (state.discovery_reads, state.jwks_reads)
    }

    /// A verifier reading its keys through the issuer's discovery document.
    fn verifier(&self) -> Verifier {
        Verifier {
            algorithm: JwtAlgorithm::Es256,
            public_key: None,
            jwks_uri: None,
            issuer: Some(self.url.clone()),
            audience: Some("hylla".into()),
            leeway: Interval::from_secs(30),
        }
    }
}

async fn provider_app(verifier: Verifier) -> Arc<App> {
    app(
        vec![mapping("/", &[], auth("partners", &["/tenant/${sub}"]))],
        verifiers(verifier),
    )
    .await
}

// r[verify auth.provider-keys]
// r[verify config.auth.verifiers]
#[tokio::test]
async fn keys_are_read_from_the_issuers_discovery_document() {
    let key = SigningKey::new("a");
    let keys = Keys::start(vec![key.jwk.clone()]).await;
    let app = provider_app(keys.verifier()).await;
    let claims = json!({ "sub": "acme", "aud": "hylla", "iss": keys.url });
    let token = key.sign(Some("a"), claims.clone());

    // Read once, for all the requests waiting on it.
    let requests = (0..10).map(|_| get_with(&app, "/tenant/acme/report.pdf", &token));
    for response in futures::future::join_all(requests).await {
        assert_eq!(response.status, StatusCode::OK);
    }
    assert_eq!(keys.reads(), (1, 1));
    assert_eq!(
        get_with(&app, "/tenant/other/report.pdf", &token)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // A key the provider doesn't have, and just said it doesn't, isn't read again for.
    let unknown = SigningKey::new("b").sign(Some("b"), claims);
    let response = get_with(&app, "/tenant/acme/report.pdf", &unknown).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(keys.reads(), (1, 1));
}

// r[verify auth.provider-keys]
#[tokio::test]
async fn keys_are_read_from_a_jwks_uri() {
    let key = SigningKey::new("a");
    let keys = Keys::start(vec![key.jwk.clone()]).await;
    let app = provider_app(Verifier {
        issuer: None,
        jwks_uri: Some(format!("{}/jwks", keys.url)),
        ..keys.verifier()
    })
    .await;
    let token = key.sign(Some("a"), json!({ "sub": "acme", "aud": "hylla" }));
    assert_eq!(
        get_with(&app, "/tenant/acme/report.pdf", &token)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(keys.reads(), (0, 1));
}

// r[verify auth.provider-keys]
// r[verify auth.provider-down]
#[tokio::test]
async fn requests_are_refused_with_503_while_there_are_no_keys() {
    let key = SigningKey::new("a");
    let claims = json!({ "sub": "acme", "aud": "hylla" });

    let keys = Keys::start(vec![key.jwk.clone()]).await;
    keys.state.lock().unwrap().down = true;
    let app = provider_app(keys.verifier()).await;
    let token = key.sign(Some("a"), claims.clone());
    let response = get_with(&app, "/tenant/acme/report.pdf", &token).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, header::CACHE_CONTROL), Some("private"));
    // Tokens that couldn't be valid are refused without waiting for the provider.
    let response = get_with(&app, "/tenant/acme/report.pdf", "garbage").await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    // A provider saying it's another issuer has no keys for this one.
    let keys = Keys::start(vec![key.jwk.clone()]).await;
    keys.state.lock().unwrap().issuer = "https://evil.example".into();
    let app = provider_app(keys.verifier()).await;
    let token = key.sign(Some("a"), claims);
    assert_eq!(
        get_with(&app, "/tenant/acme/report.pdf", &token)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(keys.reads(), (1, 0));
}

// r[verify auth.provider-key-choice]
#[tokio::test]
async fn tokens_are_checked_with_the_key_they_name() {
    let (a, b, c, d) = (
        SigningKey::new("a"),
        SigningKey::new("b"),
        SigningKey::new("c"),
        SigningKey::new("d"),
    );
    let mut wrong_algorithm = c.jwk.clone();
    wrong_algorithm["alg"] = json!("ES384");
    let mut for_encryption = d.jwk.clone();
    for_encryption["use"] = json!("enc");
    let keys = Keys::start(vec![
        a.jwk.clone(),
        b.jwk.clone(),
        wrong_algorithm,
        for_encryption,
    ])
    .await;
    let app = provider_app(keys.verifier()).await;
    let claims = json!({ "sub": "acme", "aud": "hylla", "iss": keys.url });
    let status = |token: String| {
        let app = app.clone();
        async move {
            get_with(&app, "/tenant/acme/report.pdf", &token)
                .await
                .status
        }
    };
    assert_eq!(
        status(a.sign(Some("a"), claims.clone())).await,
        StatusCode::OK
    );
    assert_eq!(
        status(b.sign(Some("b"), claims.clone())).await,
        StatusCode::OK
    );
    // Signed with a, but naming b.
    assert_eq!(
        status(a.sign(Some("b"), claims.clone())).await,
        StatusCode::UNAUTHORIZED
    );
    // Without a key id, when there's a choice of keys.
    assert_eq!(
        status(a.sign(None, claims.clone())).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(c.sign(Some("c"), claims.clone())).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(d.sign(Some("d"), claims.clone())).await,
        StatusCode::UNAUTHORIZED
    );

    // Without a key id, when there's only one key.
    let keys = Keys::start(vec![a.jwk.clone()]).await;
    let app = provider_app(keys.verifier()).await;
    let claims = json!({ "sub": "acme", "aud": "hylla", "iss": keys.url });
    let response = get_with(&app, "/tenant/acme/report.pdf", &a.sign(None, claims)).await;
    assert_eq!(response.status, StatusCode::OK);
}

// r[verify auth.private]
#[tokio::test]
async fn private_responses_keep_the_other_cache_directives() {
    let mut mapping = mapping("/", &[], auth("partners", &[]));
    mapping.cache_control = Some("public, max-age=60".into());
    let app = app(vec![mapping], verifiers(verifier(None, None))).await;
    let response = get_with(&app, "/public/app.css", &valid(json!({}))).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        header(&response, header::CACHE_CONTROL),
        Some("private, max-age=60")
    );
}

// r[verify auth.provider-https]
#[tokio::test]
async fn keys_are_only_read_over_https_without_redirects() {
    let key = SigningKey::new("a");
    let claims = |keys: &Keys| json!({ "sub": "acme", "aud": "hylla", "iss": keys.url });

    // A discovery document pointing at keys over plain HTTP elsewhere.
    let keys = Keys::start(vec![key.jwk.clone()]).await;
    keys.state.lock().unwrap().jwks_uri = Some("http://keys.example.com/jwks".into());
    let app = provider_app(keys.verifier()).await;
    let token = key.sign(Some("a"), claims(&keys));
    assert_eq!(
        get_with(&app, "/tenant/acme/report.pdf", &token)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // Keys behind a redirect.
    let keys = Keys::start(vec![key.jwk.clone()]).await;
    keys.state.lock().unwrap().jwks_uri = Some(format!("{}/moved", keys.url));
    let app = provider_app(keys.verifier()).await;
    let token = key.sign(Some("a"), claims(&keys));
    assert_eq!(
        get_with(&app, "/tenant/acme/report.pdf", &token)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(keys.reads(), (1, 0));
}
