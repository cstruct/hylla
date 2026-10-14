use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::{Body, to_bytes};
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
        public_key: PUBLIC_KEY.into(),
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
        auth,
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
                refresh: None,
            },
        )]),
        cache: Cache::default(),
        auth: verifiers,
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

fn header<'a>(response: &'a Response, name: header::HeaderName) -> Option<&'a str> {
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
    broken.public_key = "-----BEGIN PUBLIC KEY-----\nnope\n-----END PUBLIC KEY-----\n".into();
    let config = Config {
        servers: HashMap::new(),
        backends: HashMap::new(),
        cache: Cache::default(),
        auth: verifiers(broken),
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
