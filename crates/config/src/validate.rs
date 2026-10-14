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
