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
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
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
    Meta {
        backend: String,
        key: String,
    },
    /// Since when an object's metadata isn't cached: it was forgotten, or found missing.
    Absent {
        backend: String,
        key: String,
    },
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
            CacheKey::Absent { backend, key } => {
                2u8.encode(writer)?;
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
            2 => Ok(CacheKey::Absent {
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
            CacheKey::Meta { backend, key } | CacheKey::Absent { backend, key } => {
                backend.estimated_size() + key.estimated_size()
            }
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
    pub headers: ObjectHeaders,
}

// r[impl server.object-headers]
/// The headers an object was stored with, besides its type, sent with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectHeaders {
    pub cache_control: Option<String>,
    pub content_encoding: Option<String>,
    pub content_disposition: Option<String>,
    pub content_language: Option<String>,
}

impl ObjectHeaders {
    /// The headers among an object's attributes.
    pub(crate) fn from_attributes(attributes: &object_store::Attributes) -> Self {
        let text = |attribute| attributes.get(&attribute).map(|value| value.to_string());
        Self {
            cache_control: text(Attribute::CacheControl),
            content_encoding: text(Attribute::ContentEncoding),
            content_disposition: text(Attribute::ContentDisposition),
            content_language: text(Attribute::ContentLanguage),
        }
    }

    fn fields(&self) -> [&Option<String>; 4] {
        [
            &self.cache_control,
            &self.content_encoding,
            &self.content_disposition,
            &self.content_language,
        ]
    }
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
            nanos(self.fetched).encode(bytes)?;
            // Last, so metadata cached before these were kept fails to decode, and is
            // fetched again, rather than decoding as something else.
            for field in self.meta.headers.fields() {
                encode_option(field, bytes)?;
            }
            Ok(())
        };
        write(&mut bytes).expect("writing to a Vec doesn't fail");
        bytes.into()
    }

    fn from_bytes(mut bytes: &[u8]) -> foyer::Result<Self> {
        let reader = &mut bytes;
        let size = u64::decode(reader)?;
        let etag = decode_option(reader)?;
        let last_modified = from_nanos(u128::decode(reader)?);
        let content_type = decode_option(reader)?;
        let fetched = from_nanos(u128::decode(reader)?);
        let headers = ObjectHeaders {
            cache_control: decode_option(reader)?,
            content_encoding: decode_option(reader)?,
            content_disposition: decode_option(reader)?,
            content_language: decode_option(reader)?,
        };
        Ok(Self {
            meta: ObjectMeta {
                size,
                etag,
                last_modified,
                content_type,
                headers,
            },
            fetched,
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
    not_found_ttl: Duration,
    /// The background revalidations of objects, and failed fetches, by backend and key.
    revalidations: Mutex<HashMap<(String, String), Revalidation>>,
    /// The metadata fetches in flight, by backend and key, shared by the requests waiting on
    /// them.
    fetches: Mutex<HashMap<(String, String), MetadataFetch>>,
    /// Where metadata fetched from backends, preloaded objects and objects changed through
    /// this node are reported, when they're shared with a cluster.
    events: std::sync::OnceLock<tokio::sync::mpsc::Sender<CacheEvent>>,
}

/// Something worth telling other nodes about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheEvent {
    /// Metadata was fetched from the backend.
    Fetched(SharedMeta),
    /// An object was preloaded into the cache.
    Preloaded(SharedMeta),
    /// An object was changed here, so its metadata must be forgotten everywhere.
    Forgotten { backend: String, key: String },
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
    /// A fetch failed at this time; not retried until the back-off passed.
    Failed(Instant),
}

/// Why an object's metadata isn't cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Absence {
    /// It was forgotten, as after the object changed.
    Forgotten,
    /// The backend said the object is missing.
    Missing,
}

/// A metadata fetch in flight.
type MetadataFetch = Shared<BoxFuture<'static, Result<ObjectMeta, FetchError>>>;

/// Why a shared metadata fetch failed: a `ReadError` that can be cloned for every request
/// waiting on it.
#[derive(Debug, Clone)]
enum FetchError {
    NotFound,
    Backend(Arc<anyhow::Error>),
}

impl From<FetchError> for ReadError {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::NotFound => ReadError::NotFound,
            FetchError::Backend(error) => ReadError::Backend(anyhow::anyhow!("{error:#}")),
        }
    }
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
    /// The object changed since its metadata was cached, which is now forgotten.
    Changed(anyhow::Error),
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
                .with_context(|| format!("creating {}", disk.path))?;
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
            not_found_ttl: config.not_found_ttl.duration(),
            revalidations: Mutex::default(),
            fetches: Mutex::default(),
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
    /// objects stay available during outages. A missing object is forgotten. With the
    /// metadata comes its age: how long ago it was fetched from the backend.
    pub async fn metadata(
        self: &Arc<Self>,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
    ) -> Result<(ObjectMeta, Duration), ReadError> {
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
    ) -> Result<(ObjectMeta, Duration), ReadError> {
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
        if cached.is_none()
            && let Some((since, Absence::Missing)) = self.absent(backend, path).await
            && SystemTime::now()
                .duration_since(since)
                .is_ok_and(|missing_for| missing_for < self.not_found_ttl)
        {
            record("not_found");
            return Err(ReadError::NotFound);
        }
        if let Some(cached) = &cached {
            if cached.age() < self.metadata_ttl {
                record("fresh");
                return Ok((cached.meta.clone(), cached.age()));
            }
            // r[impl cache.stale-while-revalidate]
            if cached.age() < self.metadata_ttl + self.stale_while_revalidate {
                record("stale_while_revalidate");
                self.revalidate_in_background(backend, store, path);
                return Ok((cached.meta.clone(), cached.age()));
            }
            // r[impl cache.failure-backoff]
            if cached.age() < self.metadata_ttl + self.stale_if_error
                && self.failed_recently(backend, path)
            {
                record("stale_if_error");
                return Ok((cached.meta.clone(), cached.age()));
            }
        }
        match self.shared_fetch(backend, store, path).await {
            Err(ReadError::Backend(error)) => match cached {
                Some(cached) if cached.age() < self.metadata_ttl + self.stale_if_error => {
                    tracing::warn!(
                        backend,
                        %path,
                        age = ?cached.age(),
                        "serving stale metadata, the backend failed: {error:#}"
                    );
                    record("stale_if_error");
                    let age = cached.age();
                    Ok((cached.meta, age))
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
                result.map(|meta| (meta, Duration::ZERO))
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
        let backoff = self.backoff();
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
                if let Err(ReadError::Backend(error)) =
                    cache.shared_fetch(backend, store, &path).await
                {
                    tracing::warn!(backend, %path, "background revalidation failed: {error:#}");
                }
            }
            .instrument(span),
        );
    }

    /// How long after a failed fetch an object's backend isn't asked again.
    fn backoff(&self) -> Duration {
        self.metadata_ttl.max(Duration::from_secs(1))
    }

    /// Whether fetching the metadata of an object failed within the back-off.
    fn failed_recently(&self, backend: &str, path: &ObjectPath) -> bool {
        let key = (backend.to_string(), path.to_string());
        matches!(
            self.revalidations.lock().expect("not poisoned").get(&key),
            Some(Revalidation::Failed(at)) if at.elapsed() < self.backoff()
        )
    }

    // r[impl cache.coalesce]
    /// Fetches the metadata of an object, sharing the fetch with the other requests for it
    /// in flight, and remembering whether it failed.
    async fn shared_fetch(
        self: &Arc<Self>,
        backend: &str,
        store: Arc<dyn ObjectStore>,
        path: &ObjectPath,
    ) -> Result<ObjectMeta, ReadError> {
        let key = (backend.to_string(), path.to_string());
        let fetch = {
            let mut fetches = self.fetches.lock().expect("not poisoned");
            match fetches.get(&key) {
                Some(fetch) => fetch.clone(),
                None => {
                    let cache = self.clone();
                    let path = path.clone();
                    let fetching = key.clone();
                    let fetch = async move {
                        let (backend, _) = &fetching;
                        let result = cache.fetch_metadata(backend, store.as_ref(), &path).await;
                        cache
                            .fetches
                            .lock()
                            .expect("not poisoned")
                            .remove(&fetching);
                        cache.record_outcome(&fetching, result.as_ref().err());
                        result.map_err(|error| match error {
                            ReadError::NotFound => FetchError::NotFound,
                            ReadError::Backend(error) | ReadError::Changed(error) => {
                                FetchError::Backend(Arc::new(error))
                            }
                        })
                    }
                    .in_current_span()
                    .boxed()
                    .shared();
                    fetches.insert(key, fetch.clone());
                    fetch
                }
            }
        };
        fetch.await.map_err(ReadError::from)
    }

    /// Remembers whether fetching an object's metadata failed, for the back-off, forgetting
    /// failures whose back-off passed.
    fn record_outcome(&self, key: &(String, String), error: Option<&ReadError>) {
        let mut revalidations = self.revalidations.lock().expect("not poisoned");
        match error {
            Some(ReadError::Backend(_)) => {
                let backoff = self.backoff();
                revalidations.retain(|_, revalidation| match revalidation {
                    Revalidation::Running => true,
                    Revalidation::Failed(at) => at.elapsed() < backoff,
                });
                revalidations.insert(key.clone(), Revalidation::Failed(Instant::now()));
            }
            _ => {
                revalidations.remove(key);
            }
        }
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
    pub(crate) fn report_to(&self, sender: tokio::sync::mpsc::Sender<CacheEvent>) {
        let _ = self.events.set(sender);
    }

    // r[impl cluster.queue]
    /// Reports an event, dropping it when the queue of events to share is full, as when
    /// other nodes are slow to take them.
    fn report(&self, event: impl FnOnce() -> CacheEvent) {
        let Some(events) = self.events.get() else {
            return;
        };
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(event)) = events.try_send(event()) {
            let kind = match &event {
                CacheEvent::Fetched(_) => "fetched",
                CacheEvent::Preloaded(_) => "preloaded",
                CacheEvent::Forgotten { backend, key } => {
                    tracing::warn!(
                        backend,
                        key,
                        "the queue of updates for other nodes is full: they won't forget the \
                         object until its metadata expires"
                    );
                    "forgotten"
                }
            };
            METRICS
                .cluster_dropped
                .add(1, &[KeyValue::new("hylla.kind", kind)]);
        }
    }

    // r[impl cluster.apply-metadata]
    // r[impl cluster.forget-tombstones]
    /// Caches metadata another node fetched, unless the cached metadata is at least as new,
    /// or the object was forgotten since. It keeps its original fetch time, so it expires as
    /// if fetched here.
    pub(crate) async fn apply_metadata(&self, shared: SharedMeta) {
        let path = ObjectPath::from(shared.key.as_str());
        if let Some(cached) = self.cached_metadata(&shared.backend, &path).await
            && cached.fetched >= shared.fetched
        {
            return;
        }
        if let Some((since, _)) = self.absent(&shared.backend, &path).await
            && since >= shared.fetched
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
                    headers: ObjectHeaders::from_attributes(&result.attributes),
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
                self.mark_absent(backend, path, Absence::Missing);
                Err(ReadError::NotFound)
            }
            Err(error) => Err(ReadError::Backend(error.into())),
        }
    }

    /// Forgets the metadata of an object, remembering when, so older metadata from other
    /// nodes isn't applied.
    pub(crate) fn forget(&self, backend: &str, path: &ObjectPath) {
        self.mark_absent(backend, path, Absence::Forgotten);
    }

    /// Drops the metadata of an object, remembering since when and why it's absent.
    fn mark_absent(&self, backend: &str, path: &ObjectPath, absence: Absence) {
        self.entries.remove(&Self::meta_key(backend, path));
        let key = CacheKey::Absent {
            backend: backend.to_string(),
            key: path.to_string(),
        };
        let mut value = Vec::new();
        let write = |value: &mut Vec<u8>| -> foyer::Result<()> {
            nanos(SystemTime::now()).encode(value)?;
            matches!(absence, Absence::Missing).encode(value)
        };
        write(&mut value).expect("writing to a Vec doesn't fail");
        self.entries.insert(key, value.into());
    }

    /// Since when the metadata of an object is absent, and why, if that's remembered.
    async fn absent(&self, backend: &str, path: &ObjectPath) -> Option<(SystemTime, Absence)> {
        let key = CacheKey::Absent {
            backend: backend.to_string(),
            key: path.to_string(),
        };
        let entry = self.entries.get(&key).await.ok()??;
        let reader = &mut entry.value().as_ref();
        let since = from_nanos(u128::decode(reader).ok()?);
        let missing = bool::decode(reader).ok()?;
        Some((
            since,
            if missing {
                Absence::Missing
            } else {
                Absence::Forgotten
            },
        ))
    }

    /// Forgets the metadata of an object changed through this node, reporting it so the
    /// other nodes of a cluster forget it too.
    pub(crate) fn forget_everywhere(&self, backend: &str, path: &ObjectPath) {
        self.forget(backend, path);
        self.report(|| CacheEvent::Forgotten {
            backend: backend.to_string(),
            key: path.to_string(),
        });
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
        // r[impl cache.no-etag]
        // Their chunks aren't cached, so there's nothing to preload.
        if meta.etag.is_none() {
            return Ok(0);
        }
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
        // r[impl cache.no-etag]
        // Without an ETag, chunks of different versions would share keys, and a read
        // couldn't be held to one version: such objects are read from the backend every
        // time.
        let Some(etag) = meta.etag.clone() else {
            let options = GetOptions {
                range: Some(GetRange::Bounded(start..end)),
                ..Default::default()
            };
            let get = async { store.get_opts(path, options).await?.bytes().await };
            let bytes = spans::backend_call(backend, "get", Some(path), get)
                .await
                .map_err(|error| match error {
                    object_store::Error::NotFound { .. } => ReadError::NotFound,
                    error => ReadError::Backend(anyhow::Error::new(error)),
                })?;
            Span::current().record("hylla.result", "uncacheable");
            METRICS.chunks.add(
                1,
                &[
                    KeyValue::new("hylla.backend", backend.to_string()),
                    KeyValue::new("hylla.result", "uncacheable"),
                ],
            );
            METRICS.backend_bytes.add(
                bytes.len() as u64,
                &[KeyValue::new("hylla.backend", backend.to_string())],
            );
            return Ok((bytes, true));
        };
        let key = CacheKey::Chunk(ChunkKey {
            backend: backend.to_string(),
            key: path.to_string(),
            etag,
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
                    return Err(ReadError::Changed(anyhow::Error::new(error)));
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
            CacheKey::Absent {
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

    // r[verify cluster.forget-tombstones]
    #[tokio::test]
    async fn forgotten_objects_refuse_older_metadata_from_other_nodes() {
        let cache = super::ObjectCache::new(&hylla_config::Cache::default())
            .await
            .unwrap();
        let path = object_store::path::Path::from("app.css");
        let shared = |fetched| super::SharedMeta {
            backend: "assets".into(),
            key: "app.css".into(),
            meta: ObjectMeta {
                size: 1,
                etag: Some("\"1\"".into()),
                last_modified: UNIX_EPOCH,
                content_type: None,
                headers: Default::default(),
            },
            fetched,
        };
        let before = std::time::SystemTime::now();
        cache.forget("assets", &path);
        // Fetched before the object was forgotten, as when another node's update was slow.
        cache.apply_metadata(shared(before)).await;
        assert!(!cache.is_cached("assets", &path).await);
        // Fetched after.
        let after = std::time::SystemTime::now() + Duration::from_millis(10);
        cache.apply_metadata(shared(after)).await;
        assert!(cache.is_cached("assets", &path).await);
    }

    #[test]
    fn metadata_round_trips() {
        let at = |secs, nanos| UNIX_EPOCH + Duration::new(secs, nanos);
        let headers = super::ObjectHeaders {
            cache_control: Some("max-age=60".into()),
            content_encoding: Some("gzip".into()),
            content_disposition: None,
            content_language: Some("sv".into()),
        };
        for (etag, content_type, headers) in [
            (Some("\"abc\""), Some("text/css"), headers),
            (None, None, Default::default()),
        ] {
            let stored = StoredMeta {
                meta: ObjectMeta {
                    size: 3000,
                    etag: etag.map(Into::into),
                    last_modified: at(1_700_000_000, 123),
                    content_type: content_type.map(Into::into),
                    headers,
                },
                fetched: at(1_800_000_000, 456),
            };
            assert_eq!(StoredMeta::from_bytes(&stored.to_bytes()).unwrap(), stored);
        }
    }

    // r[verify cluster.queue]
    #[tokio::test]
    async fn events_beyond_the_queue_are_dropped() {
        let cache = super::ObjectCache::new(&hylla_config::Cache::default())
            .await
            .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        cache.report_to(sender);
        for key in ["a", "b", "c"] {
            cache.report(|| super::CacheEvent::Forgotten {
                backend: "assets".into(),
                key: key.into(),
            });
        }
        let mut received = Vec::new();
        while let Ok(super::CacheEvent::Forgotten { key, .. }) = receiver.try_recv() {
            received.push(key);
        }
        assert_eq!(received, ["a", "b"]);
    }
}
