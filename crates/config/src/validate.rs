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
    /// No server maps any requests to a backend, so hylla would serve nothing.
    NoMappings,
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
    /// A mapping's `konklav` has no provider to decide it, or invalid rules.
    InvalidMappingKonklav {
        server: String,
        mapping: usize,
        reason: String,
    },
    /// The Konklav provider's settings are invalid.
    InvalidKonklav { reason: String },
    /// A backend's refresh settings are invalid.
    InvalidRefresh {
        backend: String,
        rule: Option<usize>,
        reason: String,
    },
    /// The cluster settings are invalid.
    InvalidCluster { reason: String },
    /// A backend's settings are invalid.
    InvalidBackend { backend: String, reason: String },
    /// The settings of serving backends to other nodes are invalid.
    InvalidServeBackends { reason: String },
    /// A mapping can't be writable.
    InvalidWritable {
        server: String,
        mapping: usize,
        reason: String,
    },
    /// A JWT verifier has no keys, or two sources of them, or an invalid URL to read them from.
    InvalidVerifier { verifier: String, reason: String },
    /// The control plane's settings are invalid.
    InvalidControl { reason: String },
    /// A cache size setting is out of range.
    InvalidCache { reason: String },
    /// The listeners' limits are invalid.
    InvalidHttp { reason: String },
    /// The health checks' settings are invalid.
    InvalidHealth { reason: String },
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
            Self::NoMappings => write!(
                f,
                "no server maps requests to a backend: configure a server with at least one mapping"
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
            Self::InvalidMappingKonklav {
                server,
                mapping,
                reason,
            } => write!(
                f,
                "konklav of mapping {mapping} of server `{server}` is invalid: {reason}"
            ),
            Self::InvalidKonklav { reason } => write!(f, "invalid konklav config: {reason}"),
            Self::InvalidVerifier { verifier, reason } => {
                write!(f, "auth verifier `{verifier}` is invalid: {reason}")
            }
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
            Self::InvalidBackend { backend, reason } => {
                write!(f, "backend `{backend}` is invalid: {reason}")
            }
            Self::InvalidServeBackends { reason } => {
                write!(f, "invalid serve_backends config: {reason}")
            }
            Self::InvalidWritable {
                server,
                mapping,
                reason,
            } => write!(
                f,
                "mapping {mapping} of server `{server}` can't be writable: {reason}"
            ),
            Self::InvalidControl { reason } => write!(f, "invalid control config: {reason}"),
            Self::InvalidCache { reason } => write!(f, "invalid cache config: {reason}"),
            Self::InvalidHttp { reason } => write!(f, "invalid http config: {reason}"),
            Self::InvalidHealth { reason } => write!(f, "invalid health config: {reason}"),
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

        // r[impl config.validate.mappings]
        if self
            .servers
            .values()
            .all(|server| server.mappings.is_empty())
        {
            errors.push(ValidationError::NoMappings);
        }

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
                // r[impl config.validate.writable]
                if mapping.writable {
                    let mut error = |reason: &str| {
                        errors.push(ValidationError::InvalidWritable {
                            server: name.clone(),
                            mapping: index,
                            reason: reason.to_string(),
                        })
                    };
                    if !mapping.rewrites.is_empty() {
                        error(
                            "it has rewrites, so its backend keys can't be mapped back to its paths",
                        );
                    }
                    if matches!(
                        self.backends.get(&mapping.backend),
                        Some(crate::Backend::Hylla { .. })
                    ) {
                        error("its backend is another hylla node, which is read only");
                    }
                }
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
                // r[impl config.validate.konklav]
                if let Some(konklav) = &mapping.konklav {
                    for reason in self.mapping_konklav_reasons(mapping, konklav) {
                        errors.push(ValidationError::InvalidMappingKonklav {
                            server: name.clone(),
                            mapping: index,
                            reason,
                        });
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
        // r[impl config.validate.verifiers]
        errors.extend(self.validate_verifiers());
        // r[impl config.validate.konklav]
        errors.extend(self.validate_konklav());
        // r[impl config.validate.cache]
        errors.extend(self.validate_cache());
        // r[impl config.validate.refresh]
        errors.extend(self.validate_refresh());
        // r[impl config.validate.cluster]
        errors.extend(self.validate_cluster());
        // r[impl config.validate.hylla-backends]
        errors.extend(self.validate_hylla_backends());
        // r[impl config.validate.serve-backends]
        errors.extend(self.validate_serve_backends());
        // r[impl config.validate.control]
        errors.extend(self.validate_control());
        // r[impl config.validate.http]
        errors.extend(self.validate_http());
        // r[impl config.validate.health]
        errors.extend(self.validate_health());

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
    fn validate_konklav(&self) -> Vec<ValidationError> {
        let Some(konklav) = &self.konklav else {
            return Vec::new();
        };
        let mut reasons = Vec::new();
        for (field, url) in [
            ("relying_url", &konklav.relying_url),
            ("subject_url", &konklav.subject_url),
        ] {
            if !is_secure_url(url) {
                reasons.push(format!(
                    "`{field}` is `{url}`, which isn't an HTTPS URL, or an HTTP one to this host"
                ));
            }
        }
        for (field, value) in [
            ("key_id", &konklav.key_id),
            ("secret", &konklav.secret),
            ("issuer", &konklav.issuer),
            ("audience", &konklav.audience),
        ] {
            if value.trim().is_empty() {
                reasons.push(format!("`{field}` must not be empty"));
            }
        }
        if konklav.timeout.duration().is_zero() {
            reasons.push("`timeout` must be positive".to_string());
        }
        reasons
            .into_iter()
            .map(|reason| ValidationError::InvalidKonklav { reason })
            .collect()
    }

    /// Why a mapping's `konklav` is invalid.
    fn mapping_konklav_reasons(
        &self,
        mapping: &crate::Mapping,
        konklav: &crate::MappingKonklav,
    ) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.konklav.is_none() {
            reasons.push("there's no top-level `konklav` to decide its rules".to_string());
        }
        if konklav.rules.is_empty() {
            reasons.push("it has no rules, so every request would be refused".to_string());
        }
        for (index, rule) in konklav.rules.iter().enumerate() {
            let mut reason = |reason: String| reasons.push(format!("rule {index}: {reason}"));
            if rule.actions.is_empty() || rule.actions.iter().any(|action| action.is_empty()) {
                reason("`actions` must name at least one action, none of them empty".to_string());
            }
            let pattern = match crate::PathPattern::parse(&rule.path) {
                Ok(pattern) => pattern,
                Err(invalid) => {
                    reason(invalid.to_string());
                    continue;
                }
            };
            if !pattern.can_match_under(&mapping.path_prefix) {
                reason(format!(
                    "`{}` can't match paths under the mapping's prefix `{}`",
                    rule.path, mapping.path_prefix
                ));
            }
            if let Some(scope) = &rule.scope {
                if scope.kind.is_empty() {
                    reason("the scope's `kind` must not be empty".to_string());
                }
                if !pattern.captures().any(|capture| capture == scope.id) {
                    reason(format!(
                        "the scope's `id` is `{}`, which `{}` doesn't capture",
                        scope.id, rule.path
                    ));
                }
            }
        }
        reasons
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

/// Reasons TLS files are invalid: every one must be set.
fn tls_file_reasons(files: [(&str, &str); 3]) -> Vec<String> {
    files
        .into_iter()
        .filter(|(_, path)| path.is_empty())
        .map(|(field, _)| format!("`tls.{field}` must be a file"))
        .collect()
}

/// Whether `name` is a DNS name, such as `hylla.cluster`: dot-separated labels of letters,
/// digits and hyphens, not starting or ending with a hyphen, and not an address.
fn is_dns_name(name: &str) -> bool {
    let labels_valid = name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    });
    labels_valid && name.len() <= 253 && name.parse::<std::net::IpAddr>().is_err()
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
        if cluster.previous_keys.iter().any(|key| key.len() < 32) {
            reasons.push("`previous_keys` must each be at least 32 characters".to_string());
        }
        if let Some(tls) = &cluster.tls {
            reasons.extend(tls_file_reasons([
                ("ca", &tls.ca),
                ("cert", &tls.cert),
                ("key", &tls.key),
            ]));
            if !is_dns_name(&tls.server_name) {
                reasons.push(format!(
                    "`tls.server_name` is `{}`, which isn't a DNS name",
                    tls.server_name
                ));
            }
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

impl Config {
    fn validate_hylla_backends(&self) -> Vec<ValidationError> {
        let mut backends = self.backends.iter().collect::<Vec<_>>();
        backends.sort_by_key(|(name, _)| *name);
        let mut errors = Vec::new();
        for (name, backend) in backends {
            let crate::Backend::Hylla {
                url,
                backend: remote,
                tls,
                ..
            } = backend
            else {
                continue;
            };
            let mut reasons = Vec::new();
            let host = url
                .strip_prefix("https://")
                .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default());
            if host.is_none_or(str::is_empty) {
                reasons.push(format!(
                    "`url` is `{url}`, which isn't an `https://` URL with a host"
                ));
            }
            if remote.is_empty() {
                reasons.push("`backend` must name a backend of the other node".to_string());
            }
            reasons.extend(tls_file_reasons([
                ("ca", &tls.ca),
                ("cert", &tls.cert),
                ("key", &tls.key),
            ]));
            errors.extend(
                reasons
                    .into_iter()
                    .map(|reason| ValidationError::InvalidBackend {
                        backend: name.clone(),
                        reason,
                    }),
            );
        }
        errors
    }

    fn validate_serve_backends(&self) -> Vec<ValidationError> {
        let Some(serve) = &self.serve_backends else {
            return Vec::new();
        };
        let mut reasons = Vec::new();
        if serve.backends.is_empty() {
            reasons.push("`backends` must name at least one backend to serve".to_string());
        }
        for backend in &serve.backends {
            if !self.backends.contains_key(backend) {
                reasons.push(format!("`backends` names unknown backend `{backend}`"));
            }
        }
        let mut servers = self
            .servers
            .iter()
            .filter(|(_, server)| server.listen == serve.listen)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        servers.sort();
        for server in servers {
            reasons.push(format!(
                "`listen` is {}, which server `{server}` listens on too",
                serve.listen
            ));
        }
        if self
            .cluster
            .as_ref()
            .is_some_and(|cluster| cluster.listen == serve.listen)
        {
            reasons.push(format!(
                "`listen` is {}, which the cluster listens on too",
                serve.listen
            ));
        }
        reasons.extend(tls_file_reasons([
            ("ca", &serve.tls.ca),
            ("cert", &serve.tls.cert),
            ("key", &serve.tls.key),
        ]));
        if !is_dns_name(&serve.tls.client_name) {
            reasons.push(format!(
                "`tls.client_name` is `{}`, which isn't a DNS name",
                serve.tls.client_name
            ));
        }
        reasons
            .into_iter()
            .map(|reason| ValidationError::InvalidServeBackends { reason })
            .collect()
    }
}

/// Whether `url` is an HTTP or HTTPS URL with a host.
fn is_http_url(url: &str) -> bool {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .is_some_and(|host| !host.is_empty())
}

/// Whether `url` is an HTTPS URL with a host, or an HTTP one to a loopback address, which no
/// one else on the network can answer for.
pub fn is_secure_url(url: &str) -> bool {
    if url.starts_with("https://") {
        return is_http_url(url);
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl Config {
    fn validate_verifiers(&self) -> Vec<ValidationError> {
        let mut verifiers = self.auth.iter().collect::<Vec<_>>();
        verifiers.sort_by_key(|(name, _)| *name);
        let mut errors = Vec::new();
        for (name, verifier) in verifiers {
            let mut error = |reason: String| {
                errors.push(ValidationError::InvalidVerifier {
                    verifier: name.clone(),
                    reason,
                })
            };
            let url = match (&verifier.public_key, &verifier.jwks_uri, &verifier.issuer) {
                (Some(_), Some(_), _) => {
                    error("set either `public_key` or `jwks_uri`, not both".into());
                    None
                }
                (Some(_), None, _) => None,
                (None, Some(jwks_uri), _) => Some(("jwks_uri", jwks_uri)),
                (None, None, Some(issuer)) => Some(("issuer", issuer)),
                (None, None, None) => {
                    error(
                        "it needs a `public_key`, a `jwks_uri`, or an `issuer` to read its keys \
                         from"
                            .into(),
                    );
                    None
                }
            };
            // r[impl auth.provider-https]
            if let Some((field, url)) = url
                && !is_secure_url(url)
            {
                error(format!(
                    "`{field}` is `{url}`, which isn't an HTTPS URL, or an HTTP one to this host"
                ));
            }
        }
        errors
    }

    fn validate_control(&self) -> Vec<ValidationError> {
        let Some(control) = &self.control else {
            return Vec::new();
        };
        let mut reasons = Vec::new();
        if control.users.is_empty() && control.oidc.is_none() {
            reasons.push("there are no `users` and no `oidc`, so no one can sign in".to_string());
        }
        if let Some(oidc) = &control.oidc {
            if !is_http_url(&oidc.url) {
                reasons.push(format!("`oidc.url` is `{}`, which isn't a URL", oidc.url));
            }
            // r[impl auth.provider-https]
            if !is_secure_url(&oidc.issuer) {
                reasons.push(format!(
                    "`oidc.issuer` is `{}`, which isn't an HTTPS URL, or an HTTP one to this host",
                    oidc.issuer
                ));
            }
            if oidc.client_id.is_empty() {
                reasons.push("`oidc.client_id` must be set".to_string());
            }
            if oidc.groups.is_empty() {
                reasons.push("`oidc.groups` must allow at least one group in".to_string());
            }
            if oidc.session_ttl.duration().is_zero() {
                reasons.push("`oidc.session_ttl` must be positive".to_string());
            }
        }
        let mut users = control.users.iter().collect::<Vec<_>>();
        users.sort_by_key(|(name, _)| *name);
        for (name, user) in users {
            if !user.password_hash.starts_with("$argon2") {
                reasons.push(format!(
                    "the `password_hash` of user `{name}` isn't an argon2 hash, as \
                     `hylla hash-password` prints"
                ));
            }
        }
        if control.max_upload_mib == 0 {
            reasons.push("`max_upload_mib` must be positive".to_string());
        }
        let mut servers = self
            .servers
            .iter()
            .filter(|(_, server)| server.listen == control.listen)
            .map(|(name, _)| format!("server `{name}`"))
            .collect::<Vec<_>>();
        servers.sort();
        if self
            .cluster
            .as_ref()
            .is_some_and(|cluster| cluster.listen == control.listen)
        {
            servers.push("the cluster".to_string());
        }
        if self
            .serve_backends
            .as_ref()
            .is_some_and(|serve| serve.listen == control.listen)
        {
            servers.push("serve_backends".to_string());
        }
        for other in servers {
            reasons.push(format!(
                "`listen` is {}, which {other} listens on too",
                control.listen
            ));
        }
        reasons
            .into_iter()
            .map(|reason| ValidationError::InvalidControl { reason })
            .collect()
    }
}

impl Config {
    fn validate_http(&self) -> Vec<ValidationError> {
        let http = &self.http;
        let mut reasons = Vec::new();
        if http.header_read_timeout.duration().is_zero() {
            reasons.push("`header_read_timeout` must be positive".to_string());
        }
        if http.max_connections == 0 {
            reasons.push("`max_connections` must be positive".to_string());
        }
        let mut errors = Vec::new();
        let mut backends = self.backends.iter().collect::<Vec<_>>();
        backends.sort_by_key(|(name, _)| *name);
        for (name, backend) in backends {
            let timeouts = backend.timeouts();
            // r[impl config.validate.backend-timeouts]
            for (field, timeout) in [("connect", timeouts.connect), ("request", timeouts.request)] {
                if timeout.duration().is_zero() {
                    errors.push(ValidationError::InvalidBackend {
                        backend: name.clone(),
                        reason: format!("`timeouts.{field}` must be positive"),
                    });
                }
            }
        }
        errors.extend(
            reasons
                .into_iter()
                .map(|reason| ValidationError::InvalidHttp { reason }),
        );
        errors
    }

    fn validate_health(&self) -> Vec<ValidationError> {
        let Some(health) = &self.health else {
            return Vec::new();
        };
        let mut others = self
            .servers
            .iter()
            .filter(|(_, server)| server.listen == health.listen)
            .map(|(name, _)| format!("server `{name}`"))
            .collect::<Vec<_>>();
        others.sort();
        let listens = [
            (
                "the cluster",
                self.cluster.as_ref().map(|cluster| cluster.listen),
            ),
            (
                "serve_backends",
                self.serve_backends.as_ref().map(|serve| serve.listen),
            ),
            (
                "the control plane",
                self.control.as_ref().map(|control| control.listen),
            ),
        ];
        for (other, listen) in listens {
            if listen == Some(health.listen) {
                others.push(other.to_string());
            }
        }
        others
            .into_iter()
            .map(|other| ValidationError::InvalidHealth {
                reason: format!(
                    "`listen` is {}, which {other} listens on too",
                    health.listen
                ),
            })
            .collect()
    }
}
