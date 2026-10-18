//! Mutual TLS between nodes.
//!
//! Every node presents its certificate and requires one from its peers, issued by the
//! cluster CA for the cluster's server name, both when it connects and when it's connected
//! to. Nodes reach each other by address, which changes as they come and go, so their
//! certificates are checked for the server name instead of the address.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use anyhow::{Context, bail};
use hylla_config::ClusterTls;
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

/// The TLS configurations of a node's cluster listener and client.
pub struct Tls {
    pub server: Arc<ServerConfig>,
    pub client: ClientConfig,
}

// r[impl cluster.tls]
impl Tls {
    pub fn new(config: &ClusterTls) -> anyhow::Result<Self> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let roots = Arc::new(roots(Path::new(&config.ca))?);
        let server_name = ServerName::try_from(config.server_name.clone())
            .with_context(|| format!("`{}` isn't a valid server name", config.server_name))?;
        let credentials = Arc::new(Credentials::load(
            config.cert.as_ref(),
            config.key.as_ref(),
            provider.clone(),
        )?);

        let client_verifier = NodeVerifier {
            inner: WebPkiClientVerifier::builder_with_provider(roots.clone(), provider.clone())
                .build()
                .context("creating the verifier of nodes' certificates")?,
            server_name: server_name.clone(),
        };
        let mut server = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(Arc::new(client_verifier))
            .with_cert_resolver(credentials.clone());
        server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        let server_verifier = PeerVerifier {
            inner: WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
                .build()
                .context("creating the verifier of nodes' certificates")?,
            server_name,
        };
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(server_verifier))
            .with_client_cert_resolver(credentials);
        Ok(Self {
            server: Arc::new(server),
            client,
        })
    }
}

fn roots(path: &Path) -> anyhow::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let certs = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading the cluster CA `{}`", path.display()))?;
    for cert in certs {
        let cert = cert.with_context(|| format!("reading the cluster CA `{}`", path.display()))?;
        roots
            .add(cert)
            .with_context(|| format!("adding the cluster CA `{}`", path.display()))?;
    }
    if roots.is_empty() {
        bail!("the cluster CA `{}` has no certificates", path.display());
    }
    Ok(roots)
}

/// Checks a node's certificate for the cluster's server name, whatever address it was
/// reached at, and otherwise as WebPKI does.
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

/// Checks the certificate of a node connecting for the cluster's server name too, which
/// WebPKI's client verification doesn't, so certificates the CA issued for anything else
/// aren't nodes.
#[derive(Debug)]
struct NodeVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    server_name: ServerName<'static>,
}

impl ClientCertVerifier for NodeVerifier {
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
            .and_then(|cert| cert.verify_is_valid_for_subject_name(&self.server_name))
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
/// This node's certificate and key, presented to nodes it connects to and that connect to
/// it, read again for new connections when their files change.
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
                    tracing::info!(cert = %self.cert.display(), "cluster certificate reloaded");
                    *current = Current {
                        key: Arc::new(key),
                        modified,
                    };
                }
                Err(error) => tracing::warn!(
                    cert = %self.cert.display(),
                    "reloading the cluster certificate failed, keeping the one in use: {error:#}"
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
        .with_context(|| format!("reading the cluster certificate `{}`", cert.display()))?;
    if chain.is_empty() {
        bail!(
            "the cluster certificate `{}` has no certificates",
            cert.display()
        );
    }
    let private_key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading the cluster key `{}`", key.display()))?;
    CertifiedKey::from_der(chain, private_key, provider).with_context(|| {
        format!(
            "the cluster key `{}` doesn't match the certificate `{}`",
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
