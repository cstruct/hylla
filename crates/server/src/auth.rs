use std::{borrow::Cow, collections::HashMap, sync::Arc};

use anyhow::Context;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use hylla_config::{JwtAlgorithm, MappingAuth, PathTemplate, Verifier};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Map, Value};

use crate::provider::{self, Provider, Source};

/// The configured JWT verifiers, by name.
pub struct Verifiers(HashMap<String, Arc<TokenVerifier>>);

/// Checks tokens against a public key, or the keys of their provider.
pub struct TokenVerifier {
    keys: Keys,
    algorithm: Algorithm,
    validation: Validation,
}

enum Keys {
    /// The configured public key.
    Fixed(DecodingKey),
    /// The keys the provider publishes.
    Provider(Provider),
}

// r[impl auth.audience-warning]
// r[impl auth.issuer-warning]
/// What's worth warning about in the verifiers: those without an audience accept tokens
/// their issuer made for anything else, such as other services' or ID tokens, and those
/// reading a key set without an issuer accept tokens from anyone sharing it.
fn warnings(verifiers: &HashMap<String, Verifier>) -> Vec<String> {
    let mut verifiers = verifiers.iter().collect::<Vec<_>>();
    verifiers.sort_by_key(|(name, _)| *name);
    let mut warnings = Vec::new();
    for (name, verifier) in verifiers {
        if verifier.audience.is_none() {
            warnings.push(format!(
                "auth verifier `{name}` has no `audience`, so it accepts any token signed with \
                 its key, also ones made for other services"
            ));
        }
        if verifier.public_key.is_none() && verifier.issuer.is_none() {
            warnings.push(format!(
                "auth verifier `{name}` has no `issuer`, so it accepts tokens from every issuer \
                 signing with the keys at its `jwks_uri`"
            ));
        }
    }
    warnings
}

impl Verifiers {
    // r[impl auth.keys]
    /// Parses the public keys of the verifiers, failing on keys that don't parse. Keys from
    /// providers are read when first needed.
    pub fn new(verifiers: &HashMap<String, Verifier>) -> anyhow::Result<Self> {
        for warning in warnings(verifiers) {
            tracing::warn!("{warning}");
        }
        verifiers
            .iter()
            .map(|(name, verifier)| {
                let verifier = TokenVerifier::new(verifier)
                    .with_context(|| format!("auth verifier `{name}`"))?;
                Ok((name.clone(), Arc::new(verifier)))
            })
            .collect::<anyhow::Result<_>>()
            .map(Self)
    }

    /// Compiles the auth of a mapping.
    pub fn mapping(&self, auth: &MappingAuth) -> anyhow::Result<CompiledAuth> {
        let verifier = self
            .0
            .get(&auth.verifier)
            .with_context(|| format!("unknown auth verifier `{}`", auth.verifier))?
            .clone();
        let paths = auth
            .paths
            .iter()
            .map(|template| PathTemplate::parse(template).map_err(anyhow::Error::msg))
            .collect::<anyhow::Result<_>>()?;
        Ok(CompiledAuth {
            verifier,
            paths,
            private: auth.private,
        })
    }
}

impl TokenVerifier {
    fn new(verifier: &Verifier) -> anyhow::Result<Self> {
        let algorithm = match verifier.algorithm {
            JwtAlgorithm::Rs256 => Algorithm::RS256,
            JwtAlgorithm::Rs384 => Algorithm::RS384,
            JwtAlgorithm::Rs512 => Algorithm::RS512,
            JwtAlgorithm::Ps256 => Algorithm::PS256,
            JwtAlgorithm::Ps384 => Algorithm::PS384,
            JwtAlgorithm::Ps512 => Algorithm::PS512,
            JwtAlgorithm::Es256 => Algorithm::ES256,
            JwtAlgorithm::Es384 => Algorithm::ES384,
            JwtAlgorithm::EdDsa => Algorithm::EdDSA,
        };
        // r[impl auth.provider-keys]
        let keys = match (&verifier.public_key, &verifier.jwks_uri, &verifier.issuer) {
            (Some(pem), _, _) => {
                let pem = pem.as_bytes();
                let key = match verifier.algorithm {
                    JwtAlgorithm::Rs256
                    | JwtAlgorithm::Rs384
                    | JwtAlgorithm::Rs512
                    | JwtAlgorithm::Ps256
                    | JwtAlgorithm::Ps384
                    | JwtAlgorithm::Ps512 => DecodingKey::from_rsa_pem(pem),
                    JwtAlgorithm::Es256 | JwtAlgorithm::Es384 => DecodingKey::from_ec_pem(pem),
                    JwtAlgorithm::EdDsa => DecodingKey::from_ed_pem(pem),
                };
                Keys::Fixed(key.context("parsing the public key")?)
            }
            (None, Some(jwks_uri), _) => {
                Keys::Provider(Provider::new(Source::Jwks(jwks_uri.clone()))?)
            }
            (None, None, Some(issuer)) => {
                Keys::Provider(Provider::new(Source::Issuer(issuer.clone()))?)
            }
            (None, None, None) => {
                anyhow::bail!("there's no `public_key`, `jwks_uri` or `issuer` to read keys from")
            }
        };

        // r[impl auth.validate]
        // Only the configured algorithm is accepted, so tokens can't pick another one.
        let mut validation = Validation::new(algorithm);
        validation.leeway = verifier.leeway.duration().as_secs();
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // jsonwebtoken only checks `aud` and `iss` when tokens have them, so they're required
        // whenever they're configured.
        let mut required = vec!["exp"];
        match &verifier.audience {
            Some(audience) => {
                validation.set_audience(&[audience]);
                required.push("aud");
            }
            None => validation.validate_aud = false,
        }
        if let Some(issuer) = &verifier.issuer {
            validation.set_issuer(&[issuer]);
            required.push("iss");
        }
        validation.set_required_spec_claims(&required);
        Ok(Self {
            keys,
            algorithm,
            validation,
        })
    }

    /// The claims of `token`, if it is valid.
    async fn verify(&self, token: &str) -> Result<Map<String, Value>, Refusal> {
        let invalid = |error: String| Refusal::InvalidToken(error);
        let key = match &self.keys {
            Keys::Fixed(key) => Cow::Borrowed(key),
            Keys::Provider(provider) => {
                let header = jsonwebtoken::decode_header(token)
                    .map_err(|error| invalid(error.to_string()))?;
                // Tokens that can't be valid don't get to make hylla read the provider.
                if header.alg != self.algorithm {
                    return Err(invalid(format!(
                        "the token is signed with {:?}",
                        header.alg
                    )));
                }
                // r[impl auth.provider-down]
                let said = provider
                    .said(header.kid.as_deref())
                    .await
                    .map_err(|error| Refusal::KeysUnavailable(format!("{error:#}")))?;
                Cow::Owned(
                    provider::key(&said.keys, &header)
                        .map_err(|error| invalid(format!("{error:#}")))?,
                )
            }
        };
        jsonwebtoken::decode::<Map<String, Value>>(token, &key, &self.validation)
            .map(|data| data.claims)
            .map_err(|error| invalid(error.to_string()))
    }
}

/// The auth of a mapping, ready to check requests.
pub struct CompiledAuth {
    verifier: Arc<TokenVerifier>,
    paths: Vec<PathTemplate>,
    pub private: bool,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No Bearer token.
    MissingToken,
    /// A token that isn't valid.
    InvalidToken(String),
    /// A valid token that doesn't allow the path.
    Forbidden,
    /// The verifier's provider has never been read, so there are no keys to check tokens
    /// with.
    KeysUnavailable(String),
    /// A path with `.` or `..` segments.
    DotSegments,
    /// No Konklav rule of the mapping matches the path.
    NoRule,
    /// Konklav asks the caller to authenticate again, at most `max_age` seconds before.
    StepUp { max_age: Option<i64> },
    /// Konklav couldn't be asked, or didn't decide in time.
    DecisionUnavailable(String),
}

impl CompiledAuth {
    // r[impl auth.bearer]
    // r[impl auth.paths]
    /// Checks a request for `backend_path`, the path requested from the backend.
    pub async fn check(&self, headers: &HeaderMap, backend_path: &str) -> Result<(), Refusal> {
        // r[impl auth.dot-segments]
        if backend_path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(Refusal::DotSegments);
        }
        let token = bearer_token(headers).ok_or(Refusal::MissingToken)?;
        let claims = self.verifier.verify(token).await?;
        if self.paths.is_empty() {
            return Ok(());
        }
        // r[impl auth.list-claims]
        let claim = |name: &str| {
            let text = |value: &Value| match value {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            };
            match claims.get(name) {
                Some(Value::Array(items)) => items.iter().filter_map(text).collect(),
                Some(value) => text(value).into_iter().collect(),
                None => Vec::new(),
            }
        };
        let allowed = self
            .paths
            .iter()
            .any(|template| template.allows(backend_path, claim));
        if allowed {
            Ok(())
        } else {
            Err(Refusal::Forbidden)
        }
    }
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

impl Refusal {
    /// A short name for the reason, for metrics.
    pub fn reason(&self) -> &'static str {
        match self {
            Refusal::MissingToken => "missing_token",
            Refusal::InvalidToken(_) => "invalid_token",
            Refusal::Forbidden => "forbidden",
            Refusal::KeysUnavailable(_) => "keys_unavailable",
            Refusal::DotSegments => "dot_segments",
            Refusal::NoRule => "no_rule",
            Refusal::StepUp { .. } => "step_up",
            Refusal::DecisionUnavailable(_) => "decision_unavailable",
        }
    }
}

// r[impl auth.responses]
// r[impl konklav.responses]
/// The status and headers to refuse a request with.
pub fn refusal_response(refusal: &Refusal) -> (StatusCode, HeaderMap) {
    let mut headers = HeaderMap::new();
    let (status, challenge) = match refusal {
        Refusal::MissingToken => (StatusCode::UNAUTHORIZED, Some(Cow::Borrowed("Bearer"))),
        Refusal::InvalidToken(_) => (
            StatusCode::UNAUTHORIZED,
            Some(Cow::Borrowed(r#"Bearer error="invalid_token""#)),
        ),
        Refusal::Forbidden | Refusal::NoRule => (
            StatusCode::FORBIDDEN,
            Some(Cow::Borrowed(r#"Bearer error="insufficient_scope""#)),
        ),
        // As RFC 9470 asks for it.
        Refusal::StepUp { max_age } => {
            let challenge = r#"Bearer error="insufficient_user_authentication""#;
            (
                StatusCode::UNAUTHORIZED,
                Some(match max_age {
                    Some(max_age) => Cow::Owned(format!("{challenge}, max_age={max_age}")),
                    None => Cow::Borrowed(challenge),
                }),
            )
        }
        Refusal::DotSegments => (StatusCode::BAD_REQUEST, None),
        Refusal::KeysUnavailable(_) | Refusal::DecisionUnavailable(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, None)
        }
    };
    if let Some(challenge) = challenge.and_then(|challenge| HeaderValue::from_str(&challenge).ok())
    {
        headers.insert(header::WWW_AUTHENTICATE, challenge);
    }
    (status, headers)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hylla_config::{Interval, JwtAlgorithm, Verifier};

    // r[verify auth.audience-warning]
    #[test]
    fn verifiers_without_an_audience_are_warned_about() {
        let verifier = |audience: Option<&str>| Verifier {
            algorithm: JwtAlgorithm::Es256,
            public_key: Some(String::new()),
            jwks_uri: None,
            issuer: None,
            audience: audience.map(Into::into),
            leeway: Interval::from_secs(30),
        };
        let verifiers = HashMap::from([
            ("open".to_string(), verifier(None)),
            ("scoped".to_string(), verifier(Some("hylla"))),
        ]);
        let warnings = super::warnings(&verifiers);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`open` has no `audience`"));
    }

    // r[verify auth.issuer-warning]
    #[test]
    fn key_sets_without_an_issuer_are_warned_about() {
        let verifier = |issuer: Option<&str>| Verifier {
            algorithm: JwtAlgorithm::Es256,
            public_key: None,
            jwks_uri: Some("https://id.example.com/jwks".into()),
            issuer: issuer.map(Into::into),
            audience: Some("hylla".into()),
            leeway: Interval::from_secs(30),
        };
        let verifiers = HashMap::from([
            ("shared".to_string(), verifier(None)),
            (
                "scoped".to_string(),
                verifier(Some("https://id.example.com")),
            ),
        ]);
        let warnings = super::warnings(&verifiers);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`shared` has no `issuer`"));
    }
}
