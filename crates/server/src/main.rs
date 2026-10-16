use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use hylla_config::{ConfigFiles, explain, load, render};
use hylla_server::{App, backend, serve, telemetry};
use ordna_core::{ConfigPath, ValuesSource};

/// A caching layer in front of object storage.
#[derive(Parser)]
#[command(name = "hylla", version)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The config file.
    #[arg(long, default_value = "config.styx", global = true)]
    config: PathBuf,
    /// The secrets file, holding backend credentials.
    #[arg(long, global = true)]
    secrets: Option<PathBuf>,
    #[command(flatten)]
    flags: ConfigFlags,
}

#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
enum Command {
    /// Serve objects, the default
    Serve,
    /// Inspect the config, as loaded from files, the environment and flags
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
enum ConfigCommand {
    /// Print the effective config, with where each value came from
    Show,
    /// Show where a field's value came from and what it overrides, for a field or every
    /// field under a section
    Explain {
        /// A field or section, such as `telemetry.logs.level` or `backends.assets`
        path: String,
    },
}

// r[impl config.flags]
/// Settings for a single run, overriding the config file and `HYLLA__...` variables.
#[derive(clap::Args, Debug, Default)]
#[command(next_help_heading = "Config")]
struct ConfigFlags {
    /// What to log, as `tracing` filter directives such as `info` or
    /// `info,hylla_server=debug`
    #[arg(long, global = true, value_name = "FILTER")]
    log_level: Option<String>,
    /// How to log
    #[arg(long, global = true, value_parser = ["text", "json"])]
    log_format: Option<String>,
    /// The OTLP collector's base URL, such as `http://collector:4318`
    #[arg(long, global = true, value_name = "URL")]
    otlp_endpoint: Option<String>,
    /// Whether to export metrics
    #[arg(long, global = true, value_name = "BOOL", num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    metrics: Option<bool>,
    /// Whether to export traces
    #[arg(long, global = true, value_name = "BOOL", num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    traces: Option<bool>,
    /// The memory cache's capacity in MiB
    #[arg(long, global = true, value_name = "MIB")]
    cache_memory_mib: Option<u64>,
}

impl ConfigFlags {
    /// The flags given, by the config path each sets.
    fn values(&self) -> ValuesSource {
        let flags: [(&str, Option<String>); 6] = [
            ("telemetry.logs.level", self.log_level.clone()),
            ("telemetry.logs.format", self.log_format.clone()),
            ("telemetry.otlp.endpoint", self.otlp_endpoint.clone()),
            (
                "telemetry.metrics.enabled",
                self.metrics.map(|on| on.to_string()),
            ),
            (
                "telemetry.traces.enabled",
                self.traces.map(|on| on.to_string()),
            ),
            (
                "cache.memory.capacity_mib",
                self.cache_memory_mib.map(|mib| mib.to_string()),
            ),
        ];
        flags
            .into_iter()
            .filter_map(|(path, value)| {
                let path = ConfigPath::parse(path).expect("flags set valid config paths");
                Some((path, value?))
            })
            .collect()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let files = ConfigFiles {
        config: args.config,
        secrets: args.secrets,
        flags: args.flags.values(),
    };
    match args.command.unwrap_or(Command::Serve) {
        Command::Serve => run(&files).await,
        // r[impl config.show]
        Command::Config(ConfigCommand::Show) => {
            print!("{}", render(&load(&files)?));
            Ok(())
        }
        // r[impl config.explain]
        Command::Config(ConfigCommand::Explain { path }) => {
            print!("{}", explain(&load(&files)?, &path)?);
            Ok(())
        }
    }
}

async fn run(files: &ConfigFiles) -> anyhow::Result<()> {
    let loaded = load(files)?;
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

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use hylla_config::{ConfigFiles, LogFormat, load_with_env};
    use ordna_source_env::EnvSource;

    use super::{Args, Command, ConfigCommand};

    #[test]
    fn arguments_are_valid() {
        Args::command().debug_assert();
    }

    // r[verify config.show]
    // r[verify config.explain]
    #[test]
    fn subcommands_take_the_config_options_too() {
        let args = Args::try_parse_from(["hylla", "--config", "a.styx"]).unwrap();
        assert_eq!(args.command, None);

        let args = Args::try_parse_from(["hylla", "config", "show", "--config", "b.styx"]).unwrap();
        assert_eq!(args.command, Some(Command::Config(ConfigCommand::Show)));
        assert_eq!(args.config, std::path::PathBuf::from("b.styx"));

        let args = Args::try_parse_from([
            "hylla",
            "config",
            "explain",
            "telemetry.logs.level",
            "--log-level=debug",
        ])
        .unwrap();
        assert_eq!(
            args.command,
            Some(Command::Config(ConfigCommand::Explain {
                path: "telemetry.logs.level".into()
            }))
        );
        assert!(!args.flags.values().is_empty());
    }

    // r[verify config.flags]
    #[test]
    fn flags_set_their_config_fields() {
        let dir = std::env::temp_dir().join(format!("hylla-flags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.styx");
        std::fs::write(&config, "servers {}\nbackends {}\n").unwrap();
        let args = Args::try_parse_from([
            "hylla",
            "--log-level=debug",
            "--log-format",
            "json",
            "--otlp-endpoint=http://collector:4318",
            "--metrics",
            "--traces=false",
            "--cache-memory-mib",
            "512",
        ])
        .unwrap();
        let loaded = load_with_env(
            &ConfigFiles {
                config,
                secrets: None,
                flags: args.flags.values(),
            },
            EnvSource::from_vars(Vec::<(String, String)>::new()),
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let config = loaded.config();
        assert_eq!(config.telemetry.logs.level, "debug");
        assert_eq!(config.telemetry.logs.format, LogFormat::Json);
        assert_eq!(
            config.telemetry.otlp.endpoint.as_deref(),
            Some("http://collector:4318")
        );
        assert!(config.telemetry.metrics.enabled);
        assert!(!config.telemetry.traces.enabled);
        assert_eq!(config.cache.memory.capacity_mib, 512);

        // Flags left out set nothing.
        let args = Args::try_parse_from(["hylla"]).unwrap();
        assert!(args.flags.values().is_empty());
    }
}
