mod common;

use std::{collections::HashMap, sync::Arc};

use axum::body::{Body, to_bytes};
use common::Flaky;
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Cache, Config, Control, ControlUser, Interval, JwtAlgorithm, Mapping, MappingAuth,
    Refresh, RefreshAlias, RefreshMode, RefreshRule, Server, Site, ValidationError, Verifier,
};
use hylla_server::App;
use object_store::{
    Attribute, AttributeValue, Attributes, ObjectStore, PutOptions, memory::InMemory, path::Path,
};
use tower::ServiceExt;

const LISTEN: &str = "127.0.0.1:8080";
const PASSWORD: &str = "correct horse battery staple";

/// The build `pr1873` points at.
const PR: &str = "1111111111111111111111111111111111111111";
/// The build `live` points at.
const LIVE: &str = "2222222222222222222222222222222222222222";
/// A build without an `index.html`.
const BARE: &str = "3333333333333333333333333333333333333333";
/// A build nothing was uploaded for.
const GONE: &str = "4444444444444444444444444444444444444444";

fn site(alias: Option<&str>) -> Site {
    Site {
        builds: "builds/".into(),
        aliases: "aliases/".into(),
        alias: alias.map(Into::into),
        index: "index.html".into(),
        fallback: Some("index.html".into()),
        redirect: true,
        alias_cache_control: "no-cache".into(),
    }
}

fn mapping(site: Option<Site>, writable: bool) -> Mapping {
    Mapping {
        path_prefix: "/".into(),
        backend: "frontend".into(),
        rewrites: Vec::new(),
        cache_control: None,
        writable,
        auth: None,
        konklav: None,
        site,
    }
}

fn server(names: &[&str], mappings: Vec<Mapping>) -> Server {
    Server {
        listen: LISTEN.parse().unwrap(),
        server_names: names.iter().map(|name| (*name).into()).collect(),
        mappings,
    }
}

/// Previews on `*.preview.example.com`, the `live` alias on `www.example.com`, an exact
/// `www.preview.example.com` that isn't a site, and `admin.internal` writing the bucket.
fn config(metadata_ttl: u64) -> Config {
    Config {
        servers: HashMap::from([
            (
                "previews".into(),
                server(
                    &["*.preview.example.com"],
                    vec![mapping(Some(site(None)), false)],
                ),
            ),
            (
                "live".into(),
                server(
                    &["www.example.com"],
                    vec![mapping(Some(site(Some("live"))), false)],
                ),
            ),
            (
                "exact".into(),
                server(&["www.preview.example.com"], vec![mapping(None, false)]),
            ),
            (
                "admin".into(),
                server(&["admin.internal"], vec![mapping(None, true)]),
            ),
        ]),
        backends: HashMap::from([(
            "frontend".into(),
            Backend::S3 {
                bucket: "frontend".into(),
                region: "eu-north-1".into(),
                endpoint: None,
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
                timeouts: Default::default(),
                refresh: None,
            },
        )]),
        cache: Cache {
            metadata_ttl: Interval::from_secs(metadata_ttl),
            ..Cache::default()
        },
        auth: HashMap::new(),
        konklav: None,
        telemetry: Default::default(),
        control: Some(Control {
            listen: "127.0.0.1:9090".parse().unwrap(),
            users: HashMap::from([(
                "admin".into(),
                ControlUser {
                    password_hash: hash(PASSWORD),
                },
            )]),
            max_upload_mib: 32,
            oidc: None,
        }),
        serve_backends: None,
        health: None,
        http: Default::default(),
        cluster: None,
    }
}

fn hash(password: &str) -> String {
    use argon2::{Argon2, PasswordHasher};
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), b"a salt for tests")
        .unwrap()
        .to_string()
}

async fn put(store: &InMemory, key: &str, body: &str, content_type: &str) {
    let attributes = Attributes::from_iter([
        (
            Attribute::ContentType,
            AttributeValue::from(content_type.to_string()),
        ),
        (
            Attribute::CacheControl,
            AttributeValue::from("public, max-age=31536000, immutable"),
        ),
    ]);
    store
        .put_opts(
            &Path::from(key),
            body.to_string().into(),
            PutOptions {
                attributes,
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

async fn app(metadata_ttl: u64) -> (Arc<App>, Arc<InMemory>, Arc<Flaky>) {
    app_with(config(metadata_ttl)).await
}

/// An app with `config`, in front of the builds and aliases of the tests.
async fn app_with(config: Config) -> (Arc<App>, Arc<InMemory>, Arc<Flaky>) {
    let memory = Arc::new(InMemory::new());
    for (sha, page) in [(PR, "pr"), (LIVE, "live")] {
        put(
            &memory,
            &format!("builds/{sha}/index.html"),
            page,
            "text/html",
        )
        .await;
        put(
            &memory,
            &format!("builds/{sha}/assets/app.js"),
            &format!("{page}.js"),
            "text/javascript",
        )
        .await;
    }
    put(
        &memory,
        &format!("builds/{BARE}/app.js"),
        "bare",
        "text/javascript",
    )
    .await;
    for (alias, body) in [
        ("pr1873", format!("{PR}\n")),
        ("live", LIVE.to_string()),
        ("gone", GONE.to_string()),
        ("broken", "not a sha".to_string()),
        ("huge", format!("{PR}{}", " ".repeat(2000))),
    ] {
        put(&memory, &format!("aliases/{alias}"), &body, "text/plain").await;
    }
    let flaky = Flaky::new(memory.clone());
    config.validate().unwrap();
    let stores = HashMap::from([(
        "frontend".to_string(),
        flaky.clone() as Arc<dyn ObjectStore>,
    )]);
    (App::new(&config, stores).await.unwrap(), memory, flaky)
}

struct Response {
    status: StatusCode,
    headers: http::HeaderMap,
    body: String,
}

impl Response {
    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(&name)
            .map(|value| value.to_str().unwrap())
            .unwrap_or_default()
    }
}

async fn get(app: &Arc<App>, host: &str, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut request = Request::builder().uri(path).header(header::HOST, host);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .listener(LISTEN.parse().unwrap())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Response {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn preview(label: &str) -> String {
    format!("{label}.preview.example.com")
}

// r[verify site.label]
// r[verify site.keys]
// r[verify config.site]
// r[verify config.site.index]
// r[verify site.hosts.wildcard]
#[tokio::test]
async fn sha_hosts_serve_their_build() {
    let (app, _, _) = app(60).await;
    let page = get(&app, &preview(PR), "/", &[]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.body, "pr");
    assert_eq!(page.header(header::CONTENT_TYPE), "text/html");
    let script = get(&app, &preview(LIVE), "/assets/app.js", &[]).await;
    assert_eq!(script.body, "live.js");
    // Hosts are matched without case.
    let upper = get(&app, &preview(PR).to_uppercase(), "/", &[]).await;
    assert_eq!(upper.body, "pr");
}

// r[verify site.hosts.wildcard]
// r[verify site.hosts.precedence]
#[tokio::test]
async fn wildcards_match_one_label_after_exact_names() {
    let (app, memory, _) = app(60).await;
    put(&memory, "readme.txt", "exact", "text/plain").await;
    // The exact name wins over the wildcard covering it.
    let exact = get(&app, "www.preview.example.com", "/readme.txt", &[]).await;
    assert_eq!(exact.body, "exact");
    for host in [
        "preview.example.com".to_string(),
        format!("a.{}", preview(PR)),
        ".preview.example.com".to_string(),
    ] {
        let response = get(&app, &host, "/", &[]).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{host}");
        assert_eq!(response.body, "unknown host\n", "{host}");
    }
}

// r[verify site.redirect]
// r[verify site.redirect.no-store]
// r[verify site.alias-object]
// r[verify config.site.redirect]
#[tokio::test]
async fn aliases_in_the_host_redirect_to_the_sha_host() {
    let (app, _, _) = app(60).await;
    let host = format!("{}:8443", preview("pr1873"));
    let response = get(&app, &host, "/settings/a%20b?tab=2", &[]).await;
    assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.header(header::LOCATION),
        format!("//{PR}.preview.example.com:8443/settings/a%20b?tab=2")
    );
    assert_eq!(response.header(header::CACHE_CONTROL), "no-store");
}

// r[verify site.redirect.missing-build]
#[tokio::test]
async fn aliases_redirect_to_builds_that_dont_exist() {
    let (app, _, _) = app(60).await;
    let response = get(&app, &preview("gone"), "/", &[]).await;
    assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
    let missing = get(&app, &preview(GONE), "/", &[]).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

// r[verify site.alias-missing]
// r[verify site.alias-names]
#[tokio::test]
async fn unknown_and_invalid_aliases_are_not_found() {
    let (app, _, flaky) = app(60).await;
    let response = get(&app, &preview("pr9999"), "/", &[]).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    let before = flaky.requests();
    for label in ["-pr", "pr_1", &"a".repeat(64)] {
        let response = get(&app, &preview(label), "/", &[]).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{label}");
    }
    // A 64 character hex label is a SHA, not an alias: its build is looked up instead.
    assert_eq!(flaky.requests(), before + 1);
}

// r[verify site.alias-invalid]
#[tokio::test]
async fn aliases_that_arent_a_sha_fail() {
    let (app, _, _) = app(60).await;
    for alias in ["broken", "huge"] {
        let response = get(&app, &preview(alias), "/", &[]).await;
        assert_eq!(response.status, StatusCode::BAD_GATEWAY, "{alias}");
    }
}

// r[verify config.site.alias]
// r[verify site.alias-cache-control]
// r[verify config.site.alias-cache-control]
#[tokio::test]
async fn fixed_aliases_are_served_without_redirecting() {
    let (app, _, _) = app(60).await;
    let page = get(&app, "www.example.com", "/", &[]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.body, "live");
    // The same URL serves another build once the alias moves.
    assert_eq!(page.header(header::CACHE_CONTROL), "no-cache");
    // SHA hosts keep the object's own.
    let pinned = get(&app, &preview(LIVE), "/", &[]).await;
    assert_eq!(
        pinned.header(header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
}

// r[verify site.fallback]
// r[verify site.fallback.missing]
// r[verify config.site.fallback]
#[tokio::test]
async fn page_loads_of_missing_paths_fall_back() {
    let (app, _, _) = app(60).await;
    let host = preview(PR);
    for headers in [
        &[("sec-fetch-mode", "navigate")][..],
        &[("accept", "text/html,*/*;q=0.8")][..],
    ] {
        let page = get(&app, &host, "/settings/profile", headers).await;
        assert_eq!(page.status, StatusCode::OK, "{headers:?}");
        assert_eq!(page.body, "pr");
    }
    // A missing script isn't answered with a page.
    let script = get(
        &app,
        &host,
        "/assets/missing.js",
        &[("sec-fetch-mode", "no-cors"), ("accept", "text/html")],
    )
    .await;
    assert_eq!(script.status, StatusCode::NOT_FOUND);
    // A build without the fallback answers 404.
    let bare = get(
        &app,
        &preview(BARE),
        "/x",
        &[("sec-fetch-mode", "navigate")],
    )
    .await;
    assert_eq!(bare.status, StatusCode::NOT_FOUND);
}

// r[verify site.conditional]
#[tokio::test]
async fn conditional_requests_go_by_the_served_object() {
    let (app, _, _) = app(60).await;
    let page = get(&app, "www.example.com", "/", &[]).await;
    let etag = page.header(header::ETAG).to_string();
    let again = get(&app, "www.example.com", "/", &[("if-none-match", &etag)]).await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    let range = get(
        &app,
        &preview(PR),
        "/assets/app.js",
        &[("range", "bytes=0-1")],
    )
    .await;
    assert_eq!(range.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.body, "pr");
}

// r[verify site.alias-cached]
#[tokio::test]
async fn aliases_are_read_again_only_when_they_change() {
    let (app, memory, flaky) = app(0).await;
    // The build it moves to is cached already.
    get(&app, &preview(PR), "/", &[]).await;
    get(&app, "www.example.com", "/", &[]).await;
    let before = flaky.requests();
    // The alias's metadata is checked, its body isn't read again, and the page is served.
    get(&app, "www.example.com", "/", &[]).await;
    let unchanged = flaky.requests() - before;

    put(&memory, "aliases/live", PR, "text/plain").await;
    let before = flaky.requests();
    let moved = get(&app, "www.example.com", "/", &[]).await;
    assert_eq!(moved.body, "pr");
    assert_eq!(flaky.requests() - before, unchanged + 1);
}

// r[verify site.alias-stale]
#[tokio::test]
async fn aliases_are_served_stale_through_outages() {
    let (app, _, flaky) = app(1).await;
    assert_eq!(get(&app, "www.example.com", "/", &[]).await.body, "live");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    flaky.fail(true);
    let page = get(&app, "www.example.com", "/", &[]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.body, "live");
}

async fn control(app: &Arc<App>, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    use base64::Engine;
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("admin:{PASSWORD}"));
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:9090")
        .header(header::AUTHORIZATION, format!("Basic {credentials}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app
        .control_router()
        .unwrap()
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

// r[verify site.browse]
// r[verify site.alias-object]
#[tokio::test]
async fn aliases_move_at_once_through_the_control_plane() {
    let (app, _, _) = app(3600).await;
    assert_eq!(get(&app, "www.example.com", "/", &[]).await.body, "live");

    let (status, body) = control(&app, "GET", "/v1/mappings/live/0/list", "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) =
        control(&app, "PUT", "/v1/mappings/admin/0/objects/aliases/live", PR).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // Forgotten, so it's read again within the hour its metadata would be trusted.
    assert_eq!(get(&app, "www.example.com", "/", &[]).await.body, "pr");
}

fn invalid(config: &Config) -> Vec<String> {
    config
        .validate()
        .err()
        .unwrap_or_default()
        .iter()
        .map(ValidationError::to_string)
        .collect()
}

// r[verify config.validate.site+2]
#[test]
fn sites_are_validated() {
    let with_site = |site: Site, names: &[&str]| {
        let mut config = config(60);
        config.servers.insert(
            "bad".into(),
            server(names, vec![mapping(Some(site), false)]),
        );
        config
    };
    assert!(
        invalid(&config(60)).is_empty(),
        "{:?}",
        invalid(&config(60))
    );

    let mut rewritten = mapping(Some(site(Some("live"))), true);
    rewritten.rewrites = vec![hylla_config::Rewrite {
        pattern: "^/(.*)$".into(),
        replace: "/$1".into(),
    }];
    let mut config_with = config(60);
    config_with
        .servers
        .insert("bad".into(), server(&["bad.example.com"], vec![rewritten]));
    let errors = invalid(&config_with);
    assert!(
        errors.iter().any(|error| error.contains("rewrites")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("writable")),
        "{errors:?}"
    );

    let cases = [
        (site(None), "wildcard server name"),
        (site(Some("Live")), "isn't an alias name"),
        (site(Some(PR)), "isn't an alias name"),
        (
            Site {
                builds: "/builds/".into(),
                ..site(Some("live"))
            },
            "`builds` must be a key prefix",
        ),
        (
            Site {
                aliases: "aliases".into(),
                ..site(Some("live"))
            },
            "`aliases` must be a key prefix",
        ),
        (
            Site {
                aliases: "builds/aliases/".into(),
                ..site(Some("live"))
            },
            "can't overlap",
        ),
        (
            Site {
                fallback: Some("/index.html".into()),
                ..site(Some("live"))
            },
            "`fallback` must be a key within a build",
        ),
        (
            Site {
                index: String::new(),
                ..site(Some("live"))
            },
            "`index` must be a key within a build",
        ),
    ];
    for (site, expected) in cases {
        let errors = invalid(&with_site(site, &["bad.example.com"]));
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{expected}: {errors:?}"
        );
    }
}

// r[verify config.validate.wildcards]
#[test]
fn wildcards_are_validated() {
    for (name, expected) in [
        ("*.com", "at least two labels"),
        ("a.*.example.com", "whole first label"),
        ("*.*.example.com", "whole first label"),
        ("*x.example.com", "whole first label"),
    ] {
        let mut config = config(60);
        config
            .servers
            .insert("bad".into(), server(&[name], vec![mapping(None, false)]));
        let errors = invalid(&config);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{name}: {errors:?}"
        );
    }
    let mut config = config(60);
    config.servers.insert(
        "twin".into(),
        server(&["*.Preview.example.com"], vec![mapping(None, false)]),
    );
    let errors = invalid(&config);
    assert!(
        errors.iter().any(|error| error.contains("all answer to")),
        "{errors:?}"
    );
}

fn token(claims: serde_json::Value) -> String {
    let mut claims = claims;
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    claims["exp"] = serde_json::json!(expires);
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256),
        &claims,
        &jsonwebtoken::EncodingKey::from_ec_pem(include_bytes!("keys/es256-private.pem")).unwrap(),
    )
    .unwrap()
}

// r[verify site.auth]
#[tokio::test]
async fn redirects_need_tokens_for_the_alias_and_the_build() {
    let mut config = config(60);
    config.auth = HashMap::from([(
        "previews".to_string(),
        Verifier {
            algorithm: JwtAlgorithm::Es256,
            public_key: Some(include_str!("keys/es256-public.pem").into()),
            jwks_uri: None,
            issuer: None,
            audience: None,
            leeway: Interval::from_secs(30),
        },
    )]);
    let previews = config.servers.get_mut("previews").unwrap();
    previews.mappings[0].auth = Some(MappingAuth {
        verifier: "previews".into(),
        paths: vec!["/aliases/${alias}".into(), "/builds/${build}".into()],
        private: true,
    });
    let (app, _, _) = app_with(config).await;
    let bearer = |claims| format!("Bearer {}", token(claims));
    let both = bearer(serde_json::json!({"alias": "pr1873", "build": PR}));
    let alias_only = bearer(serde_json::json!({"alias": "pr1873", "build": LIVE}));
    let build_only = bearer(serde_json::json!({"alias": "live", "build": PR}));

    let redirect = |token: String| {
        let app = app.clone();
        async move { get(&app, &preview("pr1873"), "/", &[("authorization", &token)]).await }
    };
    assert_eq!(redirect(both).await.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(redirect(alias_only).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        redirect(build_only.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    let unauthenticated = get(&app, &preview("pr1873"), "/", &[]).await;
    assert_eq!(unauthenticated.status, StatusCode::UNAUTHORIZED);

    // The build's own host only needs the build.
    let page = get(&app, &preview(PR), "/", &[("authorization", &build_only)]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.header(header::CACHE_CONTROL),
        "private, max-age=31536000, immutable"
    );
    let other = get(&app, &preview(LIVE), "/", &[("authorization", &build_only)]).await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
}

/// A config whose backend preloads the build `live` points at.
fn following(alias: &str) -> Config {
    let mut config = config(0);
    let Backend::S3 { refresh, .. } = config.backends.get_mut("frontend").unwrap() else {
        unreachable!()
    };
    *refresh = Some(Refresh {
        concurrency: 4,
        rules: vec![RefreshRule {
            prefix: None,
            key: None,
            alias: Some(RefreshAlias {
                name: alias.into(),
                builds: "builds/".into(),
                aliases: "aliases/".into(),
            }),
            every: Interval::from_secs(60),
            mode: RefreshMode::Preload,
            max_objects: Some(100),
            max_mib: None,
        }],
    });
    config
}

// r[verify refresh.alias]
// r[verify config.refresh.alias]
#[tokio::test]
async fn refresh_rules_preload_the_build_an_alias_points_at() {
    let (app, memory, flaky) = app_with(following("live")).await;
    let report = app.refresh("frontend", 0).await.unwrap();
    assert_eq!((report.listed, report.preloaded), (2, 2), "{report:?}");
    let before = flaky.requests();
    let page = get(&app, "www.example.com", "/assets/app.js", &[]).await;
    assert_eq!(page.body, "live.js");
    // Only the alias's and the script's metadata are checked: nothing is fetched.
    assert!(flaky.requests() - before <= 2);

    // Moving the alias preloads the other build on the next run.
    put(&memory, "aliases/live", PR, "text/plain").await;
    let report = app.refresh("frontend", 0).await.unwrap();
    assert_eq!((report.listed, report.preloaded), (2, 2), "{report:?}");
    let report = app.refresh("frontend", 0).await.unwrap();
    assert_eq!((report.listed, report.preloaded), (2, 0), "{report:?}");
}

// r[verify refresh.alias.invalid]
#[tokio::test]
async fn refresh_rules_fail_without_a_valid_alias() {
    for alias in ["missing", "broken"] {
        let (app, _, _) = app_with(following(alias)).await;
        let error = app.refresh("frontend", 0).await.unwrap_err();
        assert!(error.to_string().contains(alias), "{error:#}");
    }
}

// r[verify config.validate.refresh-alias]
#[test]
fn alias_refresh_rules_are_validated() {
    let with = |change: fn(&mut RefreshRule)| {
        let mut config = following("live");
        let Backend::S3 { refresh, .. } = config.backends.get_mut("frontend").unwrap() else {
            unreachable!()
        };
        change(&mut refresh.as_mut().unwrap().rules[0]);
        invalid(&config)
    };
    assert!(with(|_| {}).is_empty());
    for (change, expected) in [
        (
            (|rule: &mut RefreshRule| rule.alias.as_mut().unwrap().name = "Live".into())
                as fn(&mut RefreshRule),
            "isn't an alias name",
        ),
        (
            |rule| rule.alias.as_mut().unwrap().builds = "aliases/".into(),
            "can't overlap",
        ),
        (
            |rule| rule.prefix = Some("builds/".into()),
            "exactly one of `prefix`, `key` and `alias`",
        ),
    ] {
        let errors = with(change);
        assert!(
            errors.iter().any(|error| error.contains(expected)),
            "{expected}: {errors:?}"
        );
    }
}
