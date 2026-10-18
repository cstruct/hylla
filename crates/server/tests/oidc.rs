//! Signing in to the control plane through an OpenID provider, against a fake provider
//! that signs ES256 ID tokens and checks PKCE as a real one does.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Form, Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::Engine;
use http::{Request, header};
use hylla_config::{Config, Control, ControlOidc, ControlUser, Interval, Mapping, Server};
use hylla_server::App;
use object_store::{ObjectStore, memory::InMemory};
use serde_json::{Value, json};
use tower::ServiceExt;

const CONTROL: &str = "http://127.0.0.1:9090";
const PASSWORD: &str = "correct horse battery staple";

fn b64(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// What the fake provider puts in the next ID tokens, and how it misbehaves.
#[derive(Clone)]
struct Behaviour {
    groups: Option<Vec<&'static str>>,
    /// The groups user info answers, for ID tokens without them.
    user_info_groups: Option<Vec<&'static str>>,
    audience: &'static str,
    /// Replaces the nonce of the sign-in.
    nonce: Option<&'static str>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            groups: Some(vec!["hylla-admin", "other"]),
            user_info_groups: None,
            audience: "hylla",
            nonce: None,
        }
    }
}

/// A sign-in the provider granted a code for.
struct Grant {
    challenge: String,
    nonce: String,
}

struct ProviderState {
    issuer: String,
    key: jsonwebtoken::EncodingKey,
    jwk: Value,
    behaviour: Behaviour,
    grants: HashMap<String, Grant>,
}

type Shared = Arc<Mutex<ProviderState>>;

struct Provider {
    issuer: String,
    state: Shared,
    _task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let public = key.public_key_raw();
        let jwk = json!({
            "kty": "EC", "crv": "P-256", "kid": "test-key", "alg": "ES256", "use": "sig",
            "x": b64(&public[1..33]), "y": b64(&public[33..65]),
        });
        let state = Arc::new(Mutex::new(ProviderState {
            issuer: issuer.clone(),
            key: jsonwebtoken::EncodingKey::from_ec_pem(key.serialize_pem().as_bytes()).unwrap(),
            jwk,
            behaviour: Behaviour::default(),
            grants: HashMap::new(),
        }));
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .route("/userinfo", get(user_info))
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            issuer,
            state,
            _task: task,
        }
    }

    fn behave(&self, behaviour: Behaviour) {
        self.state.lock().unwrap().behaviour = behaviour;
    }

    /// The user signs in at the provider for the sign-in hylla redirected to: the code the
    /// provider redirects back with.
    fn grant(&self, authorize: &HashMap<String, String>) -> String {
        let code = format!("code-{}", authorize["state"]);
        self.state.lock().unwrap().grants.insert(
            code.clone(),
            Grant {
                challenge: authorize["code_challenge"].clone(),
                nonce: authorize["nonce"].clone(),
            },
        );
        code
    }
}

async fn discovery(State(state): State<Shared>) -> Json<Value> {
    let issuer = state.lock().unwrap().issuer.clone();
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "end_session_endpoint": format!("{issuer}/logout"),
    }))
}

async fn jwks(State(state): State<Shared>) -> Json<Value> {
    Json(json!({ "keys": [state.lock().unwrap().jwk.clone()] }))
}

async fn token(
    State(state): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let refuse = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
    let client = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("hylla:client-secret")
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(&client)
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        );
    }
    let mut state = state.lock().unwrap();
    let Some(grant) = form.get("code").and_then(|code| state.grants.remove(code)) else {
        return refuse("invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let challenge = b64(aws_lc_rs::digest::digest(
        &aws_lc_rs::digest::SHA256,
        verifier.as_bytes(),
    ));
    if challenge != grant.challenge
        || form.get("redirect_uri").map(String::as_str)
            != Some("http://127.0.0.1:9090/auth/callback")
    {
        return refuse("invalid_grant");
    }
    let behaviour = state.behaviour.clone();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut claims = json!({
        "iss": state.issuer, "sub": "subject-1", "aud": behaviour.audience,
        "exp": now + 300, "iat": now, "email": "ada@example.com",
        "nonce": behaviour.nonce.map_or(grant.nonce, str::to_string),
    });
    if let Some(groups) = behaviour.groups {
        claims["groups"] = json!(groups);
    }
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some("test-key".into());
    let id_token = jsonwebtoken::encode(&header, &claims, &state.key).unwrap();
    (
        StatusCode::OK,
        Json(
            json!({ "access_token": "access-token", "token_type": "Bearer", "id_token": id_token }),
        ),
    )
}

async fn user_info(State(state): State<Shared>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some("Bearer access-token")
    {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let groups = state.lock().unwrap().behaviour.user_info_groups.clone();
    (
        StatusCode::OK,
        Json(json!({ "sub": "subject-1", "groups": groups })),
    )
}

async fn node(provider: &Provider) -> Arc<App> {
    node_with_issuer(&provider.issuer).await
}

async fn node_with_issuer(issuer: &str) -> Arc<App> {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let hash = Argon2::default()
        .hash_password(
            PASSWORD.as_bytes(),
            &SaltString::encode_b64(b"a salt for tests").unwrap(),
        )
        .unwrap()
        .to_string();
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
                    writable: false,
                    auth: None,
                }],
            },
        )]),
        backends: HashMap::from([(
            "assets".into(),
            hylla_config::Backend::S3 {
                bucket: "assets".into(),
                region: "eu-north-1".into(),
                endpoint: None,
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
                refresh: None,
            },
        )]),
        cache: Default::default(),
        auth: HashMap::new(),
        telemetry: Default::default(),
        cluster: None,
        control: Some(Control {
            listen: "127.0.0.1:9090".parse().unwrap(),
            users: HashMap::from([(
                "script".into(),
                ControlUser {
                    password_hash: hash,
                },
            )]),
            max_upload_mib: 1,
            oidc: Some(ControlOidc {
                issuer: issuer.to_string(),
                client_id: "hylla".into(),
                client_secret: Some("client-secret".into()),
                url: CONTROL.into(),
                groups: vec!["hylla-admin".into()],
                groups_claim: "groups".into(),
                scopes: vec!["email".into(), "groups".into()],
                session_ttl: Interval::from_secs(3600),
            }),
        }),
        serve_backends: None,
    };
    config.validate().unwrap();
    let stores = HashMap::from([(
        "assets".to_string(),
        Arc::new(InMemory::new()) as Arc<dyn ObjectStore>,
    )]);
    App::new(&config, stores).await.unwrap()
}

struct Answer {
    status: StatusCode,
    headers: http::HeaderMap,
    body: String,
}

impl Answer {
    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }

    /// The value a `Set-Cookie` header sets `name` to.
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| {
                let (pair, _) = value.split_once(';').unwrap_or((value, ""));
                let (key, value) = pair.split_once('=')?;
                (key == name).then(|| value.to_string())
            })
    }
}

async fn send(app: &Arc<App>, request: http::request::Builder) -> Answer {
    let response = app
        .control_router()
        .unwrap()
        .oneshot(
            request
                .header(header::HOST, "127.0.0.1:9090")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn page(uri: &str) -> http::request::Builder {
    Request::builder()
        .uri(uri)
        .header(header::ACCEPT, "text/html")
}

fn query(url: &str) -> HashMap<String, String> {
    serde_urlencoded::from_str(url.split_once('?').map_or("", |(_, query)| query)).unwrap()
}

/// Signs in through the provider, starting at `return_to`: the callback's answer.
async fn sign_in(app: &Arc<App>, provider: &Provider, return_to: &str) -> Answer {
    let login = send(app, page(&format!("/auth/login?return_to={return_to}"))).await;
    assert_eq!(login.status, StatusCode::SEE_OTHER, "{}", login.body);
    let state = login.cookie("hylla_login").unwrap();
    let authorize = query(login.location());
    let code = provider.grant(&authorize);
    send(
        app,
        page(&format!(
            "/auth/callback?code={code}&state={}",
            authorize["state"]
        ))
        .header(header::COOKIE, format!("hylla_login={state}")),
    )
    .await
}

// r[verify control.oidc.login]
// r[verify control.oidc.callback]
// r[verify control.oidc.session]
// r[verify control.oidc.id-token]
// r[verify config.control.oidc]
#[tokio::test]
async fn browsers_sign_in_through_the_provider() {
    let provider = Provider::start().await;
    let app = node(&provider).await;

    // A page without a session sends the browser to sign in, coming back where it was.
    let answer = send(&app, page("/mappings")).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location(), "/auth/login?return_to=%2Fmappings");

    let login = send(&app, page("/auth/login?return_to=/mappings")).await;
    let authorize = query(login.location());
    assert!(
        login
            .location()
            .starts_with(&format!("{}/authorize?", provider.issuer))
    );
    assert_eq!(authorize["response_type"], "code");
    assert_eq!(authorize["client_id"], "hylla");
    assert_eq!(
        authorize["redirect_uri"],
        "http://127.0.0.1:9090/auth/callback"
    );
    assert_eq!(authorize["scope"], "openid email groups");
    assert_eq!(authorize["code_challenge_method"], "S256");
    assert_eq!(
        login.cookie("hylla_login").as_deref(),
        Some(authorize["state"].as_str())
    );

    let callback = sign_in(&app, &provider, "/mappings").await;
    assert_eq!(callback.status, StatusCode::SEE_OTHER, "{}", callback.body);
    assert_eq!(callback.location(), "/mappings");
    let session = callback.cookie("hylla_session").unwrap();
    let set = callback
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        set.iter().any(|cookie| cookie.starts_with("hylla_session=")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Lax")),
        "{set:?}"
    );

    let signed_in = send(
        &app,
        page("/mappings").header(header::COOKIE, format!("hylla_session={session}")),
    )
    .await;
    assert_eq!(signed_in.status, StatusCode::OK);
    assert!(
        signed_in.body.contains("ada@example.com"),
        "{}",
        signed_in.body
    );
    assert!(signed_in.body.contains("Sign out"), "{}", signed_in.body);
    // The JSON API takes the session too.
    let api = send(
        &app,
        Request::builder()
            .uri("/v1/backends")
            .header(header::COOKIE, format!("hylla_session={session}")),
    )
    .await;
    assert_eq!(api.status, StatusCode::OK);
    // Somewhere else to return to is ignored.
    let elsewhere = sign_in(&app, &provider, "https://evil.example").await;
    assert_eq!(elsewhere.location(), "/");
}

// r[verify control.oidc.logout]
#[tokio::test]
async fn signing_out_ends_the_session_at_the_provider_too() {
    let provider = Provider::start().await;
    let app = node(&provider).await;
    let session = sign_in(&app, &provider, "/")
        .await
        .cookie("hylla_session")
        .unwrap();
    let cookie = format!("hylla_session={session}");

    // Not from another site.
    let forged = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/auth/logout")
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", "cross-site"),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);

    let out = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/auth/logout")
            .header(header::COOKIE, &cookie),
    )
    .await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    assert!(
        out.location()
            .starts_with(&format!("{}/logout?", provider.issuer))
    );
    let logout = query(out.location());
    assert!(logout.contains_key("id_token_hint"));
    assert_eq!(logout["post_logout_redirect_uri"], "http://127.0.0.1:9090/");
    assert_eq!(out.cookie("hylla_session").as_deref(), Some(""));

    let after = send(&app, page("/").header(header::COOKIE, &cookie)).await;
    assert_eq!(after.status, StatusCode::SEE_OTHER, "the session is gone");
}

// r[verify control.oidc.groups]
#[tokio::test]
async fn only_users_in_an_allowed_group_get_in() {
    let provider = Provider::start().await;
    let app = node(&provider).await;
    provider.behave(Behaviour {
        groups: Some(vec!["other"]),
        ..Behaviour::default()
    });
    let refused = sign_in(&app, &provider, "/").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(
        refused.body.contains("isn't in a group allowed"),
        "{}",
        refused.body
    );
    assert!(refused.cookie("hylla_session").is_none());

    // Groups the ID token leaves out are read from the user info.
    provider.behave(Behaviour {
        groups: None,
        user_info_groups: Some(vec!["hylla-admin"]),
        ..Behaviour::default()
    });
    assert_eq!(
        sign_in(&app, &provider, "/").await.status,
        StatusCode::SEE_OTHER
    );
    provider.behave(Behaviour {
        groups: None,
        user_info_groups: None,
        ..Behaviour::default()
    });
    assert_eq!(
        sign_in(&app, &provider, "/").await.status,
        StatusCode::FORBIDDEN
    );
}

// r[verify control.oidc.callback]
// r[verify control.oidc.id-token]
#[tokio::test]
async fn sign_ins_that_dont_check_out_are_refused() {
    let provider = Provider::start().await;
    let app = node(&provider).await;

    // Another browser's sign-in, or none.
    let login = send(&app, page("/auth/login")).await;
    let authorize = query(login.location());
    let code = provider.grant(&authorize);
    for cookie in [None, Some("hylla_login=another-state")] {
        let mut request = page(&format!(
            "/auth/callback?code={code}&state={}",
            authorize["state"]
        ));
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        assert_eq!(
            send(&app, request).await.status,
            StatusCode::BAD_REQUEST,
            "{cookie:?}"
        );
    }

    // ID tokens for another client or another sign-in.
    for behaviour in [
        Behaviour {
            audience: "another-client",
            ..Behaviour::default()
        },
        Behaviour {
            nonce: Some("another-nonce"),
            ..Behaviour::default()
        },
    ] {
        provider.behave(behaviour);
        let refused = sign_in(&app, &provider, "/").await;
        assert_eq!(refused.status, StatusCode::BAD_GATEWAY, "{}", refused.body);
        assert!(refused.cookie("hylla_session").is_none());
    }

    // The provider saying no.
    provider.behave(Behaviour::default());
    let login = send(&app, page("/auth/login")).await;
    let state = login.cookie("hylla_login").unwrap();
    let refused = send(
        &app,
        page(&format!("/auth/callback?error=access_denied&state={state}"))
            .header(header::COOKIE, format!("hylla_login={state}")),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.body.contains("access_denied"), "{}", refused.body);
}

// r[verify control.auth]
#[tokio::test]
async fn scripts_keep_using_basic_auth() {
    let provider = Provider::start().await;
    let app = node(&provider).await;
    // Not a page: no redirect, but a Basic challenge.
    let anonymous = send(&app, Request::builder().uri("/v1/backends")).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("script:{PASSWORD}"))
    );
    let script = send(
        &app,
        Request::builder()
            .uri("/v1/backends")
            .header(header::AUTHORIZATION, basic),
    )
    .await;
    assert_eq!(script.status, StatusCode::OK);
    // A made-up session is no session.
    let made_up = send(
        &app,
        page("/").header(header::COOKIE, "hylla_session=made-up"),
    )
    .await;
    assert_eq!(made_up.status, StatusCode::SEE_OTHER);
}

// r[verify control.oidc.issuer]
#[tokio::test]
async fn providers_must_be_the_issuer_they_say() {
    let provider = Provider::start().await;
    // The same provider, reached under another name than its discovery document gives.
    let app = node_with_issuer(&provider.issuer.replace("127.0.0.1", "localhost")).await;
    let login = send(&app, page("/auth/login")).await;
    assert_eq!(login.status, StatusCode::BAD_GATEWAY);
    assert!(login.body.contains("issuer"), "{}", login.body);
}
