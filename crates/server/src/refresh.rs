use std::sync::Arc;

use futures::{StreamExt, TryStreamExt, stream};
use hylla_config::{Refresh, RefreshRule};
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};

use crate::{
    cache::{CachedState, ObjectCache, ObjectMeta, ReadError},
    site::Failure,
    spans,
};

/// What a refresh run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Objects the rule covered.
    pub listed: u64,
    /// Cached objects found unchanged, whose metadata was marked fresh.
    pub unchanged: u64,
    /// Cached objects found changed, whose metadata was fetched again.
    pub changed: u64,
    /// Cached objects that went away, whose metadata was dropped.
    pub removed: u64,
    /// Objects preloaded into the cache.
    pub preloaded: u64,
    /// Bytes fetched by preloading.
    pub preloaded_bytes: u64,
    /// Objects skipped because the preload caps were reached.
    pub skipped: u64,
    /// Objects that failed to refresh.
    pub failed: u64,
}

/// What to do with one object.
enum Action {
    /// Marks unchanged metadata fresh, and with `announce`, announces the object to other
    /// nodes again.
    Touch(ObjectMeta, bool),
    /// Announces an unchanged preloaded object to other nodes again.
    Announce(ObjectMeta),
    Revalidate,
    Preload,
}

// r[impl refresh.run]
/// Runs `rule` of a backend once.
pub async fn run(
    cache: &Arc<ObjectCache>,
    backend: &str,
    store: Arc<dyn ObjectStore>,
    refresh: &Refresh,
    rule: &RefreshRule,
) -> anyhow::Result<RefreshReport> {
    let mut report = RefreshReport::default();
    let (revalidates, preloads) = (rule.mode.revalidates(), rule.mode.preloads());
    if !revalidates && !preloads {
        return Ok(report);
    }
    let concurrency = refresh.concurrency.max(1) as usize;

    // r[impl refresh.alias]
    // r[impl refresh.alias.invalid]
    // A rule following an alias covers the build it points at now, as a prefix rule.
    let followed = match &rule.alias {
        Some(alias) => {
            let key = format!("{}{}", alias.aliases, alias.name);
            let (sha, _) = crate::site::read_alias(cache, backend, &store, &key, |_| None)
                .await
                .map_err(|failure| match failure {
                    Failure::NotFound => anyhow::anyhow!("alias `{}` doesn't exist", alias.name),
                    Failure::BadGateway => {
                        anyhow::anyhow!("alias `{}` couldn't be read as a SHA", alias.name)
                    }
                })?;
            Some(format!("{}{sha}/", alias.builds))
        }
        None => None,
    };
    let prefix = followed.as_ref().or(rule.prefix.as_ref());

    // r[impl refresh.list]
    // r[impl refresh.list-batches]
    // Listed and handled a batch at a time, so a large bucket isn't held in memory at once.
    let mut listing = match (prefix, &rule.key) {
        (Some(prefix), _) => {
            let prefix = (!prefix.is_empty()).then(|| ObjectPath::from(prefix.as_str()));
            Listing::Prefix(store.list(prefix.as_ref()), prefix)
        }
        (None, Some(key)) => {
            let path = ObjectPath::from(key.as_str());
            match spans::backend_call(backend, "head", Some(&path), store.head(&path)).await {
                Ok(meta) => Listing::Key(Some(meta)),
                // r[impl refresh.removed]
                Err(object_store::Error::NotFound { .. }) => {
                    if cache.is_cached(backend, &path).await {
                        cache.forget(backend, &path);
                        report.removed += 1;
                    }
                    return Ok(report);
                }
                Err(error) => return Err(error.into()),
            }
        }
        (None, None) => return Ok(report),
    };
    let mut caps = Caps::default();
    loop {
        let listed = match &mut listing {
            Listing::Prefix(list, prefix) => {
                let batch = next_batch(list, LIST_BATCH);
                let batch = spans::backend_call(backend, "list", prefix.as_ref(), batch).await?;
                // A short batch is the last, so the backend isn't asked for more.
                if batch.len() < LIST_BATCH {
                    listing = Listing::Key(None);
                }
                batch
            }
            Listing::Key(meta) => meta.take().into_iter().collect(),
        };
        if listed.is_empty() {
            return Ok(report);
        }
        handle(
            cache,
            backend,
            &store,
            rule,
            concurrency,
            listed,
            &mut caps,
            &mut report,
        )
        .await;
    }
}

/// How many listed objects are handled at a time.
const LIST_BATCH: usize = 1000;

/// What a rule lists.
enum Listing {
    /// The objects under a prefix, as the backend lists them.
    Prefix(
        futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>,
        Option<ObjectPath>,
    ),
    /// A single object, until it's handled.
    Key(Option<object_store::ObjectMeta>),
}

/// The next `size` objects of a listing, fewer at its end.
async fn next_batch(
    list: &mut futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>,
    size: usize,
) -> object_store::Result<Vec<object_store::ObjectMeta>> {
    let mut batch = Vec::with_capacity(size.min(64));
    while batch.len() < size {
        match list.try_next().await? {
            Some(object) => batch.push(object),
            None => break,
        }
    }
    Ok(batch)
}

/// What a run preloaded so far, for its caps.
#[derive(Default)]
struct Caps {
    objects: u64,
    bytes: u64,
}

/// Handles a batch of listed objects.
#[allow(clippy::too_many_arguments)]
async fn handle(
    cache: &ObjectCache,
    backend: &str,
    store: &Arc<dyn ObjectStore>,
    rule: &RefreshRule,
    concurrency: usize,
    listed: Vec<object_store::ObjectMeta>,
    caps: &mut Caps,
    report: &mut RefreshReport,
) {
    let (revalidates, preloads) = (rule.mode.revalidates(), rule.mode.preloads());
    report.listed += listed.len() as u64;

    let states = stream::iter(listed)
        .map(|object| async move {
            let state = cache.cached_state(backend, &object.location, &object).await;
            (object, state)
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await;

    // Decide what to do in listing order, so the preload caps keep the first objects.
    let max_bytes = rule.max_mib.map(|mib| mib * 1024 * 1024);
    let mut actions = Vec::new();
    for (object, state) in states {
        let action = match state {
            // r[impl refresh.revalidate]
            CachedState::Unchanged(meta) if revalidates => Action::Touch(meta, preloads),
            // r[impl cluster.reannounce]
            // Announcing unchanged objects again lets nodes that joined later catch up; they
            // only pull chunks they don't have.
            CachedState::Unchanged(meta) if preloads => Action::Announce(meta),
            CachedState::Unchanged(_) => continue,
            CachedState::Changed if revalidates && !preloads => Action::Revalidate,
            // r[impl refresh.preload]
            // r[impl refresh.caps]
            CachedState::NotCached | CachedState::Changed if preloads => {
                let within_objects = rule.max_objects.is_none_or(|max| caps.objects < max);
                let within_bytes = max_bytes.is_none_or(|max| caps.bytes + object.size <= max);
                if !(within_objects && within_bytes) {
                    report.skipped += 1;
                    continue;
                }
                caps.objects += 1;
                caps.bytes += object.size;
                Action::Preload
            }
            CachedState::NotCached | CachedState::Changed => continue,
        };
        actions.push((object.location, action));
    }

    let results = stream::iter(actions)
        .map(|(path, action)| {
            let store = store.clone();
            async move {
                match action {
                    Action::Touch(meta, announce) => {
                        if announce {
                            cache.report_preloaded(backend, &path, &meta);
                        }
                        cache.touch_metadata(backend, &path, meta);
                        Outcome::Unchanged
                    }
                    Action::Announce(meta) => {
                        cache.report_preloaded(backend, &path, &meta);
                        Outcome::Unchanged
                    }
                    Action::Revalidate => {
                        match cache.fetch_metadata(backend, store.as_ref(), &path).await {
                            Ok(_) => Outcome::Changed,
                            Err(ReadError::NotFound) => Outcome::Removed,
                            Err(ReadError::Backend(error) | ReadError::Changed(error)) => {
                                Outcome::Failed(path, error)
                            }
                        }
                    }
                    Action::Preload => {
                        let meta = match cache.fetch_metadata(backend, store.as_ref(), &path).await
                        {
                            Ok(meta) => meta,
                            Err(ReadError::NotFound) => return Outcome::Removed,
                            Err(ReadError::Backend(error) | ReadError::Changed(error)) => {
                                return Outcome::Failed(path, error);
                            }
                        };
                        match cache.preload(backend, store, &path, &meta).await {
                            Ok(bytes) => {
                                cache.report_preloaded(backend, &path, &meta);
                                Outcome::Preloaded(bytes)
                            }
                            Err(ReadError::NotFound) => Outcome::Removed,
                            Err(ReadError::Backend(error) | ReadError::Changed(error)) => {
                                Outcome::Failed(path, error)
                            }
                        }
                    }
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    for outcome in results {
        match outcome {
            Outcome::Unchanged => report.unchanged += 1,
            Outcome::Changed => report.changed += 1,
            Outcome::Removed => report.removed += 1,
            Outcome::Preloaded(bytes) => {
                report.preloaded += 1;
                report.preloaded_bytes += bytes;
            }
            Outcome::Failed(path, error) => {
                tracing::warn!(backend, %path, "refresh failed: {error:#}");
                report.failed += 1;
            }
        }
    }
}

enum Outcome {
    Unchanged,
    Changed,
    Removed,
    Preloaded(u64),
    Failed(ObjectPath, anyhow::Error),
}
