use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, bail};
use hylla_config::{Backend, Config};
use object_store::{ObjectStore, aws::AmazonS3Builder};

/// The object stores of the configured backends, by backend name.
pub type Stores = HashMap<String, Arc<dyn ObjectStore>>;

// r[impl backend.s3]
/// Creates an object store for every configured backend.
///
/// Only S3 backends are supported so far; other types fail with an error.
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
                .with_secret_access_key(secret_access_key);
            if let Some(endpoint) = endpoint {
                // S3 compatible stores are often reached over plain HTTP in development.
                builder = builder
                    .with_endpoint(endpoint)
                    .with_allow_http(endpoint.starts_with("http://"));
            }
            Ok(Arc::new(builder.build()?))
        }
        Backend::Swift { .. } => bail!("swift backends are not supported yet"),
        Backend::AzureBlob { .. } => bail!("azure_blob backends are not supported yet"),
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
        }
    }

    // r[verify backend.s3]
    #[test]
    fn creates_s3_stores_and_rejects_unsupported_types() {
        let s3 = config(Backend::S3 {
            bucket: "assets".into(),
            region: "eu-north-1".into(),
            endpoint: Some("http://localhost:9000".into()),
            access_key_id: "id".into(),
            secret_access_key: "secret".into(),
            refresh: None,
        });
        let stores = super::stores(&s3).unwrap();
        assert!(stores["b"].to_string().contains("assets"));

        let swift = config(Backend::Swift {
            auth_url: "https://swift.example.com".into(),
            container: "c".into(),
            region: None,
            username: "u".into(),
            password: "p".into(),
            refresh: None,
        });
        let error = super::stores(&swift).err().unwrap();
        assert_eq!(
            format!("{error:#}"),
            "backend `b`: swift backends are not supported yet"
        );
    }
}
