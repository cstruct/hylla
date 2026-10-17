//! Showing the effective config, and where each value came from.

use std::fmt;

use facet::{Def, Facet, Peek, Shape, Type, UserType};
use facet_value::{VObject, Value};
use ordna_core::{ConfigLayer, ConfigModel, ConfigPath, ConfigValueSource, SourceResolution};

use crate::{Config, Inspected, Origin, SourceError};

/// Comments start at least this far in, so short lines line up.
const COMMENT_COLUMN: usize = 32;

/// How a secret value is shown.
const REDACTED: &str = "\"<redacted>\"";

/// How a required field no source set is shown.
const MISSING: &str = "\"<missing>\"";

/// The comment on a required field no source set.
const MISSING_COMMENT: &str = "missing: required, but not set";

// r[impl config.show]
// r[impl config.show.missing]
/// The effective config as Styx, each value with a comment saying where it came from.
/// Secret values are redacted. A config that didn't load is shown as far as it got, with the
/// required fields no source set marked as missing.
pub fn render(inspected: &Inspected) -> String {
    let shown = Shown::new(inspected);
    let mut printer = Printer {
        shown: &shown,
        lines: Vec::new(),
    };
    if let Some(object) = shown.value.as_object() {
        printer.object(object, &mut Vec::new(), 0);
    }
    printer.finish()
}

/// What `render` and `explain` show: the config's values, where they came from, and the
/// required fields missing.
struct Shown<'a> {
    inspected: &'a Inspected,
    /// The values, fields in the order they're declared, maps sorted, and missing fields as
    /// placeholders.
    value: Value,
    missing: Vec<ConfigPath>,
}

impl<'a> Shown<'a> {
    fn new(inspected: &'a Inspected) -> Self {
        match inspected {
            Inspected::Loaded { loaded, .. } => Self {
                inspected,
                value: effective(loaded.config()),
                missing: Vec::new(),
            },
            Inspected::Partial(partial) => {
                let missing = partial
                    .missing_fields()
                    .iter()
                    .map(|field| field.path.clone())
                    .collect::<Vec<_>>();
                let value = ordered(Config::SHAPE, partial.value(), &mut Vec::new(), &missing);
                Self {
                    inspected,
                    value,
                    missing,
                }
            }
        }
    }

    fn source(&self, path: &ConfigPath) -> Option<&'a SourceResolution<Origin>> {
        match self.inspected {
            Inspected::Loaded { loaded, .. } => loaded.source(path),
            Inspected::Partial(partial) => partial.source(path),
        }
    }

    fn is_missing(&self, path: &ConfigPath) -> bool {
        self.missing.contains(path)
    }

    /// The missing fields under `path`, relative to it, such as `path_prefix` under
    /// `servers.public.mappings.1`.
    fn missing_under(&self, path: &ConfigPath) -> Vec<String> {
        self.missing
            .iter()
            .filter_map(|missing| {
                let rest = missing.segments().strip_prefix(path.segments())?;
                (!rest.is_empty()).then(|| ConfigPath::new(rest.to_vec()).to_string())
            })
            .collect()
    }
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
/// from and the sources it overrides, highest precedence first. Required fields no source
/// set are said to be missing.
pub fn explain(inspected: &Inspected, path: &str) -> Result<String, ExplainError> {
    let query = ConfigPath::parse(path).map_err(|_| ExplainError::InvalidPath(path.into()))?;
    let shown = Shown::new(inspected);
    let mut leaves = Vec::new();
    if let Some(object) = shown.value.as_object() {
        collect_leaves(object, &mut Vec::new(), &mut leaves);
    }
    let matching = leaves
        .into_iter()
        .map(|(segments, value)| (ConfigPath::new(segments), value))
        .filter(|(path, _)| path.segments().starts_with(query.segments()))
        .filter(|(path, _)| !shown.is_missing(path))
        .collect::<Vec<_>>();
    let missing = shown
        .missing
        .iter()
        .filter(|path| path.segments().starts_with(query.segments()))
        .collect::<Vec<_>>();
    if matching.is_empty() && missing.is_empty() {
        // A known field or section that's unset, such as an optional setting left out or
        // an empty map.
        return if schema().layer(&query).is_some() || schema().leads_to_fields(query.segments()) {
            Ok(format!("{query} is not set\n"))
        } else {
            Err(ExplainError::UnknownField(path.into()))
        };
    }

    let mut out = String::new();
    for (path, value) in matching {
        out.push_str(&format!("{path} {}\n", leaf_text(&path, &value)));
        match shown.source(&path) {
            Some(resolution) => {
                out.push_str(&format!("  from {}\n", describe(resolution.winner())));
                for shadowed in resolution.shadowed() {
                    out.push_str(&format!("  overriding {}\n", describe(shadowed)));
                }
            }
            None => out.push_str("  from the default\n"),
        }
    }
    for path in missing {
        out.push_str(&format!("{path} is missing: required, but not set\n"));
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
    compact(&without_nulls(value))
}

/// A value as Styx on one line. Objects and lists are written here rather than by
/// facet_styx, which leaves some strings bare that don't read back, such as `<missing>`.
fn compact(value: &Value) -> String {
    if let Some(text) = value.as_string() {
        return string(text.as_str());
    }
    if let Some(object) = value.as_object() {
        let entries = object
            .iter()
            .map(|(key, value)| format!("{} {}", string(key.as_str()), compact(value)))
            .collect::<Vec<_>>();
        return format!("{{{}}}", entries.join(", "));
    }
    if let Some(items) = value.as_array() {
        let items = items.iter().map(compact).collect::<Vec<_>>();
        return format!("({})", items.join(" "));
    }
    facet_styx::to_string_compact(value).unwrap_or_else(|_| "@".to_string())
}

/// Text as Styx: bare where it reads back as itself, quoted otherwise. Multi-line text, such
/// as a public key, stays on one line with escapes, rather than as a heredoc.
fn string(text: &str) -> String {
    if !text.contains('\n')
        && let Ok(bare) = facet_styx::to_string_compact(&Value::from(text))
    {
        let read = facet_styx::from_str::<Value>(&format!("x {bare}")).ok();
        let read_text = read
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|object| object.get("x"))
            .and_then(Value::as_string)
            .map(|read| read.as_str() == text);
        if read_text == Some(true) {
            return bare;
        }
    }
    format!("{text:?}")
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
    shown: &'a Shown<'a>,
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
        if config_path
            .as_ref()
            .is_some_and(|path| self.shown.is_missing(path))
        {
            self.line(
                indent,
                format!("{key_text} {MISSING}"),
                Some(MISSING_COMMENT.to_string()),
            );
            return;
        }
        let comment = config_path
            .as_ref()
            .map(|path| provenance(self.shown.source(path)));
        let secret = config_path
            .as_ref()
            .is_some_and(|path| schema().layer(path) == Some(ConfigLayer::Secret));
        if secret {
            self.line(indent, format!("{key_text} {REDACTED}"), comment);
            return;
        }
        // The missing fields of each item, such as a mapping without a `path_prefix`.
        let item_missing = |index: usize| {
            let item = config_path
                .as_ref()
                .and_then(|path| ConfigPath::parse(&format!("{path}.{index}")).ok())?;
            let missing = self.shown.missing_under(&item);
            (!missing.is_empty()).then(|| format!("missing: {}", missing.join(", ")))
        };
        let inline = styx(value);
        match value.as_array() {
            // Lists of structs, such as mappings, get a line per item, as do lists with
            // items missing fields, to mark them.
            Some(items)
                if !items.is_empty()
                    && (inline.len() > 60
                        || (0..items.len()).any(|i| item_missing(i).is_some())) =>
            {
                let item_comments = (0..items.len()).map(item_missing).collect::<Vec<_>>();
                self.line(indent, format!("{key_text} ("), comment);
                for (item, item_comment) in items.iter().zip(item_comments) {
                    self.line(indent + 1, styx(item), item_comment);
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

/// A partial load's values in the order `effective` gives them: fields in the order they're
/// declared and maps sorted, with the `missing` fields as placeholders where they belong.
fn ordered(
    shape: &'static Shape,
    value: &Value,
    path: &mut Vec<String>,
    missing: &[ConfigPath],
) -> Value {
    match shape.def {
        Def::Option(option) => return ordered(option.t(), value, path, missing),
        Def::Pointer(pointer) => {
            if let Some(pointee) = pointer.pointee() {
                return ordered(pointee, value, path, missing);
            }
        }
        Def::List(list) => {
            if let Some(items) = value.as_array() {
                return items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        path.push(index.to_string());
                        let item = ordered(list.t(), item, path, missing);
                        path.pop();
                        item
                    })
                    .collect::<facet_value::VArray>()
                    .into();
            }
        }
        Def::Map(map) => {
            if let Some(object) = value.as_object() {
                let mut entries = object.iter().collect::<Vec<_>>();
                entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                let mut sorted = VObject::new();
                for (key, entry) in entries {
                    path.push(key.as_str().to_string());
                    sorted.insert(key.as_str(), ordered(map.v(), entry, path, missing));
                    path.pop();
                }
                return sorted.into();
            }
        }
        _ => {}
    }
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let fields = match shape.ty {
        Type::User(UserType::Struct(struct_type)) => struct_type.fields,
        // Internally tagged enums, such as backends: the tag, then the variant's fields.
        Type::User(UserType::Enum(enum_type)) => {
            let Some(tag) = shape.get_tag_attr() else {
                return value.clone();
            };
            let variant = object.get(tag).and_then(Value::as_string).and_then(|name| {
                enum_type
                    .variants
                    .iter()
                    .find(|variant| variant.effective_name() == name.as_str())
            });
            let Some(variant) = variant else {
                return value.clone();
            };
            let mut fields = VObject::new();
            fields.insert(tag, Value::from(variant.effective_name()));
            fill_fields(&mut fields, variant.data.fields, object, path, missing);
            return fields.into();
        }
        _ => return value.clone(),
    };
    let mut ordered_fields = VObject::new();
    fill_fields(&mut ordered_fields, fields, object, path, missing);
    ordered_fields.into()
}

/// `fields` of `object` into `into`, in order, with missing ones as placeholders.
fn fill_fields(
    into: &mut VObject,
    fields: &'static [facet::Field],
    object: &VObject,
    path: &mut Vec<String>,
    missing: &[ConfigPath],
) {
    for field in fields {
        let name = field.effective_name();
        path.push(name.to_string());
        match object.get(name) {
            Some(value) => {
                into.insert(name, ordered(field.shape(), value, path, missing));
            }
            None => {
                let is_missing = ConfigPath::parse(&path.join("."))
                    .is_ok_and(|field_path| missing.contains(&field_path));
                if is_missing {
                    into.insert(name, Value::from("<missing>"));
                }
            }
        }
        path.pop();
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
