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

/// Why a template couldn't be filled in from a token's claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The claim is missing, or isn't a string or number.
    MissingClaim(String),
    /// The claim's value isn't a single path segment, such as `a/b` or `..`.
    UnsafeClaim(String),
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
    /// The path prefix for a token, given its claims as strings.
    ///
    /// Claim values must be single path segments, so a claim can't widen the prefix.
    pub fn render(&self, claim: impl Fn(&str) -> Option<String>) -> Result<String, RenderError> {
        let mut path = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(literal) => path.push_str(literal),
                Part::Claim(name) => {
                    let value =
                        claim(name).ok_or_else(|| RenderError::MissingClaim(name.clone()))?;
                    if value.is_empty() || value == "." || value == ".." || value.contains('/') {
                        return Err(RenderError::UnsafeClaim(name.clone()));
                    }
                    path.push_str(&value);
                }
            }
        }
        Ok(path)
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
    use super::{InvalidTemplate, PathTemplate, RenderError, under_prefix};

    fn claims(name: &str) -> Option<String> {
        match name {
            "sub" => Some("acme".into()),
            "region" => Some("eu".into()),
            "evil" => Some("../other".into()),
            "nested" => Some("a/b".into()),
            _ => None,
        }
    }

    // r[verify config.auth.paths]
    #[test]
    fn templates_parse_and_render() {
        let render = |template| PathTemplate::parse(template).unwrap().render(claims);
        assert_eq!(render("/tenant/${sub}"), Ok("/tenant/acme".into()));
        assert_eq!(
            render("/${region}/${sub}-files"),
            Ok("/eu/acme-files".into())
        );
        assert_eq!(render("/price$$/${sub}"), Ok("/price$/acme".into()));
        assert_eq!(render("/shared"), Ok("/shared".into()));
        assert_eq!(
            render("/${missing}"),
            Err(RenderError::MissingClaim("missing".into()))
        );

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
        let render = |template| PathTemplate::parse(template).unwrap().render(claims);
        assert_eq!(
            render("/tenant/${evil}"),
            Err(RenderError::UnsafeClaim("evil".into()))
        );
        assert_eq!(
            render("/tenant/${nested}"),
            Err(RenderError::UnsafeClaim("nested".into()))
        );
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
