//! hylla's configuration, loaded with [ordna](../../../ordna).
//!
//! ```yaml
//! servers:
//!   public:
//!     listen: 0.0.0.0:8080
//!     server_names: [cache.example.com]
//!     mappings:                  # first match wins
//!       - path_prefix: /assets
//!         backend: assets
//!         rewrites:              # optional, applied in order
//!           - pattern: ^/assets/(.*)$
//!             replace: /$1       # /assets/x → /x on the backend
//! backends:
//!   assets:
//!     type: s3
//!     bucket: assets
//!     region: eu-north-1
//! ```
//!
//! Backend credentials are secret: they can only come from the secrets file.

mod load;
mod router;
mod validate;

use std::{collections::HashMap, net::SocketAddr};

use ordna_core::{config, config_tree};

pub use load::{
    ConfigError, ConfigFiles, ENV_PREFIX, Loaded, Origin, SourceError, load, load_with_env,
};
pub use router::{Route, Router};
pub use validate::ValidationError;

config_tree! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct Config {
        // r[impl config.servers]
        /// Servers by name.
        pub servers: HashMap<String, pub struct Server {
            // r[impl config.server.listen]
            /// The address and port to listen on, such as `0.0.0.0:8080`.
            pub listen: SocketAddr,
            // r[impl config.server.names]
            /// The host names this server answers to, matched against the `Host` header.
            /// The first name is the server's public host, used for absolute URLs.
            pub server_names: Vec<String>,
            // r[impl config.server.mappings]
            /// Mappings from request paths to backends, in priority order.
            pub mappings: Vec<pub struct Mapping {
                /// The request path prefix this mapping applies to, such as `/assets`.
                pub path_prefix: String,
                /// The name of the backend requests are sent to.
                pub backend: String,
                // r[impl config.mappings.rewrites]
                /// Regex replacements applied in order to the path sent to the backend.
                pub rewrites: Vec<pub struct Rewrite {
                    /// The regex to replace, such as `^/assets/(.*)$`.
                    pub pattern: String,
                    /// The replacement, which can refer to capture groups as `$1` or
                    /// `${name}`.
                    pub replace: String,
                }>,
            }>,
        }>,
        // r[impl config.backends]
        /// Backends by name.
        pub backends: HashMap<String, Backend>,
        // r[impl config.cache]
        /// The object cache. Optional; defaults to a memory only cache.
        #[facet(default)]
        pub cache: pub struct Cache {
            // r[impl config.cache.chunks]
            /// Objects are cached in chunks of this size, in KiB.
            #[facet(default = 1024)]
            pub chunk_size_kib: u32,
            // r[impl config.cache.metadata-ttl]
            /// How long object metadata (size, ETag, content type) is cached, in seconds.
            #[facet(default = 30)]
            pub metadata_ttl_seconds: u64,
            // r[impl config.cache.stale-if-error]
            /// How long past its TTL cached metadata may still be used when the backend fails,
            /// in seconds, so cached objects stay available during backend outages. `0`
            /// disables serving stale objects.
            #[facet(default = 86400)]
            pub stale_if_error_seconds: u64,
            // r[impl config.cache.write-policy]
            /// When chunks are written to disk.
            #[facet(default)]
            pub write_policy: #[derive(Default)] #[facet(rename_all = "snake_case")] pub enum WritePolicy {
                /// When they are evicted from memory, keeping the disk for colder data.
                #[default]
                OnEviction,
                /// As soon as they are cached, so they survive restarts sooner.
                OnInsertion,
            },
            // r[impl config.cache.memory]
            /// The memory tier.
            #[facet(default)]
            pub memory: pub struct MemoryCache {
                /// Capacity in MiB.
                #[facet(default = 256)]
                pub capacity_mib: u64,
            },
            // r[impl config.cache.disk]
            /// The disk tier. Without it, only memory is used.
            pub disk: Option<pub struct DiskCache {
                /// The directory the cache is stored in.
                pub path: std::path::PathBuf,
                /// Capacity in MiB.
                pub capacity_mib: u64,
                /// Size of foyer's disk blocks in MiB. A chunk must fit in a block.
                #[facet(default = 16)]
                pub block_size_mib: u32,
            }>,
        },
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            chunk_size_kib: 1024,
            metadata_ttl_seconds: 30,
            stale_if_error_seconds: 86400,
            write_policy: WritePolicy::default(),
            memory: MemoryCache::default(),
            disk: None,
        }
    }
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self { capacity_mib: 256 }
    }
}

impl Cache {
    /// The chunk size in bytes.
    pub fn chunk_size(&self) -> u64 {
        u64::from(self.chunk_size_kib) * 1024
    }

    /// How long object metadata is cached.
    pub fn metadata_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.metadata_ttl_seconds)
    }

    /// How long past its TTL metadata may be used when the backend fails.
    pub fn stale_if_error(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.stale_if_error_seconds)
    }
}

// r[impl config.backends.types]
// r[impl config.backends.secrets]
/// An object storage backend, tagged by `type`.
#[config]
#[derive(Debug, Clone, PartialEq)]
#[facet(tag = "type", rename_all = "snake_case")]
pub enum Backend {
    /// Amazon S3 or an S3 compatible store.
    S3 {
        bucket: String,
        region: String,
        /// Overrides the endpoint, for S3 compatible stores.
        endpoint: Option<String>,
        #[config(secret)]
        access_key_id: String,
        #[config(secret)]
        secret_access_key: String,
    },
    /// OpenStack Swift.
    Swift {
        auth_url: String,
        container: String,
        region: Option<String>,
        username: String,
        #[config(secret)]
        password: String,
    },
    /// Azure Blob Storage.
    AzureBlob {
        account: String,
        container: String,
        /// Overrides the endpoint, such as for Azurite.
        endpoint: Option<String>,
        #[config(secret)]
        account_key: String,
    },
}

impl Server {
    /// The server's public host: its first server name.
    pub fn public_host(&self) -> Option<&str> {
        self.server_names.first().map(String::as_str)
    }
}

impl Mapping {
    // r[impl config.mappings.prefix]
    /// Whether `path` is under this mapping's prefix, i.e. equal to it or continuing with
    /// a `/`, so `/assets` matches `/assets/x` but not `/assets2`.
    pub fn matches(&self, path: &str) -> bool {
        let prefix = self.path_prefix.trim_end_matches('/');
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::Server;

    #[test]
    fn the_public_host_is_the_first_server_name() {
        let server = Server {
            listen: "127.0.0.1:8080".parse().unwrap(),
            server_names: vec!["cache.example.com".into(), "cdn.example.com".into()],
            mappings: Vec::new(),
        };
        assert_eq!(server.public_host(), Some("cache.example.com"));
    }
}
