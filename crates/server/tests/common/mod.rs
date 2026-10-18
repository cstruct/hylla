//! Helpers shared by the server tests.

#![allow(dead_code)]

use std::sync::Arc;

use object_store::{ObjectStore, PutOptions, memory::InMemory, path::Path};

/// A store that fails every request while `failing` is set, as during an outage.
#[derive(Debug)]
pub struct Flaky {
    inner: Arc<InMemory>,
    failing: std::sync::atomic::AtomicBool,
    requests: std::sync::atomic::AtomicUsize,
}

impl Flaky {
    pub fn new(inner: Arc<InMemory>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            failing: false.into(),
            requests: 0.into(),
        })
    }

    pub fn fail(&self, failing: bool) {
        self.failing
            .store(failing, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn requests(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn check(&self) -> object_store::Result<()> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.failing.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(object_store::Error::Generic {
                store: "flaky",
                source: "the backend is down".into(),
            });
        }
        Ok(())
    }
}

impl std::fmt::Display for Flaky {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Flaky({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Flaky {
    async fn put_opts(
        &self,
        location: &Path,
        payload: object_store::PutPayload,
        options: PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.check()?;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        use futures::StreamExt;
        if let Err(error) = self.check() {
            return futures::stream::once(async { Err(error) }).boxed();
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// A certificate and key issued by a [`Pki`], as the paths of their PEM files.
pub struct Issued {
    pub cert: String,
    pub key: String,
}

/// A CA issuing certificates into a directory of its own, removed when it's dropped.
pub struct Pki {
    dir: std::path::PathBuf,
    ca: std::path::PathBuf,
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    issued: std::sync::atomic::AtomicUsize,
}

impl Pki {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hylla-pki-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("{name} CA"));
        let key = rcgen::KeyPair::generate().unwrap();
        let ca = dir.join("ca.crt");
        std::fs::write(&ca, params.self_signed(&key).unwrap().pem()).unwrap();
        Self {
            dir,
            ca,
            issuer: rcgen::Issuer::new(params, key),
            issued: 0.into(),
        }
    }

    /// The path of the CA's certificate.
    pub fn ca(&self) -> String {
        self.ca.display().to_string()
    }

    /// A certificate for `name`, for both serving and connecting.
    pub fn issue(&self, name: &str) -> Issued {
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        let issued = self
            .issued
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (cert_path, key_path) = (
            self.dir.join(format!("issued-{issued}.crt")),
            self.dir.join(format!("issued-{issued}.key")),
        );
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        Issued {
            cert: cert_path.display().to_string(),
            key: key_path.display().to_string(),
        }
    }
}

impl Drop for Pki {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
