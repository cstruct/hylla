use regex::Regex;

use crate::{Mapping, Server};

/// Routes requests of one server to its backends, with its rewrites compiled.
#[derive(Debug, Clone)]
pub struct Router {
    mappings: Vec<(Mapping, Vec<(Regex, String)>)>,
}

/// Where to send a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route<'a> {
    /// The name of the backend.
    pub backend: &'a str,
    /// The path to request from the backend.
    pub path: String,
}

impl Router {
    /// Compiles the rewrites of `server`'s mappings.
    ///
    /// Fails with the indices of the first mapping and rewrite whose pattern doesn't
    /// compile; validated configs have none.
    pub fn new(server: &Server) -> Result<Self, (usize, usize, regex::Error)> {
        let mappings = server
            .mappings
            .iter()
            .enumerate()
            .map(|(mapping_index, mapping)| {
                let rewrites = mapping
                    .rewrites
                    .iter()
                    .enumerate()
                    .map(|(rewrite_index, rewrite)| {
                        Regex::new(&rewrite.pattern)
                            .map(|regex| (regex, rewrite.replace.clone()))
                            .map_err(|error| (mapping_index, rewrite_index, error))
                    })
                    .collect::<Result<_, _>>()?;
                Ok((mapping.clone(), rewrites))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { mappings })
    }

    // r[impl config.mappings.priority]
    // r[impl config.mappings.rewrites]
    /// Routes `path` with the first matching mapping, applying its rewrites in order.
    pub fn route(&self, path: &str) -> Option<Route<'_>> {
        let (mapping, rewrites) = self
            .mappings
            .iter()
            .find(|(mapping, _)| mapping.matches(path))?;
        let path = rewrites
            .iter()
            .fold(path.to_string(), |path, (regex, replace)| {
                regex.replace(&path, replace.as_str()).into_owned()
            });
        Some(Route {
            backend: &mapping.backend,
            path,
        })
    }
}

// r[impl config.validate.replacements]
/// The capture group references in `replacement` that `regex` has no group for, as
/// written, such as `$2` or `${name}`.
///
/// Mirrors how the regex crate expands replacements: `$$` is a literal `$`, `${...}` is a
/// reference to whatever is in the braces, and `$` followed by letters, digits and
/// underscores refers to the longest such run (so `$1a` refers to a group named `1a`). Any
/// other `$` is literal. Numeric references are group indices, others group names.
pub(crate) fn unknown_group_refs(regex: &Regex, replacement: &str) -> Vec<String> {
    let mut unknown = Vec::new();
    let mut rest = replacement;
    while let Some(i) = rest.find('$') {
        rest = &rest[i..];
        if rest.starts_with("$$") {
            rest = &rest[2..];
            continue;
        }
        let Some((name, len)) = group_ref(rest) else {
            rest = &rest[1..];
            continue;
        };
        let known = match name.parse::<usize>() {
            Ok(index) => index < regex.captures_len(),
            Err(_) => regex.capture_names().flatten().any(|group| group == name),
        };
        if !known {
            unknown.push(rest[..len].to_string());
        }
        rest = &rest[len..];
    }
    unknown
}

/// The group name of the reference at the start of `rest` (which starts with `$`), and
/// the reference's length.
fn group_ref(rest: &str) -> Option<(&str, usize)> {
    let after = &rest[1..];
    if let Some(braced) = after.strip_prefix('{') {
        let end = braced.find('}')?;
        return Some((&braced[..end], end + 3));
    }
    let end = after
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or(after.len());
    (end > 0).then(|| (&after[..end], end + 1))
}

#[cfg(test)]
mod tests {
    use super::{Route, Router};
    use crate::{Mapping, Rewrite, Server};

    fn mapping(path_prefix: &str, backend: &str, rewrites: &[(&str, &str)]) -> Mapping {
        Mapping {
            path_prefix: path_prefix.into(),
            backend: backend.into(),
            rewrites: rewrites
                .iter()
                .map(|(pattern, replace)| Rewrite {
                    pattern: (*pattern).into(),
                    replace: (*replace).into(),
                })
                .collect(),
        }
    }

    fn router(mappings: Vec<Mapping>) -> Router {
        Router::new(&Server {
            listen: "127.0.0.1:8080".parse().unwrap(),
            server_names: vec!["cache.example.com".into()],
            mappings,
        })
        .unwrap()
    }

    fn route(router: &Router, path: &str) -> Option<(String, String)> {
        router
            .route(path)
            .map(|Route { backend, path }| (backend.to_string(), path))
    }

    // r[verify config.mappings.priority]
    #[test]
    fn the_first_matching_mapping_wins() {
        let router = router(vec![
            mapping("/assets/images", "images", &[]),
            mapping("/assets", "assets", &[]),
            mapping("/", "default", &[]),
        ]);
        let backend = |path| route(&router, path).map(|(backend, _)| backend);
        assert_eq!(backend("/assets/images/a.png").as_deref(), Some("images"));
        assert_eq!(backend("/assets/a.css").as_deref(), Some("assets"));
        assert_eq!(backend("/other").as_deref(), Some("default"));
    }

    // r[verify config.mappings.prefix]
    #[test]
    fn prefixes_match_whole_segments() {
        let mapping = mapping("/assets", "assets", &[]);
        assert!(mapping.matches("/assets"));
        assert!(mapping.matches("/assets/a.css"));
        assert!(!mapping.matches("/assets2/a.css"));
        assert!(!mapping.matches("/other"));
        assert_eq!(route(&router(vec![mapping]), "/other"), None);
    }

    // r[verify config.mappings.rewrites]
    #[test]
    fn rewrites_apply_in_order() {
        let router = router(vec![
            mapping(
                "/assets",
                "assets",
                &[
                    (r"^/assets/(.*)$", "/$1"),
                    (r"^/v(?<version>\d+)/", "/releases/${version}/"),
                    (r"\.min\.js$", ".js"),
                ],
            ),
            mapping("/", "default", &[]),
        ]);
        assert_eq!(
            route(&router, "/assets/v2/app.min.js"),
            Some(("assets".into(), "/releases/2/app.js".into()))
        );
        // Rewrites that don't match leave the path as is.
        assert_eq!(
            route(&router, "/assets/app.css"),
            Some(("assets".into(), "/app.css".into()))
        );
        assert_eq!(
            route(&router, "/index.html"),
            Some(("default".into(), "/index.html".into()))
        );
    }

    // r[verify config.validate.replacements]
    #[test]
    fn replacements_may_only_refer_to_existing_groups() {
        let regex = regex::Regex::new(r"^/(?<dir>[^/]+)/(.*)$").unwrap();
        let unknown = |replacement| super::unknown_group_refs(&regex, replacement);
        assert!(unknown("/$1/${2}/${dir}/$dir/$0").is_empty());
        assert!(unknown("/literal/$$3/$/${unclosed").is_empty());
        assert_eq!(unknown("/$3/${4}"), vec!["$3", "${4}"]);
        assert_eq!(unknown("/${name}/$other-x"), vec!["${name}", "$other"]);
        // `$1a` refers to a group named `1a`, not group 1 followed by `a`.
        assert_eq!(unknown("/$1a"), vec!["$1a"]);
        assert!(unknown("/${1}a").is_empty());
    }

    #[test]
    fn invalid_patterns_fail_to_compile() {
        let server = Server {
            listen: "127.0.0.1:8080".parse().unwrap(),
            server_names: vec!["cache.example.com".into()],
            mappings: vec![
                mapping("/", "a", &[]),
                mapping("/", "b", &[("^/ok$", "/"), ("(unclosed", "")]),
            ],
        };
        let Err((mapping, rewrite, _)) = Router::new(&server) else {
            panic!("expected an invalid pattern");
        };
        assert_eq!((mapping, rewrite), (1, 1));
    }
}
