use std::{collections::BTreeMap, fmt, net::SocketAddr};

use crate::{Config, router::unknown_group_refs};

/// A problem with a loaded config that its structure doesn't rule out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// A mapping refers to a backend that isn't configured.
    UnknownBackend {
        server: String,
        mapping: usize,
        backend: String,
    },
    /// A server has no server names.
    NoServerNames { server: String },
    /// A mapping's path prefix doesn't start with `/`.
    RelativePath {
        server: String,
        mapping: usize,
        path: String,
    },
    /// A rewrite's pattern isn't a valid regex.
    InvalidRewrite {
        server: String,
        mapping: usize,
        rewrite: usize,
        error: String,
    },
    /// A rewrite's replacement refers to a capture group its pattern doesn't have.
    UnknownGroup {
        server: String,
        mapping: usize,
        rewrite: usize,
        reference: String,
    },
    /// A mapping's auth refers to a verifier that isn't configured, or has an invalid path
    /// template.
    InvalidAuth {
        server: String,
        mapping: usize,
        reason: String,
    },
    /// A backend's refresh settings are invalid.
    InvalidRefresh {
        backend: String,
        rule: Option<usize>,
        reason: String,
    },
    /// The cluster settings are invalid.
    InvalidCluster { reason: String },
    /// A cache size setting is out of range.
    InvalidCache { reason: String },
    /// Several servers answer to the same host name on the same address.
    DuplicateHost {
        listen: SocketAddr,
        server_name: String,
        servers: Vec<String>,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBackend {
                server,
                mapping,
                backend,
            } => write!(
                f,
                "mapping {mapping} of server `{server}` uses unknown backend `{backend}`"
            ),
            Self::NoServerNames { server } => write!(f, "server `{server}` has no server names"),
            Self::RelativePath {
                server,
                mapping,
                path,
            } => write!(
                f,
                "mapping {mapping} of server `{server}` has path `{path}`, which doesn't start with `/`"
            ),
            Self::InvalidRewrite {
                server,
                mapping,
                rewrite,
                error,
            } => write!(
                f,
                "rewrite {rewrite} of mapping {mapping} of server `{server}` is invalid: {error}"
            ),
            Self::UnknownGroup {
                server,
                mapping,
                rewrite,
                reference,
            } => {
                write!(
                    f,
                    "rewrite {rewrite} of mapping {mapping} of server `{server}` refers to \
                     `{reference}`, which its pattern has no capture group for"
                )?;
                // `$1a` is a reference to a group named `1a`; `${1}a` was likely meant.
                let digits = reference[1..]
                    .find(|ch: char| !ch.is_ascii_digit())
                    .unwrap_or(reference.len() - 1);
                if !reference.starts_with("${") && digits > 0 && digits < reference.len() - 1 {
                    let (number, rest) = reference[1..].split_at(digits);
                    write!(f, " (did you mean `${{{number}}}{rest}`?)")?;
                }
                Ok(())
            }
            Self::InvalidAuth {
                server,
                mapping,
                reason,
            } => write!(
                f,
                "auth of mapping {mapping} of server `{server}` is invalid: {reason}"
            ),
            Self::InvalidRefresh {
                backend,
                rule: Some(rule),
                reason,
            } => write!(
                f,
                "refresh rule {rule} of backend `{backend}` is invalid: {reason}"
            ),
            Self::InvalidRefresh {
                backend,
                rule: None,
                reason,
            } => write!(f, "refresh of backend `{backend}` is invalid: {reason}"),
            Self::InvalidCluster { reason } => write!(f, "invalid cluster config: {reason}"),
            Self::InvalidCache { reason } => write!(f, "invalid cache config: {reason}"),
            Self::DuplicateHost {
                listen,
                server_name,
                servers,
            } => write!(
                f,
                "servers {} all answer to `{server_name}` on {listen}",
                servers
                    .iter()
                    .map(|server| format!("`{server}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

impl Config {
    /// Checks what the config's structure doesn't rule out, returning every problem found.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        let mut hosts = BTreeMap::<(SocketAddr, String), Vec<String>>::new();

        let mut servers = self.servers.iter().collect::<Vec<_>>();
        servers.sort_by_key(|(name, _)| *name);
        for (name, server) in servers {
            // r[impl config.validate.server-names]
            if server.server_names.is_empty() {
                errors.push(ValidationError::NoServerNames {
                    server: name.clone(),
                });
            }
            // r[impl config.validate.unique-hosts]
            for server_name in &server.server_names {
                hosts
                    .entry((server.listen, server_name.to_lowercase()))
                    .or_default()
                    .push(name.clone());
            }
            for (index, mapping) in server.mappings.iter().enumerate() {
                // r[impl config.validate.backend-refs]
                if !self.backends.contains_key(&mapping.backend) {
                    errors.push(ValidationError::UnknownBackend {
                        server: name.clone(),
                        mapping: index,
                        backend: mapping.backend.clone(),
                    });
                }
                // r[impl config.validate.auth]
                if let Some(auth) = &mapping.auth {
                    let mut error = |reason: String| {
                        errors.push(ValidationError::InvalidAuth {
                            server: name.clone(),
                            mapping: index,
                            reason,
                        })
                    };
                    if !self.auth.contains_key(&auth.verifier) {
                        error(format!("unknown verifier `{}`", auth.verifier));
                    }
                    for template in &auth.paths {
                        if let Err(invalid) = crate::PathTemplate::parse(template) {
                            error(invalid.to_string());
                        }
                    }
                }
                // r[impl config.validate.absolute-paths]
                if !mapping.path_prefix.starts_with('/') {
                    errors.push(ValidationError::RelativePath {
                        server: name.clone(),
                        mapping: index,
                        path: mapping.path_prefix.clone(),
                    });
                }
                // r[impl config.validate.rewrites]
                for (rewrite_index, rewrite) in mapping.rewrites.iter().enumerate() {
                    match regex::Regex::new(&rewrite.pattern) {
                        Ok(regex) => {
                            for reference in unknown_group_refs(&regex, &rewrite.replace) {
                                errors.push(ValidationError::UnknownGroup {
                                    server: name.clone(),
                                    mapping: index,
                                    rewrite: rewrite_index,
                                    reference,
                                });
                            }
                        }
                        Err(error) => errors.push(ValidationError::InvalidRewrite {
                            server: name.clone(),
                            mapping: index,
                            rewrite: rewrite_index,
                            error: error.to_string(),
                        }),
                    }
                }
            }
        }
        // r[impl config.validate.cache]
        errors.extend(self.validate_cache());
        // r[impl config.validate.refresh]
        errors.extend(self.validate_refresh());
        // r[impl config.validate.cluster]
        errors.extend(self.validate_cluster());

        for ((listen, server_name), servers) in hosts {
            if servers.len() > 1 {
                errors.push(ValidationError::DuplicateHost {
                    listen,
                    server_name,
                    servers,
                });
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Config {
    fn validate_cache(&self) -> Vec<ValidationError> {
        let cache = &self.cache;
        let mut reasons = Vec::new();
        if cache.chunk_size_kib == 0 {
            reasons.push("`chunk_size_kib` must be positive".to_string());
        }
        if cache.memory.capacity_mib == 0 {
            reasons.push("`memory.capacity_mib` must be positive".to_string());
        }
        if let Some(disk) = &cache.disk {
            let block_kib = u64::from(disk.block_size_mib) * 1024;
            // A block also holds an entry's key and header, so the chunk must be smaller.
            if u64::from(cache.chunk_size_kib) >= block_kib {
                reasons.push(format!(
                    "`chunk_size_kib` ({}) must be smaller than `disk.block_size_mib` ({} MiB), \
                     since a chunk is stored in a single block",
                    cache.chunk_size_kib, disk.block_size_mib
                ));
            }
            if disk.capacity_mib < u64::from(disk.block_size_mib) * 4 {
                reasons.push(format!(
                    "`disk.capacity_mib` ({}) must hold at least 4 blocks of {} MiB",
                    disk.capacity_mib, disk.block_size_mib
                ));
            }
        }
        reasons
            .into_iter()
            .map(|reason| ValidationError::InvalidCache { reason })
            .collect()
    }
}

impl Config {
    fn validate_refresh(&self) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        let mut backends = self.backends.iter().collect::<Vec<_>>();
        backends.sort_by_key(|(name, _)| *name);
        for (name, backend) in backends {
            let Some(refresh) = backend.refresh() else {
                continue;
            };
            let mut error = |rule: Option<usize>, reason: &str| {
                errors.push(ValidationError::InvalidRefresh {
                    backend: name.clone(),
                    rule,
                    reason: reason.to_string(),
                });
            };
            if refresh.concurrency == 0 {
                error(None, "`concurrency` must be positive");
            }
            for (index, rule) in refresh.rules.iter().enumerate() {
                let rule_error = |reason: &str| (Some(index), reason.to_string());
                let mut reasons = Vec::new();
                match (&rule.prefix, &rule.key) {
                    (Some(_), Some(_)) | (None, None) => {
                        reasons.push(rule_error("set exactly one of `prefix` and `key`"))
                    }
                    (Some(path), None) | (None, Some(path)) if path.starts_with('/') => reasons
                        .push(rule_error(
                            "keys and prefixes are backend keys, which don't start with `/`",
                        )),
                    (None, Some(key)) if key.is_empty() => {
                        reasons.push(rule_error("`key` must not be empty"))
                    }
                    _ => {}
                }
                if rule.every.duration().is_zero() {
                    reasons.push(rule_error("`every` must be positive"));
                }
                if rule.mode.preloads() && rule.max_objects.is_none() && rule.max_mib.is_none() {
                    reasons.push(rule_error(
                        "preloading needs `max_objects` or `max_mib`, so it can't fill the cache \
                         with objects nobody reads",
                    ));
                }
                for (rule, reason) in reasons {
                    error(rule, &reason);
                }
            }
        }
        errors
    }
}

impl Config {
    fn validate_cluster(&self) -> Vec<ValidationError> {
        let Some(cluster) = &self.cluster else {
            return Vec::new();
        };
        let mut reasons = Vec::new();
        if cluster.advertise.is_none() && cluster.listen.ip().is_unspecified() {
            reasons.push(format!(
                "`listen` is {}, so set `advertise` to the address other nodes reach this \
                 one at",
                cluster.listen
            ));
        }
        if cluster
            .advertise
            .is_some_and(|advertise| advertise.ip().is_unspecified())
        {
            reasons.push("`advertise` must be a specific address".to_string());
        }
        if cluster.key.len() < 32 {
            reasons.push("`key` must be at least 32 characters".to_string());
        }
        for seed in &cluster.seeds {
            let valid = seed
                .rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
            if !valid {
                reasons.push(format!("seed `{seed}` must be `host:port`"));
            }
        }
        if cluster.gossip_interval.duration().is_zero() {
            reasons.push("`gossip_interval` must be positive".to_string());
        }
        if cluster.failure_timeout.duration() <= cluster.gossip_interval.duration() {
            reasons.push("`failure_timeout` must be longer than `gossip_interval`".to_string());
        }
        reasons
            .into_iter()
            .map(|reason| ValidationError::InvalidCluster { reason })
            .collect()
    }
}
