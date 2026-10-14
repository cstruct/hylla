use std::{fmt, path::PathBuf};

use ordna_core::{
    ConfigBuilder, ConfigLoadError, ConfigPath, LoadedConfig, SourceFieldPath, SourceRegistration,
};
use ordna_source_env::{EnvSource, EnvSourceError, EnvVarDetail};
use ordna_source_file::{FileSourceError, FileValueDetail};
use ordna_source_yaml::{DeserializeError, YamlSource, yaml_file};

use crate::{Config, ValidationError};

/// The prefix of environment variables setting config values, as in
/// `HYLLA__SERVERS__PUBLIC__LISTEN`.
pub const ENV_PREFIX: &str = "HYLLA";

/// Where to load the config from.
#[derive(Debug, Clone)]
pub struct ConfigFiles {
    /// The public config file.
    pub config: PathBuf,
    /// The secrets file, holding backend credentials.
    pub secrets: Option<PathBuf>,
}

/// Where a config value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Env {
        var: String,
        path: ConfigPath,
    },
    File {
        file: PathBuf,
        key: String,
        path: ConfigPath,
    },
}

impl SourceFieldPath for Origin {
    fn field_path(&self) -> &ConfigPath {
        match self {
            Origin::Env { path, .. } | Origin::File { path, .. } => path,
        }
    }

    fn source_kind(&self) -> &'static str {
        match self {
            Origin::Env { .. } => "env",
            Origin::File { .. } => "file",
        }
    }
}

/// A source failing to load.
#[derive(Debug)]
pub enum SourceError {
    Env(EnvSourceError),
    File(FileSourceError<DeserializeError>),
}

/// The loaded config, with the provenance of every value.
pub type Loaded = LoadedConfig<'static, Config, Origin, SourceError>;

#[derive(Debug)]
pub enum ConfigError {
    /// A source failed to load, or values were invalid for the config's structure.
    Load(ConfigLoadError<SourceError>),
    /// The loaded config is invalid.
    Invalid(Vec<ValidationError>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(f, "failed to load the config: {error:?}"),
            Self::Invalid(errors) => {
                write!(f, "invalid config:")?;
                for error in errors {
                    write!(f, "\n- {error}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Loads and validates the config from `files`, overridden by `HYLLA__...` environment
/// variables.
pub fn load(files: &ConfigFiles) -> Result<Loaded, ConfigError> {
    load_with_env(files, EnvSource::with_prefix(ENV_PREFIX))
}

// r[impl config.sources]
// r[impl config.ordna]
/// Loads and validates the config from `files`, overridden by `env`.
pub fn load_with_env(files: &ConfigFiles, env: EnvSource) -> Result<Loaded, ConfigError> {
    let builder = ConfigBuilder::<Config, _, _>::new().source_named(
        "config",
        file_registration(),
        yaml_file(&files.config),
    );
    let builder = match &files.secrets {
        Some(secrets) => {
            builder.source_named("secrets", file_registration(), yaml_file(secrets).secrets())
        }
        None => builder,
    };
    let loaded = builder
        .source_named("env", env_registration(), env)
        .load_with_meta()
        .map_err(ConfigError::Load)?;
    // r[impl config.validate]
    loaded.config().validate().map_err(ConfigError::Invalid)?;
    Ok(loaded)
}

fn file_registration() -> SourceRegistration<YamlSource, Origin, SourceError> {
    SourceRegistration::new(SourceError::File, |path, detail: FileValueDetail| {
        Origin::File {
            file: detail.file,
            key: detail.key,
            path,
        }
    })
}

fn env_registration() -> SourceRegistration<EnvSource, Origin, SourceError> {
    SourceRegistration::new(SourceError::Env, |path, detail: EnvVarDetail| Origin::Env {
        var: detail.name,
        path,
    })
}
