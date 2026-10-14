use std::sync::Arc;

use futures::{StreamExt, TryStreamExt, stream};
use hylla_config::{Refresh, RefreshRule};
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjectPath};

use crate::{
    cache::{CachedState, ObjectCache, ObjectMeta, ReadError},
    metrics,
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
    cache: &ObjectCache,
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

    // r[impl refresh.list]
    let listed = match (&rule.prefix, &rule.key) {
        (Some(prefix), _) => {
            let prefix = (!prefix.is_empty()).then(|| ObjectPath::from(prefix.as_str()));
            let started = std::time::Instant::now();
            let listed = store.list(prefix.as_ref()).try_collect::<Vec<_>>().await;
            metrics::backend_request(backend, "list", started, metrics::outcome(&listed));
            listed?
        }
        (None, Some(key)) => {
            let path = ObjectPath::from(key.as_str());
            let started = std::time::Instant::now();
            let head = store.head(&path).await;
            metrics::backend_request(backend, "head", started, metrics::outcome(&head));
            match head {
                Ok(meta) => vec![meta],
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
    report.listed = listed.len() as u64;

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
    let (mut preload_objects, mut preload_bytes) = (0u64, 0u64);
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
                let within_objects = rule.max_objects.is_none_or(|max| preload_objects < max);
                let within_bytes = max_bytes.is_none_or(|max| preload_bytes + object.size <= max);
                if !(within_objects && within_bytes) {
                    report.skipped += 1;
                    continue;
                }
                preload_objects += 1;
                preload_bytes += object.size;
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
                            Err(ReadError::Backend(error)) => Outcome::Failed(path, error),
                        }
                    }
                    Action::Preload => {
                        let meta = match cache.fetch_metadata(backend, store.as_ref(), &path).await
                        {
                            Ok(meta) => meta,
                            Err(ReadError::NotFound) => return Outcome::Removed,
                            Err(ReadError::Backend(error)) => return Outcome::Failed(path, error),
                        };
                        match cache.preload(backend, store, &path, &meta).await {
                            Ok(bytes) => {
                                cache.report_preloaded(backend, &path, &meta);
                                Outcome::Preloaded(bytes)
                            }
                            Err(ReadError::NotFound) => Outcome::Removed,
                            Err(ReadError::Backend(error)) => Outcome::Failed(path, error),
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
    Ok(report)
}

enum Outcome {
    Unchanged,
    Changed,
    Removed,
    Preloaded(u64),
    Failed(ObjectPath, anyhow::Error),
}
