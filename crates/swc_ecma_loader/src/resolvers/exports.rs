use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};

/// The parsed representation of the "exports" field in a package.json file.
/// See https://nodejs.org/api/packages.html#package-entry-points for syntax.
#[derive(Debug)]
pub(super) struct Exports {
    subpaths: IndexMap<String, Subpath>,
}

impl Exports {
    /// Resolves a relative path to candidate target file paths, in priority order.
    /// The caller should use the first candidate that exists.
    pub fn resolve_import_path(
        &self,
        rel_path: &str,
        conditions: &HashSet<&str>,
    ) -> Option<Vec<PathBuf>> {
        let mut wildcard_match = None;
        for (candidate, subpath) in self.subpaths.iter() {
            match candidate_matches(candidate, rel_path) {
                None => continue,
                Some(Match::Exact) => {
                    return subpath.matches(&conditions).and_then(|m| match m {
                        SubpathMatch::Targets(paths) => {
                            Some(paths.into_iter().map(PathBuf::from).collect())
                        }
                        SubpathMatch::Exclude => None,
                    });
                }
                Some(Match::Wildcard { replacement }) => {
                    match subpath.matches(&conditions) {
                        None => continue,

                        // If we have a target, save it as a candidate.
                        // We have to keep looking as there may be an exclude directive later.
                        Some(SubpathMatch::Targets(paths)) => {
                            if wildcard_match.is_none() {
                                wildcard_match = Some((paths, replacement));
                            }
                        }

                        // If we have an exclude, stop looking and immediately return none.
                        Some(SubpathMatch::Exclude) => return None,
                    }
                }
            }
        }

        match wildcard_match {
            None => None,
            Some((paths, replacement)) => Some(
                paths
                    .into_iter()
                    .map(|path| path.replace("*", replacement).into())
                    .collect(),
            ),
        }
    }
}

enum Match<'a> {
    Exact,
    Wildcard { replacement: &'a str },
}

fn candidate_matches<'a>(candidate: &str, rel_path: &'a str) -> Option<Match<'a>> {
    let candidate = match (candidate, rel_path) {
        (".", "") => return Some(Match::Exact),
        (".", _) => return None,
        (_, _) => {
            // Strip "./" prefix.
            let candidate = candidate.strip_prefix("./").unwrap_or(candidate);
            if candidate == rel_path {
                return Some(Match::Exact);
            }
            candidate
        }
    };

    if let Some(idx) = candidate.find('*') {
        let (prefix, suffix) = candidate.split_at(idx);
        // Strip the "*" from the suffix.
        let suffix = &suffix[1..];
        if rel_path.starts_with(prefix) && rel_path.ends_with(suffix) {
            // Get the middle part of the path, to be injected into the result.
            let replacement = &rel_path[prefix.len()..(rel_path.len() - suffix.len())];
            return Some(Match::Wildcard { replacement });
        }
    }

    None
}

#[derive(Debug, PartialEq, Eq)]
enum Subpath {
    Targets(Vec<String>),
    Conditions(IndexMap<String, Subpath>),
    Exclude,
}

enum SubpathMatch<'a> {
    /// Candidate targets, in priority order. The first one that exists wins.
    Targets(Vec<&'a str>),
    Exclude,
}

impl Subpath {
    /// Returns the targets this subpath maps to under the active conditions.
    ///
    /// Like TypeScript, a matching condition doesn't end the search: the targets of
    /// later matching conditions are kept as fallbacks, in object order. So when a
    /// "types" target doesn't exist on disk, the "import" target is tried next.
    /// A null target (exclude) ends the search.
    fn matches(&self, active_conditions: &HashSet<&str>) -> Option<SubpathMatch<'_>> {
        match self {
            Subpath::Targets(paths) => Some(SubpathMatch::Targets(
                paths.iter().map(String::as_str).collect(),
            )),
            Subpath::Exclude => Some(SubpathMatch::Exclude),
            Subpath::Conditions(conds) => {
                let mut targets = Vec::new();
                for (cond, subpath) in conds.iter() {
                    if !active_conditions.contains(cond.as_str()) {
                        continue;
                    }
                    match subpath.matches(active_conditions) {
                        // Nested conditions with no match: try the next condition.
                        None => continue,
                        Some(SubpathMatch::Targets(paths)) => targets.extend(paths),
                        Some(SubpathMatch::Exclude) if targets.is_empty() => {
                            return Some(SubpathMatch::Exclude)
                        }
                        Some(SubpathMatch::Exclude) => break,
                    }
                }
                if targets.is_empty() {
                    None
                } else {
                    Some(SubpathMatch::Targets(targets))
                }
            }
        }
    }
}

// The "exports" value can be defined in lots of ways.
//
// - A single subpath, e.g. "exports": "./index.js".
//   Syntactic sugar for {".": "./index.js"}
//
// - A condition spec: {"node": "./index.node.js"}
//   Conditions can be nested: {"node": {"import": "./index.node.js", "default": "./index.js"}}
//
// - An ordered map of subpaths key-value pairs: {".": "./index.js", "./foo/*": "./foo/*.js"}
// - Each subpath value can be one of:
//   - A single target path, e.g. "./index.js" (possibly with wildcards, "./foo/*.js")
//   - An array of target paths
//   - A condition spec.
//   - Null, indicating the subpath is not exported (overriding other exports that may match).

impl<'de> Deserialize<'de> for Exports {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringOrMap;
        impl<'de> Visitor<'de> for StringOrMap {
            type Value = Exports;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("string or map")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let mut subpaths = IndexMap::<String, Subpath>::new();
                subpaths.insert(".".into(), Subpath::Targets(vec![value.to_string()]));
                Ok(Exports { subpaths })
            }

            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut subpaths = IndexMap::<String, Subpath>::new();

                let targets: Vec<String> =
                    Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))?;

                subpaths.insert(".".into(), Subpath::Targets(targets));
                Ok(Exports { subpaths })
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subpaths = IndexMap::new();

                // Peek at the first entry to decide whether it's a map of subpaths or conditions.
                let Some((key, value)) = access.next_entry::<String, Subpath>()? else {
                    // Empty map.
                    return Ok(Exports { subpaths });
                };

                if !key.starts_with(".") {
                    // Keep the peeked condition first: condition order is priority order.
                    let mut conditions = IndexMap::new();
                    conditions.insert(key, value);
                    let rest: IndexMap<String, Subpath> =
                        Deserialize::deserialize(de::value::MapAccessDeserializer::new(access))?;
                    conditions.extend(rest);
                    subpaths.insert(".".to_string(), Subpath::Conditions(conditions));
                    return Ok(Exports { subpaths });
                }

                subpaths.insert(key, value);
                while let Some((key, value)) = access.next_entry::<String, Subpath>()? {
                    subpaths.insert(key, value);
                }

                Ok(Exports { subpaths })
            }
        }

        deserializer.deserialize_any(StringOrMap)
    }
}

struct SubpathVisitor;

impl<'de> Visitor<'de> for SubpathVisitor {
    type Value = Subpath;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("string, map or sequence")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Subpath::Exclude)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Subpath::Targets(vec![value.to_string()]))
    }

    fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let conditions: IndexMap<String, Subpath> =
            Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(Subpath::Conditions(conditions))
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let targets: Vec<String> =
            Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
        Ok(Subpath::Targets(targets))
    }
}

impl<'de> Deserialize<'de> for Subpath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(SubpathVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subpaths() {
        let exports: Exports = serde_json::from_str(
            r#"{
            ".": "./index.js",
            "./foo/*": "./foo/*.js",
            "./bar": {"node": "./bar.node.js", "default": "./bar.js"},
            "./baz": {"node": "./baz.node.js", "default": null},
            "./qux": null
        }"#,
        )
        .unwrap();
        assert_eq!(exports.subpaths.len(), 5);
        assert_eq!(
            exports.subpaths.get(".").unwrap(),
            &Subpath::Targets(vec!["./index.js".into()])
        );
        assert_eq!(
            exports.subpaths.get("./foo/*").unwrap(),
            &Subpath::Targets(vec!["./foo/*.js".into()])
        );
        assert_eq!(
            exports.subpaths.get("./bar").unwrap(),
            &Subpath::Conditions({
                let mut map = IndexMap::new();
                map.insert(
                    "node".to_owned(),
                    Subpath::Targets(vec!["./bar.node.js".into()]),
                );
                map.insert(
                    "default".to_owned(),
                    Subpath::Targets(vec!["./bar.js".into()]),
                );
                map
            })
        );
        assert_eq!(
            exports.subpaths.get("./baz").unwrap(),
            &Subpath::Conditions({
                let mut map = IndexMap::new();
                map.insert(
                    "node".to_owned(),
                    Subpath::Targets(vec!["./baz.node.js".into()]),
                );
                map.insert("default".to_owned(), Subpath::Exclude);
                map
            })
        );
        assert_eq!(exports.subpaths.get("./qux").unwrap(), &Subpath::Exclude);
    }

    #[test]
    fn parse_toplevel_conditions() {
        let exports: Exports = serde_json::from_str(
            r#"{
            "node": {"import": "./bar.node.js", "default": "./bar.js"},
            "default": "./index.js"
        }"#,
        )
        .unwrap();
        assert_eq!(exports.subpaths.len(), 1);
        assert_eq!(
            exports.subpaths.get(".").unwrap(),
            &Subpath::Conditions({
                let mut map = IndexMap::new();
                map.insert(
                    "node".to_owned(),
                    Subpath::Conditions({
                        let mut map = IndexMap::new();
                        map.insert(
                            "import".to_owned(),
                            Subpath::Targets(vec!["./bar.node.js".into()]),
                        );
                        map.insert(
                            "default".to_owned(),
                            Subpath::Targets(vec!["./bar.js".into()]),
                        );
                        map
                    }),
                );
                map.insert(
                    "default".to_owned(),
                    Subpath::Targets(vec!["./index.js".into()]),
                );
                map
            })
        );
    }

    fn resolve(exports: &str, rel_path: &str, conditions: &[&str]) -> Option<Vec<PathBuf>> {
        let exports: Exports = serde_json::from_str(exports).unwrap();
        let conditions: HashSet<&str> = conditions.iter().copied().collect();
        exports.resolve_import_path(rel_path, &conditions)
    }

    fn paths(paths: &[&str]) -> Option<Vec<PathBuf>> {
        Some(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn condition_order() {
        let exports = r#"{
            "types": "./dist/index.d.ts",
            "import": "./dist/index.mjs",
            "default": "./dist/index.js"
        }"#;

        // Object order decides priority, not the order of the active conditions.
        assert_eq!(
            resolve(exports, "", &["default", "import", "types"]),
            paths(&["./dist/index.d.ts", "./dist/index.mjs", "./dist/index.js"])
        );
        assert_eq!(
            resolve(exports, "", &["import", "default"]),
            paths(&["./dist/index.mjs", "./dist/index.js"])
        );
    }

    #[test]
    fn wildcard_types_falls_back_to_import() {
        // The "./*" entry of @modelcontextprotocol/sdk >= 1.29.0. For "server/mcp.js" the
        // "types" target is "dist/esm/server/mcp.js.d.ts", which doesn't exist, so the
        // "import" target must remain a candidate.
        let exports = r#"{
            ".": {
                "types": "./dist/esm/index.d.ts",
                "import": "./dist/esm/index.js",
                "require": "./dist/cjs/index.js"
            },
            "./*": {
                "types": "./dist/esm/*.d.ts",
                "import": "./dist/esm/*",
                "require": "./dist/cjs/*"
            }
        }"#;

        assert_eq!(
            resolve(exports, "server/mcp.js", &["types", "import", "require"]),
            paths(&[
                "./dist/esm/server/mcp.js.d.ts",
                "./dist/esm/server/mcp.js",
                "./dist/cjs/server/mcp.js",
            ])
        );
    }

    #[test]
    fn nested_conditions_without_match_fall_through() {
        let exports = r#"{
            "node": {"import": "./index.node.mjs"},
            "default": "./index.js"
        }"#;

        assert_eq!(
            resolve(exports, "", &["node", "require", "default"]),
            paths(&["./index.js"])
        );
    }

    #[test]
    fn null_target_ends_search() {
        // A null target as the first match excludes the subpath.
        assert_eq!(
            resolve(
                r#"{"./internal/*": {"node": null, "default": "./internal/*.js"}}"#,
                "internal/foo",
                &["node", "default"]
            ),
            None
        );

        // A null target after a match drops the later fallbacks.
        assert_eq!(
            resolve(
                r#"{"types": "./index.d.ts", "node": null, "default": "./index.js"}"#,
                "",
                &["types", "node", "default"]
            ),
            paths(&["./index.d.ts"])
        );
    }
}
