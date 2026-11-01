mod hylla;

use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use hylla_config::{Backend, Config};
use object_store::{
    BackoffConfig, ClientOptions, ObjectStore, RetryConfig, aws::AmazonS3Builder,
    azure::MicrosoftAzureBuilder,
};

/// The object stores of the configured backends, by backend name.
pub type Stores = HashMap<String, Arc<dyn ObjectStore>>;

// r[impl backend.s3]
/// Creates an object store for every configured backend.
///
/// S3, Azure Blob Storage and other hylla nodes' backends are supported.
pub fn stores(config: &Config) -> anyhow::Result<Stores> {
    config
        .backends
        .iter()
        .map(|(name, backend)| {
            let store = store(backend).with_context(|| format!("backend `{name}`"))?;
            Ok((name.clone(), store))
        })
        .collect()
}

fn store(backend: &Backend) -> anyhow::Result<Arc<dyn ObjectStore>> {
    // r[impl backend.timeouts]
    let timeouts = backend.timeouts();
    let client = ClientOptions::new()
        .with_connect_timeout(timeouts.connect.duration())
        .with_timeout(timeouts.request.duration());
    // r[impl backend.retries]
    // Few retries, within the request timeout: object_store's own default retries for three
    // minutes, which clients would wait through before getting a stale object.
    let retry = RetryConfig {
        backoff: BackoffConfig::default(),
        max_retries: timeouts.retries as usize,
        retry_timeout: timeouts.request.duration(),
    };
    match backend {
        Backend::S3 {
            bucket,
            region,
            endpoint,
            access_key_id,
            secret_access_key,
            ..
        } => {
            let mut builder = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_access_key_id(access_key_id)
                .with_secret_access_key(secret_access_key)
                .with_client_options(client)
                .with_retry(retry);
            if let Some(endpoint) = endpoint {
                // S3 compatible stores are often reached over plain HTTP in development.
                builder = builder
                    .with_endpoint(endpoint)
                    .with_allow_http(endpoint.starts_with("http://"));
            }
            Ok(Arc::new(builder.build()?))
        }
        // r[impl backend.azure-blob]
        Backend::AzureBlob {
            account,
            container,
            endpoint,
            account_key,
            ..
        } => {
            let mut builder = MicrosoftAzureBuilder::new()
                .with_account(account)
                .with_container_name(container)
                .with_access_key(account_key)
                .with_client_options(client)
                .with_retry(retry);
            if let Some(endpoint) = endpoint {
                // Such as Azurite's `http://127.0.0.1:10000/devstoreaccount1`: the endpoint
                // is the account's URL, which blobs are addressed under as
                // `{endpoint}/{container}/{key}`.
                builder = builder
                    .with_endpoint(endpoint.clone())
                    .with_allow_http(endpoint.starts_with("http://"));
            }
            Ok(Arc::new(builder.build()?))
        }
        Backend::Hylla {
            url, backend, tls, ..
        } => Ok(Arc::new(hylla::HyllaStore::new(
            url, backend, tls, timeouts,
        )?)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hylla_config::{Backend, Config};

    fn config(backend: Backend) -> Config {
        Config {
            servers: HashMap::new(),
            backends: HashMap::from([("b".into(), backend)]),
            cache: Default::default(),
            auth: HashMap::new(),
            telemetry: Default::default(),
            cluster: None,
            control: None,
            serve_backends: None,
            health: None,
            http: Default::default(),
        }
    }

    // r[verify backend.azure-blob]
    #[test]
    fn creates_azure_blob_stores() {
        // Azurite's well-known development account key.
        let key = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
        let azure = |endpoint: Option<&str>| {
            config(Backend::AzureBlob {
                account: "devstoreaccount1".into(),
                container: "assets".into(),
                endpoint: endpoint.map(Into::into),
                account_key: key.into(),
                timeouts: Default::default(),
                refresh: None,
            })
        };
        let stores = super::stores(&azure(None)).unwrap();
        assert!(stores["b"].to_string().contains("assets"));
        let stores =
            super::stores(&azure(Some("http://127.0.0.1:10000/devstoreaccount1"))).unwrap();
        assert!(stores["b"].to_string().contains("assets"));
    }

    // r[verify backend.s3]
    #[test]
    fn creates_s3_stores() {
        let s3 = config(Backend::S3 {
            bucket: "assets".into(),
            region: "eu-north-1".into(),
            endpoint: Some("http://localhost:9000".into()),
            access_key_id: "id".into(),
            secret_access_key: "secret".into(),
            timeouts: Default::default(),
            refresh: None,
        });
        let stores = super::stores(&s3).unwrap();
        assert!(stores["b"].to_string().contains("assets"));
    }
}
