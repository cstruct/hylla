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

mod interval;
mod load;
mod router;
mod template;
mod validate;

use std::{collections::HashMap, net::SocketAddr};

use ordna_core::{config, config_tree};

pub use interval::Interval;
pub use load::{
    ConfigError, ConfigFiles, ENV_PREFIX, Loaded, Origin, SourceError, load, load_with_env,
};
pub use router::{Route, Router};
pub use template::{InvalidTemplate, PathTemplate, RenderError, under_prefix};
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
                // r[impl config.auth.mappings]
                /// Requires requests to carry a valid JWT. Without it, the mapping is public.
                pub auth: Option<pub struct MappingAuth {
                    /// The name of the verifier in `auth` that checks tokens.
                    pub verifier: String,
                    /// Backend path prefixes a token may read, with claims filled in, such as
                    /// `/tenant/${sub}`. A request must be under one of them. Without any, a
                    /// valid token may read anything the mapping serves.
                    #[facet(default)]
                    pub paths: Vec<String>,
                    /// Whether responses are marked `Cache-Control: private`, so shared
                    /// caches in front of hylla don't serve them to others.
                    #[facet(default = true)]
                    pub private: bool,
                }>,
            }>,
        }>,
        // r[impl config.backends]
        /// Backends by name.
        pub backends: HashMap<String, Backend>,
        // r[impl config.cluster]
        /// Other hylla nodes to share revalidations and preloads with. Optional.
        pub cluster: Option<pub struct Cluster {
            /// The address of the cluster listener, for gossip and data between nodes. Keep
            /// it on a private network.
            pub listen: SocketAddr,
            /// The address other nodes reach this one at. Defaults to `listen`, which must
            /// then be a specific address rather than `0.0.0.0` or `[::]`.
            pub advertise: Option<SocketAddr>,
            // r[impl config.cluster.seeds]
            /// Nodes to join through, as `host:port`. Names are resolved again on every
            /// gossip round, so a DNS name returning every node, such as a Kubernetes
            /// headless service, works as well as a list of addresses.
            #[facet(default)]
            pub seeds: Vec<String>,
            /// How often membership is gossiped, such as `1s`.
            #[facet(default = Interval::from_secs(1))]
            pub gossip_interval: Interval,
            /// How long a node may go without a heartbeat before it's considered gone.
            #[facet(default = Interval::from_secs(10))]
            pub failure_timeout: Interval,
            // r[impl config.cluster.key]
            /// The key shared by the nodes, authenticating everything between them. At
            /// least 32 characters.
            #[config(secret)]
            pub key: String,
            // r[impl config.cluster.share]
            /// What the nodes share.
            #[facet(default)]
            pub share: pub struct ClusterShare {
                /// Pushes revalidated metadata to the other nodes, so they don't each ask
                /// the backend.
                #[facet(default = true)]
                pub metadata: bool,
                /// Lets one node run each refresh rule and the others pull what it preloads
                /// from it, so the backend serves each preload once.
                #[facet(default = true)]
                pub preload: bool,
            },
        }>,
        // r[impl config.auth.verifiers]
        /// JWT verifiers by name, used by mappings' `auth`.
        #[facet(default)]
        pub auth: HashMap<String, Verifier>,
        // r[impl config.cache]
        /// The object cache. Optional; defaults to a memory only cache.
        #[facet(default)]
        pub cache: pub struct Cache {
            // r[impl config.cache.chunks]
            /// Objects are cached in chunks of this size, in KiB.
            #[facet(default = 1024)]
            pub chunk_size_kib: u32,
            // r[impl config.cache.metadata-ttl]
            /// How long object metadata (size, ETag, content type) is cached, such as `30s`.
            #[facet(default = Interval::from_secs(30))]
            pub metadata_ttl: Interval,
            // r[impl config.cache.stale-while-revalidate]
            /// How long past its TTL cached metadata is still served while it is revalidated in
            /// the background, such as `5m`, so requests don't wait on the backend. `0s`
            /// disables it, revalidating before serving.
            #[facet(default = Interval::from_secs(0))]
            pub stale_while_revalidate: Interval,
            // r[impl config.cache.stale-if-error]
            /// How long past its TTL cached metadata may still be used when the backend fails,
            /// such as `1d`, so cached objects stay available during backend outages. `0s`
            /// disables serving stale objects.
            #[facet(default = Interval::from_secs(24 * 60 * 60))]
            pub stale_if_error: Interval,
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
            metadata_ttl: Interval::from_secs(30),
            stale_while_revalidate: Interval::from_secs(0),
            stale_if_error: Interval::from_secs(24 * 60 * 60),
            write_policy: WritePolicy::default(),
            memory: MemoryCache::default(),
            disk: None,
        }
    }
}

impl Default for ClusterShare {
    fn default() -> Self {
        Self {
            metadata: true,
            preload: true,
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
        /// Background refresh of cached objects.
        refresh: Option<Refresh>,
    },
    /// Azure Blob Storage.
    AzureBlob {
        account: String,
        container: String,
        /// Overrides the account's URL, which defaults to
        /// `https://{account}.blob.core.windows.net`, such as
        /// `http://127.0.0.1:10000/devstoreaccount1` for Azurite.
        endpoint: Option<String>,
        #[config(secret)]
        account_key: String,
        /// Background refresh of cached objects.
        refresh: Option<Refresh>,
    },
}

/// Checks JWTs: their signature against a public key, and their `exp` and `nbf` claims, and
/// optionally their `iss` and `aud`.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct Verifier {
    /// The one algorithm tokens must be signed with.
    pub algorithm: JwtAlgorithm,
    /// The public key, PEM encoded.
    pub public_key: String,
    /// When set, tokens must have this `iss`.
    pub issuer: Option<String>,
    /// When set, tokens must have this `aud`.
    pub audience: Option<String>,
    /// How much clock skew `exp` and `nbf` allow, such as `30s`.
    #[facet(default = Interval::from_secs(30))]
    pub leeway: Interval,
}

// r[impl config.auth.algorithms]
/// The asymmetric JWT signature algorithms. Shared secret (`HS*`) and `none` algorithms are
/// deliberately missing.
#[config]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtAlgorithm {
    #[facet(rename = "RS256")]
    Rs256,
    #[facet(rename = "RS384")]
    Rs384,
    #[facet(rename = "RS512")]
    Rs512,
    #[facet(rename = "PS256")]
    Ps256,
    #[facet(rename = "PS384")]
    Ps384,
    #[facet(rename = "PS512")]
    Ps512,
    #[facet(rename = "ES256")]
    Es256,
    #[facet(rename = "ES384")]
    Es384,
    #[facet(rename = "EdDSA")]
    EdDsa,
}

// r[impl config.refresh]
/// Background refresh of a backend's cached objects.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct Refresh {
    /// How many objects are refreshed at the same time.
    #[facet(default = 4)]
    pub concurrency: u32,
    /// What to refresh, and how often.
    pub rules: Vec<RefreshRule>,
}

// r[impl config.refresh.rules]
/// Refreshes the objects under a key prefix, or a single key, of a backend.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshRule {
    /// A backend key prefix, such as `css/`; directory-like, so `css` doesn't match
    /// `cssx/app.css`. Empty for the whole backend.
    pub prefix: Option<String>,
    /// A single backend key, such as `css/app.css`.
    pub key: Option<String>,
    /// How often to refresh, such as `15m`. The first refresh runs at startup.
    pub every: Interval,
    #[facet(default)]
    pub mode: RefreshMode,
    /// The most objects one preload fetches.
    pub max_objects: Option<u64>,
    /// The most data one preload fetches, in MiB.
    pub max_mib: Option<u64>,
}

// r[impl config.refresh.modes]
/// What a refresh rule does.
#[config]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[facet(rename_all = "snake_case")]
pub enum RefreshMode {
    /// Nothing, disabling the rule. Spelled `disabled` rather than `none` or `off`, which
    /// YAML parsers may read as null or false.
    Disabled,
    /// Re-checks cached objects, updating their metadata, so changes are picked up without
    /// requests waiting on the backend.
    #[default]
    Revalidate,
    /// Fetches objects that aren't cached, or changed, into the cache.
    Preload,
    /// Both revalidates and preloads.
    Both,
}

impl RefreshMode {
    pub fn revalidates(self) -> bool {
        matches!(self, Self::Revalidate | Self::Both)
    }

    pub fn preloads(self) -> bool {
        matches!(self, Self::Preload | Self::Both)
    }
}

impl Backend {
    /// The backend's background refresh, if any.
    pub fn refresh(&self) -> Option<&Refresh> {
        match self {
            Backend::S3 { refresh, .. } | Backend::AzureBlob { refresh, .. } => refresh.as_ref(),
        }
    }
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
