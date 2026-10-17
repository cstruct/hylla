use std::path::PathBuf;

use hylla_config::{
    Backend, ConfigError, ConfigFiles, DiskCache, Interval, JwtAlgorithm, Mapping, MappingAuth,
    Origin, OtlpProtocol, RefreshMode, RefreshRule, Rewrite, Route, Router, ValidationError,
    WritePolicy, load_with_env,
};
use ordna_core::{ConfigLoadError, ConfigPath, ConfigValueSource};
use ordna_source_env::EnvSource;

const CONFIG: &str = r#"
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com cdn.example.com)
    mappings (
      {
        path_prefix /assets
        backend assets
        rewrites ({pattern "^/assets/(.*)$", replace /$1})
      }
      {path_prefix /, backend archive}
    )
  }
  internal {
    listen 0.0.0.0:8080
    server_names (cache.internal)
    mappings ({path_prefix /, backend assets})
  }
}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
  }
  archive {
    type azure_blob
    account hyllaarchive
    container archive
  }
}
"#;

const SECRETS: &str = "
backends {
  assets {
    access_key_id AKIA123
    secret_access_key s3cr3t
  }
  archive {
    account_key hunter2
  }
}
";

/// A server mapping requests to a backend, for tests of other settings: a config without
/// one is invalid. Its credentials are [`MINIMAL_SECRETS`].
const MINIMAL: &str = "servers {public {listen 0.0.0.0:8080, server_names (cache.example.com), mappings ({path_prefix /, backend assets})}}\nbackends {assets {type s3, bucket assets, region eu-north-1}}\n";

const MINIMAL_SECRETS: &str = "backends {assets {access_key_id a, secret_access_key b}}\n";

struct Files {
    dir: PathBuf,
}

impl Files {
    fn new(name: &str, config: &str, secrets: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hylla-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.styx"), config).unwrap();
        std::fs::write(dir.join("secrets.styx"), secrets).unwrap();
        Self { dir }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config.styx")
    }

    fn secrets(&self) -> PathBuf {
        self.dir.join("secrets.styx")
    }

    fn load(
        &self,
        env: &[(&'static str, &'static str)],
    ) -> Result<hylla_config::Loaded, ConfigError> {
        self.load_with_flags(env, &[])
    }

    /// The config as far as it loads, without the secrets file when `secrets` is false.
    fn inspect(&self, secrets: bool) -> hylla_config::Inspected {
        hylla_config::inspect_with_env(
            &ConfigFiles {
                config: self.config(),
                config_required: true,
                secrets: secrets.then(|| self.secrets()),
                flags: Default::default(),
            },
            EnvSource::from_vars::<&str, &str>([]).prefix("HYLLA"),
        )
        .unwrap()
    }

    fn load_with_flags(
        &self,
        env: &[(&'static str, &'static str)],
        flags: &[(&str, &str)],
    ) -> Result<hylla_config::Loaded, ConfigError> {
        load_with_env(
            &ConfigFiles {
                config: self.config(),
                config_required: true,
                secrets: Some(self.secrets()),
                flags: flags
                    .iter()
                    .map(|(field, value)| (path(field), value.to_string()))
                    .collect(),
            },
            EnvSource::from_vars(env.iter().copied()).prefix("HYLLA"),
        )
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn path(path: &str) -> ConfigPath {
    ConfigPath::parse(path).unwrap()
}

// r[verify config.servers]
// r[verify config.server.listen]
// r[verify config.server.names]
// r[verify config.server.mappings]
// r[verify config.backends]
// r[verify config.backends.types]
// r[verify config.sources]
// r[verify config.ordna]
#[test]
fn loads_servers_and_backends() {
    let files = Files::new("load", CONFIG, SECRETS);
    let loaded = files
        .load(&[("HYLLA__SERVERS__INTERNAL__LISTEN", "127.0.0.1:9090")])
        .unwrap();
    let config = loaded.config();

    let public = &config.servers["public"];
    assert_eq!(public.listen, "0.0.0.0:8080".parse().unwrap());
    assert_eq!(
        public.server_names,
        vec!["cache.example.com", "cdn.example.com"]
    );
    assert_eq!(public.public_host(), Some("cache.example.com"));
    assert_eq!(
        public.mappings,
        vec![
            Mapping {
                path_prefix: "/assets".into(),
                backend: "assets".into(),
                rewrites: vec![Rewrite {
                    pattern: "^/assets/(.*)$".into(),
                    replace: "/$1".into(),
                }],
                auth: None,
            },
            Mapping {
                path_prefix: "/".into(),
                backend: "archive".into(),
                rewrites: Vec::new(),
                auth: None,
            },
        ]
    );
    let router = Router::new(public).unwrap();
    assert_eq!(
        router.route("/assets/app.css"),
        Some(Route {
            backend: "assets",
            path: "/app.css".into(),
            mapping: 0,
            auth: None,
        })
    );

    // Overridden by the environment.
    assert_eq!(
        config.servers["internal"].listen,
        "127.0.0.1:9090".parse().unwrap()
    );
    let listen = loaded.source(&path("servers.internal.listen")).unwrap();
    assert_eq!(
        listen.winner(),
        &ConfigValueSource::Direct(Origin::Env {
            var: "HYLLA__SERVERS__INTERNAL__LISTEN".into(),
            path: path("servers.internal.listen"),
        })
    );

    assert_eq!(
        config.backends["assets"],
        Backend::S3 {
            bucket: "assets".into(),
            region: "eu-north-1".into(),
            endpoint: None,
            access_key_id: "AKIA123".into(),
            secret_access_key: "s3cr3t".into(),
            refresh: None,
        }
    );
    assert_eq!(
        config.backends["archive"],
        Backend::AzureBlob {
            account: "hyllaarchive".into(),
            container: "archive".into(),
            endpoint: None,
            account_key: "hunter2".into(),
            refresh: None,
        }
    );
}

// r[verify config.backends.secrets]
#[test]
fn backend_credentials_are_secret() {
    let config = CONFIG.replace(
        "container archive",
        "container archive\n    account_key leaked",
    );
    let files = Files::new("secrets", &config, SECRETS);
    let Err(ConfigError::Load(ConfigLoadError::Violations(violations))) =
        files.load(&[("HYLLA__BACKENDS__ASSETS__SECRET_ACCESS_KEY", "leaked")])
    else {
        panic!("expected violations");
    };
    let mut paths = violations
        .layer_mismatches()
        .iter()
        .map(|issue| issue.path.to_string())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "backends.archive.account_key",
            "backends.assets.secret_access_key"
        ]
    );
}

// r[verify config.validate]
// r[verify config.validate.backend-refs]
// r[verify config.validate.server-names]
// r[verify config.validate.absolute-paths]
// r[verify config.validate.rewrites]
// r[verify config.validate.replacements]
// r[verify config.validate.unique-hosts]
#[test]
fn invalid_configs_report_every_problem() {
    let config = r#"
servers {
  a {
    listen 0.0.0.0:8080
    server_names (cache.example.com)
    mappings (
      {
        path_prefix assets
        backend missing
        rewrites (
          {pattern "(unclosed", replace /}
          {pattern "^/(.*)$", replace /$1a}
        )
      }
    )
  }
  b {
    listen 0.0.0.0:8080
    server_names (Cache.Example.com)
  }
  c {
    listen 0.0.0.0:9090
    server_names ()
  }
}
backends {}
"#;
    let files = Files::new("invalid", config, "");
    #[allow(clippy::invalid_regex, reason = "the config's broken pattern")]
    let invalid_regex_error = || regex::Regex::new("(unclosed").unwrap_err().to_string();
    let Err(ConfigError::Invalid(errors)) = files.load(&[]) else {
        panic!("expected validation errors");
    };
    assert_eq!(
        errors,
        vec![
            ValidationError::UnknownBackend {
                server: "a".into(),
                mapping: 0,
                backend: "missing".into(),
            },
            ValidationError::RelativePath {
                server: "a".into(),
                mapping: 0,
                path: "assets".into(),
            },
            ValidationError::InvalidRewrite {
                server: "a".into(),
                mapping: 0,
                rewrite: 0,
                error: invalid_regex_error(),
            },
            ValidationError::UnknownGroup {
                server: "a".into(),
                mapping: 0,
                rewrite: 1,
                reference: "$1a".into(),
            },
            ValidationError::NoServerNames { server: "c".into() },
            ValidationError::DuplicateHost {
                listen: "0.0.0.0:8080".parse().unwrap(),
                server_name: "cache.example.com".into(),
                servers: vec!["a".into(), "b".into()],
            },
        ]
    );
}

// r[verify config.server.listen]
#[test]
fn listen_must_be_an_address_and_port() {
    let config = CONFIG.replace(
        "listen 0.0.0.0:8080\n    server_names (cache.example.com cdn",
        "listen 8080\n    server_names (cache.example.com cdn",
    );
    let files = Files::new("listen", &config, SECRETS);
    let Err(ConfigError::Load(ConfigLoadError::Violations(violations))) = files.load(&[]) else {
        panic!("expected violations");
    };
    assert_eq!(violations.decode_errors().len(), 1);
}

#[test]
fn unknown_group_errors_suggest_braces() {
    let error = ValidationError::UnknownGroup {
        server: "a".into(),
        mapping: 0,
        rewrite: 1,
        reference: "$1a".into(),
    };
    assert_eq!(
        error.to_string(),
        "rewrite 1 of mapping 0 of server `a` refers to `$1a`, which its pattern has no \
         capture group for (did you mean `${1}a`?)"
    );
}

// r[verify config.cache]
// r[verify config.cache.chunks]
// r[verify config.cache.metadata-ttl]
// r[verify config.cache.stale-if-error]
// r[verify config.cache.stale-while-revalidate]
// r[verify config.cache.write-policy]
// r[verify config.cache.memory]
// r[verify config.cache.disk]
#[test]
fn cache_settings_default_when_left_out() {
    let files = Files::new("cache-defaults", CONFIG, SECRETS);
    let cache = files.load(&[]).unwrap().into_config().cache;
    assert_eq!(cache, hylla_config::Cache::default());
    assert_eq!(cache.chunk_size(), 1024 * 1024);
    assert_eq!(cache.metadata_ttl, Interval::from_secs(30));
    assert_eq!(cache.stale_while_revalidate, Interval::from_secs(0));
    assert_eq!(cache.stale_if_error, Interval::from_secs(24 * 60 * 60));
    assert_eq!(cache.write_policy, WritePolicy::OnEviction);
    assert_eq!(cache.memory.capacity_mib, 256);
    assert_eq!(cache.disk, None);

    let config = format!(
        "{CONFIG}cache {{
  metadata_ttl 5m
  stale_while_revalidate 1m
  stale_if_error 0s
  write_policy on_insertion
  memory {{capacity_mib 64}}
  disk {{
    path /var/cache/hylla
    capacity_mib 1024
  }}
}}
"
    );
    let files = Files::new("cache-settings", &config, SECRETS);
    let cache = files
        .load(&[("HYLLA__CACHE__CHUNK_SIZE_KIB", "512")])
        .unwrap()
        .into_config()
        .cache;
    assert_eq!(cache.chunk_size_kib, 512);
    assert_eq!(cache.metadata_ttl, Interval::from_secs(5 * 60));
    assert_eq!(cache.stale_while_revalidate, Interval::from_secs(60));
    assert_eq!(cache.stale_if_error, Interval::from_secs(0));
    assert_eq!(cache.write_policy, WritePolicy::OnInsertion);
    assert_eq!(cache.memory.capacity_mib, 64);
    assert_eq!(
        cache.disk,
        Some(DiskCache {
            path: "/var/cache/hylla".into(),
            capacity_mib: 1024,
            block_size_mib: 16,
        })
    );
}

// r[verify config.validate.cache]
#[test]
fn cache_sizes_are_validated() {
    let config = format!(
        "{CONFIG}cache {{
  chunk_size_kib 16384
  disk {{
    path /var/cache/hylla
    capacity_mib 32
  }}
}}
"
    );
    let files = Files::new("cache-invalid", &config, SECRETS);
    let Err(ConfigError::Invalid(errors)) = files.load(&[]) else {
        panic!("expected validation errors");
    };
    let reasons = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(reasons.len(), 2, "{reasons:#?}");
    assert!(reasons[0].contains("must be smaller than `disk.block_size_mib`"));
    assert!(reasons[1].contains("must hold at least 4 blocks"));
}

const REFRESH: &str = r#"
servers {public {listen 0.0.0.0:8080, server_names (cache.example.com), mappings ({path_prefix /, backend assets})}}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
    refresh {
      rules (
        {prefix css/, every 15m}
        {key index.html, every "1h 30m", mode both, max_objects 1}
        {prefix "", every 1d, mode disabled}
      )
    }
  }
}
"#;

// r[verify config.refresh]
// r[verify config.refresh.rules]
// r[verify config.refresh.modes]
// r[verify config.durations]
#[test]
fn backends_configure_refresh_rules() {
    let files = Files::new(
        "refresh",
        REFRESH,
        "backends {assets {access_key_id a, secret_access_key b}}",
    );
    let config = files
        .load(&[("HYLLA__BACKENDS__ASSETS__REFRESH__CONCURRENCY", "8")])
        .unwrap()
        .into_config();
    let refresh = config.backends["assets"].refresh().unwrap();
    assert_eq!(refresh.concurrency, 8);
    let minutes = |minutes: u64| Interval(std::time::Duration::from_secs(minutes * 60));
    assert_eq!(
        refresh.rules,
        vec![
            RefreshRule {
                prefix: Some("css/".into()),
                key: None,
                every: minutes(15),
                mode: RefreshMode::Revalidate,
                max_objects: None,
                max_mib: None,
            },
            RefreshRule {
                prefix: None,
                key: Some("index.html".into()),
                every: minutes(90),
                mode: RefreshMode::Both,
                max_objects: Some(1),
                max_mib: None,
            },
            RefreshRule {
                prefix: Some(String::new()),
                key: None,
                every: minutes(24 * 60),
                mode: RefreshMode::Disabled,
                max_objects: None,
                max_mib: None,
            },
        ]
    );
    assert!(RefreshMode::Both.revalidates() && RefreshMode::Both.preloads());
    assert!(!RefreshMode::Disabled.revalidates() && !RefreshMode::Disabled.preloads());

    // Durations must parse.
    let invalid = REFRESH.replace("every 15m", "every soon");
    let files = Files::new(
        "refresh-duration",
        &invalid,
        "backends {assets {access_key_id a, secret_access_key b}}",
    );
    let Err(ConfigError::Load(ConfigLoadError::Violations(violations))) = files.load(&[]) else {
        panic!("expected violations");
    };
    assert_eq!(violations.decode_errors().len(), 1);
}

// r[verify config.validate.refresh]
#[test]
fn refresh_rules_are_validated() {
    let config = "
servers {public {listen 0.0.0.0:8080, server_names (cache.example.com), mappings ({path_prefix /, backend assets})}}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
    refresh {
      concurrency 0
      rules (
        {prefix css/, key app.css, every 15m}
        {key /app.css, every 0s}
        {prefix css/, every 15m, mode preload}
      )
    }
  }
}
";
    let files = Files::new(
        "refresh-invalid",
        config,
        "backends {assets {access_key_id a, secret_access_key b}}",
    );
    let Err(ConfigError::Invalid(errors)) = files.load(&[]) else {
        panic!("expected validation errors");
    };
    let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        errors,
        vec![
            "refresh of backend `assets` is invalid: `concurrency` must be positive",
            "refresh rule 0 of backend `assets` is invalid: set exactly one of `prefix` and `key`",
            "refresh rule 1 of backend `assets` is invalid: keys and prefixes are backend keys, \
             which don't start with `/`",
            "refresh rule 1 of backend `assets` is invalid: `every` must be positive",
            "refresh rule 2 of backend `assets` is invalid: preloading needs `max_objects` or \
             `max_mib`, so it can't fill the cache with objects nobody reads",
        ]
    );
}

const AUTH: &str = r#"
auth {
  partners {
    algorithm ES256
    public_key "-----BEGIN PUBLIC KEY-----\nkey\n-----END PUBLIC KEY-----\n"
    audience hylla
  }
  internal {
    algorithm EdDSA
    public_key key
    leeway 1m
  }
}
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com)
    mappings (
      {
        path_prefix /tenant
        backend assets
        auth {verifier partners, paths ("/tenant/${sub}" /shared)}
      }
      {
        path_prefix /reports
        backend assets
        auth {verifier internal, private false}
      }
      {path_prefix /, backend assets}
    )
  }
}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
  }
}
"#;

// r[verify config.auth.verifiers]
// r[verify config.auth.algorithms]
// r[verify config.auth.mappings]
// r[verify config.auth.paths]
#[test]
fn mappings_configure_auth() {
    let files = Files::new(
        "auth",
        AUTH,
        "backends {assets {access_key_id a, secret_access_key b}}",
    );
    let config = files.load(&[]).unwrap().into_config();

    let partners = &config.auth["partners"];
    assert_eq!(partners.algorithm, JwtAlgorithm::Es256);
    assert!(
        partners
            .public_key
            .starts_with("-----BEGIN PUBLIC KEY-----\nkey\n")
    );
    assert_eq!(partners.audience.as_deref(), Some("hylla"));
    assert_eq!(partners.issuer, None);
    assert_eq!(partners.leeway, Interval::from_secs(30));
    let internal = &config.auth["internal"];
    assert_eq!(internal.algorithm, JwtAlgorithm::EdDsa);
    assert_eq!(internal.leeway, Interval::from_secs(60));

    let mappings = &config.servers["public"].mappings;
    assert_eq!(
        mappings[0].auth,
        Some(MappingAuth {
            verifier: "partners".into(),
            paths: vec!["/tenant/${sub}".into(), "/shared".into()],
            private: true,
        })
    );
    assert_eq!(
        mappings[1].auth,
        Some(MappingAuth {
            verifier: "internal".into(),
            paths: Vec::new(),
            private: false,
        })
    );
    assert_eq!(mappings[2].auth, None);

    // Algorithms are the asymmetric JWT ones only.
    let hmac = AUTH.replace("algorithm ES256", "algorithm HS256");
    let files = Files::new("auth-hmac", &hmac, "");
    assert!(matches!(
        files.load(&[]),
        Err(ConfigError::Load(ConfigLoadError::Violations(_)))
    ));
}

// r[verify config.validate.auth]
#[test]
fn mapping_auth_is_validated() {
    let config = AUTH
        .replace("verifier internal", "verifier missing")
        .replace(
            r#"paths ("/tenant/${sub}" /shared)"#,
            r#"paths ("tenant/${sub}" "/${a b}")"#,
        );
    let files = Files::new(
        "auth-invalid",
        &config,
        "backends {assets {access_key_id a, secret_access_key b}}",
    );
    let Err(ConfigError::Invalid(errors)) = files.load(&[]) else {
        panic!("expected validation errors");
    };
    let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(errors.len(), 3, "{errors:#?}");
    assert!(errors[0].contains("mapping 0") && errors[0].contains("must start with `/`"));
    assert!(errors[1].contains("mapping 0") && errors[1].contains("claim names may only contain"));
    assert!(errors[2].contains("mapping 1") && errors[2].contains("unknown verifier `missing`"));
}

const CLUSTER: &str = "
servers {public {listen 0.0.0.0:8080, server_names (cache.example.com), mappings ({path_prefix /, backend assets})}}
backends {assets {type s3, bucket assets, region eu-north-1}}
cluster {
  listen 0.0.0.0:7946
  advertise 10.0.0.5:7946
  seeds (10.0.0.6:7946 hylla.cache.svc.cluster.local:7946)
  gossip_interval 500ms
}
";

// r[verify config.cluster]
// r[verify config.cluster.seeds]
// r[verify config.cluster.key]
// r[verify config.cluster.share]
#[test]
fn clusters_are_configured_with_a_secret_key() {
    let secrets = &format!(
        r#"cluster {{key "a cluster key of at least thirty-two characters"}}
{MINIMAL_SECRETS}"#
    );
    let files = Files::new("cluster", CLUSTER, secrets);
    let cluster = files.load(&[]).unwrap().into_config().cluster.unwrap();
    assert_eq!(cluster.listen, "0.0.0.0:7946".parse().unwrap());
    assert_eq!(cluster.advertise, Some("10.0.0.5:7946".parse().unwrap()));
    assert_eq!(
        cluster.seeds,
        vec!["10.0.0.6:7946", "hylla.cache.svc.cluster.local:7946"]
    );
    assert_eq!(
        cluster.gossip_interval,
        Interval(std::time::Duration::from_millis(500))
    );
    assert_eq!(cluster.failure_timeout, Interval::from_secs(10));
    assert_eq!(
        cluster.key,
        "a cluster key of at least thirty-two characters"
    );
    assert!(cluster.share.metadata && cluster.share.preload);

    // The key is secret: the public file can't set it.
    let public = CLUSTER.replace(
        "gossip_interval 500ms",
        "gossip_interval 500ms\n  key \"a cluster key of at least thirty-two characters\"",
    );
    let files = Files::new("cluster-public-key", &public, secrets);
    let Err(ConfigError::Load(ConfigLoadError::Violations(violations))) = files.load(&[]) else {
        panic!("expected violations");
    };
    assert_eq!(
        violations.layer_mismatches()[0].path.to_string(),
        "cluster.key"
    );
}

// r[verify config.validate.cluster]
#[test]
fn cluster_settings_are_validated() {
    let config = "
servers {public {listen 0.0.0.0:8080, server_names (cache.example.com), mappings ({path_prefix /, backend assets})}}
backends {assets {type s3, bucket assets, region eu-north-1}}
cluster {
  listen 0.0.0.0:7946
  seeds (10.0.0.6 :7946)
  gossip_interval 10s
}
";
    let files = Files::new(
        "cluster-invalid",
        config,
        &format!("cluster {{key short}}\n{MINIMAL_SECRETS}"),
    );
    let Err(ConfigError::Invalid(errors)) = files.load(&[]) else {
        panic!("expected validation errors");
    };
    let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        errors,
        vec![
            "invalid cluster config: `listen` is 0.0.0.0:7946, so set `advertise` to the \
             address other nodes reach this one at",
            "invalid cluster config: `key` must be at least 32 characters",
            "invalid cluster config: seed `10.0.0.6` must be `host:port`",
            "invalid cluster config: seed `:7946` must be `host:port`",
            "invalid cluster config: `failure_timeout` must be longer than `gossip_interval`",
        ]
    );
}

const TELEMETRY: &str = r#"
servers {public {listen 0.0.0.0:8080, server_names (cache.example.com), mappings ({path_prefix /, backend assets})}}
backends {assets {type s3, bucket assets, region eu-north-1}}
telemetry {
  resource ({key deployment.environment, value production})
  otlp {
    endpoint http://collector:4318
  }
  metrics {
    enabled true
    endpoint http://prometheus:9090/api/v1/otlp/v1/metrics
    interval 15s
  }
  traces {
    enabled true
    endpoint http://quickwit:7280/api/v1/otlp/v1/traces
    sample_ratio 0.25
  }
  logs {
    level "info,hylla_server=debug"
    format json
    export true
  }
}
"#;

// r[verify config.telemetry]
// r[verify config.telemetry.otlp]
// r[verify config.telemetry.metrics]
// r[verify config.telemetry.traces]
// r[verify config.telemetry.logs]
#[test]
fn telemetry_is_configured_with_secret_headers() {
    let files = Files::new("telemetry-defaults", MINIMAL, MINIMAL_SECRETS);
    let telemetry = files.load(&[]).unwrap().into_config().telemetry;
    assert_eq!(telemetry, hylla_config::Telemetry::default());
    assert_eq!(telemetry.service_name, "hylla");
    assert!(!telemetry.metrics.enabled);
    assert_eq!(telemetry.otlp.protocol, OtlpProtocol::HttpProtobuf);
    assert!(!telemetry.traces.enabled);
    assert_eq!(telemetry.traces.sample_ratio, 1.0);
    assert!(!telemetry.traces.record_paths);
    assert_eq!(telemetry.logs.level, "info");
    assert_eq!(telemetry.logs.format, hylla_config::LogFormat::Text);
    assert!(!telemetry.logs.export);

    let secrets = &format!(
        r#"telemetry {{otlp {{headers {{authorization "Bearer token"}}}}}}
{MINIMAL_SECRETS}"#
    );
    let files = Files::new("telemetry", TELEMETRY, secrets);
    let telemetry = files
        .load(&[("HYLLA__TELEMETRY__OTLP__PROTOCOL", "grpc")])
        .unwrap()
        .into_config()
        .telemetry;
    assert_eq!(
        telemetry.resource,
        vec![hylla_config::ResourceAttribute {
            key: "deployment.environment".into(),
            value: "production".into(),
        }]
    );
    assert_eq!(
        telemetry.otlp.endpoint.as_deref(),
        Some("http://collector:4318")
    );
    assert_eq!(telemetry.otlp.protocol, OtlpProtocol::Grpc);
    assert_eq!(telemetry.otlp.headers["authorization"], "Bearer token");
    assert_eq!(telemetry.otlp.timeout, Interval::from_secs(10));
    assert!(telemetry.metrics.enabled);
    assert_eq!(
        telemetry.metrics.endpoint.as_deref(),
        Some("http://prometheus:9090/api/v1/otlp/v1/metrics")
    );
    assert_eq!(telemetry.metrics.interval, Interval::from_secs(15));
    assert!(telemetry.traces.enabled);
    assert_eq!(
        telemetry.traces.endpoint.as_deref(),
        Some("http://quickwit:7280/api/v1/otlp/v1/traces")
    );
    assert_eq!(telemetry.traces.sample_ratio, 0.25);
    assert_eq!(telemetry.logs.level, "info,hylla_server=debug");
    assert_eq!(telemetry.logs.format, hylla_config::LogFormat::Json);
    assert!(telemetry.logs.export);
    assert_eq!(telemetry.logs.endpoint, None);

    // Headers are secret: the public file can't set them.
    let public = TELEMETRY.replace(
        "endpoint http://collector:4318",
        "headers {authorization leaked}",
    );
    let files = Files::new("telemetry-public-headers", &public, "");
    let Err(ConfigError::Load(ConfigLoadError::Violations(violations))) = files.load(&[]) else {
        panic!("expected violations");
    };
    assert_eq!(
        violations.layer_mismatches()[0].path.to_string(),
        "telemetry.otlp.headers.authorization"
    );
}

// r[verify config.flags]
#[test]
fn flags_override_the_config_file_and_environment() {
    let config = format!("{MINIMAL}telemetry {{logs {{level warn}}}}\n");
    let files = Files::new("flags", &config, MINIMAL_SECRETS);
    let env = [("HYLLA__TELEMETRY__LOGS__LEVEL", "info")];
    let loaded = files
        .load_with_flags(&env, &[("telemetry.logs.level", "debug")])
        .unwrap();
    assert_eq!(loaded.config().telemetry.logs.level, "debug");
    assert_eq!(
        loaded
            .source(&path("telemetry.logs.level"))
            .unwrap()
            .winner(),
        &ConfigValueSource::Direct(hylla_config::Origin::Flag {
            path: path("telemetry.logs.level"),
        })
    );

    // Without the flag, the environment wins over the file.
    let loaded = files.load(&env).unwrap();
    assert_eq!(loaded.config().telemetry.logs.level, "info");
}

/// The ```styx block of the README starting with `first_line`.
fn readme_block(first_line: &str) -> String {
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md")).unwrap();
    let start = readme
        .find(&format!("```styx\n{first_line}\n"))
        .unwrap_or_else(|| panic!("no README block starting with `{first_line}`"))
        + "```styx\n".len();
    let end = start + readme[start..].find("```").unwrap();
    readme[start..end].to_string()
}

// r[verify config.sources]
#[test]
fn the_readme_example_loads() {
    let files = Files::new(
        "readme",
        &readme_block("// config.styx"),
        &readme_block("// secrets.styx"),
    );
    let config = files.load(&[]).unwrap().into_config();
    assert_eq!(config.servers["public"].mappings.len(), 3);
    assert_eq!(config.backends["assets"].refresh().unwrap().rules.len(), 2);
    assert!(
        config.auth["partners"]
            .public_key
            .starts_with("-----BEGIN PUBLIC KEY-----\n")
    );
}

const SHOW: &str = "
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com)
    mappings ({path_prefix /, backend assets})
  }
}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
  }
}
cache {
  metadata_ttl 5m
}
";

fn shown() -> hylla_config::Loaded {
    let secrets = "backends {assets {access_key_id AKIA123, secret_access_key s3cr3t}}";
    let files = Files::new("show", SHOW, secrets);
    files
        .load_with_flags(
            &[
                ("HYLLA__CACHE__METADATA_TTL", "1m"),
                ("HYLLA__CACHE__CHUNK_SIZE_KIB", "512"),
            ],
            &[("cache.metadata_ttl", "30s")],
        )
        .unwrap()
}

// r[verify config.show]
#[test]
fn show_prints_the_effective_config_with_sources() {
    let loaded = shown();
    let config = loaded_file(&loaded, "config.styx");
    let secrets = loaded_file(&loaded, "secrets.styx");
    let shown = hylla_config::render(&loaded.into());
    for expected in [
        "    listen 0.0.0.0:8080".to_string(),
        format!("// {config}"),
        "    access_key_id \"<redacted>\"".to_string(),
        format!("// {secrets}"),
        "  metadata_ttl 30s".to_string(),
        format!("// a command line flag, overriding env HYLLA__CACHE__METADATA_TTL, {config}"),
        "  chunk_size_kib 512".to_string(),
        "// env HYLLA__CACHE__CHUNK_SIZE_KIB".to_string(),
        "  stale_if_error 1day".to_string(),
        "// default".to_string(),
    ] {
        assert!(
            shown.contains(&expected),
            "`{expected}` missing from:\n{shown}"
        );
    }
    // Secrets never show, and optional settings left out don't either.
    assert!(
        !shown.contains("AKIA123") && !shown.contains("s3cr3t"),
        "{shown}"
    );
    assert!(!shown.contains("endpoint"), "{shown}");
    // It's Styx, comments and all.
    facet_styx::from_str::<facet_value::Value>(&shown).unwrap();
}

/// The path of the test's `name` file, as shown.
fn loaded_file(loaded: &hylla_config::Loaded, name: &str) -> String {
    let source = loaded.source(&path("servers.public.listen")).unwrap();
    let ConfigValueSource::Direct(Origin::File { file, .. }) = source.winner() else {
        panic!("expected a file");
    };
    file.with_file_name(name).display().to_string()
}

// r[verify config.explain]
#[test]
fn explain_shows_the_shadow_chain() {
    let loaded = shown();
    let config = loaded_file(&loaded, "config.styx");
    let loaded = hylla_config::Inspected::from(loaded);
    assert_eq!(
        hylla_config::explain(&loaded, "cache.metadata_ttl").unwrap(),
        format!(
            "cache.metadata_ttl 30s
  from a command line flag
  overriding env HYLLA__CACHE__METADATA_TTL
  overriding {config}
"
        )
    );
    // A section explains every field under it, defaults too.
    let memory = hylla_config::explain(&loaded, "cache.memory").unwrap();
    assert_eq!(
        memory,
        "cache.memory.capacity_mib 256\n  from the default\n"
    );
    // Secrets are redacted.
    assert!(
        hylla_config::explain(&loaded, "backends.assets.secret_access_key")
            .unwrap()
            .starts_with("backends.assets.secret_access_key \"<redacted>\"\n")
    );
    // Known fields left out, and unknown ones.
    assert_eq!(
        hylla_config::explain(&loaded, "backends.assets.endpoint").unwrap(),
        "backends.assets.endpoint is not set\n"
    );
    assert_eq!(
        hylla_config::explain(&loaded, "telemetry.otlp.headers").unwrap(),
        "telemetry.otlp.headers is not set\n"
    );
    assert_eq!(
        hylla_config::explain(&loaded, "cache.nope"),
        Err(hylla_config::ExplainError::UnknownField(
            "cache.nope".into()
        ))
    );
    assert_eq!(
        hylla_config::explain(&loaded, "cache..ttl"),
        Err(hylla_config::ExplainError::InvalidPath("cache..ttl".into()))
    );
}

// r[verify config.show]
#[test]
fn the_readme_example_shows_as_styx() {
    let files = Files::new(
        "readme-show",
        &readme_block("// config.styx"),
        &readme_block("// secrets.styx"),
    );
    let shown = hylla_config::render(&files.load(&[]).unwrap().into());
    facet_styx::from_str::<facet_value::Value>(&shown).unwrap();
    assert!(
        shown.contains("public_key \"-----BEGIN PUBLIC KEY-----\\n"),
        "{shown}"
    );
}

const INCOMPLETE: &str = "
servers {
  public {
    listen 0.0.0.0:8080
    server_names (cache.example.com)
    mappings (
      {path_prefix /assets, backend assets}
      {backend assets}
    )
  }
}
backends {
  assets {
    type s3
    bucket assets
    region eu-north-1
  }
}
cluster {
  listen 10.0.0.1:7000
}
";

// r[verify config.show.missing]
#[test]
fn show_marks_the_missing_fields_of_a_config_that_does_not_load() {
    let files = Files::new("show-missing", INCOMPLETE, "");
    let inspected = files.inspect(false);
    let shown = hylla_config::render(&inspected);
    for expected in [
        "    bucket assets",
        "    access_key_id \"<missing>\"      // missing: required, but not set",
        "    secret_access_key \"<missing>\"  // missing: required, but not set",
        "  key \"<missing>\"                 // missing: required, but not set",
        // Items of lists are marked on their line.
        "      {path_prefix \"<missing>\", backend assets, rewrites ()}  // missing: path_prefix",
        // Defaults are filled in, in the order a loaded config shows them.
        "  seeds ()                        // default\n  gossip_interval 1s",
    ] {
        assert!(
            shown.contains(expected),
            "`{expected}` missing from:\n{shown}"
        );
    }
    facet_styx::from_str::<facet_value::Value>(&shown).unwrap();

    let error = inspected.into_loaded().unwrap_err().to_string();
    assert_eq!(
        error,
        "failed to load the config:
- missing required field `servers.public.mappings.1.path_prefix`
- missing required field `backends.assets.access_key_id`
- missing required field `backends.assets.secret_access_key`
- missing required field `cluster.key`"
    );
}

// r[verify config.show.missing]
#[test]
fn explain_says_which_fields_are_missing() {
    let files = Files::new("explain-missing", INCOMPLETE, "");
    let inspected = files.inspect(false);
    let explained = hylla_config::explain(&inspected, "backends.assets").unwrap();
    assert!(
        explained.ends_with(
            "backends.assets.access_key_id is missing: required, but not set
backends.assets.secret_access_key is missing: required, but not set
"
        ),
        "{explained}"
    );
    assert_eq!(
        hylla_config::explain(&inspected, "servers.public.mappings.1.path_prefix").unwrap(),
        "servers.public.mappings.1.path_prefix is missing: required, but not set\n"
    );
}

// r[verify config.show.missing]
#[test]
fn show_prints_an_invalid_config_before_failing() {
    let config = CONFIG.replace("backend archive", "backend nope");
    let files = Files::new("show-invalid", &config, SECRETS);
    let inspected = files.inspect(true);
    let shown = hylla_config::render(&inspected);
    assert!(shown.contains("backend nope"), "{shown}");
    assert!(!shown.contains("<missing>"), "{shown}");
    let Err(ConfigError::Invalid(errors)) = inspected.into_loaded() else {
        panic!("expected validation errors");
    };
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, ValidationError::UnknownBackend { backend, .. } if backend == "nope")),
        "{errors:?}"
    );
}

// r[verify config.file]
#[test]
fn a_required_config_file_must_exist_and_an_optional_one_may_not() {
    let files = Files::new("config-file", "", "");
    let missing = files.dir.join("nope.styx");
    let inspect = |config_required| {
        hylla_config::inspect_with_env(
            &ConfigFiles {
                config: missing.clone(),
                config_required,
                secrets: None,
                flags: Default::default(),
            },
            EnvSource::from_vars([("HYLLA__CACHE__METADATA_TTL", "1m")]).prefix("HYLLA"),
        )
    };

    let error = inspect(true).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("failed to read `{}`: ", missing.display())),
        "{error}"
    );

    // Without the file, the config is loaded from the other sources. Without a server
    // it's invalid, but still shows.
    let hylla_config::Inspected::Loaded { loaded, invalid } = inspect(false).unwrap() else {
        panic!("expected the config to load");
    };
    assert_eq!(invalid, [ValidationError::NoMappings]);
    assert_eq!(loaded.config().cache.metadata_ttl, Interval::from_secs(60));
}

// r[verify config.validate.mappings]
#[test]
fn a_config_must_map_requests_to_a_backend() {
    let config = "
servers {public {listen 0.0.0.0:8080, server_names (cache.example.com)}}
backends {assets {type s3, bucket assets, region eu-north-1}}
";
    let files = Files::new("no-mappings", config, MINIMAL_SECRETS);
    let Err(ConfigError::Invalid(errors)) = files.load(&[]) else {
        panic!("expected validation errors");
    };
    assert_eq!(errors, [ValidationError::NoMappings]);

    // It still shows, failing afterwards.
    let inspected = files.inspect(true);
    let shown = hylla_config::render(&inspected);
    assert!(shown.contains("    listen 0.0.0.0:8080"), "{shown}");
    assert_eq!(
        inspected.into_loaded().unwrap_err().to_string(),
        "invalid config:\n- no server maps requests to a backend: configure a server with at \
         least one mapping"
    );

    // One server with a mapping is enough.
    let files = Files::new("one-mapping", MINIMAL, MINIMAL_SECRETS);
    files.load(&[]).unwrap();
}
