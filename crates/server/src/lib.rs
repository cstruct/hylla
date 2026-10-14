//! hylla's server: serves objects from object storage through a hybrid RAM and disk
//! cache.
//!
//! Each `listen` address of the config gets a listener. A request is matched to a server
//! by its `Host` header, routed to a backend by the server's mappings, and answered from
//! the cache, which holds objects in fixed-size chunks fetched from the backend on miss.

mod app;
mod auth;
pub mod backend;
mod cache;
mod cluster;
mod range;
mod refresh;

pub use app::App;
pub use cache::ChunkKey;
pub use refresh::RefreshReport;

use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;

/// Serves `app` on all its listen addresses until `shutdown` completes.
pub async fn serve(
    app: Arc<App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let (stop, _) = tokio::sync::broadcast::channel::<()>(1);
    let mut tasks = Vec::new();
    for listen in app.listen_addresses() {
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding {listen}"))?;
        tracing::info!(%listen, "listening");
        let router = app.listener(listen);
        let mut stopped = stop.subscribe();
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.recv().await;
                })
                .await
        }));
    }
    let mut background = app.start_cluster().await?;
    background.extend(app.spawn_refresh());
    shutdown.await;
    for task in &background {
        task.abort();
    }
    let _ = stop.send(());
    for task in tasks {
        task.await??;
    }
    app.close().await
}
