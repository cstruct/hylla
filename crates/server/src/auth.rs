use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use hylla_config::{JwtAlgorithm, MappingAuth, PathTemplate, Verifier, under_prefix};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Map, Value};

/// The configured JWT verifiers, by name.
pub struct Verifiers(HashMap<String, Arc<TokenVerifier>>);

/// Checks tokens against one public key.
pub struct TokenVerifier {
    key: DecodingKey,
    validation: Validation,
}

impl Verifiers {
    // r[impl auth.keys]
    /// Parses the public keys of the verifiers, failing on keys that don't parse.
    pub fn new(verifiers: &HashMap<String, Verifier>) -> anyhow::Result<Self> {
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
        let pem = verifier.public_key.as_bytes();
        let (algorithm, key) = match verifier.algorithm {
            JwtAlgorithm::Rs256 => (Algorithm::RS256, DecodingKey::from_rsa_pem(pem)),
            JwtAlgorithm::Rs384 => (Algorithm::RS384, DecodingKey::from_rsa_pem(pem)),
            JwtAlgorithm::Rs512 => (Algorithm::RS512, DecodingKey::from_rsa_pem(pem)),
            JwtAlgorithm::Ps256 => (Algorithm::PS256, DecodingKey::from_rsa_pem(pem)),
            JwtAlgorithm::Ps384 => (Algorithm::PS384, DecodingKey::from_rsa_pem(pem)),
            JwtAlgorithm::Ps512 => (Algorithm::PS512, DecodingKey::from_rsa_pem(pem)),
            JwtAlgorithm::Es256 => (Algorithm::ES256, DecodingKey::from_ec_pem(pem)),
            JwtAlgorithm::Es384 => (Algorithm::ES384, DecodingKey::from_ec_pem(pem)),
            JwtAlgorithm::EdDsa => (Algorithm::EdDSA, DecodingKey::from_ed_pem(pem)),
        };
        let key = key.context("parsing the public key")?;

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
        Ok(Self { key, validation })
    }

    /// The claims of `token`, if it is valid.
    fn verify(&self, token: &str) -> Result<Map<String, Value>, jsonwebtoken::errors::Error> {
        jsonwebtoken::decode::<Map<String, Value>>(token, &self.key, &self.validation)
            .map(|data| data.claims)
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
    /// A path with `.` or `..` segments.
    DotSegments,
}

impl CompiledAuth {
    // r[impl auth.bearer]
    // r[impl auth.paths]
    /// Checks a request for `backend_path`, the path requested from the backend.
    pub fn check(&self, headers: &HeaderMap, backend_path: &str) -> Result<(), Refusal> {
        // r[impl auth.dot-segments]
        if backend_path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(Refusal::DotSegments);
        }
        let token = bearer_token(headers).ok_or(Refusal::MissingToken)?;
        let claims = self
            .verifier
            .verify(token)
            .map_err(|error| Refusal::InvalidToken(error.to_string()))?;
        if self.paths.is_empty() {
            return Ok(());
        }
        let claim = |name: &str| match claims.get(name)? {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        };
        let allowed = self.paths.iter().any(|template| {
            template
                .render(claim)
                .is_ok_and(|prefix| under_prefix(backend_path, &prefix))
        });
        if allowed {
            Ok(())
        } else {
            Err(Refusal::Forbidden)
        }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

// r[impl auth.responses]
/// The status and headers to refuse a request with.
pub fn refusal_response(refusal: &Refusal) -> (StatusCode, HeaderMap) {
    let mut headers = HeaderMap::new();
    let (status, challenge) = match refusal {
        Refusal::MissingToken => (StatusCode::UNAUTHORIZED, Some("Bearer")),
        Refusal::InvalidToken(_) => (
            StatusCode::UNAUTHORIZED,
            Some(r#"Bearer error="invalid_token""#),
        ),
        Refusal::Forbidden => (
            StatusCode::FORBIDDEN,
            Some(r#"Bearer error="insufficient_scope""#),
        ),
        Refusal::DotSegments => (StatusCode::BAD_REQUEST, None),
    };
    if let Some(challenge) = challenge {
        headers.insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(challenge),
        );
    }
    (status, headers)
}
