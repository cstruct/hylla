//! Mutual TLS between hylla nodes: between the nodes of a cluster, and between nodes and
//! the nodes they use as backends.
//!
//! Both sides present a certificate and require one from the other, issued by a CA they
//! trust for an expected name. Certificates are read again when their files change, so they
//! can be rotated without a restart.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, RootCertStore,
    ServerConfig, SignatureScheme,
    client::{
        ResolvesClientCert, WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
    server::{
        ClientHello, ResolvesServerCert, WebPkiClientVerifier,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
    sign::CertifiedKey,
};

/// The PEM files of a node's certificate and key, and of the CA it trusts.
pub struct Files<'a> {
    pub ca: &'a str,
    pub cert: &'a str,
    pub key: &'a str,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn dns_name(name: &str) -> anyhow::Result<ServerName<'static>> {
    ServerName::try_from(name.to_string()).with_context(|| format!("`{name}` isn't a valid name"))
}

/// The TLS configuration of a listener presenting `files`' certificate and requiring
/// clients' certificates issued by its CA for `client_name`.
pub fn server_config(files: &Files<'_>, client_name: &str) -> anyhow::Result<Arc<ServerConfig>> {
    let provider = provider();
    let roots = Arc::new(roots(Path::new(files.ca))?);
    let credentials = Arc::new(Credentials::load(
        files.cert.as_ref(),
        files.key.as_ref(),
        provider.clone(),
    )?);
    let client_verifier = NameVerifier {
        inner: WebPkiClientVerifier::builder_with_provider(roots, provider.clone())
            .build()
            .context("creating the verifier of client certificates")?,
        name: dns_name(client_name)?,
    };
    let mut server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(client_verifier))
        .with_cert_resolver(credentials);
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(server))
}

/// The TLS configuration of a client presenting `files`' certificate and requiring servers'
/// certificates issued by its CA: for `server_name` whatever address the server was reached
/// at, or without it, for the host connected to, as HTTPS clients do.
pub fn client_config(files: &Files<'_>, server_name: Option<&str>) -> anyhow::Result<ClientConfig> {
    let provider = provider();
    let roots = Arc::new(roots(Path::new(files.ca))?);
    let credentials = Arc::new(Credentials::load(
        files.cert.as_ref(),
        files.key.as_ref(),
        provider.clone(),
    )?);
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?;
    let config = match server_name {
        Some(server_name) => {
            let verifier = PeerVerifier {
                inner: WebPkiServerVerifier::builder_with_provider(roots, provider)
                    .build()
                    .context("creating the verifier of server certificates")?,
                server_name: dns_name(server_name)?,
            };
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_client_cert_resolver(credentials)
        }
        None => builder
            .with_root_certificates(roots)
            .with_client_cert_resolver(credentials),
    };
    Ok(config)
}

/// Serves `router` on `listener` over TLS, requiring clients' certificates as `tls` does.
/// `what` names the listener in logs, such as `cluster`.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    tls: Arc<ServerConfig>,
    what: &'static str,
) {
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto,
        service::TowerToHyperService,
    };

    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // Such as running out of file descriptors: wait for some to free up.
                tracing::warn!("accepting a {what} connection failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let handshake = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream));
            let stream = match handshake.await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    tracing::warn!(%peer, "refused a {what} connection: {error}");
                    return;
                }
                Err(_) => {
                    tracing::warn!(%peer, "refused a {what} connection: the handshake timed out");
                    return;
                }
            };
            let served = auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), TowerToHyperService::new(router))
                .await;
            if let Err(error) = served {
                tracing::debug!(%peer, "{what} connection failed: {error}");
            }
        });
    }
}

fn roots(path: &Path) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let certs = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading the CA `{}`", path.display()))?;
    for cert in certs {
        let cert = cert.with_context(|| format!("reading the CA `{}`", path.display()))?;
        roots
            .add(cert)
            .with_context(|| format!("adding the CA `{}`", path.display()))?;
    }
    if roots.is_empty() {
        bail!("the CA `{}` has no certificates", path.display());
    }
    Ok(roots)
}

/// Checks a server's certificate for a fixed name, whatever address it was reached at, as
/// nodes of a cluster are, and otherwise as WebPKI does.
#[derive(Debug)]
struct PeerVerifier {
    inner: Arc<WebPkiServerVerifier>,
    server_name: ServerName<'static>,
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            &self.server_name,
            ocsp_response,
            now,
        )
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Checks a client's certificate for a name too, which WebPKI's client verification
/// doesn't, so certificates the CA issued for anything else are refused.
#[derive(Debug)]
struct NameVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    name: ServerName<'static>,
}

impl ClientCertVerifier for NameVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        webpki::EndEntityCert::try_from(end_entity)
            .and_then(|cert| cert.verify_is_valid_for_subject_name(&self.name))
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::NotValidForName))?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

// r[impl cluster.tls.rotate]
/// A node's certificate and key, presented to what it connects to and what connects to it,
/// read again for new connections when their files change.
#[derive(Debug)]
struct Credentials {
    cert: PathBuf,
    key: PathBuf,
    provider: Arc<CryptoProvider>,
    current: Mutex<Current>,
}

#[derive(Debug)]
struct Current {
    key: Arc<CertifiedKey>,
    /// When the files were modified when they were read.
    modified: (Option<SystemTime>, Option<SystemTime>),
}

impl Credentials {
    fn load(cert: &Path, key: &Path, provider: Arc<CryptoProvider>) -> anyhow::Result<Self> {
        let modified = (modified(cert), modified(key));
        let certified = read(cert, key, &provider)?;
        Ok(Self {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
            provider,
            current: Mutex::new(Current {
                key: Arc::new(certified),
                modified,
            }),
        })
    }

    /// The certificate and key, read again if either file changed. If they can't be read,
    /// such as while one is replaced and the other isn't yet, the ones in use are kept, and
    /// reading is tried again on the next connection.
    fn get(&self) -> Arc<CertifiedKey> {
        let mut current = self.current.lock().expect("not poisoned");
        let modified = (modified(&self.cert), modified(&self.key));
        if modified != current.modified {
            match read(&self.cert, &self.key, &self.provider) {
                Ok(key) => {
                    tracing::info!(cert = %self.cert.display(), "certificate reloaded");
                    *current = Current {
                        key: Arc::new(key),
                        modified,
                    };
                }
                Err(error) => tracing::warn!(
                    cert = %self.cert.display(),
                    "reloading the certificate failed, keeping the one in use: {error:#}"
                ),
            }
        }
        current.key.clone()
    }
}

impl ResolvesServerCert for Credentials {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.get())
    }
}

impl ResolvesClientCert for Credentials {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(self.get())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// A certificate chain and its key, failing if they don't match.
fn read(cert: &Path, key: &Path, provider: &CryptoProvider) -> anyhow::Result<CertifiedKey> {
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .with_context(|| format!("reading the certificate `{}`", cert.display()))?;
    if chain.is_empty() {
        bail!("the certificate `{}` has no certificates", cert.display());
    }
    let private_key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading the key `{}`", key.display()))?;
    CertifiedKey::from_der(chain, private_key, provider).with_context(|| {
        format!(
            "the key `{}` doesn't match the certificate `{}`",
            key.display(),
            cert.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use rcgen::{CertificateParams, KeyPair};

    use super::Credentials;

    fn self_signed(name: &str) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn set_modified(path: &std::path::Path, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    // r[verify cluster.tls.rotate]
    #[test]
    fn certificates_are_read_again_when_they_change() {
        let dir = std::env::temp_dir().join(format!("hylla-tls-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert_path, key_path) = (dir.join("tls.crt"), dir.join("tls.key"));
        let (cert, key) = self_signed("hylla.cluster");
        std::fs::write(&cert_path, &cert).unwrap();
        std::fs::write(&key_path, &key).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let credentials = Credentials::load(&cert_path, &key_path, provider).unwrap();
        let first = credentials.get();
        assert!(
            Arc::ptr_eq(&first, &credentials.get()),
            "unchanged files are kept"
        );

        // A new certificate without its key yet doesn't match, so the old pair stays.
        let (new_cert, new_key) = self_signed("hylla.cluster");
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::write(&cert_path, &new_cert).unwrap();
        set_modified(&cert_path, later);
        assert!(Arc::ptr_eq(&first, &credentials.get()));

        // Once the key is there too, both are used.
        std::fs::write(&key_path, &new_key).unwrap();
        set_modified(&key_path, later);
        let second = credentials.get();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_ne!(
            first.end_entity_cert().unwrap(),
            second.end_entity_cert().unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
