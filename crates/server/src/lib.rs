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
mod control;
mod health;
mod listen;
mod logs;
mod metrics;
mod oidc;
mod provider;
mod range;
mod refresh;
mod spans;
mod status;
pub mod telemetry;
mod tls;
mod upstream;

pub use app::App;
pub use cache::ChunkKey;
pub use refresh::RefreshReport;

use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;

/// Serves `app` on all its listen addresses until `shutdown` completes, then finishes the
/// requests in flight, for the shutdown timeout at most, and writes the cache to disk.
pub async fn serve(
    app: Arc<App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    // The health checks keep answering while the other listeners finish, reporting the node
    // not ready, so load balancers stop sending it requests.
    let (_health_stopping, health_shutdown) = {
        let stopping = tokio::sync::watch::Sender::new(false);
        let shutdown = listen::Shutdown::new(&stopping);
        (stopping, shutdown)
    };
    // r[impl telemetry.version]
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting hylla");
    let mut background = Vec::new();
    if let Some(listen) = app.health {
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding the health checks on {listen}"))?;
        tracing::info!(%listen, "health checks listening");
        background.push(tokio::spawn(listen::serve(
            listener,
            health::router(&app),
            None,
            app.limits.clone(),
            health_shutdown,
            "health",
        )));
    }
    let mut tasks = Vec::new();
    for listen in app.listen_addresses() {
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding {listen}"))?;
        tracing::info!(%listen, "listening");
        tasks.push(tokio::spawn(listen::serve(
            listener,
            app.listener(listen),
            None,
            app.limits.clone(),
            app.shutdown(),
            "server",
        )));
    }
    background.extend(app.start_cluster().await?);
    background.extend(app.start_upstream().await?);
    background.extend(app.start_control().await?);
    background.extend(app.spawn_refresh());
    app.mark_serving();
    shutdown.await;
    app.stop();
    // The servers' listeners finish their requests within the shutdown timeout, as the
    // others do at the same time.
    for task in tasks {
        task.await?;
    }
    for task in &background {
        task.abort();
    }
    app.close().await
}
