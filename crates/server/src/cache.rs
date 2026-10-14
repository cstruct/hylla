use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, Code, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    HybridCachePolicy,
};
use hylla_config::WritePolicy;
use object_store::{Attribute, GetOptions, GetRange, ObjectStore, path::Path as ObjectPath};

/// Identifies a chunk of an object.
///
/// The ETag is part of the key, so a changed object misses instead of serving stale data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub backend: String,
    pub key: String,
    pub etag: String,
    pub index: u64,
}

impl Code for ChunkKey {
    fn encode(&self, writer: &mut impl std::io::Write) -> foyer::Result<()> {
        self.backend.encode(writer)?;
        self.key.encode(writer)?;
        self.etag.encode(writer)?;
        self.index.encode(writer)
    }

    fn decode(reader: &mut impl std::io::Read) -> foyer::Result<Self> {
        Ok(Self {
            backend: String::decode(reader)?,
            key: String::decode(reader)?,
            etag: String::decode(reader)?,
            index: u64::decode(reader)?,
        })
    }

    fn estimated_size(&self) -> usize {
        self.backend.estimated_size()
            + self.key.estimated_size()
            + self.etag.estimated_size()
            + self.index.estimated_size()
    }
}

/// What the cache knows about an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: SystemTime,
    pub content_type: Option<String>,
}

/// The chunk cache, in memory and optionally on disk, and the object metadata cache.
pub struct ObjectCache {
    chunks: HybridCache<ChunkKey, Bytes>,
    metadata: foyer::Cache<(String, String), Arc<(ObjectMeta, Instant)>>,
    chunk_size: u64,
    metadata_ttl: Duration,
}

/// Failure to read an object.
#[derive(Debug)]
pub enum ReadError {
    NotFound,
    Backend(anyhow::Error),
}

impl ObjectCache {
    // r[impl cache.ram]
    // r[impl cache.disk]
    pub async fn new(config: &hylla_config::Cache) -> anyhow::Result<Self> {
        let memory = usize::try_from(config.memory.capacity_mib * 1024 * 1024)?;
        let policy = match config.write_policy {
            WritePolicy::OnEviction => HybridCachePolicy::WriteOnEviction,
            WritePolicy::OnInsertion => HybridCachePolicy::WriteOnInsertion,
        };
        let mut builder = HybridCacheBuilder::new()
            .with_name("hylla")
            .with_policy(policy)
            .memory(memory)
            .with_weighter(|key: &ChunkKey, value: &Bytes| key.estimated_size() + value.len())
            .storage();
        if let Some(disk) = &config.disk {
            std::fs::create_dir_all(&disk.path)
                .with_context(|| format!("creating {}", disk.path.display()))?;
            let device = FsDeviceBuilder::new(&disk.path)
                .with_capacity(usize::try_from(disk.capacity_mib * 1024 * 1024)?)
                .build()?;
            let block_size = usize::try_from(disk.block_size_mib)? * 1024 * 1024;
            builder = builder
                .with_engine_config(BlockEngineConfig::new(device).with_block_size(block_size));
        }
        Ok(Self {
            chunks: builder.build().await?,
            metadata: foyer::CacheBuilder::new(64 * 1024).build(),
            chunk_size: config.chunk_size(),
            metadata_ttl: config.metadata_ttl(),
        })
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    // r[impl cache.metadata]
    /// The metadata of an object, cached for the configured TTL.
    pub async fn metadata(
        &self,
        backend: &str,
        store: &dyn ObjectStore,
        path: &ObjectPath,
    ) -> Result<ObjectMeta, ReadError> {
        let key = (backend.to_string(), path.to_string());
        if let Some(entry) = self.metadata.get(&key) {
            let (meta, fetched) = entry.value().as_ref();
            if fetched.elapsed() < self.metadata_ttl {
                return Ok(meta.clone());
            }
        }
        let options = GetOptions {
            head: true,
            ..Default::default()
        };
        let result = store.get_opts(path, options).await.map_err(read_error)?;
        let meta = ObjectMeta {
            size: result.meta.size,
            etag: result.meta.e_tag.clone(),
            last_modified: SystemTime::from(result.meta.last_modified),
            content_type: result
                .attributes
                .get(&Attribute::ContentType)
                .map(|value| value.to_string()),
        };
        self.metadata
            .insert(key, Arc::new((meta.clone(), Instant::now())));
        Ok(meta)
    }

    /// Forgets the metadata of an object, such as after it changed under us.
    pub fn forget(&self, backend: &str, path: &ObjectPath) {
        self.metadata
            .remove(&(backend.to_string(), path.to_string()));
    }

    // r[impl cache.chunks]
    /// Chunk `index` of an object, from the cache or fetched from the store.
    pub async fn chunk(
        &self,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
        meta: &ObjectMeta,
        index: u64,
    ) -> Result<Bytes, ReadError> {
        let start = index * self.chunk_size;
        let end = (start + self.chunk_size).min(meta.size);
        let key = ChunkKey {
            backend: backend.to_string(),
            key: path.to_string(),
            etag: meta.etag.clone().unwrap_or_default(),
            index,
        };
        let options = GetOptions {
            range: Some(GetRange::Bounded(start..end)),
            // Only read the version the metadata describes.
            if_match: meta.etag.clone(),
            ..Default::default()
        };
        let fetch_path = path.clone();
        let entry = self
            .chunks
            .get_or_fetch(&key, move || async move {
                let result = store.get_opts(&fetch_path, options).await?;
                Ok::<_, object_store::Error>(result.bytes().await?)
            })
            .await;
        match entry {
            Ok(entry) => Ok(entry.value().clone()),
            Err(error) => {
                // The object may have changed or gone; refetch its metadata next time.
                self.forget(backend, path);
                Err(ReadError::Backend(anyhow::Error::new(error)))
            }
        }
    }

    /// Flushes the disk tier.
    pub async fn close(&self) -> anyhow::Result<()> {
        Ok(self.chunks.close().await?)
    }
}

fn read_error(error: object_store::Error) -> ReadError {
    match error {
        object_store::Error::NotFound { .. } => ReadError::NotFound,
        error => ReadError::Backend(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use foyer::Code;

    use super::ChunkKey;

    #[test]
    fn chunk_keys_round_trip() {
        let key = ChunkKey {
            backend: "assets".into(),
            key: "css/app.css".into(),
            etag: "\"abc\"".into(),
            index: 7,
        };
        let mut encoded = Vec::new();
        key.encode(&mut encoded).unwrap();
        assert_eq!(encoded.len(), key.estimated_size());
        assert_eq!(ChunkKey::decode(&mut encoded.as_slice()).unwrap(), key);
    }
}
