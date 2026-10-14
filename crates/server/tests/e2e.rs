//! End-to-end tests running the `hylla` binary against real backends: VersityGW for S3 and
//! Azurite for Azure Blob Storage, and several nodes sharing over a cluster.
//!
//! They need `versitygw` and `azurite-blob` on the `PATH`, which `mise run e2e` installs and
//! provides, so they're ignored by a plain `cargo test`.

use std::{
    fs,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use aws_lc_rs::hmac;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use object_store::{
    Attribute, AttributeValue, Attributes, ObjectStore, PutOptions, aws::AmazonS3Builder,
    azure::MicrosoftAzureBuilder, path::Path as ObjectPath,
};

const IGNORE: &str = "needs versitygw and azurite: run with `mise run e2e`";

const S3_ACCESS: &str = "hylla";
const S3_SECRET: &str = "hylla-e2e-secret";
/// Azurite's well-known development account and key.
const AZURE_ACCOUNT: &str = "devstoreaccount1";
const AZURE_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
const CLUSTER_KEY: &str = "an end-to-end cluster key of at least thirty-two characters";

/// 3000 bytes, three 1 KiB chunks, with distinct bytes so ranges are checkable.
fn content() -> Vec<u8> {
    (0..3000u32).map(|i| (i % 251) as u8).collect()
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A temporary directory for one test, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("hylla-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A process, killed on drop.
struct Process {
    name: String,
    child: Option<Child>,
    log: PathBuf,
}

impl Process {
    fn spawn(name: &str, command: &mut Command, log: PathBuf) -> Self {
        let output = fs::File::create(&log).unwrap();
        let child = command
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap_or_else(|error| panic!("starting {name} ({error}); {IGNORE}"));
        Self {
            name: name.into(),
            child: Some(child),
            log,
        }
    }

    /// Waits until the process listens on `addr`.
    fn wait_for(&mut self, addr: SocketAddr) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("{} exited with {status}:\n{}", self.name, self.logs());
            }
            assert!(
                Instant::now() < deadline,
                "{} didn't listen on {addr}:\n{}",
                self.name,
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn logs(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// VersityGW serving `root` over S3, with every bucket a directory of it.
struct VersityGw {
    process: Process,
    addr: SocketAddr,
    access_log: PathBuf,
}

impl VersityGw {
    fn start(dir: &TempDir, buckets: &[&str]) -> Self {
        let root = dir.path("versitygw");
        for bucket in buckets {
            fs::create_dir_all(root.join(bucket)).unwrap();
        }
        let addr = free_addr();
        let access_log = dir.path("versitygw-access.log");
        let mut process = Process::spawn(
            "versitygw",
            Command::new("versitygw")
                .args(["--access", S3_ACCESS, "--secret", S3_SECRET, "--quiet"])
                .arg("--port")
                .arg(addr.to_string())
                .arg("--access-log")
                .arg(&access_log)
                .arg("posix")
                .arg(&root),
            dir.path("versitygw.log"),
        );
        process.wait_for(addr);
        Self {
            process,
            addr,
            access_log,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn store(&self, bucket: &str) -> Arc<dyn ObjectStore> {
        Arc::new(
            AmazonS3Builder::new()
                .with_endpoint(self.endpoint())
                .with_allow_http(true)
                .with_region("us-east-1")
                .with_bucket_name(bucket)
                .with_access_key_id(S3_ACCESS)
                .with_secret_access_key(S3_SECRET)
                .build()
                .unwrap(),
        )
    }

    /// How many times `key` was read with GetObject, from the access log.
    fn reads(&self, key: &str) -> usize {
        let needle = format!(" s3_GetObject {key} ");
        fs::read_to_string(&self.access_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(&needle))
            .count()
    }
}

/// Azurite's blob service, with its development account.
struct Azurite {
    process: Process,
    addr: SocketAddr,
}

impl Azurite {
    fn start(dir: &TempDir) -> Self {
        let addr = free_addr();
        let location = dir.path("azurite");
        fs::create_dir_all(&location).unwrap();
        let mut process = Process::spawn(
            "azurite",
            Command::new("azurite-blob")
                .args([
                    "--blobHost",
                    "127.0.0.1",
                    "--silent",
                    "--skipApiVersionCheck",
                ])
                .arg("--blobPort")
                .arg(addr.port().to_string())
                .arg("--location")
                .arg(&location),
            dir.path("azurite.log"),
        );
        process.wait_for(addr);
        Self { process, addr }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/{AZURE_ACCOUNT}", self.addr)
    }

    fn store(&self, container: &str) -> Arc<dyn ObjectStore> {
        Arc::new(
            MicrosoftAzureBuilder::new()
                .with_account(AZURE_ACCOUNT)
                .with_access_key(AZURE_KEY)
                .with_container_name(container)
                .with_endpoint(self.endpoint())
                .with_allow_http(true)
                .build()
                .unwrap(),
        )
    }

    /// Creates a container, which object_store can't, signing the request with SharedKey.
    async fn create_container(&self, container: &str) {
        let date = httpdate::fmt_http_date(std::time::SystemTime::now());
        let version = "2021-08-06";
        let string_to_sign = format!(
            "PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:{version}\n\
             /{AZURE_ACCOUNT}/{AZURE_ACCOUNT}/{container}\nrestype:container"
        );
        let key = hmac::Key::new(hmac::HMAC_SHA256, &BASE64.decode(AZURE_KEY).unwrap());
        let signature = BASE64.encode(hmac::sign(&key, string_to_sign.as_bytes()));
        let response = reqwest::Client::new()
            .put(format!("{}/{container}?restype=container", self.endpoint()))
            .header("x-ms-date", date)
            .header("x-ms-version", version)
            .header("content-length", "0")
            .header(
                "authorization",
                format!("SharedKey {AZURE_ACCOUNT}:{signature}"),
            )
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "creating container `{container}`: {}",
            response.text().await.unwrap()
        );
    }
}

async fn upload(store: &Arc<dyn ObjectStore>, key: &str, data: Vec<u8>, content_type: &str) {
    let attributes = Attributes::from_iter([(
        Attribute::ContentType,
        AttributeValue::from(content_type.to_string()),
    )]);
    store
        .put_opts(
            &ObjectPath::from(key),
            data.into(),
            PutOptions {
                attributes,
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

/// A running hylla node.
struct Hylla {
    process: Process,
    addr: SocketAddr,
}

impl Hylla {
    fn start(dir: &TempDir, name: &str, config: &str, secrets: &str) -> Self {
        let config_path = dir.path(&format!("{name}.yaml"));
        let secrets_path = dir.path(&format!("{name}-secrets.yaml"));
        fs::write(&config_path, config).unwrap();
        fs::write(&secrets_path, secrets).unwrap();
        let addr = listen_addr(config);
        let mut process = Process::spawn(
            name,
            Command::new(env!("CARGO_BIN_EXE_hylla"))
                .arg("--config")
                .arg(&config_path)
                .arg("--secrets")
                .arg(&secrets_path)
                .env("RUST_LOG", "info"),
            dir.path(&format!("{name}.log")),
        );
        process.wait_for(addr);
        Self { process, addr }
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .get(format!("http://{}{path}", self.addr))
            .header("host", "cache.example.com");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap()
    }

    async fn body(&self, path: &str) -> (reqwest::StatusCode, Vec<u8>) {
        let response = self.get(path, &[]).await;
        let status = response.status();
        (status, response.bytes().await.unwrap_or_default().to_vec())
    }
}

/// The `listen` address of the config's server.
fn listen_addr(config: &str) -> SocketAddr {
    config
        .lines()
        .find_map(|line| line.trim().strip_prefix("listen: "))
        .unwrap()
        .parse()
        .unwrap()
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// r[verify backend.s3]
// r[verify backend.azure-blob]
// r[verify cache.ram]
#[tokio::test]
#[ignore = "needs versitygw and azurite: run with `mise run e2e`"]
async fn serves_from_versitygw_and_azurite() {
    let dir = TempDir::new("backends");
    let mut versitygw = VersityGw::start(&dir, &["assets"]);
    let mut azurite = Azurite::start(&dir);
    azurite.create_container("media").await;
    upload(
        &versitygw.store("assets"),
        "css/app.css",
        content(),
        "text/css",
    )
    .await;
    upload(
        &azurite.store("media"),
        "video/clip.bin",
        content(),
        "video/mp4",
    )
    .await;

    let listen = free_addr();
    let config = format!(
        "\
servers:
  public:
    listen: {listen}
    server_names: [cache.example.com]
    mappings:
      - path_prefix: /s3
        backend: assets
        rewrites: [{{pattern: '^/s3/(.*)$', replace: '/$1'}}]
      - path_prefix: /azure
        backend: media
        rewrites: [{{pattern: '^/azure/(.*)$', replace: '/$1'}}]
backends:
  assets:
    type: s3
    bucket: assets
    region: us-east-1
    endpoint: {s3}
  media:
    type: azure_blob
    account: {AZURE_ACCOUNT}
    container: media
    endpoint: {azure}
cache:
  chunk_size_kib: 1
  metadata_ttl: 1h
",
        s3 = versitygw.endpoint(),
        azure = azurite.endpoint(),
    );
    let secrets = format!(
        "\
backends:
  assets:
    access_key_id: {S3_ACCESS}
    secret_access_key: {S3_SECRET}
  media:
    account_key: {AZURE_KEY}
"
    );
    let hylla = Hylla::start(&dir, "hylla", &config, &secrets);

    for (path, content_type) in [
        ("/s3/css/app.css", "text/css"),
        ("/azure/video/clip.bin", "video/mp4"),
    ] {
        let response = hylla.get(path, &[]).await;
        assert_eq!(response.status(), 200, "{path}:\n{}", hylla.process.logs());
        assert_eq!(response.headers()["content-type"], content_type, "{path}");
        let etag = response.headers()["etag"].to_str().unwrap().to_string();
        assert_eq!(
            response.bytes().await.unwrap().to_vec(),
            content(),
            "{path}"
        );

        let response = hylla.get(path, &[("range", "bytes=1000-2100")]).await;
        assert_eq!(response.status(), 206, "{path}");
        assert_eq!(response.headers()["content-range"], "bytes 1000-2100/3000");
        assert_eq!(
            response.bytes().await.unwrap().to_vec(),
            content()[1000..2101]
        );

        let response = hylla.get(path, &[("if-none-match", &etag)]).await;
        assert_eq!(response.status(), 304, "{path}");
    }
    assert_eq!(hylla.body("/s3/css/missing.css").await.0, 404);

    // With both backends gone, cached objects are still served.
    versitygw.process.kill();
    azurite.process.kill();
    for path in ["/s3/css/app.css", "/azure/video/clip.bin"] {
        assert_eq!(
            hylla.body(path).await,
            (reqwest::StatusCode::OK, content()),
            "{path}"
        );
    }
}

fn cluster_config(listen: SocketAddr, cluster: SocketAddr, seeds: &[String], s3: &str) -> String {
    format!(
        "\
servers:
  public:
    listen: {listen}
    server_names: [cache.example.com]
    mappings:
      - path_prefix: /
        backend: assets
backends:
  assets:
    type: s3
    bucket: assets
    region: us-east-1
    endpoint: {s3}
    refresh:
      rules:
        - prefix: ''
          every: 1h
          mode: preload
          max_objects: 10
cache:
  chunk_size_kib: 1
  metadata_ttl: 1h
cluster:
  listen: {cluster}
  seeds: [{seeds}]
  gossip_interval: 300ms
  failure_timeout: 3s
",
        seeds = seeds.join(", "),
    )
}

fn cluster_secrets() -> String {
    format!(
        "\
backends:
  assets:
    access_key_id: {S3_ACCESS}
    secret_access_key: {S3_SECRET}
cluster:
  key: {CLUSTER_KEY}
"
    )
}

// r[verify cluster.membership]
// r[verify cluster.seeds]
// r[verify cluster.refresh]
// r[verify cluster.share-preload]
// r[verify cluster.pull]
#[tokio::test]
#[ignore = "needs versitygw and azurite: run with `mise run e2e`"]
async fn cluster_nodes_share_preloads_directly() {
    let dir = TempDir::new("cluster");
    let mut versitygw = VersityGw::start(&dir, &["assets"]);
    let store = versitygw.store("assets");
    upload(&store, "a.css", content(), "text/css").await;
    upload(&store, "b.css", content(), "text/css").await;

    let clusters = [free_addr(), free_addr(), free_addr()];
    // The second node joins through the first's address, the third through a DNS name.
    let seeds = [
        Vec::new(),
        vec![clusters[0].to_string()],
        vec![format!("localhost:{}", clusters[0].port())],
    ];
    let nodes = (0..3)
        .map(|i| {
            let config = cluster_config(free_addr(), clusters[i], &seeds[i], &versitygw.endpoint());
            Hylla::start(&dir, &format!("node{i}"), &config, &cluster_secrets())
        })
        .collect::<Vec<_>>();

    // The rule runs once membership settled, on one node, which reads every chunk of
    // both objects from the backend; the others pull the chunks from it.
    wait_until("the preload", || {
        versitygw.reads("a.css") >= 3 && versitygw.reads("b.css") >= 3
    });
    std::thread::sleep(Duration::from_secs(2));
    let logs = || {
        nodes
            .iter()
            .map(|node| node.process.logs())
            .collect::<Vec<_>>()
            .join("\n---\n")
    };
    assert_eq!(
        (versitygw.reads("a.css"), versitygw.reads("b.css")),
        (3, 3),
        "each chunk is read from the backend once for the whole cluster:\n{}",
        logs()
    );

    // Every node serves both objects without the backend.
    versitygw.process.kill();
    for (i, node) in nodes.iter().enumerate() {
        for key in ["/a.css", "/b.css"] {
            assert_eq!(
                node.body(key).await,
                (reqwest::StatusCode::OK, content()),
                "node {i}, {key}:\n{}",
                node.process.logs()
            );
        }
    }
}
