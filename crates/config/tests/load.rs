use std::path::PathBuf;

use hylla_config::{
    Backend, ConfigError, ConfigFiles, Mapping, Origin, Rewrite, Route, Router, ValidationError,
    load_with_env,
};
use ordna_core::{ConfigLoadError, ConfigPath, ConfigValueSource};
use source_env::EnvSource;

const CONFIG: &str = "\
servers:
  public:
    listen: 0.0.0.0:8080
    server_names: [cache.example.com, cdn.example.com]
    mappings:
      - path_prefix: /assets
        backend: assets
        rewrites:
          - pattern: ^/assets/(.*)$
            replace: /$1
      - path_prefix: /
        backend: archive
  internal:
    listen: 0.0.0.0:8080
    server_names: [cache.internal]
    mappings:
      - path_prefix: /
        backend: assets
backends:
  assets:
    type: s3
    bucket: assets
    region: eu-north-1
  archive:
    type: swift
    auth_url: https://swift.example.com/v3
    container: archive
    username: hylla
";

const SECRETS: &str = "\
backends:
  assets:
    access_key_id: AKIA123
    secret_access_key: s3cr3t
  archive:
    password: hunter2
";

struct Files {
    dir: PathBuf,
}

impl Files {
    fn new(name: &str, config: &str, secrets: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hylla-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), config).unwrap();
        std::fs::write(dir.join("secrets.yaml"), secrets).unwrap();
        Self { dir }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config.yaml")
    }

    fn secrets(&self) -> PathBuf {
        self.dir.join("secrets.yaml")
    }

    fn load(
        &self,
        env: &[(&'static str, &'static str)],
    ) -> Result<hylla_config::Loaded, ConfigError> {
        load_with_env(
            &ConfigFiles {
                config: self.config(),
                secrets: Some(self.secrets()),
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
            },
            Mapping {
                path_prefix: "/".into(),
                backend: "archive".into(),
                rewrites: Vec::new(),
            },
        ]
    );
    let router = Router::new(public).unwrap();
    assert_eq!(
        router.route("/assets/app.css"),
        Some(Route {
            backend: "assets",
            path: "/app.css".into(),
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
        }
    );
    assert_eq!(
        config.backends["archive"],
        Backend::Swift {
            auth_url: "https://swift.example.com/v3".into(),
            container: "archive".into(),
            region: None,
            username: "hylla".into(),
            password: "hunter2".into(),
        }
    );
}

// r[verify config.backends.secrets]
#[test]
fn backend_credentials_are_secret() {
    let config = format!("{CONFIG}    password: leaked\n");
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
            "backends.archive.password",
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
    let config = "\
servers:
  a:
    listen: 0.0.0.0:8080
    server_names: [cache.example.com]
    mappings:
      - path_prefix: assets
        backend: missing
        rewrites:
          - pattern: (unclosed
            replace: /
          - pattern: ^/(.*)$
            replace: /$1a
  b:
    listen: 0.0.0.0:8080
    server_names: [Cache.Example.com]
  c:
    listen: 0.0.0.0:9090
    server_names: []
backends: {}
";
    let files = Files::new("invalid", config, "{}");
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
                error: regex::Regex::new("(unclosed").unwrap_err().to_string(),
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
        "listen: 0.0.0.0:8080\n    server_names: [cache.example.com, cdn",
        "listen: 8080\n    server_names: [cache.example.com, cdn",
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
