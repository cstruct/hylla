//! An identity provider's keys, and its discovery document, read when first needed and kept,
//! for checking the tokens it signs.
//!
//! What the provider said is read again after an hour, or when a token names a key it
//! didn't have, as after the provider rotated its keys, but not more than once every 30
//! seconds, so tokens can't make hylla hammer the provider. While the provider can't be
//! reached, what it said before is used.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use jsonwebtoken::{
    Algorithm, DecodingKey, Header,
    jwk::{Jwk, JwkSet, PublicKeyUse},
};
use serde::Deserialize;
use tokio::sync::RwLock;

/// How long what the provider said is used before it's read again.
const TTL: Duration = Duration::from_secs(3600);
/// How long after reading the provider it isn't read again, even for unknown keys.
const REREAD_AFTER: Duration = Duration::from_secs(30);

/// Where the keys come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Through the discovery document of an issuer.
    Issuer(String),
    /// From a JSON Web Key Set.
    Jwks(String),
}

/// The endpoints a provider's discovery document names.
#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub jwks_uri: String,
    pub userinfo_endpoint: Option<String>,
    pub end_session_endpoint: Option<String>,
}

/// What the provider said.
#[derive(Debug)]
pub struct Said {
    /// Its discovery document, when the keys came through one.
    pub discovery: Option<Discovery>,
    pub keys: JwkSet,
    read: Instant,
}

pub struct Provider {
    source: Source,
    client: reqwest::Client,
    said: RwLock<Option<Arc<Said>>>,
    /// When the provider was last read, or tried.
    tried: Mutex<Option<Instant>>,
    /// Held while the provider is read, so concurrent requests read it once.
    reading: tokio::sync::Mutex<()>,
    pub(crate) ttl: Duration,
    pub(crate) reread_after: Duration,
}

impl Said {
    fn has(&self, kid: Option<&str>) -> bool {
        kid.is_none_or(|kid| self.keys.find(kid).is_some())
    }
}

impl Provider {
    pub fn new(source: Source) -> anyhow::Result<Self> {
        Ok(Self {
            source,
            // r[impl auth.provider-https]
            // Redirects could lead anywhere, such as to plain HTTP.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            said: RwLock::default(),
            tried: Mutex::default(),
            reading: tokio::sync::Mutex::default(),
            ttl: TTL,
            reread_after: REREAD_AFTER,
        })
    }

    /// The client to talk to the provider with.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    // r[impl auth.provider-keys]
    // r[impl auth.provider-down]
    // r[impl control.oidc.keys]
    /// What the provider said, read again when it's old, or when `kid` isn't among its keys.
    /// Fails only when the provider was never read.
    pub async fn said(&self, kid: Option<&str>) -> anyhow::Result<Arc<Said>> {
        let current = |said: &Option<Arc<Said>>| {
            said.as_ref()
                .filter(|said| said.read.elapsed() < self.ttl && said.has(kid))
                .cloned()
        };
        if let Some(said) = current(&*self.said.read().await) {
            return Ok(said);
        }
        let _reading = match self.reading.try_lock() {
            Ok(reading) => reading,
            Err(_) => {
                // While another request reads the provider, what it said before is good
                // enough when it has the key.
                if let Some(said) = self.said.read().await.as_ref().filter(|said| said.has(kid)) {
                    return Ok(said.clone());
                }
                self.reading.lock().await
            }
        };
        // Another request may have read it while this one waited.
        if let Some(said) = current(&*self.said.read().await) {
            return Ok(said);
        }
        let before = self.said.read().await.clone();
        let recently = {
            let mut tried = self.tried.lock().expect("not poisoned");
            let recently = tried.is_some_and(|tried| tried.elapsed() < self.reread_after);
            if !recently {
                *tried = Some(Instant::now());
            }
            recently
        };
        if recently {
            return before.context("the provider couldn't be read a moment ago");
        }
        match self.read().await {
            Ok(said) => {
                let said = Arc::new(said);
                *self.said.write().await = Some(said.clone());
                Ok(said)
            }
            Err(error) => match before {
                Some(before) => {
                    tracing::warn!(
                        "reading the provider again failed, using what it said before: {error:#}"
                    );
                    Ok(before)
                }
                None => {
                    tracing::warn!("reading the provider failed: {error:#}");
                    Err(error)
                }
            },
        }
    }

    /// What the provider says now.
    async fn read(&self) -> anyhow::Result<Said> {
        let (discovery, jwks_uri) = match &self.source {
            Source::Issuer(issuer) => {
                let issuer = issuer.trim_end_matches('/');
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
                let jwks_uri = discovery.jwks_uri.clone();
                (Some(discovery), jwks_uri)
            }
            Source::Jwks(uri) => (None, uri.clone()),
        };
        let keys = self
            .get_json(&jwks_uri)
            .await
            .context("reading the provider's keys")?;
        Ok(Said {
            discovery,
            keys,
            read: Instant::now(),
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        if !hylla_config::is_secure_url(url) {
            bail!("{url} isn't an HTTPS URL, or an HTTP one to this host");
        }
        let response = self.client.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} answered {status}");
        }
        let body = crate::limited::read(response, crate::limited::JSON).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

/// Whether `jwk` is for `algorithm`, when it says what it's for, and for signatures.
fn usable(jwk: &Jwk, algorithm: Algorithm) -> bool {
    let for_algorithm = jwk
        .common
        .key_algorithm
        .is_none_or(|key| format!("{key:?}") == format!("{algorithm:?}"));
    let for_signing = jwk
        .common
        .public_key_use
        .as_ref()
        .is_none_or(|key_use| *key_use == PublicKeyUse::Signature);
    for_algorithm && for_signing
}

// r[impl control.oidc.id-token-key]
// r[impl auth.provider-key-choice]
/// The key among `keys` to check a token with `header` with: the one it names, or without a
/// key id, the only one.
pub fn key(keys: &JwkSet, header: &Header) -> anyhow::Result<DecodingKey> {
    let jwk = match &header.kid {
        Some(kid) => keys.find(kid),
        None if keys.keys.len() == 1 => keys.keys.first(),
        None => bail!("the token names no key, and the provider has several"),
    }
    .context("the token's key isn't among the provider's keys")?;
    if !usable(jwk, header.alg) {
        bail!(
            "the token is signed with {:?}, but its key is for another algorithm or use",
            header.alg
        );
    }
    DecodingKey::from_jwk(jwk).context("reading the provider's key")
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

    use super::{Provider, Source};

    #[derive(Default)]
    struct Fake {
        reads: AtomicUsize,
        down: AtomicBool,
        /// Whether reads hang, as a provider that doesn't answer.
        slow: AtomicBool,
    }

    async fn jwks(State(fake): State<Arc<Fake>>) -> Result<Json<serde_json::Value>, StatusCode> {
        let reads = fake.reads.fetch_add(1, Ordering::SeqCst);
        if fake.slow.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        if fake.down.load(Ordering::SeqCst) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        // Slow, so concurrent requests overlap. Each read has a key of its own, as though the
        // provider rotated it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(Json(serde_json::json!({"keys": [
            {"kty": "oct", "kid": "a", "k": "c2VjcmV0"},
            {"kty": "oct", "kid": format!("rotated-{reads}"), "k": "c2VjcmV0"},
        ]})))
    }

    async fn fake() -> (Arc<Fake>, String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/jwks", listener.local_addr().unwrap());
        let fake = Arc::new(Fake::default());
        let router = Router::new()
            .route("/jwks", get(jwks))
            .with_state(fake.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (fake, url, server)
    }

    // r[verify auth.provider-keys]
    // r[verify auth.provider-down]
    #[tokio::test]
    async fn keys_are_read_again_when_old_or_unknown_but_not_too_often() {
        let (fake, url, server) = fake().await;
        let mut provider = Provider::new(Source::Jwks(url)).unwrap();
        provider.ttl = Duration::from_millis(400);
        provider.reread_after = Duration::from_millis(200);
        let provider = Arc::new(provider);
        let reads = || fake.reads.load(Ordering::SeqCst);

        let concurrent = (0..10).map(|_| {
            let provider = provider.clone();
            async move { provider.said(Some("a")).await.map(|_| ()) }
        });
        for said in futures::future::join_all(concurrent).await {
            said.unwrap();
        }
        assert_eq!(reads(), 1);

        // An unknown key right after a read isn't read again for.
        let said = provider.said(Some("rotated-1")).await.unwrap();
        assert!(said.keys.find("rotated-1").is_none());
        assert_eq!(reads(), 1);
        // A little later, it is.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let said = provider.said(Some("rotated-1")).await.unwrap();
        assert!(said.keys.find("rotated-1").is_some());
        assert_eq!(reads(), 2);

        // Past the TTL, with the provider down, what it said before is still used.
        fake.down.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(450)).await;
        provider.said(Some("a")).await.unwrap();
        assert_eq!(reads(), 3);
        provider.said(Some("a")).await.unwrap();
        assert_eq!(reads(), 3);

        // Requests with keys it had don't wait for a read that hangs.
        fake.down.store(false, Ordering::SeqCst);
        fake.slow.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(450)).await;
        let hanging = tokio::spawn({
            let provider = provider.clone();
            async move { provider.said(Some("a")).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let quick = tokio::time::timeout(Duration::from_millis(500), provider.said(Some("a")));
        quick.await.expect("doesn't wait").unwrap();
        hanging.abort();
        server.abort();
    }

    // r[verify auth.provider-down]
    #[tokio::test]
    async fn providers_never_read_fail_until_read() {
        let (fake, url, server) = fake().await;
        fake.down.store(true, Ordering::SeqCst);
        let mut provider = Provider::new(Source::Jwks(url)).unwrap();
        provider.reread_after = Duration::from_millis(100);
        assert!(provider.said(None).await.is_err());
        // Not read again right away.
        assert!(provider.said(None).await.is_err());
        assert_eq!(fake.reads.load(Ordering::SeqCst), 1);
        fake.down.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        provider.said(None).await.unwrap();
        server.abort();
    }
}
