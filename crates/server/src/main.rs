use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use hylla_config::{ConfigFiles, load};
use hylla_server::{App, backend, serve, telemetry};

/// A caching layer in front of object storage.
#[derive(Parser)]
#[command(name = "hylla", version)]
struct Args {
    /// The config file.
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
    /// The secrets file, holding backend credentials.
    #[arg(long)]
    secrets: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let loaded = load(&ConfigFiles {
        config: args.config,
        secrets: args.secrets,
    })?;
    let config = loaded.into_config();
    // Logging is set up here too, as configured.
    let telemetry = telemetry::init(&config.telemetry).context("setting up telemetry")?;
    let stores = backend::stores(&config).context("creating backends")?;
    let app = App::new(&config, stores)
        .await
        .context("creating the cache")?;

    let served = serve(app, shutdown_signal()).await;
    telemetry.shutdown();
    served
}

// r[impl server.shutdown]
/// Completes on Ctrl-C or SIGTERM, which is how containers are stopped. As a container's
/// first process, hylla would otherwise ignore SIGTERM and be killed without writing its
/// cache to disk.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!("listening for SIGTERM: {error}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
