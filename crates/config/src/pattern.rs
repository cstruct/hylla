use std::fmt;

// r[impl konklav.patterns]
/// A request path pattern of a Konklav rule, such as `/object/{object_id}/*`.
///
/// It's matched segment by segment: a literal segment matches itself, `{name}` matches any
/// one non-empty segment, capturing it, and a final `*` matches the rest of the path,
/// including nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Capture(String),
    Rest,
}

/// Why a path pattern is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPattern(pub String);

impl fmt::Display for InvalidPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl PathPattern {
    pub fn parse(pattern: &str) -> Result<Self, InvalidPattern> {
        let Some(rest) = pattern.strip_prefix('/') else {
            return Err(InvalidPattern(format!("`{pattern}` must start with `/`")));
        };
        let parts = rest.split('/').collect::<Vec<_>>();
        let mut segments = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let segment = if *part == "*" {
                if index + 1 != parts.len() {
                    return Err(InvalidPattern(format!(
                        "`{pattern}` has a `*` that isn't its last segment"
                    )));
                }
                Segment::Rest
            } else if let Some(name) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                {
                    return Err(InvalidPattern(format!(
                        "`{pattern}` captures `{name}`; capture names may only contain ASCII \
                         alphanumerics and underscores"
                    )));
                }
                if segments.contains(&Segment::Capture(name.to_string())) {
                    return Err(InvalidPattern(format!(
                        "`{pattern}` captures `{name}` twice"
                    )));
                }
                Segment::Capture(name.to_string())
            } else if part.contains(['{', '}', '*']) {
                return Err(InvalidPattern(format!(
                    "`{pattern}` has segment `{part}`; `{{name}}` and `*` must be whole segments"
                )));
            } else {
                Segment::Literal((*part).to_string())
            };
            segments.push(segment);
        }
        Ok(Self { segments })
    }

    /// The names the pattern captures, in order.
    pub fn captures(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Capture(name) => Some(name.as_str()),
            _ => None,
        })
    }

    /// The captures of `path`, by name, if the pattern matches it.
    pub fn matches<'a>(&'a self, path: &'a str) -> Option<Vec<(&'a str, &'a str)>> {
        let mut parts = path.strip_prefix('/')?.split('/');
        let mut captures = Vec::new();
        for segment in &self.segments {
            match segment {
                Segment::Rest => return Some(captures),
                Segment::Literal(literal) => {
                    if parts.next()? != literal {
                        return None;
                    }
                }
                Segment::Capture(name) => {
                    let value = parts.next().filter(|value| !value.is_empty())?;
                    captures.push((name.as_str(), value));
                }
            }
        }
        parts.next().is_none().then_some(captures)
    }

    /// Whether the pattern can match a path under `prefix`, as a mapping's path prefix
    /// matches them: `prefix` itself, or continuing with a `/`.
    pub fn can_match_under(&self, prefix: &str) -> bool {
        let prefix = prefix.trim_end_matches('/');
        let Some(prefix) = prefix.strip_prefix('/') else {
            // `/`, under which every path is.
            return true;
        };
        let mut segments = self.segments.iter();
        for part in prefix.split('/') {
            match segments.next() {
                None => return false,
                Some(Segment::Rest) => return true,
                Some(Segment::Literal(literal)) if literal != part => return false,
                Some(Segment::Capture(_)) if part.is_empty() => return false,
                Some(_) => {}
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::PathPattern;

    fn captures(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
        let pattern = PathPattern::parse(pattern).unwrap();
        pattern.matches(path).map(|captures| {
            captures
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect()
        })
    }

    fn capture(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    // r[verify konklav.patterns]
    #[test]
    fn captures_segments_and_matches_the_rest() {
        for path in ["/object/o-1", "/object/o-1/", "/object/o-1/a/b"] {
            assert_eq!(
                captures("/object/{object_id}/*", path),
                Some(vec![capture("object_id", "o-1")]),
                "{path}"
            );
        }
        assert_eq!(captures("/object/{object_id}/*", "/object"), None);
        assert_eq!(captures("/object/{object_id}/*", "/object/"), None);
        assert_eq!(captures("/object/{object_id}/*", "/objects/o-1"), None);
        assert_eq!(
            captures("/t/{tenant}/o/{object}", "/t/acme/o/report"),
            Some(vec![capture("tenant", "acme"), capture("object", "report")])
        );
        assert_eq!(
            captures("/t/{tenant}/o/{object}", "/t/acme/o/report/x"),
            None
        );
        assert_eq!(captures("/t/{tenant}/o/{object}", "/t/acme/o"), None);
        assert_eq!(captures("/public/*", "/public"), Some(Vec::new()));
        assert_eq!(captures("/*", "/anything/at/all"), Some(Vec::new()));
        assert_eq!(captures("/exact", "/exact"), Some(Vec::new()));
        assert_eq!(captures("/exact", "/exact/"), None);
    }

    // r[verify config.validate.konklav]
    #[test]
    fn invalid_patterns_are_refused() {
        for pattern in [
            "object/{id}",
            "/object/*/x",
            "/object/{}",
            "/object/{object-id}",
            "/object/{id}/{id}",
            "/object/x{id}",
            "/object/{id",
            "/object/x*",
        ] {
            assert!(PathPattern::parse(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn patterns_are_checked_against_mapping_prefixes() {
        let pattern = PathPattern::parse("/object/{object_id}/*").unwrap();
        assert!(pattern.can_match_under("/"));
        assert!(pattern.can_match_under("/object"));
        assert!(pattern.can_match_under("/object/"));
        assert!(pattern.can_match_under("/object/o-1/files"));
        assert!(!pattern.can_match_under("/objects"));
        assert!(!pattern.can_match_under("/other/o-1"));
        let exact = PathPattern::parse("/object/{object_id}").unwrap();
        assert!(exact.can_match_under("/object/o-1"));
        assert!(!exact.can_match_under("/object/o-1/files"));
    }
}
