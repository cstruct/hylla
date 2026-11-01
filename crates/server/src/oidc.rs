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
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use base64::Engine;
use hylla_config::ControlOidc;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

/// The cookie binding a sign-in to the browser that started it.
pub const LOGIN_COOKIE: &str = "hylla_login";
/// The cookie naming a session.
pub const SESSION_COOKIE: &str = "hylla_session";
/// How long a sign-in may take at the provider.
const LOGIN_TTL: Duration = Duration::from_secs(600);
/// How long the provider's discovery document and keys are used before they're read again.
const PROVIDER_TTL: Duration = Duration::from_secs(3600);

/// The endpoints the provider's discovery document names.
#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    userinfo_endpoint: Option<String>,
    end_session_endpoint: Option<String>,
}

struct Provider {
    discovery: Discovery,
    keys: JwkSet,
    read: Instant,
}

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
    client: reqwest::Client,
    provider: RwLock<Option<Provider>>,
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
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            provider: RwLock::default(),
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

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let response = self.client.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} answered {status}");
        }
        Ok(serde_json::from_slice(&response.bytes().await?)?)
    }

    /// The provider's endpoints and keys, read again when they're old, or when `kid` isn't
    /// among the keys, as after the provider rotated them.
    async fn discovery_and_keys(&self, kid: Option<&str>) -> anyhow::Result<(Discovery, JwkSet)> {
        {
            let provider = self.provider.read().await;
            if let Some(provider) = provider.as_ref()
                && provider.read.elapsed() < PROVIDER_TTL
                && kid.is_none_or(|kid| provider.keys.find(kid).is_some())
            {
                return Ok((provider.discovery.clone(), provider.keys.clone()));
            }
        }
        let issuer = self.config.issuer.trim_end_matches('/');
        let discovery: Discovery = self
            .get_json(&format!("{issuer}/.well-known/openid-configuration"))
            .await
            .context("reading the provider's discovery document")?;
        // r[impl control.oidc.issuer]
        if discovery.issuer.trim_end_matches('/') != issuer {
            bail!(
                "the provider says its issuer is `{}`, not `{issuer}`",
                discovery.issuer
            );
        }
        let keys: JwkSet = self
            .get_json(&discovery.jwks_uri)
            .await
            .context("reading the provider's keys")?;
        *self.provider.write().await = Some(Provider {
            discovery: discovery.clone(),
            keys: keys.clone(),
            read: Instant::now(),
        });
        Ok((discovery, keys))
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
        let url = with_query(
            &discovery.authorization_endpoint,
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
        let mut pending = self.pending.lock().expect("not poisoned");
        pending.retain(|_, pending| pending.started.elapsed() < LOGIN_TTL);
        pending.insert(
            state.clone(),
            Pending {
                verifier,
                nonce,
                return_to: safe_return_to(return_to),
                started: Instant::now(),
            },
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
        let mut request = self.client.post(&discovery.token_endpoint);
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
            serde_json::from_slice(&response.bytes().await?).context("reading the tokens")?;
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
        let (discovery, keys) = self.discovery_and_keys(header.kid.as_deref()).await?;
        let jwk = match &header.kid {
            Some(kid) => keys.find(kid),
            None => keys.keys.first(),
        }
        .context("the ID token's key isn't among the provider's keys")?;
        let key = DecodingKey::from_jwk(jwk).context("reading the provider's key")?;
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
            .client
            .get(endpoint)
            .bearer_auth(access_token)
            .send()
            .await
            .context("reading the user info")?;
        if !response.status().is_success() {
            bail!("the user info endpoint answered {}", response.status());
        }
        Ok(Some(
            serde_json::from_slice(&response.bytes().await?).context("reading the user info")?,
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

/// `return_to` if it's a path of the control plane, so signing in can't redirect elsewhere,
/// or `/`.
fn safe_return_to(return_to: &str) -> String {
    if return_to.starts_with('/') && !return_to.starts_with("//") && !return_to.contains('\\') {
        return_to.to_string()
    } else {
        "/".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{groups, safe_return_to};

    #[test]
    fn signing_in_only_returns_to_the_control_plane() {
        assert_eq!(safe_return_to("/mappings?x=1"), "/mappings?x=1");
        for elsewhere in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
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
