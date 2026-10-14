use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, Code, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    HybridCachePolicy,
};
use hylla_config::WritePolicy;
use object_store::{Attribute, GetOptions, GetRange, ObjectStore, path::Path as ObjectPath};
use opentelemetry::KeyValue;
use tracing::{Instrument, Span, field::Empty};

use crate::{
    metrics::{self, METRICS},
    spans,
};

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

/// A key of the cache: an object's chunk or its metadata.
///
/// Metadata is cached with the chunks so it is persisted with them on disk, which lets
/// cached objects be served after a restart without asking the backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CacheKey {
    Chunk(ChunkKey),
    Meta { backend: String, key: String },
}

impl Code for CacheKey {
    fn encode(&self, writer: &mut impl std::io::Write) -> foyer::Result<()> {
        match self {
            CacheKey::Chunk(chunk) => {
                0u8.encode(writer)?;
                chunk.backend.encode(writer)?;
                chunk.key.encode(writer)?;
                chunk.etag.encode(writer)?;
                chunk.index.encode(writer)
            }
            CacheKey::Meta { backend, key } => {
                1u8.encode(writer)?;
                backend.encode(writer)?;
                key.encode(writer)
            }
        }
    }

    fn decode(reader: &mut impl std::io::Read) -> foyer::Result<Self> {
        match u8::decode(reader)? {
            0 => Ok(CacheKey::Chunk(ChunkKey {
                backend: String::decode(reader)?,
                key: String::decode(reader)?,
                etag: String::decode(reader)?,
                index: u64::decode(reader)?,
            })),
            1 => Ok(CacheKey::Meta {
                backend: String::decode(reader)?,
                key: String::decode(reader)?,
            }),
            tag => Err(foyer::Error::io_error(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown cache key tag {tag}"),
            ))),
        }
    }

    fn estimated_size(&self) -> usize {
        1 + match self {
            CacheKey::Chunk(chunk) => {
                chunk.backend.estimated_size()
                    + chunk.key.estimated_size()
                    + chunk.etag.estimated_size()
                    + chunk.index.estimated_size()
            }
            CacheKey::Meta { backend, key } => backend.estimated_size() + key.estimated_size(),
        }
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

/// Metadata as cached, with when it was fetched. Wall clock time, since it must survive
/// restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredMeta {
    meta: ObjectMeta,
    fetched: SystemTime,
}

impl StoredMeta {
    fn to_bytes(&self) -> Bytes {
        let mut bytes = Vec::new();
        let write = |bytes: &mut Vec<u8>| -> foyer::Result<()> {
            self.meta.size.encode(bytes)?;
            encode_option(&self.meta.etag, bytes)?;
            nanos(self.meta.last_modified).encode(bytes)?;
            encode_option(&self.meta.content_type, bytes)?;
            nanos(self.fetched).encode(bytes)
        };
        write(&mut bytes).expect("writing to a Vec doesn't fail");
        bytes.into()
    }

    fn from_bytes(mut bytes: &[u8]) -> foyer::Result<Self> {
        let reader = &mut bytes;
        Ok(Self {
            meta: ObjectMeta {
                size: u64::decode(reader)?,
                etag: decode_option(reader)?,
                last_modified: from_nanos(u128::decode(reader)?),
                content_type: decode_option(reader)?,
            },
            fetched: from_nanos(u128::decode(reader)?),
        })
    }

    fn age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.fetched)
            .unwrap_or_default()
    }
}

fn encode_option(value: &Option<String>, writer: &mut impl std::io::Write) -> foyer::Result<()> {
    value.is_some().encode(writer)?;
    match value {
        Some(value) => value.encode(writer),
        None => Ok(()),
    }
}

fn decode_option(reader: &mut impl std::io::Read) -> foyer::Result<Option<String>> {
    Ok(match bool::decode(reader)? {
        true => Some(String::decode(reader)?),
        false => None,
    })
}

fn nanos(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn from_nanos(nanos: u128) -> SystemTime {
    let secs = (nanos / 1_000_000_000) as u64;
    UNIX_EPOCH + Duration::new(secs, (nanos % 1_000_000_000) as u32)
}

/// The cache of object chunks and metadata, in memory and optionally on disk.
pub struct ObjectCache {
    entries: HybridCache<CacheKey, Bytes>,
    chunk_size: u64,
    metadata_ttl: Duration,
    stale_while_revalidate: Duration,
    stale_if_error: Duration,
    /// The background revalidations of objects, by backend and key.
    revalidations: Mutex<HashMap<(String, String), Revalidation>>,
    /// Where metadata fetched from backends, and preloaded objects, are reported, when
    /// they're shared with a cluster.
    events: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<CacheEvent>>,
}

/// Something worth telling other nodes about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheEvent {
    /// Metadata was fetched from the backend.
    Fetched(SharedMeta),
    /// An object was preloaded into the cache.
    Preloaded(SharedMeta),
}

/// An object's metadata, as shared between nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedMeta {
    pub backend: String,
    pub key: String,
    pub meta: ObjectMeta,
    /// When it was fetched from the backend.
    pub fetched: SystemTime,
}

/// The state of an object's background revalidation.
enum Revalidation {
    Running,
    /// Failed at this time; not retried until the back-off passed.
    Failed(Instant),
}

/// How an object listed by the backend compares to the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CachedState {
    NotCached,
    /// Cached with the listed ETag and size.
    Unchanged(ObjectMeta),
    Changed,
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
        // r[impl telemetry.metrics.foyer]
        let mut builder = HybridCacheBuilder::new()
            .with_name("hylla")
            .with_metrics_registry(Box::new(
                mixtrics::registry::opentelemetry_0_33::OpenTelemetryMetricsRegistry::new(
                    metrics::meter(),
                ),
            ))
            .with_policy(policy)
            .memory(memory)
            .with_weighter(|key: &CacheKey, value: &Bytes| key.estimated_size() + value.len())
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
            entries: builder.build().await?,
            chunk_size: config.chunk_size(),
            metadata_ttl: config.metadata_ttl.duration(),
            stale_while_revalidate: config.stale_while_revalidate.duration(),
            stale_if_error: config.stale_if_error.duration(),
            revalidations: Mutex::default(),
            events: std::sync::OnceLock::new(),
        })
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    // r[impl cache.metadata]
    // r[impl cache.stale-if-error]
    // r[impl cache.not-found]
    /// The metadata of an object.
    ///
    /// Cached metadata younger than the TTL is used as is. Metadata within the
    /// stale-while-revalidate window past it is used too, while it is revalidated in the
    /// background. Older metadata is refreshed from the backend before it is used; if the
    /// backend fails, metadata within the stale-if-error window is used anyway, so cached
    /// objects stay available during outages. A missing object is forgotten.
    pub async fn metadata(
        self: &Arc<Self>,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
    ) -> Result<ObjectMeta, ReadError> {
        // r[impl telemetry.traces.spans]
        let span = tracing::info_span!(
            "metadata",
            hylla.backend = backend,
            hylla.result = Empty,
            hylla.object = Empty,
        );
        spans::record_object(&span, path.as_ref());
        self.lookup_metadata(backend, store, path)
            .instrument(span)
            .await
    }

    async fn lookup_metadata(
        self: &Arc<Self>,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
    ) -> Result<ObjectMeta, ReadError> {
        // r[impl telemetry.metrics.cache]
        let record = |result: &'static str| {
            Span::current().record("hylla.result", result);
            METRICS.metadata.add(
                1,
                &[
                    KeyValue::new("hylla.backend", backend.to_string()),
                    KeyValue::new("hylla.result", result),
                ],
            );
        };
        let cached = self.cached_metadata(backend, path).await;
        if let Some(cached) = &cached {
            if cached.age() < self.metadata_ttl {
                record("fresh");
                return Ok(cached.meta.clone());
            }
            // r[impl cache.stale-while-revalidate]
            if cached.age() < self.metadata_ttl + self.stale_while_revalidate {
                record("stale_while_revalidate");
                self.revalidate_in_background(backend, store, path);
                return Ok(cached.meta.clone());
            }
        }
        match self.fetch_metadata(backend, store.as_ref(), path).await {
            Err(ReadError::Backend(error)) => match cached {
                Some(cached) if cached.age() < self.metadata_ttl + self.stale_if_error => {
                    tracing::warn!(
                        backend,
                        %path,
                        age = ?cached.age(),
                        "serving stale metadata, the backend failed: {error:#}"
                    );
                    record("stale_if_error");
                    Ok(cached.meta)
                }
                _ => {
                    record("error");
                    Err(ReadError::Backend(error))
                }
            },
            Err(ReadError::NotFound) => {
                record("not_found");
                Err(ReadError::NotFound)
            }
            result => {
                record("fetched");
                result
            }
        }
    }

    // r[impl cache.revalidation-debounce]
    /// Revalidates an object's metadata in a background task, unless a revalidation of it is
    /// already running, or failed within the back-off (the metadata TTL, at least a second).
    fn revalidate_in_background(
        self: &Arc<Self>,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
    ) {
        let key = (backend.to_string(), path.to_string());
        let backoff = self.metadata_ttl.max(Duration::from_secs(1));
        {
            let mut revalidations = self.revalidations.lock().expect("not poisoned");
            match revalidations.get(&key) {
                Some(Revalidation::Running) => return,
                Some(Revalidation::Failed(at)) if at.elapsed() < backoff => return,
                _ => {}
            }
            revalidations.insert(key.clone(), Revalidation::Running);
        }
        let cache = self.clone();
        let path = path.clone();
        let span = tracing::info_span!("revalidate", hylla.backend = backend, hylla.object = Empty);
        spans::record_object(&span, path.as_ref());
        tokio::spawn(
            async move {
                let (backend, _) = &key;
                let result = cache.fetch_metadata(backend, store.as_ref(), &path).await;
                let mut revalidations = cache.revalidations.lock().expect("not poisoned");
                match result {
                    Ok(_) | Err(ReadError::NotFound) => {
                        revalidations.remove(&key);
                    }
                    Err(ReadError::Backend(error)) => {
                        tracing::warn!(backend, %path, "background revalidation failed: {error:#}");
                        revalidations.insert(key.clone(), Revalidation::Failed(Instant::now()));
                    }
                }
            }
            .instrument(span),
        );
    }

    fn meta_key(backend: &str, path: &ObjectPath) -> CacheKey {
        CacheKey::Meta {
            backend: backend.to_string(),
            key: path.to_string(),
        }
    }

    /// The cached metadata of an object, whatever its age.
    async fn cached_metadata(&self, backend: &str, path: &ObjectPath) -> Option<StoredMeta> {
        match self.entries.get(&Self::meta_key(backend, path)).await {
            Ok(entry) => entry.and_then(|entry| StoredMeta::from_bytes(entry.value()).ok()),
            Err(error) => {
                tracing::warn!(%path, "reading cached metadata: {error}");
                None
            }
        }
    }

    /// Caches `meta` as fetched now.
    fn store_metadata(&self, backend: &str, path: &ObjectPath, meta: ObjectMeta) {
        let stored = StoredMeta {
            meta,
            fetched: SystemTime::now(),
        };
        self.entries
            .insert(Self::meta_key(backend, path), stored.to_bytes());
    }

    /// Reports events to `sender`, for sharing them with a cluster.
    pub(crate) fn report_to(&self, sender: tokio::sync::mpsc::UnboundedSender<CacheEvent>) {
        let _ = self.events.set(sender);
    }

    fn report(&self, event: impl FnOnce() -> CacheEvent) {
        if let Some(events) = self.events.get() {
            let _ = events.send(event());
        }
    }

    // r[impl cluster.apply-metadata]
    /// Caches metadata another node fetched, unless the cached metadata is at least as new.
    /// It keeps its original fetch time, so it expires as if fetched here.
    pub(crate) async fn apply_metadata(&self, shared: SharedMeta) {
        let path = ObjectPath::from(shared.key.as_str());
        if let Some(cached) = self.cached_metadata(&shared.backend, &path).await
            && cached.fetched >= shared.fetched
        {
            return;
        }
        let stored = StoredMeta {
            meta: shared.meta,
            fetched: shared.fetched,
        };
        self.entries
            .insert(Self::meta_key(&shared.backend, &path), stored.to_bytes());
    }

    /// Reports that an object was preloaded.
    pub(crate) fn report_preloaded(&self, backend: &str, path: &ObjectPath, meta: &ObjectMeta) {
        self.report(|| {
            CacheEvent::Preloaded(SharedMeta {
                backend: backend.to_string(),
                key: path.to_string(),
                meta: meta.clone(),
                fetched: SystemTime::now(),
            })
        });
    }

    /// A chunk, if it is cached; never fetched from the backend.
    pub(crate) async fn cached_chunk(&self, key: &ChunkKey) -> Option<Bytes> {
        match self.entries.get(&CacheKey::Chunk(key.clone())).await {
            Ok(entry) => entry.map(|entry| entry.value().clone()),
            Err(error) => {
                tracing::warn!(key = key.key, "reading a cached chunk: {error}");
                None
            }
        }
    }

    /// Caches a chunk another node had.
    pub(crate) fn insert_chunk(&self, key: ChunkKey, bytes: Bytes) {
        self.entries.insert(CacheKey::Chunk(key), bytes);
    }

    /// The number of chunks an object of `size` bytes has.
    pub(crate) fn chunk_count(&self, size: u64) -> u64 {
        size.div_ceil(self.chunk_size)
    }

    /// Fetches the metadata of an object from the backend and caches it, forgetting the
    /// object if it is missing.
    pub(crate) async fn fetch_metadata(
        &self,
        backend: &str,
        store: &dyn ObjectStore,
        path: &ObjectPath,
    ) -> Result<ObjectMeta, ReadError> {
        let options = GetOptions {
            head: true,
            ..Default::default()
        };
        let result =
            spans::backend_call(backend, "head", Some(path), store.get_opts(path, options)).await;
        match result {
            Ok(result) => {
                let meta = ObjectMeta {
                    size: result.meta.size,
                    etag: result.meta.e_tag.clone(),
                    last_modified: SystemTime::from(result.meta.last_modified),
                    content_type: result
                        .attributes
                        .get(&Attribute::ContentType)
                        .map(|value| value.to_string()),
                };
                self.store_metadata(backend, path, meta.clone());
                self.report(|| {
                    CacheEvent::Fetched(SharedMeta {
                        backend: backend.to_string(),
                        key: path.to_string(),
                        meta: meta.clone(),
                        fetched: SystemTime::now(),
                    })
                });
                Ok(meta)
            }
            Err(object_store::Error::NotFound { .. }) => {
                self.forget(backend, path);
                Err(ReadError::NotFound)
            }
            Err(error) => Err(ReadError::Backend(error.into())),
        }
    }

    /// Forgets the metadata of an object.
    pub(crate) fn forget(&self, backend: &str, path: &ObjectPath) {
        self.entries.remove(&Self::meta_key(backend, path));
    }

    /// How a listed object compares to the cache.
    pub(crate) async fn cached_state(
        &self,
        backend: &str,
        path: &ObjectPath,
        listed: &object_store::ObjectMeta,
    ) -> CachedState {
        match self.cached_metadata(backend, path).await {
            None => CachedState::NotCached,
            Some(cached) if cached.meta.etag == listed.e_tag && cached.meta.size == listed.size => {
                CachedState::Unchanged(cached.meta)
            }
            Some(_) => CachedState::Changed,
        }
    }

    /// Whether the metadata of an object is cached.
    pub(crate) async fn is_cached(&self, backend: &str, path: &ObjectPath) -> bool {
        self.cached_metadata(backend, path).await.is_some()
    }

    /// Marks cached metadata that is still current as fetched now.
    pub(crate) fn touch_metadata(&self, backend: &str, path: &ObjectPath, meta: ObjectMeta) {
        self.store_metadata(backend, path, meta);
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
        self.fetch_chunk(backend, store, path, meta, index)
            .await
            .map(|(bytes, _)| bytes)
    }

    /// Fetches every chunk of an object that isn't cached, returning how many bytes were
    /// fetched from the backend.
    pub(crate) async fn preload(
        &self,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
        meta: &ObjectMeta,
    ) -> Result<u64, ReadError> {
        let mut fetched = 0;
        for index in 0..meta.size.div_ceil(self.chunk_size) {
            let (bytes, was_fetched) = self
                .fetch_chunk(backend, store.clone(), path, meta, index)
                .await?;
            if was_fetched {
                fetched += bytes.len() as u64;
            }
        }
        Ok(fetched)
    }

    /// A chunk, and whether it had to be fetched from the backend.
    async fn fetch_chunk(
        &self,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
        meta: &ObjectMeta,
        index: u64,
    ) -> Result<(Bytes, bool), ReadError> {
        let span = tracing::info_span!(
            "chunk",
            hylla.backend = backend,
            hylla.chunk = index,
            hylla.result = Empty,
            hylla.object = Empty,
        );
        spans::record_object(&span, path.as_ref());
        self.lookup_chunk(backend, store, path, meta, index)
            .instrument(span)
            .await
    }

    async fn lookup_chunk(
        &self,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
        meta: &ObjectMeta,
        index: u64,
    ) -> Result<(Bytes, bool), ReadError> {
        let start = index * self.chunk_size;
        let end = (start + self.chunk_size).min(meta.size);
        let key = CacheKey::Chunk(ChunkKey {
            backend: backend.to_string(),
            key: path.to_string(),
            etag: meta.etag.clone().unwrap_or_default(),
            index,
        });
        let options = GetOptions {
            range: Some(GetRange::Bounded(start..end)),
            // Only read the version the metadata describes.
            if_match: meta.etag.clone(),
            ..Default::default()
        };
        let changed = Arc::new(AtomicBool::new(false));
        let fetched = Arc::new(AtomicBool::new(false));
        let (fetch_changed, fetch_fetched) = (changed.clone(), fetched.clone());
        let fetch_path = path.clone();
        let fetch_backend = backend.to_string();
        // foyer may fetch in a task of its own, outside this span.
        let span = Span::current();
        let entry = self
            .entries
            .get_or_fetch(&key, move || {
                async move {
                    fetch_fetched.store(true, Ordering::Relaxed);
                    let get = async { store.get_opts(&fetch_path, options).await?.bytes().await };
                    let result =
                        spans::backend_call(&fetch_backend, "get", Some(&fetch_path), get).await;
                    let bytes = match result {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let gone = matches!(
                                error,
                                object_store::Error::Precondition { .. }
                                    | object_store::Error::NotFound { .. }
                            );
                            fetch_changed.store(gone, Ordering::Relaxed);
                            return Err(error);
                        }
                    };
                    METRICS.backend_bytes.add(
                        bytes.len() as u64,
                        &[KeyValue::new("hylla.backend", fetch_backend.clone())],
                    );
                    Ok::<_, object_store::Error>(bytes)
                }
                .instrument(span)
            })
            .await;
        let record = |result: &'static str| {
            Span::current().record("hylla.result", result);
            METRICS.chunks.add(
                1,
                &[
                    KeyValue::new("hylla.backend", backend.to_string()),
                    KeyValue::new("hylla.result", result),
                ],
            );
        };
        match entry {
            Ok(entry) => {
                let fetched = fetched.load(Ordering::Relaxed);
                record(if fetched { "miss" } else { "hit" });
                Ok((entry.value().clone(), fetched))
            }
            Err(error) => {
                record("error");
                // r[impl cache.changed]
                // Only an object that changed or went away invalidates its metadata; other
                // failures, such as an outage, keep it so cached chunks stay servable.
                if changed.load(Ordering::Relaxed) {
                    self.forget(backend, path);
                }
                Err(ReadError::Backend(anyhow::Error::new(error)))
            }
        }
    }

    /// Flushes the memory tier to disk.
    pub async fn close(&self) -> anyhow::Result<()> {
        Ok(self.entries.close().await?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use foyer::Code;

    use super::{CacheKey, ChunkKey, ObjectMeta, StoredMeta};

    #[test]
    fn cache_keys_round_trip() {
        let keys = [
            CacheKey::Chunk(ChunkKey {
                backend: "assets".into(),
                key: "css/app.css".into(),
                etag: "\"abc\"".into(),
                index: 7,
            }),
            CacheKey::Meta {
                backend: "assets".into(),
                key: "css/app.css".into(),
            },
        ];
        for key in keys {
            let mut encoded = Vec::new();
            key.encode(&mut encoded).unwrap();
            assert_eq!(encoded.len(), key.estimated_size());
            assert_eq!(CacheKey::decode(&mut encoded.as_slice()).unwrap(), key);
        }
    }

    #[test]
    fn metadata_round_trips() {
        let at = |secs, nanos| UNIX_EPOCH + Duration::new(secs, nanos);
        for (etag, content_type) in [(Some("\"abc\""), Some("text/css")), (None, None)] {
            let stored = StoredMeta {
                meta: ObjectMeta {
                    size: 3000,
                    etag: etag.map(Into::into),
                    last_modified: at(1_700_000_000, 123),
                    content_type: content_type.map(Into::into),
                },
                fetched: at(1_800_000_000, 456),
            };
            assert_eq!(StoredMeta::from_bytes(&stored.to_bytes()).unwrap(), stored);
        }
    }
}
