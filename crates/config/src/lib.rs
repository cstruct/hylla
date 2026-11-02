//! hylla's configuration, loaded with [ordna](https://github.com/cstruct/ordna) from
//! [Styx](https://github.com/bearcove/styx) files.
//!
//! ```styx
//! servers {
//!   public {
//!     listen 0.0.0.0:8080
//!     server_names (cache.example.com)
//!     // First match wins.
//!     mappings (
//!       {
//!         path_prefix /assets
//!         backend assets
//!         // Optional, applied in order: /assets/x is /x on the backend.
//!         rewrites ({pattern "^/assets/(.*)$", replace /$1})
//!       }
//!     )
//!   }
//! }
//! backends {
//!   assets {
//!     type s3
//!     bucket assets
//!     region eu-north-1
//!   }
//! }
//! ```
//!
//! Backend credentials are secret: they can only come from the secrets file.

mod interval;
mod load;
mod router;
mod show;
mod site;
mod template;
mod validate;

use std::{collections::HashMap, net::SocketAddr};

use ordna_core::{config, config_tree};

pub use interval::Interval;
pub use load::{
    ConfigError, ConfigFiles, ENV_PREFIX, Inspected, Loaded, Origin, SourceError, inspect,
    inspect_with_env, load, load_with_env,
};
pub use router::{Route, Router};
pub use show::{ExplainError, explain, render};
pub use site::{is_alias_name, is_sha, wildcard_suffix};
pub use template::{InvalidTemplate, PathTemplate, under_prefix};
pub use validate::{ValidationError, is_secure_url};

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
                // r[impl config.mappings.cache-control]
                /// The `Cache-Control` of objects that have none of their own, such as
                /// `public, max-age=300`. Without it, they're answered without one.
                pub cache_control: Option<String>,
                // r[impl config.mappings.writable]
                /// Whether the control plane may change the mapping's objects: upload,
                /// delete and move them, under its path prefix only. Public listeners never
                /// write.
                #[facet(default)]
                pub writable: bool,
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
                // r[impl config.site]
                /// Serves frontend builds, one per commit, chosen by SHA or by alias, rather
                /// than mapping paths to keys.
                pub site: Option<pub struct Site {
                    // r[impl config.site.prefixes]
                    /// The backend key prefix builds are stored under, each in a directory
                    /// named by its commit's SHA.
                    #[facet(default = String::from("builds/"))]
                    pub builds: String,
                    /// The backend key prefix aliases are stored under, each an object whose
                    /// body is the SHA it points at.
                    #[facet(default = String::from("aliases/"))]
                    pub aliases: String,
                    // r[impl config.site.alias]
                    /// The alias always served, such as `live`. Without it, the build is chosen
                    /// by the label a wildcard server name matched.
                    pub alias: Option<String>,
                    // r[impl config.site.index]
                    /// The object served for paths ending with `/`.
                    #[facet(default = String::from("index.html"))]
                    pub index: String,
                    // r[impl config.site.fallback]
                    /// The object served for page loads of paths the build doesn't have, such as
                    /// `index.html` for single page apps.
                    pub fallback: Option<String>,
                    // r[impl config.site.redirect]
                    /// Whether requests choosing an alias by their host are redirected to the
                    /// build's SHA host, so a browser stays on one build while the alias moves.
                    #[facet(default = true)]
                    pub redirect: bool,
                    // r[impl config.site.alias-cache-control]
                    /// The `Cache-Control` of responses served through an alias without
                    /// redirecting, since the same URL serves another build once it moves.
                    #[facet(default = String::from("no-cache"))]
                    pub alias_cache_control: String,
                }>,
            }>,
        }>,
        // r[impl config.backends]
        /// Backends by name.
        pub backends: HashMap<String, Backend>,
        // r[impl config.telemetry]
        /// Exporting telemetry over OTLP. Off unless enabled per signal.
        #[facet(default)]
        pub telemetry: pub struct Telemetry {
            /// The `service.name` resource attribute.
            #[facet(default = String::from("hylla"))]
            pub service_name: String,
            /// More resource attributes, such as `deployment.environment`. A list, since
            /// attribute names contain dots, which map keys can't. `OTEL_RESOURCE_ATTRIBUTES`
            /// is read as well.
            #[facet(default)]
            pub resource: Vec<pub struct ResourceAttribute {
                pub key: String,
                pub value: String,
            }>,
            // r[impl config.telemetry.otlp]
            /// Where and how to export. Settings left out fall back to the standard
            /// `OTEL_EXPORTER_OTLP_*` environment variables.
            #[facet(default)]
            pub otlp: pub struct Otlp {
                /// The collector's base URL, such as `http://collector:4318`; over HTTP, the
                /// signal's path, such as `/v1/metrics`, is appended.
                pub endpoint: Option<String>,
                #[facet(default)]
                pub protocol: #[derive(Default)] #[facet(rename_all = "snake_case")] pub enum OtlpProtocol {
                    /// Protobuf over HTTP, conventionally on port 4318.
                    #[default]
                    HttpProtobuf,
                    /// gRPC, conventionally on port 4317.
                    Grpc,
                },
                /// Headers sent with every export, such as an API key.
                #[config(secret)]
                #[facet(default)]
                pub headers: HashMap<String, String>,
                /// How long an export may take, such as `10s`.
                #[facet(default = Interval::from_secs(10))]
                pub timeout: Interval,
            },
            // r[impl config.telemetry.metrics]
            /// Exporting metrics.
            #[facet(default)]
            pub metrics: pub struct MetricsExport {
                #[facet(default)]
                pub enabled: bool,
                /// The full URL to export metrics to, overriding `otlp.endpoint`, such as
                /// `http://prometheus:9090/api/v1/otlp/v1/metrics`.
                pub endpoint: Option<String>,
                /// How often metrics are exported, such as `30s`.
                #[facet(default = Interval::from_secs(30))]
                pub interval: Interval,
            },
            // r[impl config.telemetry.traces]
            /// Exporting traces.
            #[facet(default)]
            pub traces: pub struct TracesExport {
                #[facet(default)]
                pub enabled: bool,
                /// The full URL to export traces to, overriding `otlp.endpoint`, such as
                /// `http://quickwit:7280/api/v1/otlp/v1/traces`.
                pub endpoint: Option<String>,
                /// The share of traces to sample, from 0 to 1. Requests whose caller sampled
                /// them are traced either way.
                #[facet(default = 1.0)]
                pub sample_ratio: f64,
                /// Whether to record request and object paths in spans. Off by default, since
                /// paths can tell more than they should, such as who a tenant is.
                #[facet(default)]
                pub record_paths: bool,
                /// Whether requests to public listeners continue the traces their
                /// `traceparent` header names, following their callers' sampling. Off by
                /// default, since anyone can send one: turn it on behind a proxy that sets it.
                /// Requests between nodes continue their traces either way.
                #[facet(default)]
                pub follow_callers: bool,
            },
            // r[impl config.telemetry.logs]
            /// Logging, to stderr and optionally exported.
            #[facet(default)]
            pub logs: pub struct Logs {
                /// What to log, as `tracing` filter directives such as `info` or
                /// `info,hylla_server=debug`. `RUST_LOG` overrides it.
                #[facet(default = String::from("info"))]
                pub level: String,
                #[facet(default)]
                pub format: #[derive(Default)] #[facet(rename_all = "snake_case")] pub enum LogFormat {
                    /// Human-readable lines.
                    #[default]
                    Text,
                    /// A JSON object per line, with the trace and span IDs of traced requests.
                    Json,
                },
                /// Whether to export logs over OTLP too.
                #[facet(default)]
                pub export: bool,
                /// The full URL to export logs to, overriding `otlp.endpoint`, such as
                /// `http://quickwit:7280/api/v1/otlp/v1/logs`.
                pub endpoint: Option<String>,
            },
        },
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
            // r[impl config.cluster.previous-keys]
            /// Keys still accepted from other nodes, though not signed with, while the key is
            /// changed one node at a time.
            #[config(secret)]
            #[facet(default)]
            pub previous_keys: Vec<String>,
            // r[impl config.cluster.tls]
            /// Mutual TLS between the nodes, encrypting what they send each other and
            /// checking their certificates. Without it, nodes talk plain HTTP, though signed
            /// with the key either way.
            pub tls: Option<pub struct ClusterTls {
                /// The PEM file of the CA certificates nodes' certificates must be issued
                /// by. Use a CA for the cluster only: any certificate it issued for
                /// `server_name` is a node. Read again when it changes.
                pub ca: String,
                /// The PEM file of this node's certificate chain, read again when it
                /// changes, so certificates can be rotated without a restart.
                pub cert: String,
                /// The PEM file of this node's private key, read again when it changes.
                pub key: String,
                /// The DNS name every node's certificate is issued for, such as
                /// `hylla.cluster`. Nodes reach each other by address, which changes as
                /// nodes come and go, so certificates are checked for this name instead.
                pub server_name: String,
            }>,
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
        // r[impl config.serve-backends]
        /// Serving backends to other hylla nodes, which use them as `hylla` backends, over
        /// mutual TLS. Optional.
        pub serve_backends: Option<pub struct ServeBackends {
            /// The address of the listener, such as `0.0.0.0:7947`.
            pub listen: SocketAddr,
            /// The names of the backends served. Requests for others are refused.
            pub backends: Vec<String>,
            /// The listener's certificate, and what other nodes' certificates must be.
            pub tls: pub struct ServeTls {
                /// The PEM file of the CA certificates other nodes' certificates must be
                /// issued by. Read again when it changes.
                pub ca: String,
                /// The PEM file of this node's certificate chain, issued for the host other
                /// nodes reach it at. Read again when it changes.
                pub cert: String,
                /// The PEM file of this node's private key, read again when it changes.
                pub key: String,
                /// The DNS name other nodes' certificates must be issued for, such as
                /// `hylla.downstream`, so not every certificate the CA issued is a client.
                pub client_name: String,
            },
        }>,
        // r[impl config.control]
        /// The control plane: a web UI and JSON API showing the status of each backend and
        /// browsing the mappings, changing the objects of writable ones. Optional.
        pub control: Option<pub struct Control {
            /// The address to listen on, such as `127.0.0.1:9090`. Keep it private.
            pub listen: SocketAddr,
            // r[impl config.control.users]
            /// The users signing in, with HTTP Basic auth, by name.
            #[facet(default)]
            pub users: HashMap<String, pub struct ControlUser {
                /// An argon2 hash of the user's password, as `hylla hash-password` prints
                /// it, so only the secrets file can set it.
                #[config(secret)]
                pub password_hash: String,
            }>,
            /// The largest upload, in MiB.
            #[facet(default = 256)]
            pub max_upload_mib: u64,
            // r[impl config.control.oidc]
            /// Signing in through an OpenID provider, such as Konklav, with the
            /// authorization code flow and PKCE. Basic auth keeps working for `users`.
            pub oidc: Option<pub struct ControlOidc {
                /// The provider's issuer, such as `https://id.example.com`. Its discovery
                /// document is read from `{issuer}/.well-known/openid-configuration`.
                pub issuer: String,
                /// The client id hylla is registered with.
                pub client_id: String,
                /// The client's secret, for a confidential client. Without it, hylla signs
                /// in as a public client, protected by PKCE alone.
                #[config(secret)]
                pub client_secret: Option<String>,
                /// The control plane's own URL, as browsers reach it, such as
                /// `https://hylla-control.internal`. The provider redirects back to
                /// `{url}/auth/callback`, and to `{url}/` after signing out.
                pub url: String,
                /// The groups allowed in: a user must be in at least one.
                pub groups: Vec<String>,
                /// The claim of the ID token, or of the user info, listing the user's
                /// groups.
                #[facet(default = String::from("groups"))]
                pub groups_claim: String,
                /// The scopes asked for besides `openid`, such as `email` and `groups`.
                #[facet(default = vec![String::from("email"), String::from("groups")])]
                pub scopes: Vec<String>,
                /// How long a sign-in lasts, such as `8h`.
                #[facet(default = Interval::from_secs(8 * 60 * 60))]
                pub session_ttl: Interval,
            }>,
        }>,
        // r[impl config.health]
        /// Health checks for load balancers and orchestrators, on a listener of their own:
        /// `/healthz` while the process runs, `/readyz` while it serves. Optional.
        pub health: Option<pub struct Health {
            /// The address to listen on, such as `0.0.0.0:8081`.
            pub listen: SocketAddr,
        }>,
        // r[impl config.http]
        /// Limits of every listener: the servers', the control plane's, the cluster's,
        /// `serve_backends`' and the health checks'. Optional; every setting has a default.
        #[facet(default)]
        pub http: pub struct Http {
            /// How long a client may take to send a request's headers, such as `10s`.
            #[facet(default = Interval::from_secs(10))]
            pub header_read_timeout: Interval,
            /// How long a connection may go without sending or receiving anything, idle
            /// between requests or stalled, before it's closed, such as `60s`. `0s` disables
            /// it.
            #[facet(default = Interval::from_secs(60))]
            pub idle_timeout: Interval,
            /// The most connections each listener has open at a time. Further ones wait to
            /// be accepted.
            #[facet(default = 10000)]
            pub max_connections: u32,
            /// On SIGTERM, how long requests in flight may take to finish before their
            /// connections are closed, such as `20s`. Keep it below the time hylla is given
            /// to stop, such as Kubernetes' `terminationGracePeriodSeconds`, so the cache is
            /// written to disk before hylla is killed.
            #[facet(default = Interval::from_secs(20))]
            pub shutdown_timeout: Interval,
        },
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
            // r[impl config.cache.not-found-ttl]
            /// How long an object the backend said is missing is answered with 404 without
            /// asking it again, such as `10s`, so requests for missing objects don't all reach
            /// the backend. Changing it through the control plane ends it. `0s` disables it.
            #[facet(default = Interval::from_secs(10))]
            pub not_found_ttl: Interval,
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
                pub path: String,
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
            not_found_ttl: Interval::from_secs(10),
            write_policy: WritePolicy::default(),
            memory: MemoryCache::default(),
            disk: None,
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            service_name: "hylla".into(),
            resource: Vec::new(),
            otlp: Otlp::default(),
            metrics: MetricsExport::default(),
            traces: TracesExport::default(),
            logs: Logs::default(),
        }
    }
}

impl Default for Otlp {
    fn default() -> Self {
        Self {
            endpoint: None,
            protocol: OtlpProtocol::default(),
            headers: HashMap::new(),
            timeout: Interval::from_secs(10),
        }
    }
}

impl Default for MetricsExport {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            interval: Interval::from_secs(30),
        }
    }
}

impl Default for TracesExport {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            sample_ratio: 1.0,
            record_paths: false,
            follow_callers: false,
        }
    }
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::default(),
            export: false,
            endpoint: None,
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

impl Default for Http {
    fn default() -> Self {
        Self {
            header_read_timeout: Interval::from_secs(10),
            idle_timeout: Interval::from_secs(60),
            max_connections: 10000,
            shutdown_timeout: Interval::from_secs(20),
        }
    }
}

impl Default for BackendTimeouts {
    fn default() -> Self {
        Self {
            connect: Interval::from_secs(5),
            request: Interval::from_secs(30),
            retries: 2,
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
        /// How long requests to the backend may take.
        #[facet(default)]
        timeouts: BackendTimeouts,
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
        /// How long requests to the backend may take.
        #[facet(default)]
        timeouts: BackendTimeouts,
        /// Background refresh of cached objects.
        refresh: Option<Refresh>,
    },
    // r[impl config.backends.hylla]
    /// Another hylla node serving one of its backends with `serve_backends`, over mutual
    /// TLS. Objects come through that node's cache, so it fetches them from its backend once
    /// for every node using it.
    Hylla {
        /// The node's base URL, such as `https://shield.internal:7947`. Its certificate must
        /// be issued for the URL's host.
        url: String,
        /// The name of the backend on that node.
        backend: String,
        tls: HyllaTls,
        /// How long requests to the backend may take.
        #[facet(default)]
        timeouts: BackendTimeouts,
        /// Background refresh of cached objects.
        refresh: Option<Refresh>,
    },
}

// r[impl config.backends.timeouts]
/// How long requests to a backend may take.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct BackendTimeouts {
    /// Connecting, such as `5s`.
    #[facet(default = Interval::from_secs(5))]
    pub connect: Interval,
    /// A whole request, from sending it to reading the last byte of the response, such as
    /// `30s`. A chunk must be fetched within it, and so must a listing for a refresh rule
    /// from a `hylla` backend, which comes as one response.
    #[facet(default = Interval::from_secs(30))]
    pub request: Interval,
    /// How many times requests to S3 or Azure that failed with a server error, or without an
    /// answer, are retried, with backoff, within `request` from the first attempt.
    #[facet(default = 2)]
    pub retries: u32,
}

/// Mutual TLS with a `hylla` backend.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct HyllaTls {
    /// The PEM file of the CA certificates the node's certificate must be issued by. Read
    /// again when it changes.
    pub ca: String,
    /// The PEM file of this node's certificate chain, presented to the other node, issued for
    /// its `client_name`. Read again when it changes.
    pub cert: String,
    /// The PEM file of this node's private key, read again when it changes.
    pub key: String,
}

/// Checks JWTs: their signature against a public key, or the keys of their provider, and
/// their `exp` and `nbf` claims, and optionally their `iss` and `aud`.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct Verifier {
    /// The one algorithm tokens must be signed with.
    pub algorithm: JwtAlgorithm,
    /// The public key, PEM encoded. Without it, keys are read from `jwks_uri`, or the
    /// issuer's discovery document, and read again as the provider rotates them.
    pub public_key: Option<String>,
    /// Where the provider publishes its keys, as a JSON Web Key Set, for providers without a
    /// discovery document.
    pub jwks_uri: Option<String>,
    /// When set, tokens must have this `iss`. Without a `public_key` or `jwks_uri`, keys are
    /// read through its discovery document, at `{issuer}/.well-known/openid-configuration`.
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

// r[impl config.refresh.rules+2]
/// Refreshes the objects under a key prefix, or a single key, of a backend.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshRule {
    /// A backend key prefix, such as `css/`; directory-like, so `css` doesn't match
    /// `cssx/app.css`. Empty for the whole backend.
    pub prefix: Option<String>,
    /// A single backend key, such as `css/app.css`.
    pub key: Option<String>,
    /// The build an alias points at, read again on every run, such as the live site's.
    pub alias: Option<RefreshAlias>,
    /// How often to refresh, such as `15m`. The first refresh runs at startup.
    pub every: Interval,
    #[facet(default)]
    pub mode: RefreshMode,
    /// The most objects one preload fetches.
    pub max_objects: Option<u64>,
    /// The most data one preload fetches, in MiB.
    pub max_mib: Option<u64>,
}

// r[impl config.refresh.alias]
/// The alias a refresh rule follows, covering the build it points at.
#[config]
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshAlias {
    /// The alias, such as `live`.
    pub name: String,
    /// The backend key prefix builds are stored under, as the site's `builds`.
    #[facet(default = String::from("builds/"))]
    pub builds: String,
    /// The backend key prefix aliases are stored under, as the site's `aliases`.
    #[facet(default = String::from("aliases/"))]
    pub aliases: String,
}

// r[impl config.refresh.modes]
/// What a refresh rule does.
#[config]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[facet(rename_all = "snake_case")]
pub enum RefreshMode {
    /// Nothing, disabling the rule. Spelled `disabled` rather than `none` or `off`, which
    /// some config formats read as null or false.
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
    /// How long requests to the backend may take.
    pub fn timeouts(&self) -> &BackendTimeouts {
        match self {
            Backend::S3 { timeouts, .. }
            | Backend::AzureBlob { timeouts, .. }
            | Backend::Hylla { timeouts, .. } => timeouts,
        }
    }

    /// The backend's background refresh, if any.
    pub fn refresh(&self) -> Option<&Refresh> {
        match self {
            Backend::S3 { refresh, .. }
            | Backend::AzureBlob { refresh, .. }
            | Backend::Hylla { refresh, .. } => refresh.as_ref(),
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
