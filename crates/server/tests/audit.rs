//! The control plane's audit log, in a test binary of its own: it sets the global
//! subscriber, which no other test may race with.

use std::{collections::HashMap, sync::Arc};

use axum::body::Body;
use base64::Engine;
use http::{Request, StatusCode, header};
use hylla_config::{Backend, Config, Control, ControlUser, Mapping, Server};
use hylla_server::App;
use object_store::{ObjectStore, memory::InMemory};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";

/// Log lines written, as text.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn node() -> Arc<App> {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let salt = SaltString::encode_b64(b"a salt for tests").unwrap();
    let hash = Argon2::default()
        .hash_password(PASSWORD.as_bytes(), &salt)
        .unwrap()
        .to_string();
    let mapping = |path_prefix: &str, writable| Mapping {
        path_prefix: path_prefix.into(),
        backend: "assets".into(),
        rewrites: Vec::new(),
        writable,
        cache_control: None,
        auth: None,
    };
    let config = Config {
        servers: HashMap::from([(
            "public".into(),
            Server {
                listen: "127.0.0.1:8080".parse().unwrap(),
                server_names: vec!["cache.example.com".into()],
                mappings: vec![mapping("/uploads", true), mapping("/", false)],
            },
        )]),
        backends: HashMap::from([(
            "assets".into(),
            Backend::S3 {
                bucket: "assets".into(),
                region: "eu-north-1".into(),
                endpoint: None,
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
                timeouts: Default::default(),
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
                    password_hash: hash,
                },
            )]),
            max_upload_mib: 1,
            oidc: None,
        }),
        serve_backends: None,
        health: None,
        http: Default::default(),
    };
    let stores = HashMap::from([(
        "assets".to_string(),
        Arc::new(InMemory::new()) as Arc<dyn ObjectStore>,
    )]);
    App::new(&config, stores).await.unwrap()
}

async fn send(app: &Arc<App>, method: &str, uri: &str, body: Body) -> StatusCode {
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("admin:{PASSWORD}"));
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:9090")
        .header(header::AUTHORIZATION, format!("Basic {credentials}"))
        .body(body)
        .unwrap();
    app.control_router()
        .unwrap()
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

// r[verify control.audit]
#[tokio::test]
async fn changes_are_audit_logged() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();
    let app = node().await;

    let objects = "/v1/mappings/public/0/objects";
    let status = send(
        &app,
        "PUT",
        &format!("{objects}/new.txt"),
        Body::from("hello"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let status = send(&app, "DELETE", &format!("{objects}/new.txt"), Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Refused changes are logged too.
    let status = send(
        &app,
        "DELETE",
        "/v1/mappings/public/1/objects/readme.txt",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let logged = String::from_utf8_lossy(&captured.0.lock().unwrap()).to_string();
    let audit = logged
        .lines()
        .filter(|line| line.contains("hylla::audit"))
        .collect::<Vec<_>>();
    assert_eq!(audit.len(), 3, "{logged}");
    for line in &audit {
        assert!(line.contains(r#"user="admin""#), "{line}");
        assert!(line.contains(r#"signed_in_with="basic""#), "{line}");
    }
    assert!(audit[0].contains(r#"action="upload""#), "{}", audit[0]);
    assert!(audit[0].contains(r#"path="new.txt""#) && audit[0].contains("size=5"));
    assert!(audit[1].contains(r#"action="delete""#) && audit[1].contains("changed an object"));
    assert!(audit[2].contains("WARN") && audit[2].contains("failed to change an object"));
}
