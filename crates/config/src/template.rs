use std::fmt;

// r[impl config.auth.paths]
/// A path prefix with claims in it, such as `/tenant/${sub}`.
///
/// `${name}` is replaced by the value of claim `name`, and `$$` is a literal `$`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathTemplate {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Claim(String),
}

/// Why a path template is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTemplate(pub String);

impl fmt::Display for InvalidTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl PathTemplate {
    pub fn parse(template: &str) -> Result<Self, InvalidTemplate> {
        if !template.starts_with('/') {
            return Err(InvalidTemplate(format!("`{template}` must start with `/`")));
        }
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut rest = template;
        while let Some(i) = rest.find('$') {
            literal.push_str(&rest[..i]);
            rest = &rest[i..];
            if let Some(after) = rest.strip_prefix("$$") {
                literal.push('$');
                rest = after;
                continue;
            }
            let Some(braced) = rest.strip_prefix("${") else {
                return Err(InvalidTemplate(format!(
                    "`{template}` has a `$` that isn't `${{claim}}` or `$$`"
                )));
            };
            let Some(end) = braced.find('}') else {
                return Err(InvalidTemplate(format!(
                    "`{template}` has an unclosed `${{`"
                )));
            };
            let name = &braced[..end];
            if name.is_empty()
                || !name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                return Err(InvalidTemplate(format!(
                    "`{template}` refers to claim `{name}`; claim names may only contain ASCII \
                     alphanumerics, underscores and hyphens"
                )));
            }
            if !literal.is_empty() {
                parts.push(Part::Literal(std::mem::take(&mut literal)));
            }
            parts.push(Part::Claim(name.to_string()));
            rest = &braced[end + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Ok(Self { parts })
    }

    // r[impl auth.claim-values]
    // r[impl auth.list-claims]
    /// Whether `path` is under a prefix the template renders to, given the values of each
    /// claim: one for most claims, and each item for lists.
    ///
    /// Values must be single path segments, so a claim can't widen the prefix; others are
    /// skipped.
    pub fn allows(&self, path: &str, claim: impl Fn(&str) -> Vec<String>) -> bool {
        let values = self
            .parts
            .iter()
            .map(|part| match part {
                Part::Literal(_) => Vec::new(),
                Part::Claim(name) => claim(name)
                    .into_iter()
                    .filter(|value| {
                        !value.is_empty() && value != "." && value != ".." && !value.contains('/')
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        matches(&self.parts, &values, path)
    }
}

/// Whether `rest` is under the prefix `parts` render to, with `values` for each part.
fn matches(parts: &[Part], values: &[Vec<String>], rest: &str) -> bool {
    let Some((part, others)) = parts.split_first() else {
        return rest.is_empty() || rest.starts_with('/');
    };
    let next = |rest: &str| matches(others, &values[1..], rest);
    match part {
        // As in `under_prefix`, a trailing `/` matches the end of the path too.
        Part::Literal(literal) if others.is_empty() => rest
            .strip_prefix(literal.trim_end_matches('/'))
            .is_some_and(next),
        Part::Literal(literal) => rest.strip_prefix(literal.as_str()).is_some_and(next),
        // Values can be prefixes of each other, so each one that fits is tried.
        Part::Claim(_) => values[0]
            .iter()
            .any(|value| rest.strip_prefix(value.as_str()).is_some_and(next)),
    }
}

/// Whether `path` is under `prefix`, i.e. equal to it or continuing with a `/`, so
/// `/tenant/acme` matches `/tenant/acme/x` but not `/tenant/acme2/x`.
pub fn under_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::{InvalidTemplate, PathTemplate, under_prefix};

    fn claims(name: &str) -> Vec<String> {
        let values: &[&str] = match name {
            "sub" => &["acme"],
            "region" => &["eu"],
            "evil" => &["../other"],
            "nested" => &["a/b"],
            "empty" => &[""],
            "groups" => &["red", "../blue", "", "green", "a/b"],
            _ => &[],
        };
        values.iter().map(|value| value.to_string()).collect()
    }

    fn allows(template: &str, path: &str) -> bool {
        PathTemplate::parse(template).unwrap().allows(path, claims)
    }

    // r[verify config.auth.paths]
    #[test]
    fn templates_parse_and_match() {
        assert!(allows("/tenant/${sub}", "/tenant/acme/x.pdf"));
        assert!(allows("/tenant/${sub}", "/tenant/acme"));
        assert!(!allows("/tenant/${sub}", "/tenant/acme2/x.pdf"));
        assert!(!allows("/tenant/${sub}", "/tenant/other/x.pdf"));
        assert!(allows("/${region}/${sub}-files", "/eu/acme-files/x"));
        assert!(!allows("/${region}/${sub}-files", "/eu/acme/x"));
        assert!(allows("/price$$/${sub}", "/price$/acme/x"));
        assert!(allows("/shared", "/shared/x"));
        assert!(allows("/tenant/${sub}/", "/tenant/acme/x"));
        assert!(allows("/", "/anything"));
        assert!(!allows("/${missing}", "/x"));
        assert!(!allows("/${missing}/x", "/x"));

        let invalid = |template| PathTemplate::parse(template).unwrap_err();
        assert!(matches!(invalid("tenant/${sub}"), InvalidTemplate(_)));
        assert!(matches!(invalid("/${sub"), InvalidTemplate(_)));
        assert!(matches!(invalid("/${}"), InvalidTemplate(_)));
        assert!(matches!(invalid("/$sub"), InvalidTemplate(_)));
        assert!(matches!(invalid("/${a b}"), InvalidTemplate(_)));
    }

    // r[verify auth.claim-values]
    #[test]
    fn claims_must_be_single_segments() {
        assert!(!allows("/tenant/${evil}", "/tenant/../other/x"));
        assert!(!allows("/tenant/${evil}", "/other/x"));
        assert!(!allows("/tenant/${nested}", "/tenant/a/b/x"));
        assert!(!allows("/tenant/${empty}", "/tenant/x"));
        assert!(!allows("/tenant/${empty}", "/tenant/"));
    }

    // r[verify auth.list-claims]
    #[test]
    fn list_claims_allow_any_of_their_items() {
        assert!(allows("/teams/${groups}", "/teams/red/x"));
        assert!(allows("/teams/${groups}", "/teams/green/x"));
        assert!(!allows("/teams/${groups}", "/teams/blue/x"));
        assert!(!allows("/teams/${groups}", "/teams/a/b/x"));
        assert!(!allows("/teams/${groups}", "/teams/x"));
        // Items are tried in every place, even when one item is a prefix of another.
        let prefixes = |name: &str| match name {
            "a" => vec!["x".to_string(), "xy".to_string()],
            "b" => vec!["z".to_string()],
            _ => Vec::new(),
        };
        let template = PathTemplate::parse("/${a}${b}/${a}").unwrap();
        assert!(template.allows("/xyz/x/file", prefixes));
        assert!(template.allows("/xz/xy", prefixes));
        assert!(!template.allows("/xyz/y", prefixes));
    }

    #[test]
    fn prefixes_match_whole_segments() {
        assert!(under_prefix("/tenant/acme", "/tenant/acme"));
        assert!(under_prefix("/tenant/acme/x.pdf", "/tenant/acme"));
        assert!(under_prefix("/tenant/acme/x.pdf", "/tenant/acme/"));
        assert!(!under_prefix("/tenant/acme2/x.pdf", "/tenant/acme"));
        assert!(under_prefix("/anything", "/"));
    }
}
