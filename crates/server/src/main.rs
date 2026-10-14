use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use hylla_config::{ConfigFiles, load};
use hylla_server::{App, backend, serve};
use tracing_subscriber::EnvFilter;

/// A caching layer in front of object storage.
#[derive(Parser)]
#[command(version)]
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    let loaded = load(&ConfigFiles {
        config: args.config,
        secrets: args.secrets,
    })?;
    let config = loaded.into_config();
    let stores = backend::stores(&config).context("creating backends")?;
    let app = App::new(&config, stores)
        .await
        .context("creating the cache")?;

    serve(app, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await
}
