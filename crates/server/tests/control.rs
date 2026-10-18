use std::{collections::HashMap, sync::Arc};

use axum::body::{Body, to_bytes};
use base64::Engine;
use http::{Request, StatusCode, header};
use hylla_config::{
    Backend, Config, Control, ControlUser, Mapping, Rewrite, Server, ValidationError,
};
use hylla_server::App;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
use tower::ServiceExt;

const LISTEN: &str = "127.0.0.1:8080";
const PASSWORD: &str = "correct horse battery staple";

fn hash(password: &str) -> String {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let salt = SaltString::encode_b64(b"a salt for tests").unwrap();
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

fn mapping(path_prefix: &str, writable: bool) -> Mapping {
    Mapping {
        path_prefix: path_prefix.into(),
        backend: String::new(),
        rewrites: Vec::new(),
        writable,
        auth: None,
    }
}

/// A node whose `backend` holds `uploads/a.txt`, `uploads/docs/b.txt` and `readme.txt`,
/// mapped writable under `/uploads`, read only under `/`, and read only with a rewrite under
/// `/rewritten`.
async fn node(backend: &str) -> (Arc<App>, Arc<InMemory>) {
    let memory = Arc::new(InMemory::new());
    for (key, content) in [
        ("uploads/a.txt", "a"),
        ("uploads/docs/b.txt", "b"),
        ("readme.txt", "readme"),
    ] {
        memory
            .put(&Path::from(key), content.as_bytes().to_vec().into())
            .await
            .unwrap();
    }
    let mut rewritten = mapping("/rewritten", false);
    rewritten.rewrites = vec![Rewrite {
        pattern: "^/rewritten/(.*)$".into(),
        replace: "/$1".into(),
    }];
    let mappings = [mapping("/uploads", true), rewritten, mapping("/", false)]
        .into_iter()
        .map(|mut mapping| {
            mapping.backend = backend.into();
            mapping
        })
        .collect();
    let config = Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: LISTEN.parse().unwrap(),
                server_names: vec!["cache.example.com".into()],
                mappings,
            },
        )]),
        backends: HashMap::from([(
            backend.into(),
            Backend::S3 {
                bucket: "assets".into(),
                region: "eu-north-1".into(),
                endpoint: None,
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
                refresh: None,
            },
        )]),
        cache: Default::default(),
        auth: HashMap::new(),
        telemetry: Default::default(),
        cluster: None,
        control: Some(Control {
            listen: "127.0.0.1:9090".parse().unwrap(),
            users: HashMap::from([(
                "admin".into(),
                ControlUser {
                    password_hash: hash(PASSWORD),
                },
            )]),
            max_upload_mib: 1,
        }),
        serve_backends: None,
    };
    config.validate().unwrap();
    let stores = HashMap::from([(backend.to_string(), memory.clone() as Arc<dyn ObjectStore>)]);
    (App::new(&config, stores).await.unwrap(), memory)
}

fn basic(user: &str, password: &str) -> String {
    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {credentials}")
}

struct Answer {
    status: StatusCode,
    headers: http::HeaderMap,
    body: String,
}

async fn send(app: &Arc<App>, request: http::request::Builder, body: Body) -> Answer {
    let response = app
        .control_router()
        .unwrap()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn signed_in(method: &str, uri: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:9090")
        .header(header::AUTHORIZATION, basic("admin", PASSWORD))
}

async fn control(app: &Arc<App>, method: &str, uri: &str) -> Answer {
    send(app, signed_in(method, uri), Body::empty()).await
}

/// A request to the public listener, answering its status and body.
async fn public(app: &Arc<App>, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .header(header::HOST, "cache.example.com")
        .body(Body::empty())
        .unwrap();
    let response = app
        .listener(LISTEN.parse().unwrap())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

// r[verify control.auth]
// r[verify config.control]
// r[verify config.control.users]
#[tokio::test]
async fn the_control_plane_requires_a_user() {
    let (app, _) = node("auth").await;
    let request = |authorization: Option<String>| {
        let mut request = Request::builder().uri("/v1/backends");
        if let Some(authorization) = authorization {
            request = request.header(header::AUTHORIZATION, authorization);
        }
        request
    };
    for authorization in [
        None,
        Some(basic("admin", "wrong")),
        Some(basic("nobody", PASSWORD)),
        Some("Bearer token".to_string()),
    ] {
        let answer = send(&app, request(authorization.clone()), Body::empty()).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        assert!(
            answer.headers[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .starts_with("Basic")
        );
    }
    let answer = send(&app, request(Some(basic("admin", PASSWORD))), Body::empty()).await;
    assert_eq!(answer.status, StatusCode::OK);
}

// r[verify control.backends]
// r[verify control.api]
#[tokio::test]
async fn backends_report_their_last_request() {
    let (app, _) = node("status").await;
    let backends: serde_json::Value =
        serde_json::from_str(&control(&app, "GET", "/v1/backends").await.body).unwrap();
    assert_eq!(backends[0]["name"], "status");
    assert_eq!(backends[0]["kind"], "s3");
    assert!(backends[0]["last"].is_null());

    assert_eq!(public(&app, "/readme.txt").await.1, "readme");
    let backends: serde_json::Value =
        serde_json::from_str(&control(&app, "GET", "/v1/backends").await.body).unwrap();
    assert_eq!(backends[0]["last"]["outcome"], "ok");
    assert!(backends[0]["last_error"].is_null());

    // The UI shows the same.
    let page = control(&app, "GET", "/").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("<td>status</td>"), "{}", page.body);
    assert!(page.body.contains(">ok<"), "{}", page.body);
}

// r[verify control.browse]
#[tokio::test]
async fn mappings_are_browsed_by_folder() {
    let (app, _) = node("browse").await;
    let listing: serde_json::Value = serde_json::from_str(
        &control(&app, "GET", "/v1/mappings/public/0/list")
            .await
            .body,
    )
    .unwrap();
    assert_eq!(listing["folders"], serde_json::json!(["docs/"]));
    assert_eq!(listing["objects"][0]["path"], "a.txt");
    let listing: serde_json::Value = serde_json::from_str(
        &control(&app, "GET", "/v1/mappings/public/0/list?prefix=docs/")
            .await
            .body,
    )
    .unwrap();
    assert_eq!(listing["objects"][0]["path"], "docs/b.txt");
    assert_eq!(listing["objects"][0]["size"], 1);

    // Mappings with rewrites can't be listed, since keys don't map back to their paths.
    let answer = control(&app, "GET", "/v1/mappings/public/1/list").await;
    assert_eq!(answer.status, StatusCode::CONFLICT);

    // The UI lists the same, with changes for writable mappings only.
    let page = control(&app, "GET", "/mappings").await;
    assert!(page.body.contains("/uploads"), "{}", page.body);
    let page = control(&app, "GET", "/mappings/public/0").await;
    assert!(
        page.body.contains("a.txt") && page.body.contains("docs/"),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("Delete") && page.body.contains("Upload"),
        "{}",
        page.body
    );
    let page = control(&app, "GET", "/mappings/public/2").await;
    assert!(page.body.contains("readme.txt"), "{}", page.body);
    assert!(!page.body.contains("Delete"), "{}", page.body);
}

// r[verify control.writes]
// r[verify control.invalidate]
// r[verify config.mappings.writable]
#[tokio::test]
async fn writable_mappings_change_objects_under_their_prefix() {
    let (app, memory) = node("writes").await;
    // Cached before it changes.
    assert_eq!(public(&app, "/uploads/a.txt").await.1, "a");

    let answer = send(
        &app,
        signed_in("PUT", "/v1/mappings/public/0/objects/a.txt"),
        Body::from("changed"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(public(&app, "/uploads/a.txt").await.1, "changed");

    let answer = send(
        &app,
        signed_in("POST", "/v1/mappings/public/0/move")
            .header(header::CONTENT_TYPE, "application/json"),
        Body::from(r#"{"from":"a.txt","to":"docs/moved.txt"}"#),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(
        public(&app, "/uploads/a.txt").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(public(&app, "/uploads/docs/moved.txt").await.1, "changed");

    let answer = control(
        &app,
        "DELETE",
        "/v1/mappings/public/0/objects/docs/moved.txt",
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(
        public(&app, "/uploads/docs/moved.txt").await.0,
        StatusCode::NOT_FOUND
    );

    // Nothing outside the prefix, and nothing through read-only mappings.
    for (method, uri, status) in [
        (
            "DELETE",
            "/v1/mappings/public/0/objects/..%2Freadme.txt",
            StatusCode::BAD_REQUEST,
        ),
        (
            "DELETE",
            "/v1/mappings/public/0/objects/docs/..%2F..%2Freadme.txt",
            StatusCode::BAD_REQUEST,
        ),
        (
            "DELETE",
            "/v1/mappings/public/2/objects/readme.txt",
            StatusCode::FORBIDDEN,
        ),
        (
            "DELETE",
            "/v1/mappings/public/9/objects/readme.txt",
            StatusCode::NOT_FOUND,
        ),
    ] {
        assert_eq!(control(&app, method, uri).await.status, status, "{uri}");
    }
    assert!(memory.head(&Path::from("readme.txt")).await.is_ok());

    // Uploads are limited, here to 1 MiB.
    let answer = send(
        &app,
        signed_in("PUT", "/v1/mappings/public/0/objects/big.bin"),
        Body::from(vec![0u8; 2 * 1024 * 1024]),
    )
    .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
}

// r[verify control.same-origin]
#[tokio::test]
async fn cross_site_changes_are_refused() {
    let (app, memory) = node("origin").await;
    let delete = || signed_in("DELETE", "/v1/mappings/public/0/objects/a.txt");
    for request in [
        delete().header("sec-fetch-site", "cross-site"),
        delete().header("sec-fetch-site", "same-site"),
        delete().header(header::ORIGIN, "https://evil.example.com"),
    ] {
        assert_eq!(
            send(&app, request, Body::empty()).await.status,
            StatusCode::FORBIDDEN
        );
    }
    assert!(memory.head(&Path::from("uploads/a.txt")).await.is_ok());
    // Reading isn't a change.
    let read = signed_in("GET", "/v1/backends").header("sec-fetch-site", "cross-site");
    assert_eq!(send(&app, read, Body::empty()).await.status, StatusCode::OK);
    // The UI's own forms are same-origin.
    let request = delete()
        .header("sec-fetch-site", "same-origin")
        .header(header::ORIGIN, "http://127.0.0.1:9090");
    assert_eq!(
        send(&app, request, Body::empty()).await.status,
        StatusCode::NO_CONTENT
    );
}

// r[verify control.writes]
// r[verify control.ui]
#[tokio::test]
async fn the_ui_uploads_and_deletes_with_plain_forms() {
    let (app, memory) = node("forms").await;
    let boundary = "hyllaboundary";
    let form = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"C:\\\\Users\\\\me\\\\new.txt\"\r\n\
         Content-Type: text/plain\r\n\r\nnew\r\n--{boundary}--\r\n"
    );
    let answer = send(
        &app,
        signed_in(
            "POST",
            "/v1/mappings/public/0/upload?prefix=docs/&redirect=1",
        )
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        ),
        Body::from(form),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(
        answer.headers[header::LOCATION],
        "/mappings/public/0?prefix=docs%2F"
    );
    let uploaded = memory
        .get(&Path::from("uploads/docs/new.txt"))
        .await
        .unwrap();
    assert_eq!(uploaded.bytes().await.unwrap(), "new".as_bytes());

    // Deleting posts the page's form to its server function, which works without
    // JavaScript too, redirecting back.
    let page = control(&app, "GET", "/mappings/public/0?prefix=docs/").await;
    let action = page
        .body
        .split("action=\"")
        .skip(1)
        .map(|rest| &rest[..rest.find('"').unwrap()])
        .find(|action| action.contains("delete_object"))
        .unwrap_or_else(|| panic!("no delete form in {}", page.body))
        .to_string();
    let answer = send(
        &app,
        signed_in("POST", &action)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            // As browsers send for forms.
            .header(header::ACCEPT, "text/html,application/xhtml+xml")
            .header(
                header::REFERER,
                "http://127.0.0.1:9090/mappings/public/0?prefix=docs/",
            ),
        Body::from("server=public&index=0&path=docs%2Fnew.txt"),
    )
    .await;
    assert!(
        answer.status.is_redirection(),
        "{} {}",
        answer.status,
        answer.body
    );
    assert!(
        memory
            .head(&Path::from("uploads/docs/new.txt"))
            .await
            .is_err()
    );
}

// r[verify config.validate.writable]
// r[verify config.validate.control]
#[test]
fn writable_mappings_and_the_control_plane_are_validated() {
    let mut config = Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: LISTEN.parse().unwrap(),
                server_names: vec!["cache.example.com".into()],
                mappings: vec![Mapping {
                    path_prefix: "/".into(),
                    backend: "shield".into(),
                    rewrites: vec![Rewrite {
                        pattern: "^/(.*)$".into(),
                        replace: "/x/$1".into(),
                    }],
                    writable: true,
                    auth: None,
                }],
            },
        )]),
        backends: HashMap::from([(
            "shield".into(),
            Backend::Hylla {
                url: "https://shield.internal:7947".into(),
                backend: "assets".into(),
                tls: hylla_config::HyllaTls {
                    ca: "ca.crt".into(),
                    cert: "tls.crt".into(),
                    key: "tls.key".into(),
                },
                refresh: None,
            },
        )]),
        cache: Default::default(),
        auth: HashMap::new(),
        telemetry: Default::default(),
        cluster: None,
        control: Some(Control {
            listen: LISTEN.parse().unwrap(),
            users: HashMap::from([(
                "admin".into(),
                ControlUser {
                    password_hash: "hunter2".into(),
                },
            )]),
            max_upload_mib: 0,
        }),
        serve_backends: None,
    };
    let errors = config
        .validate()
        .unwrap_err()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        errors,
        [
            "mapping 0 of server `public` can't be writable: it has rewrites, so its backend keys \
             can't be mapped back to its paths",
            "mapping 0 of server `public` can't be writable: its backend is another hylla node, \
             which is read only",
            "invalid control config: the `password_hash` of user `admin` isn't an argon2 hash, as \
             `hylla hash-password` prints",
            "invalid control config: `max_upload_mib` must be positive",
            "invalid control config: `listen` is 127.0.0.1:8080, which server `public` listens on \
             too",
        ]
    );

    config.control.as_mut().unwrap().users.clear();
    let errors = config.validate().unwrap_err();
    assert!(errors.contains(&ValidationError::InvalidControl {
        reason: "there are no `users`, so no one can sign in".into()
    }));
}
