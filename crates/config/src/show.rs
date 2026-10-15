//! Showing the effective config, and where each value came from.

use std::fmt;

use facet::{Def, Peek, Type, UserType};
use facet_value::{VObject, Value};
use ordna_core::{ConfigLayer, ConfigModel, ConfigPath, ConfigValueSource, SourceResolution};

use crate::{Config, Loaded, Origin, SourceError};

/// Comments start at least this far in, so short lines line up.
const COMMENT_COLUMN: usize = 32;

/// How a secret value is shown.
const REDACTED: &str = "\"<redacted>\"";

// r[impl config.show]
/// The effective config as Styx, each value with a comment saying where it came from.
/// Secret values are redacted.
pub fn render(loaded: &Loaded) -> String {
    let mut printer = Printer {
        loaded,
        lines: Vec::new(),
    };
    if let Some(object) = effective(loaded.config()).as_object() {
        printer.object(object, &mut Vec::new(), 0);
    }
    printer.finish()
}

/// Why a field's provenance can't be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainError {
    /// The path isn't a valid config path.
    InvalidPath(String),
    /// No field has the path, or is under it.
    UnknownField(String),
}

impl fmt::Display for ExplainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(f, "`{path}` is not a valid config path"),
            Self::UnknownField(path) => write!(f, "no config field is at or under `{path}`"),
        }
    }
}

impl std::error::Error for ExplainError {}

// r[impl config.explain]
/// The value of the field at `path`, or of every field under it, with the source it came
/// from and the sources it overrides, highest precedence first.
pub fn explain(loaded: &Loaded, path: &str) -> Result<String, ExplainError> {
    let query = ConfigPath::parse(path).map_err(|_| ExplainError::InvalidPath(path.into()))?;
    let mut leaves = Vec::new();
    if let Some(object) = effective(loaded.config()).as_object() {
        collect_leaves(object, &mut Vec::new(), &mut leaves);
    }
    let matching = leaves
        .into_iter()
        .filter(|(segments, _)| segments.starts_with(query.segments()))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        // A known field or section that's unset, such as an optional setting left out or
        // an empty map.
        return if schema().layer(&query).is_some() || schema().leads_to_fields(query.segments()) {
            Ok(format!("{query} is not set\n"))
        } else {
            Err(ExplainError::UnknownField(path.into()))
        };
    }

    let mut out = String::new();
    for (segments, value) in matching {
        let path = ConfigPath::new(segments);
        out.push_str(&format!("{path} {}\n", leaf_text(&path, &value)));
        match loaded.source(&path) {
            Some(resolution) => {
                out.push_str(&format!("  from {}\n", describe(resolution.winner())));
                for shadowed in resolution.shadowed() {
                    out.push_str(&format!("  overriding {}\n", describe(shadowed)));
                }
            }
            None => out.push_str("  from the default\n"),
        }
    }
    Ok(out)
}

fn schema() -> &'static ordna_core::Schema {
    <Config as ConfigModel<'static, Origin, SourceError>>::schema()
}

/// Where a value came from, as in `env HYLLA__PORT`.
fn describe(source: &ConfigValueSource<Origin>) -> String {
    match source {
        ConfigValueSource::Direct(Origin::File { file, .. }) => file.display().to_string(),
        ConfigValueSource::Direct(Origin::Env { var, .. }) => format!("env {var}"),
        ConfigValueSource::Direct(Origin::Flag { .. }) => "a command line flag".to_string(),
        ConfigValueSource::Derived { configured_by, .. } => describe(configured_by),
    }
}

/// The comment for a leaf: where its value came from, and what it overrides.
fn provenance(resolution: Option<&SourceResolution<Origin>>) -> String {
    let Some(resolution) = resolution else {
        return "default".to_string();
    };
    let mut comment = describe(resolution.winner());
    if !resolution.shadowed().is_empty() {
        let shadowed = resolution
            .shadowed()
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(", ");
        comment.push_str(&format!(", overriding {shadowed}"));
    }
    comment
}

/// A leaf's value as Styx, redacted if it's secret.
fn leaf_text(path: &ConfigPath, value: &Value) -> String {
    if schema().layer(path) == Some(ConfigLayer::Secret) {
        REDACTED.to_string()
    } else {
        styx(value)
    }
}

/// A value as Styx on one line, quoted where needed, without the optional settings left
/// out, such as a mapping's `auth`.
fn styx(value: &Value) -> String {
    // Multi-line text, such as a public key, stays on one line with escapes, rather than
    // as a heredoc.
    if let Some(text) = value.as_string()
        && text.as_str().contains('\n')
    {
        return format!("{:?}", text.as_str());
    }
    facet_styx::to_string_compact(&without_nulls(value)).unwrap_or_else(|_| "@".to_string())
}

fn without_nulls(value: &Value) -> Value {
    if let Some(object) = value.as_object() {
        let mut cleaned = VObject::new();
        for (key, value) in object.iter() {
            if !value.is_null() {
                cleaned.insert(key.as_str(), without_nulls(value));
            }
        }
        cleaned.into()
    } else if let Some(items) = value.as_array() {
        items
            .iter()
            .map(without_nulls)
            .collect::<facet_value::VArray>()
            .into()
    } else {
        value.clone()
    }
}

/// The leaves of an effective config: scalars and lists, with their paths, in order.
fn collect_leaves(
    object: &VObject,
    path: &mut Vec<ordna_core::ConfigPathSegment>,
    leaves: &mut Vec<(Vec<ordna_core::ConfigPathSegment>, Value)>,
) {
    for (key, value) in object.iter() {
        if value.is_null() {
            continue;
        }
        let Ok(segment) = ordna_core::ConfigPathSegment::parse(key.as_str().to_string()) else {
            continue;
        };
        path.push(segment);
        match value.as_object() {
            Some(object) => collect_leaves(object, path, leaves),
            None => leaves.push((path.clone(), value.clone())),
        }
        path.pop();
    }
}

struct Line {
    indent: usize,
    text: String,
    comment: Option<String>,
}

struct Printer<'a> {
    loaded: &'a Loaded,
    lines: Vec<Line>,
}

impl Printer<'_> {
    fn line(&mut self, indent: usize, text: String, comment: Option<String>) {
        self.lines.push(Line {
            indent,
            text,
            comment,
        });
    }

    fn object(&mut self, object: &VObject, path: &mut Vec<String>, indent: usize) {
        for (key, value) in object.iter() {
            // Optional settings left out.
            if value.is_null() {
                continue;
            }
            let key_text = styx(&Value::from(key.as_str()));
            path.push(key.as_str().to_string());
            if let Some(nested) = value.as_object() {
                if nested.is_empty() {
                    self.line(indent, format!("{key_text} {{}}"), None);
                } else {
                    self.line(indent, format!("{key_text} {{"), None);
                    self.object(nested, path, indent + 1);
                    self.line(indent, "}".to_string(), None);
                }
            } else {
                self.leaf(&key_text, value, path, indent);
            }
            path.pop();
        }
    }

    /// A scalar or a list, with the comment saying where it came from.
    fn leaf(&mut self, key_text: &str, value: &Value, path: &[String], indent: usize) {
        let config_path = ConfigPath::parse(&path.join(".")).ok();
        let comment = config_path
            .as_ref()
            .map(|path| provenance(self.loaded.source(path)));
        let secret = config_path
            .as_ref()
            .is_some_and(|path| schema().layer(path) == Some(ConfigLayer::Secret));
        if secret {
            self.line(indent, format!("{key_text} {REDACTED}"), comment);
            return;
        }
        let inline = styx(value);
        match value.as_array() {
            // Lists of structs, such as mappings, get a line per item.
            Some(items) if inline.len() > 60 && !items.is_empty() => {
                self.line(indent, format!("{key_text} ("), comment);
                for item in items.iter() {
                    self.line(indent + 1, styx(item), None);
                }
                self.line(indent, ")".to_string(), None);
            }
            _ => self.line(indent, format!("{key_text} {inline}"), comment),
        }
    }

    /// The lines, with the comments of neighbouring lines lined up.
    fn finish(self) -> String {
        let width = |line: &Line| line.indent * 2 + line.text.chars().count();
        let mut out = String::new();
        let mut start = 0;
        while start < self.lines.len() {
            // Neighbouring lines at the same indent share a comment column, up to a block
            // opening or closing; a list's opening line, which has a comment, ends the run.
            let mut end = start;
            while end < self.lines.len() && self.lines[end].indent == self.lines[start].indent {
                let line = &self.lines[end];
                if line.text.ends_with('{') || line.text == "}" || line.text == ")" {
                    break;
                }
                end += 1;
                if line.text.ends_with('(') {
                    break;
                }
            }
            let run = start..end.max(start + 1);
            let column = self.lines[run.clone()]
                .iter()
                .map(width)
                .max()
                .unwrap_or(0)
                .max(COMMENT_COLUMN);
            for line in &self.lines[run.clone()] {
                out.push_str(&"  ".repeat(line.indent));
                out.push_str(&line.text);
                if let Some(comment) = &line.comment {
                    out.push_str(&" ".repeat(column - width(line) + 2));
                    out.push_str("// ");
                    out.push_str(comment);
                }
                out.push('\n');
            }
            start = run.end;
        }
        out
    }
}

/// The effective config as a generic value, with fields named as in config files and
/// scalars as their text.
fn effective(config: &Config) -> Value {
    value_of(Peek::new(config))
}

fn value_of(peek: Peek<'_, '_>) -> Value {
    // Types configured through a proxy, such as durations written as `30s`.
    if let Ok(Some(proxy)) = peek.custom_serialization_from_shape() {
        return value_of(proxy.as_peek());
    }
    match peek.shape().def {
        Def::Option(_) => match peek.into_option().ok().and_then(|option| option.value()) {
            Some(value) => value_of(value),
            None => Value::NULL,
        },
        Def::List(_) | Def::Array(_) | Def::Slice(_) => peek
            .into_list_like()
            .map(|list| {
                list.iter()
                    .map(value_of)
                    .collect::<facet_value::VArray>()
                    .into()
            })
            .unwrap_or(Value::NULL),
        Def::Set(_) => peek
            .into_set()
            .map(|set| {
                set.iter()
                    .map(value_of)
                    .collect::<facet_value::VArray>()
                    .into()
            })
            .unwrap_or(Value::NULL),
        Def::Map(_) => {
            let mut object = VObject::new();
            if let Ok(map) = peek.into_map() {
                let mut entries = map
                    .iter()
                    .map(|(key, value)| (scalar_text(key), value_of(value)))
                    .collect::<Vec<_>>();
                // Maps are unordered; show them sorted.
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (key, value) in entries {
                    object.insert(key, value);
                }
            }
            object.into()
        }
        _ => match peek.shape().ty {
            Type::User(UserType::Struct(_)) => fields_of(peek),
            Type::User(UserType::Enum(_)) => variant_of(peek),
            _ => scalar_of(peek),
        },
    }
}

fn fields_of(peek: Peek<'_, '_>) -> Value {
    let Type::User(UserType::Struct(struct_type)) = peek.shape().ty else {
        return Value::NULL;
    };
    let Ok(peek_struct) = peek.into_struct() else {
        return Value::NULL;
    };
    let mut object = VObject::new();
    for (index, field) in struct_type.fields.iter().enumerate() {
        if let Ok(value) = peek_struct.field(index) {
            object.insert(field.effective_name(), value_of(value));
        }
    }
    object.into()
}

/// Internally tagged enums, such as backends, as their tag and the variant's fields; other
/// enums, such as `json` for the log format, as the variant's name.
fn variant_of(peek: Peek<'_, '_>) -> Value {
    let shape = peek.shape();
    let Ok(peek_enum) = peek.into_enum() else {
        return Value::NULL;
    };
    let Ok(variant) = peek_enum.active_variant() else {
        return Value::NULL;
    };
    let name = variant.effective_name();
    let Some(tag) = shape.get_tag_attr() else {
        return Value::from(name);
    };
    let mut object = VObject::new();
    object.insert(tag, Value::from(name));
    for (index, field) in variant.data.fields.iter().enumerate() {
        if let Ok(Some(value)) = peek_enum.field(index) {
            object.insert(field.effective_name(), value_of(value));
        }
    }
    object.into()
}

fn scalar_of(peek: Peek<'_, '_>) -> Value {
    if let Ok(value) = peek.get::<bool>() {
        return Value::from(*value);
    }
    Value::from(scalar_text(peek))
}

fn scalar_text(peek: Peek<'_, '_>) -> String {
    match peek.as_str() {
        Some(text) => text.to_string(),
        None => peek.to_string(),
    }
}
