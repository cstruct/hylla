use std::{fmt, path::PathBuf};

use ordna_core::{
    ConfigBuilder, ConfigLoadError, ConfigPath, LoadedConfig, PartialConfig, PartialLoadError,
    SourceFieldPath, SourceRegistration, ValueDetail, ValuesSource,
};
use ordna_source_env::{EnvSource, EnvSourceError, EnvVarDetail};
use ordna_source_file::{FileSourceError, FileValueDetail};
use ordna_source_styx::{DeserializeError, StyxSource, styx_file};

use crate::{Config, ValidationError};

/// The prefix of environment variables setting config values, as in
/// `HYLLA__SERVERS__PUBLIC__LISTEN`.
pub const ENV_PREFIX: &str = "HYLLA";

/// Where to load the config from.
#[derive(Debug, Clone)]
pub struct ConfigFiles {
    /// The public config file.
    pub config: PathBuf,
    /// Whether the config file must exist. Without it, a missing file sets nothing, as for
    /// the default file, which hylla runs without when it isn't there.
    pub config_required: bool,
    /// The secrets file, holding backend credentials.
    pub secrets: Option<PathBuf>,
    /// Values given by command line flags, overriding everything else.
    pub flags: ValuesSource,
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
    /// A command line flag.
    Flag {
        path: ConfigPath,
    },
}

impl SourceFieldPath for Origin {
    fn field_path(&self) -> &ConfigPath {
        match self {
            Origin::Env { path, .. } | Origin::File { path, .. } | Origin::Flag { path } => path,
        }
    }

    fn source_kind(&self) -> &'static str {
        match self {
            Origin::Env { .. } => "env",
            Origin::File { .. } => "file",
            Origin::Flag { .. } => "flag",
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

/// The config as far as it loads, for showing it even when it's incomplete or invalid.
#[derive(Debug)]
pub enum Inspected {
    /// The config loaded, invalid if any of its checks failed.
    Loaded {
        loaded: Box<Loaded>,
        invalid: Vec<ValidationError>,
    },
    /// Loading failed with violations, such as required fields that aren't set: the values
    /// set so far, with their provenance and the defaults filled in.
    Partial(Box<PartialConfig<Origin>>),
}

impl Inspected {
    /// The loaded config, or why it doesn't load.
    pub fn into_loaded(self) -> Result<Loaded, ConfigError> {
        match self {
            Self::Loaded { loaded, invalid } if invalid.is_empty() => Ok(*loaded),
            Self::Loaded { invalid, .. } => Err(ConfigError::Invalid(invalid)),
            Self::Partial(partial) => Err(ConfigError::Load(ConfigLoadError::Violations(
                partial.into_violations(),
            ))),
        }
    }
}

impl From<Loaded> for Inspected {
    fn from(loaded: Loaded) -> Self {
        Self::Loaded {
            loaded: Box::new(loaded),
            invalid: Vec::new(),
        }
    }
}

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
            Self::Load(ConfigLoadError::Violations(violations)) => {
                write!(f, "failed to load the config:")?;
                // Missing fields are decode violations too, as `missing required field ..`.
                for error in violations.decode_errors() {
                    write!(f, "\n- {}", error.message)?;
                }
                for issue in violations.unknown_paths() {
                    write!(
                        f,
                        "\n- `{}` from {} is not a config field",
                        issue.path,
                        source_name(&issue.source)
                    )?;
                }
                for issue in violations.conflicts() {
                    write!(
                        f,
                        "\n- `{}` is set more than once in {}",
                        issue.path,
                        source_name(&issue.source)
                    )?;
                }
                for issue in violations.layer_mismatches() {
                    write!(
                        f,
                        "\n- `{}` can't be set from {}: it's a {} field",
                        issue.path,
                        source_name(&issue.source),
                        format!("{:?}", issue.field_layer).to_lowercase()
                    )?;
                }
                Ok(())
            }
            Self::Load(ConfigLoadError::Source(error)) => match &error.error {
                SourceError::File(error) => write!(f, "{error}"),
                SourceError::Env(error) => write!(f, "failed to read the environment: {error}"),
            },
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

fn source_name(source: &ordna_core::SourceMeta) -> &str {
    source.name.as_deref().unwrap_or("a source")
}

/// Loads and validates the config from `files`, overridden by `HYLLA__...` environment
/// variables, and those by the flags given.
pub fn load(files: &ConfigFiles) -> Result<Loaded, ConfigError> {
    load_with_env(files, EnvSource::with_prefix(ENV_PREFIX))
}

/// Loads and validates the config from `files`, overridden by `env`.
pub fn load_with_env(files: &ConfigFiles, env: EnvSource) -> Result<Loaded, ConfigError> {
    inspect_with_env(files, env)?.into_loaded()
}

/// Loads the config as far as it goes, from `files`, overridden by `HYLLA__...` environment
/// variables, and those by the flags given. Fails only when a source can't be read.
pub fn inspect(files: &ConfigFiles) -> Result<Inspected, ConfigError> {
    inspect_with_env(files, EnvSource::with_prefix(ENV_PREFIX))
}

// r[impl config.sources]
// r[impl config.ordna]
/// Loads the config as far as it goes, from `files`, overridden by `env`.
pub fn inspect_with_env(files: &ConfigFiles, env: EnvSource) -> Result<Inspected, ConfigError> {
    let builder = ConfigBuilder::<Config, _, _>::new().source_named(
        "config",
        file_registration(),
        config_file(files),
    );
    let builder = match &files.secrets {
        Some(secrets) => {
            builder.source_named("secrets", file_registration(), styx_file(secrets).secrets())
        }
        None => builder,
    };
    // r[impl config.flags]
    let loaded = builder
        .source_named("env", env_registration(), env)
        .source_named("flags", flag_registration(), files.flags.clone())
        .load_with_meta_or_partial();
    match loaded {
        Ok(loaded) => {
            // r[impl config.validate]
            let invalid = loaded.config().validate().err().unwrap_or_default();
            Ok(Inspected::Loaded {
                loaded: Box::new(loaded),
                invalid,
            })
        }
        Err(PartialLoadError::Violations(partial)) => Ok(Inspected::Partial(partial)),
        Err(PartialLoadError::Source(error)) => {
            Err(ConfigError::Load(ConfigLoadError::Source(error)))
        }
    }
}

// r[impl config.file]
fn config_file(files: &ConfigFiles) -> StyxSource {
    let file = styx_file(&files.config);
    if files.config_required {
        file
    } else {
        file.optional()
    }
}

fn flag_registration() -> SourceRegistration<ValuesSource, Origin, SourceError> {
    SourceRegistration::new(
        |never| match never {},
        |path, _: ValueDetail| Origin::Flag { path },
    )
}

fn file_registration() -> SourceRegistration<StyxSource, Origin, SourceError> {
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
