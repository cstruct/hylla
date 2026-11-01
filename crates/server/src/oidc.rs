//! Signing in to the control plane through an OpenID provider, such as Konklav, with the
//! authorization code flow and PKCE.
//!
//! Signing in redirects to the provider with a fresh state, nonce and PKCE challenge, bound
//! to the browser by a short-lived cookie. The provider redirects back with a code, which is
//! exchanged for an ID token, checked against the provider's keys, issuer, client id and
//! nonce. Users in one of the allowed groups get a session, kept in memory and named by an
//! HTTP-only cookie.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use base64::Engine;
use hylla_config::ControlOidc;
use jsonwebtoken::{Algorithm, Validation};
use serde_json::Value;

use crate::provider::{self, Discovery, Provider, Said, Source};

/// The cookie binding a sign-in to the browser that started it.
const LOGIN_COOKIE: &str = "hylla_login";
/// The cookie naming a session.
const SESSION_COOKIE: &str = "hylla_session";
/// How long a sign-in may take at the provider.
const LOGIN_TTL: Duration = Duration::from_secs(600);

/// The most sign-ins in progress at a time.
const MAX_PENDING: usize = 10_000;

/// A sign-in started at the provider.
struct Pending {
    verifier: String,
    nonce: String,
    return_to: String,
    started: Instant,
}

/// Someone signed in.
#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    id_token: String,
    expires: Instant,
}

/// Why a sign-in failed, as told to the browser.
#[derive(Debug)]
pub enum SignInError {
    /// The browser didn't start this sign-in, or took too long.
    UnknownState,
    /// The provider said no, such as when the user cancelled.
    Provider(String),
    /// The user isn't in any of the allowed groups.
    NotAllowed(String),
    /// Something failed talking to the provider, or its answer didn't check out.
    Failed(anyhow::Error),
}

impl std::fmt::Display for SignInError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownState => write!(f, "this sign-in wasn't started here, or took too long"),
            Self::Provider(error) => write!(f, "the provider refused the sign-in: {error}"),
            Self::NotAllowed(name) => {
                write!(f, "{name} isn't in a group allowed into the control plane")
            }
            Self::Failed(error) => write!(f, "signing in failed: {error:#}"),
        }
    }
}

pub struct Oidc {
    config: ControlOidc,
    provider: Provider,
    pending: Mutex<HashMap<String, Pending>>,
    /// Sessions by the SHA-256 of their cookie's value.
    sessions: Mutex<HashMap<Vec<u8>, Session>>,
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).expect("the system has randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256(value: &str) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, value.as_bytes())
        .as_ref()
        .to_vec()
}

/// A URL with query parameters appended.
fn with_query(url: &str, query: &[(&str, &str)]) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let query = serde_urlencoded::to_string(query).expect("strings encode");
    format!("{url}{separator}{query}")
}

impl Oidc {
    pub fn new(config: &ControlOidc) -> anyhow::Result<Self> {
        Ok(Self {
            config: config.clone(),
            provider: Provider::new(Source::Issuer(config.issuer.clone()))?,
            pending: Mutex::default(),
            sessions: Mutex::default(),
        })
    }

    fn url(&self) -> &str {
        self.config.url.trim_end_matches('/')
    }

    fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.url())
    }

    /// How long a sign-in lasts.
    pub fn session_ttl(&self) -> Duration {
        self.config.session_ttl.duration()
    }

    /// Whether cookies need `Secure`: the control plane is reached over HTTPS.
    pub fn secure_cookies(&self) -> bool {
        self.config.url.starts_with("https://")
    }

    // r[impl control.oidc.cookies]
    /// The name of the login cookie: over HTTPS, `__Secure-` prefixed, so browsers only take
    /// it from a secure origin.
    pub fn login_cookie(&self) -> &'static str {
        if self.secure_cookies() {
            "__Secure-hylla_login"
        } else {
            LOGIN_COOKIE
        }
    }

    /// The name of the session cookie: over HTTPS, `__Host-` prefixed, so it can't be set by
    /// another host of the domain, or for a path or domain of its own.
    pub fn session_cookie(&self) -> &'static str {
        if self.secure_cookies() {
            "__Host-hylla_session"
        } else {
            SESSION_COOKIE
        }
    }

    /// The provider's endpoints and what it said, with its keys, read again when they're
    /// old, or when `kid` isn't among the keys, as after the provider rotated them.
    // r[impl control.oidc.keys]
    async fn discovery_and_keys(
        &self,
        kid: Option<&str>,
    ) -> anyhow::Result<(Discovery, Arc<Said>)> {
        let said = self.provider.said(kid).await?;
        let discovery = said
            .discovery
            .clone()
            .context("the provider has no discovery document")?;
        Ok((discovery, said))
    }

    // r[impl control.oidc.login]
    /// Starts signing in: the provider's URL to redirect to, and the state for the login
    /// cookie. `return_to` is where to go once signed in, a path of the control plane.
    pub async fn start(&self, return_to: &str) -> anyhow::Result<(String, String)> {
        let (discovery, _) = self.discovery_and_keys(None).await?;
        let (state, verifier, nonce) = (random(), random(), random());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha256(&verifier));
        let scope = std::iter::once("openid")
            .chain(self.config.scopes.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let redirect_uri = self.redirect_uri();
        let authorization_endpoint = discovery
            .authorization_endpoint
            .context("the provider names no authorization endpoint")?;
        let url = with_query(
            &authorization_endpoint,
            &[
                ("response_type", "code"),
                ("client_id", &self.config.client_id),
                ("redirect_uri", &redirect_uri),
                ("scope", &scope),
                ("state", &state),
                ("nonce", &nonce),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        );
        insert_pending(
            &mut self.pending.lock().expect("not poisoned"),
            state.clone(),
            Pending {
                verifier,
                nonce,
                return_to: safe_return_to(return_to),
                started: Instant::now(),
            },
            MAX_PENDING,
        );
        Ok((url, state))
    }

    // r[impl control.oidc.callback]
    /// Finishes signing in, with the provider's answer and the login cookie's state: the
    /// new session's cookie value, and where to go.
    pub async fn finish(
        &self,
        query: &HashMap<String, String>,
        cookie_state: Option<&str>,
    ) -> Result<(String, String), SignInError> {
        let state = query.get("state").map(String::as_str);
        let pending = match (state, cookie_state) {
            (Some(state), Some(cookie)) if state == cookie => self
                .pending
                .lock()
                .expect("not poisoned")
                .remove(state)
                .filter(|pending| pending.started.elapsed() < LOGIN_TTL),
            _ => None,
        }
        .ok_or(SignInError::UnknownState)?;
        if let Some(error) = query.get("error") {
            let description = query.get("error_description").map_or("", String::as_str);
            return Err(SignInError::Provider(
                format!("{error} {description}").trim().to_string(),
            ));
        }
        let code = query
            .get("code")
            .ok_or_else(|| SignInError::Provider("no code".into()))?;
        let (id_token, claims, access_token) = self
            .exchange(code, &pending)
            .await
            .map_err(SignInError::Failed)?;

        // r[impl control.oidc.groups]
        let mut groups = groups(&claims, &self.config.groups_claim);
        if groups.is_none()
            && let Some(access_token) = access_token
        {
            groups = self
                .user_info(&access_token)
                .await
                .map_err(SignInError::Failed)?
                .and_then(|info| self::groups(&info, &self.config.groups_claim));
        }
        let name = ["preferred_username", "email", "sub"]
            .iter()
            .find_map(|claim| claims.get(*claim).and_then(Value::as_str))
            .unwrap_or("someone")
            .to_string();
        let allowed = groups
            .unwrap_or_default()
            .iter()
            .any(|group| self.config.groups.contains(group));
        if !allowed {
            return Err(SignInError::NotAllowed(name));
        }

        let cookie = random();
        let mut sessions = self.sessions.lock().expect("not poisoned");
        sessions.retain(|_, session| session.expires > Instant::now());
        sessions.insert(
            sha256(&cookie),
            Session {
                name,
                id_token,
                expires: Instant::now() + self.config.session_ttl.duration(),
            },
        );
        Ok((cookie, pending.return_to))
    }

    /// Exchanges a code for tokens, checking the ID token: its ID token, claims, and access
    /// token.
    async fn exchange(
        &self,
        code: &str,
        pending: &Pending,
    ) -> anyhow::Result<(String, serde_json::Map<String, Value>, Option<String>)> {
        let (discovery, _) = self.discovery_and_keys(None).await?;
        let redirect_uri = self.redirect_uri();
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &pending.verifier),
        ];
        let token_endpoint = discovery
            .token_endpoint
            .context("the provider names no token endpoint")?;
        let mut request = self.provider.client().post(&token_endpoint);
        match &self.config.client_secret {
            Some(secret) => request = request.basic_auth(&self.config.client_id, Some(secret)),
            None => form.push(("client_id", &self.config.client_id)),
        }
        let form = serde_urlencoded::to_string(&form).expect("strings encode");
        let response = request
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .context("exchanging the code")?;
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&crate::limited::read(response, crate::limited::JSON).await?)
                .context("reading the tokens")?;
        if !status.is_success() {
            bail!("the provider refused the code: {status} {body}");
        }
        let id_token = body
            .get("id_token")
            .and_then(Value::as_str)
            .context("the provider sent no ID token")?
            .to_string();
        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        let claims = self.verify(&id_token, &pending.nonce).await?;
        Ok((id_token, claims, access_token))
    }

    // r[impl control.oidc.id-token]
    /// The claims of an ID token, if the provider signed it for this client and sign-in.
    async fn verify(
        &self,
        id_token: &str,
        nonce: &str,
    ) -> anyhow::Result<serde_json::Map<String, Value>> {
        let header = jsonwebtoken::decode_header(id_token).context("reading the ID token")?;
        // Only asymmetric signatures: a token signed with a shared secret could have been
        // made by anyone holding it.
        if matches!(
            header.alg,
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
        ) {
            bail!(
                "the ID token is signed with {:?}, a shared secret",
                header.alg
            );
        }
        let (discovery, said) = self.discovery_and_keys(header.kid.as_deref()).await?;
        let key = provider::key(&said.keys, &header).context("checking the ID token's key")?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&discovery.issuer]);
        validation.set_audience(&[&self.config.client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 60;
        let token =
            jsonwebtoken::decode::<serde_json::Map<String, Value>>(id_token, &key, &validation)
                .context("checking the ID token")?;
        if token.claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
            bail!("the ID token is for another sign-in: its nonce doesn't match");
        }
        Ok(token.claims)
    }

    async fn user_info(
        &self,
        access_token: &str,
    ) -> anyhow::Result<Option<serde_json::Map<String, Value>>> {
        let (discovery, _) = self.discovery_and_keys(None).await?;
        let Some(endpoint) = &discovery.userinfo_endpoint else {
            return Ok(None);
        };
        let response = self
            .provider
            .client()
            .get(endpoint)
            .bearer_auth(access_token)
            .send()
            .await
            .context("reading the user info")?;
        if !response.status().is_success() {
            bail!("the user info endpoint answered {}", response.status());
        }
        Ok(Some(
            serde_json::from_slice(&crate::limited::read(response, crate::limited::JSON).await?)
                .context("reading the user info")?,
        ))
    }

    /// The session a cookie value names, if it's current.
    pub fn session(&self, cookie: &str) -> Option<Session> {
        self.sessions
            .lock()
            .expect("not poisoned")
            .get(&sha256(cookie))
            .filter(|session| session.expires > Instant::now())
            .cloned()
    }

    // r[impl control.oidc.logout]
    /// Ends a session: where to send the browser, the provider's logout when it has one.
    pub async fn end(&self, cookie: Option<&str>) -> String {
        let session = cookie.and_then(|cookie| {
            self.sessions
                .lock()
                .expect("not poisoned")
                .remove(&sha256(cookie))
        });
        let home = format!("{}/", self.url());
        let end_session = match self.discovery_and_keys(None).await {
            Ok((discovery, _)) => discovery.end_session_endpoint,
            Err(_) => None,
        };
        match (session, end_session) {
            (Some(session), Some(endpoint)) => with_query(
                &endpoint,
                &[
                    ("id_token_hint", &session.id_token),
                    ("client_id", &self.config.client_id),
                    ("post_logout_redirect_uri", &home),
                ],
            ),
            _ => "/".to_string(),
        }
    }
}

/// The groups in a claim, as a list of strings or a space-separated string.
fn groups(claims: &serde_json::Map<String, Value>, claim: &str) -> Option<Vec<String>> {
    match claims.get(claim)? {
        Value::Array(groups) => Some(
            groups
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        Value::String(groups) => Some(groups.split_whitespace().map(str::to_string).collect()),
        _ => None,
    }
}

// r[impl control.oidc.pending-limit]
/// Adds a sign-in in progress, dropping those that took too long, and the oldest when there
/// are `max`: anyone can start signing in, so they can't grow without bound.
fn insert_pending(
    pending: &mut HashMap<String, Pending>,
    state: String,
    started: Pending,
    max: usize,
) {
    pending.retain(|_, pending| pending.started.elapsed() < LOGIN_TTL);
    if pending.len() >= max
        && let Some(oldest) = pending
            .iter()
            .min_by_key(|(_, pending)| pending.started)
            .map(|(state, _)| state.clone())
    {
        pending.remove(&oldest);
    }
    pending.insert(state, started);
}

/// `return_to` if it's a path of the control plane, so signing in can't redirect elsewhere,
/// or `/`.
// r[impl control.oidc.return-to]
fn safe_return_to(return_to: &str) -> String {
    // Browsers drop tabs and newlines from URLs, so `/\t/evil.example` would go to
    // `//evil.example`: only printable ASCII, without spaces, is let through.
    let printable = return_to.bytes().all(|byte| byte.is_ascii_graphic());
    if printable
        && return_to.starts_with('/')
        && !return_to.starts_with("//")
        && !return_to.contains('\\')
    {
        return_to.to_string()
    } else {
        "/".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use super::{LOGIN_TTL, Oidc, Pending, groups, insert_pending, safe_return_to};

    fn config(url: &str, issuer: &str) -> hylla_config::ControlOidc {
        hylla_config::ControlOidc {
            issuer: issuer.into(),
            client_id: "hylla".into(),
            client_secret: None,
            url: url.into(),
            groups: vec!["hylla-admin".into()],
            groups_claim: "groups".into(),
            scopes: Vec::new(),
            session_ttl: hylla_config::Interval::from_secs(3600),
        }
    }

    // r[verify control.oidc.cookies]
    #[test]
    fn cookies_are_prefixed_over_https() {
        let oidc = Oidc::new(&config("https://hylla.internal", "https://id.example")).unwrap();
        assert_eq!(oidc.session_cookie(), "__Host-hylla_session");
        assert_eq!(oidc.login_cookie(), "__Secure-hylla_login");
        // Browsers refuse prefixed cookies without `Secure`, which plain HTTP can't have.
        let oidc = Oidc::new(&config("http://127.0.0.1:9090", "https://id.example")).unwrap();
        assert_eq!(oidc.session_cookie(), "hylla_session");
        assert_eq!(oidc.login_cookie(), "hylla_login");
    }

    // r[verify control.oidc.keys]
    #[tokio::test]
    async fn the_provider_is_read_once_and_used_while_it_is_down() {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

        #[derive(Default)]
        struct Fake {
            issuer: std::sync::OnceLock<String>,
            reads: AtomicUsize,
            down: AtomicBool,
        }
        async fn discovery(
            State(fake): State<Arc<Fake>>,
        ) -> Result<Json<serde_json::Value>, StatusCode> {
            fake.reads.fetch_add(1, Ordering::SeqCst);
            if fake.down.load(Ordering::SeqCst) {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            // Slow, so concurrent sign-ins overlap.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let issuer = fake.issuer.get().unwrap();
            Ok(Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
            })))
        }
        async fn jwks() -> Json<serde_json::Value> {
            Json(serde_json::json!({"keys": [{
                "kty": "oct", "kid": "a", "k": "c2VjcmV0"
            }]}))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(Fake::default());
        fake.issuer.set(issuer.clone()).unwrap();
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .with_state(fake.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await });

        let mut oidc = Oidc::new(&config("http://127.0.0.1:9090", &issuer)).unwrap();
        oidc.provider.ttl = Duration::from_millis(200);
        oidc.provider.reread_after = Duration::ZERO;
        let oidc = Arc::new(oidc);
        let reads = (0..10).map(|_| {
            let oidc = oidc.clone();
            async move { oidc.discovery_and_keys(None).await.map(|_| ()) }
        });
        for read in futures::future::join_all(reads).await {
            read.unwrap();
        }
        assert_eq!(fake.reads.load(Ordering::SeqCst), 1);

        // Past the TTL, with the provider down, what it said is still used, for keys it had.
        fake.down.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(250)).await;
        oidc.discovery_and_keys(Some("a")).await.unwrap();
        assert!(fake.reads.load(Ordering::SeqCst) > 1);
        let (_, said) = oidc.discovery_and_keys(Some("rotated")).await.unwrap();
        assert!(said.keys.find("rotated").is_none());
        server.abort();
    }

    // r[verify control.oidc.pending-limit]
    #[test]
    fn sign_ins_in_progress_are_limited() {
        let started = |at: Instant| Pending {
            verifier: String::new(),
            nonce: String::new(),
            return_to: "/".into(),
            started: at,
        };
        let now = Instant::now();
        let mut pending = HashMap::new();
        if let Some(expired) = now.checked_sub(LOGIN_TTL) {
            pending.insert("expired".to_string(), started(expired));
        }
        for (index, state) in ["a", "b", "c"].into_iter().enumerate() {
            let at = now + std::time::Duration::from_millis(index as u64);
            insert_pending(&mut pending, state.into(), started(at), 2);
        }
        let mut states = pending.keys().cloned().collect::<Vec<_>>();
        states.sort();
        assert_eq!(states, ["b", "c"]);
    }

    // r[verify control.oidc.return-to]
    #[test]
    fn signing_in_only_returns_to_the_control_plane() {
        assert_eq!(safe_return_to("/mappings?x=1"), "/mappings?x=1");
        for elsewhere in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "/\r\n/evil.example",
            "/ /evil.example",
            "/mappings\u{7f}",
            "/caf\u{e9}",
            "",
        ] {
            assert_eq!(safe_return_to(elsewhere), "/", "{elsewhere}");
        }
    }

    #[test]
    fn groups_are_lists_or_space_separated() {
        let claims = serde_json::json!({"a": ["x", "y"], "b": "x y", "c": 1});
        let claims = claims.as_object().unwrap();
        assert_eq!(groups(claims, "a"), Some(vec!["x".into(), "y".into()]));
        assert_eq!(groups(claims, "b"), Some(vec!["x".into(), "y".into()]));
        assert_eq!(groups(claims, "c"), None);
        assert_eq!(groups(claims, "d"), None);
    }
}
