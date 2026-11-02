//! Leaving authorization to Konklav, through its relying SDK.
//!
//! Mappings' rules turn a request path into requirements, which Konklav's relying client
//! decides for the request's access token: locally, from entitlement sets it caches and
//! keeps fresh from Konklav's change feed, or through Konklav's check API while those can't
//! be trusted.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::Context;
use http::HeaderMap;
use hylla_config::{Konklav as KonklavConfig, MappingKonklav, PathPattern};
use konklav_authz::{Decision, DenyReason, Requirements, ScopeRequirement};
use konklav_relying_client::{ClientConfig, RelyingClient};

use crate::auth::{Refusal, bearer_token};

/// The relying client, shared by every mapping with `konklav`.
pub struct Konklav {
    client: RelyingClient,
    timeout: Duration,
    /// The change feed and background refreshes, stopped when the app is dropped.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Konklav {
    // r[impl konklav.client]
    /// Starts the relying client, which logs in and follows the change feed in the
    /// background, so startup doesn't wait for Konklav.
    pub fn start(config: &KonklavConfig) -> Arc<Self> {
        let client = RelyingClient::new(ClientConfig {
            relying_url: config.relying_url.trim_end_matches('/').to_string(),
            subject_url: config.subject_url.trim_end_matches('/').to_string(),
            key_id: config.key_id.clone(),
            secret: config.secret.clone(),
            core: konklav_relying::Config {
                issuer: config.issuer.clone(),
                audience: config.audience.clone(),
                ..Default::default()
            },
        });
        let tasks = client.start();
        Arc::new(Self {
            client,
            timeout: config.timeout.duration(),
            tasks,
        })
    }

    /// Compiles the `konklav` of a mapping.
    pub fn mapping(self: &Arc<Self>, konklav: &MappingKonklav) -> anyhow::Result<CompiledKonklav> {
        let rules = konklav
            .rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let pattern = PathPattern::parse(&rule.path)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("konklav rule {index}"))?;
                Ok(Rule {
                    pattern,
                    actions: rule.actions.iter().cloned().collect(),
                    scope: rule
                        .scope
                        .as_ref()
                        .map(|scope| (scope.kind.clone(), scope.id.clone())),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(CompiledKonklav {
            konklav: self.clone(),
            rules,
            private: konklav.private,
        })
    }
}

impl Drop for Konklav {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The `konklav` of a mapping, ready to decide requests.
pub struct CompiledKonklav {
    konklav: Arc<Konklav>,
    rules: Vec<Rule>,
    pub private: bool,
}

struct Rule {
    pattern: PathPattern,
    actions: BTreeSet<String>,
    /// The scope's kind, and the name of the capture that is its id.
    scope: Option<(String, String)>,
}

impl CompiledKonklav {
    // r[impl konklav.mappings]
    /// Decides a request for `path`, the decoded request path.
    pub async fn check(&self, headers: &HeaderMap, path: &str) -> Result<(), Refusal> {
        // r[impl konklav.dot-segments]
        if path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(Refusal::DotSegments);
        }
        // r[impl konklav.bearer]
        let token = bearer_token(headers).ok_or(Refusal::MissingToken)?;
        let requirements = self.requirements(path).ok_or(Refusal::NoRule)?;
        // r[impl konklav.decide]
        // r[impl konklav.fail-closed]
        // r[impl konklav.cache]
        // Every request is decided by the relying client, which caches what it decides
        // from; hylla caches no decisions of its own.
        let decision = tokio::time::timeout(
            self.konklav.timeout,
            self.konklav.client.decide(token, &requirements),
        )
        .await
        .map_err(|_| Refusal::DecisionUnavailable("Konklav didn't decide in time".into()))?
        .map_err(|error| Refusal::DecisionUnavailable(error.to_string()))?;
        allowed(decision)
    }

    // r[impl konklav.first-match]
    // r[impl konklav.requirements]
    // r[impl konklav.tenancy]
    /// The requirements of the first rule matching `path`, if any does. Scopes don't name
    /// their tenant, so Konklav decides who owns them.
    fn requirements(&self, path: &str) -> Option<Requirements> {
        self.rules.iter().find_map(|rule| {
            let captures = rule.pattern.matches(path)?;
            let mut requirements = Requirements::default();
            match &rule.scope {
                Some((kind, capture)) => {
                    let (_, id) = captures.iter().find(|(name, _)| name == capture)?;
                    requirements.scopes.push(ScopeRequirement {
                        tenant: None,
                        kind: kind.clone(),
                        id: (*id).to_string(),
                        actions: rule.actions.clone(),
                    });
                }
                None => requirements.tenant = rule.actions.clone(),
            }
            Some(requirements)
        })
    }
}

/// Whether Konklav's decision lets the request be served.
fn allowed(decision: Decision) -> Result<(), Refusal> {
    match decision {
        // r[impl konklav.obligations]
        // Shares may come with attributes, such as a date range, that what's served would
        // have to be narrowed to, which hylla can't do.
        Decision::Allow { obligations } => {
            if obligations
                .iter()
                .all(|obligation| obligation.attributes.is_empty())
            {
                Ok(())
            } else {
                Err(Refusal::Forbidden)
            }
        }
        Decision::Deny {
            reason: DenyReason::InvalidToken,
        } => Err(Refusal::InvalidToken("Konklav denied the token".into())),
        Decision::Deny { .. } => Err(Refusal::Forbidden),
        Decision::StepUp { requirement } => Err(Refusal::StepUp {
            max_age: requirement.max_age_seconds,
        }),
    }
}

#[cfg(test)]
mod tests {
    use konklav_authz::{Decision, DenyReason, Obligation, StepUp};
    use serde_json::json;

    use super::{Refusal, allowed};

    fn obligation(attributes: serde_json::Value) -> Obligation {
        serde_json::from_value(json!({
            "share": "0193a2f0-0000-7000-8000-00000000000a",
            "tenant": "0193a2f0-0000-7000-8000-000000000001",
            "kind": "object",
            "id": "o-1",
            "attributes": attributes,
        }))
        .unwrap()
    }

    // r[verify konklav.obligations]
    #[test]
    fn allows_with_attributes_are_refused() {
        assert_eq!(allowed(Decision::allow()), Ok(()));
        assert_eq!(
            allowed(Decision::Allow {
                obligations: vec![obligation(json!({}))],
            }),
            Ok(())
        );
        assert_eq!(
            allowed(Decision::Allow {
                obligations: vec![obligation(json!({
                    "data_range": {"from": "2026-01-01", "until": "2026-06-30"},
                }))],
            }),
            Err(Refusal::Forbidden)
        );
    }

    #[test]
    fn denials_and_step_ups_are_refused() {
        assert!(matches!(
            allowed(Decision::Deny {
                reason: DenyReason::InvalidToken,
            }),
            Err(Refusal::InvalidToken(_))
        ));
        assert_eq!(
            allowed(Decision::Deny {
                reason: DenyReason::TokenKind,
            }),
            Err(Refusal::Forbidden)
        );
        assert_eq!(
            allowed(Decision::StepUp {
                requirement: StepUp {
                    max_age_seconds: Some(300),
                    ..Default::default()
                },
            }),
            Err(Refusal::StepUp { max_age: Some(300) })
        );
    }
}
