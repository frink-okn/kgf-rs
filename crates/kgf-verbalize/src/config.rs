//! The verbalization config: what a person writes, and what it resolves to.
//!
//! Two types for one document, on the same split `kgf build` makes for its own
//! config. [`Config`] is the on-disk shape — permissive, everything optional,
//! unknown keys refused so a misspelled key cannot silently do nothing.
//! [`Resolved`] is the fully defaulted, validated form: IRIs have parsed,
//! templates have been split into literal text and field references, every
//! placeholder names a declared field, and per-target lists have been merged
//! over the defaults. Nothing past [`Config::resolve`] reads a string it has
//! not already checked.
//!
//! The document answers four questions: which classes become roots; how a root
//! and the nodes it mentions get a name; which predicates are walked; and how
//! many values one predicate may contribute. What it deliberately does not
//! carry is the label cascade the bundle already declares — that is layered
//! in at bind time from the manifest's `label` role, behind a target's own
//! `label_predicates`, as the answer for whatever the config did not name.
//!
//! # Two label mechanisms, because two questions
//!
//! A **profile** is keyed by class and applies to a node wherever it appears:
//! as a root, or mentioned in some other root's text. A target's
//! **`label_template`** applies only when the node is the root. They differ by
//! position, not by syntax: a Location describes *itself* as
//! `Location 123: Program X`, while a Site that mentions it says
//! `has location: Program X`. Collapsing the two would push the rich template
//! into every document that references the class.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// How many values one predicate contributes when a config does not say.
pub const DEFAULT_PREDICATE_LIMIT: u32 = 3;

/// The verbalization section as written: in `build.yaml`, or in the body of a
/// preview request.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The most values any one predicate contributes to a text; a stable
    /// uniform sample survives past it. Per-target `predicate_limit` overrides
    /// this for one target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate_limit: Option<u32>,

    /// Predicate lists merged into every target: a target's own list is
    /// appended, without duplicates.
    #[serde(default)]
    pub defaults: Lists,

    /// Label templates keyed by class, applied to a node of that class in any
    /// position. The key is a local name for the profile.
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,

    /// The classes whose members become roots. The key is a local name for
    /// the target, used in output and diagnostics.
    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
}

/// The three predicate lists, as written at either level.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lists {
    /// Predicates tried for a name, in this order, before the bundle's own
    /// `label` cascade. Names the domain predicate that carries the real name
    /// ahead of a generic one that may carry boilerplate, and may go on to
    /// description, identifier, citation: text that names a node *somehow*
    /// when nothing better exists, and that a label endpoint should never
    /// return. Full IRIs.
    #[serde(default)]
    pub label_predicates: Vec<String>,

    /// Predicates left out of the walk. Full IRIs.
    #[serde(default)]
    pub ignore_predicates: Vec<String>,

    /// When non-empty, the only predicates walked; `ignore_predicates` still
    /// applies within it. Full IRIs.
    #[serde(default)]
    pub include_predicates: Vec<String>,
}

/// A label template for one class.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The class IRI this profile names members of.
    #[serde(rename = "type")]
    pub class: String,
    /// The template, with `{field}` placeholders naming entries of `fields`.
    pub template: String,
    /// Placeholder name → predicate IRI whose value fills it.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// One class whose members become roots.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// The class IRI.
    #[serde(rename = "type")]
    pub class: String,
    /// See [`Lists::label_predicates`].
    #[serde(default)]
    pub label_predicates: Vec<String>,
    /// See [`Lists::ignore_predicates`].
    #[serde(default)]
    pub ignore_predicates: Vec<String>,
    /// See [`Lists::include_predicates`].
    #[serde(default)]
    pub include_predicates: Vec<String>,
    /// Overrides the document's `predicate_limit` for this target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate_limit: Option<u32>,
    /// A template for the root's own label, used only when the node is the
    /// root of a record. `{field}` placeholders name entries of `label_fields`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_template: Option<String>,
    /// Placeholder name → predicate IRI, for `label_template`.
    #[serde(default)]
    pub label_fields: BTreeMap<String, String>,
}

/// Why a config could not be resolved. Every variant names the key at fault.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A config with nothing to verbalize.
    #[error("the config declares no targets")]
    NoTargets,
    /// A value that must be a full IRI is not one.
    #[error("{at}: {iri:?} is not an absolute IRI: {detail}")]
    NotAnIri {
        /// Where in the document, as a dotted key path.
        at: String,
        /// The offending value.
        iri: String,
        /// What the IRI parser objected to.
        detail: String,
    },
    /// A `predicate_limit` of zero would empty every text.
    #[error("{at}: predicate_limit must be at least 1")]
    ZeroLimit {
        /// Where in the document.
        at: String,
    },
    /// A template placeholder with no field behind it.
    #[error("{at}: template placeholder {{{field}}} names no entry of its fields")]
    UnknownField {
        /// Where in the document.
        at: String,
        /// The placeholder's name.
        field: String,
    },
    /// A template with an unbalanced or nested brace.
    #[error("{at}: template {template:?} has an unbalanced brace")]
    MalformedTemplate {
        /// Where in the document.
        at: String,
        /// The template as written.
        template: String,
    },
    /// Two profiles claim one class; which would win is not defined.
    #[error("profiles {first:?} and {second:?} both name class {class}")]
    DuplicateProfileClass {
        /// The first profile's name.
        first: String,
        /// The second profile's name.
        second: String,
        /// The class both claim.
        class: String,
    },
}

/// A validated, fully defaulted config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The document-level lists as written, so a diagnostic about an IRI that
    /// reached every target through them can name where it was written.
    pub defaults: Lists,
    /// Profiles in name order. Classes are distinct.
    pub profiles: Vec<ResolvedProfile>,
    /// Targets in name order, each with the defaults merged in.
    pub targets: Vec<ResolvedTarget>,
}

/// A profile with its template parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// The profile's local name.
    pub name: String,
    /// The class it applies to.
    pub class: String,
    /// The parsed template; every field reference is in `fields`.
    pub template: Template,
    /// Placeholder name → predicate IRI.
    pub fields: BTreeMap<String, String>,
}

/// A target with defaults merged and its template parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// The target's local name.
    pub name: String,
    /// The class whose members are roots.
    pub class: String,
    /// Extra label predicates, defaults first, without duplicates.
    pub label_predicates: Vec<String>,
    /// Predicates left out of the walk, defaults first, without duplicates.
    pub ignore_predicates: Vec<String>,
    /// If non-empty, the only predicates walked.
    pub include_predicates: Vec<String>,
    /// Values per predicate. At least 1.
    pub predicate_limit: u32,
    /// The root's own label template, if any, with its fields.
    pub label_template: Option<(Template, BTreeMap<String, String>)>,
}

/// A label template split into the text between placeholders and the
/// placeholders themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<TemplatePart>,
}

/// One piece of a [`Template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    /// Literal text, copied through.
    Text(String),
    /// A `{name}` placeholder, filled from the named field.
    Field(String),
}

impl Template {
    /// Split `{field}` placeholders out of a template.
    ///
    /// Braces do not nest and cannot be escaped: the syntax is for
    /// `Location {id}: {program}`, not for a templating language, and a
    /// literal brace in a label is not a case worth a syntax.
    pub fn parse(template: &str) -> Option<Self> {
        let mut parts = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}')? + open;
            if rest[open + 1..close].contains('{') {
                return None;
            }
            if open > 0 {
                parts.push(TemplatePart::Text(rest[..open].to_owned()));
            }
            let field = rest[open + 1..close].trim();
            if field.is_empty() {
                return None;
            }
            parts.push(TemplatePart::Field(field.to_owned()));
            rest = &rest[close + 1..];
        }
        if rest.contains('}') {
            return None;
        }
        if !rest.is_empty() {
            parts.push(TemplatePart::Text(rest.to_owned()));
        }
        Some(Self { parts })
    }

    /// The pieces, in order.
    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    /// Every placeholder name, in order of first appearance.
    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            TemplatePart::Field(field) => Some(field.as_str()),
            TemplatePart::Text(_) => None,
        })
    }
}

impl Config {
    /// Validate and default the document. See the module docs for what that
    /// establishes.
    pub fn resolve(&self) -> Result<Resolved, ConfigError> {
        if self.targets.is_empty() {
            return Err(ConfigError::NoTargets);
        }
        if let Some(0) = self.predicate_limit {
            return Err(ConfigError::ZeroLimit {
                at: "predicate_limit".to_owned(),
            });
        }
        check_iris("defaults", &self.defaults)?;

        let mut profiles = Vec::with_capacity(self.profiles.len());
        let mut classes: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, profile) in &self.profiles {
            let at = format!("profiles.{name}");
            check_iri(&format!("{at}.type"), &profile.class)?;
            for (field, iri) in &profile.fields {
                check_iri(&format!("{at}.fields.{field}"), iri)?;
            }
            let template = parse_template(&at, &profile.template, &profile.fields)?;
            if let Some(first) = classes.insert(&profile.class, name) {
                return Err(ConfigError::DuplicateProfileClass {
                    first: first.to_owned(),
                    second: name.clone(),
                    class: profile.class.clone(),
                });
            }
            profiles.push(ResolvedProfile {
                name: name.clone(),
                class: profile.class.clone(),
                template,
                fields: profile.fields.clone(),
            });
        }

        let mut targets = Vec::with_capacity(self.targets.len());
        for (name, target) in &self.targets {
            let at = format!("targets.{name}");
            check_iri(&format!("{at}.type"), &target.class)?;
            let own = Lists {
                label_predicates: target.label_predicates.clone(),
                ignore_predicates: target.ignore_predicates.clone(),
                include_predicates: target.include_predicates.clone(),
            };
            check_iris(&at, &own)?;
            for (field, iri) in &target.label_fields {
                check_iri(&format!("{at}.label_fields.{field}"), iri)?;
            }
            let predicate_limit = match target.predicate_limit {
                Some(0) => {
                    return Err(ConfigError::ZeroLimit {
                        at: format!("{at}.predicate_limit"),
                    });
                }
                Some(limit) => limit,
                None => self.predicate_limit.unwrap_or(DEFAULT_PREDICATE_LIMIT),
            };
            let label_template = target
                .label_template
                .as_deref()
                .map(|template| {
                    parse_template(&at, template, &target.label_fields)
                        .map(|parsed| (parsed, target.label_fields.clone()))
                })
                .transpose()?;
            targets.push(ResolvedTarget {
                name: name.clone(),
                class: target.class.clone(),
                label_predicates: merged(&self.defaults.label_predicates, &own.label_predicates),
                ignore_predicates: merged(&self.defaults.ignore_predicates, &own.ignore_predicates),
                include_predicates: merged(
                    &self.defaults.include_predicates,
                    &own.include_predicates,
                ),
                predicate_limit,
                label_template,
            });
        }

        Ok(Resolved {
            defaults: self.defaults.clone(),
            profiles,
            targets,
        })
    }
}

/// Defaults first, then the target's own entries, each IRI once.
fn merged(defaults: &[String], own: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    defaults
        .iter()
        .chain(own)
        .filter(|iri| seen.insert(iri.as_str()))
        .cloned()
        .collect()
}

fn check_iris(at: &str, lists: &Lists) -> Result<(), ConfigError> {
    for (key, list) in [
        ("label_predicates", &lists.label_predicates),
        ("ignore_predicates", &lists.ignore_predicates),
        ("include_predicates", &lists.include_predicates),
    ] {
        for iri in list {
            check_iri(&format!("{at}.{key}"), iri)?;
        }
    }
    Ok(())
}

fn check_iri(at: &str, iri: &str) -> Result<(), ConfigError> {
    oxiri::Iri::parse(iri)
        .map(|_| ())
        .map_err(|error| ConfigError::NotAnIri {
            at: at.to_owned(),
            iri: iri.to_owned(),
            detail: error.to_string(),
        })
}

fn parse_template(
    at: &str,
    template: &str,
    fields: &BTreeMap<String, String>,
) -> Result<Template, ConfigError> {
    let parsed = Template::parse(template).ok_or_else(|| ConfigError::MalformedTemplate {
        at: at.to_owned(),
        template: template.to_owned(),
    })?;
    for field in parsed.fields() {
        if !fields.contains_key(field) {
            return Err(ConfigError::UnknownField {
                at: at.to_owned(),
                field: field.to_owned(),
            });
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Config {
        serde_json::from_str(json).expect("a well-formed config")
    }

    #[test]
    fn templates_split_into_text_and_fields() {
        let template = Template::parse("Location {id}: {program}").unwrap();
        assert_eq!(
            template.parts(),
            &[
                TemplatePart::Text("Location ".to_owned()),
                TemplatePart::Field("id".to_owned()),
                TemplatePart::Text(": ".to_owned()),
                TemplatePart::Field("program".to_owned()),
            ]
        );
        assert_eq!(
            Template::parse("{ value } {unit}")
                .unwrap()
                .fields()
                .collect::<Vec<_>>(),
            ["value", "unit"]
        );
        assert_eq!(Template::parse("plain").unwrap().parts().len(), 1);
        assert!(Template::parse("").unwrap().parts().is_empty());
        assert!(Template::parse("{unclosed").is_none());
        assert!(Template::parse("stray}").is_none());
        assert!(Template::parse("{a{b}}").is_none());
        assert!(Template::parse("{}").is_none());
    }

    #[test]
    fn defaults_merge_under_a_target_without_duplicates() {
        let config = parse(
            r#"{
              "defaults": {
                "ignore_predicates": ["http://a/", "http://b/"],
                "label_predicates": ["http://l/"]
              },
              "targets": {
                "t": {
                  "type": "http://example.org/T",
                  "ignore_predicates": ["http://b/", "http://c/"]
                }
              }
            }"#,
        );
        let resolved = config.resolve().unwrap();
        let target = &resolved.targets[0];
        assert_eq!(target.name, "t");
        assert_eq!(
            target.ignore_predicates,
            ["http://a/", "http://b/", "http://c/"]
        );
        assert_eq!(target.label_predicates, ["http://l/"]);
        assert!(target.include_predicates.is_empty());
        assert_eq!(target.predicate_limit, DEFAULT_PREDICATE_LIMIT);
        assert!(target.label_template.is_none());
    }

    #[test]
    fn a_target_limit_overrides_the_document_limit() {
        let config = parse(
            r#"{
              "predicate_limit": 5,
              "targets": {
                "a": {"type": "http://example.org/A"},
                "b": {"type": "http://example.org/B", "predicate_limit": 2}
              }
            }"#,
        );
        let resolved = config.resolve().unwrap();
        assert_eq!(resolved.targets[0].predicate_limit, 5);
        assert_eq!(resolved.targets[1].predicate_limit, 2);
    }

    #[test]
    fn a_zero_limit_is_refused_where_it_was_written() {
        let config = parse(r#"{"predicate_limit": 0, "targets": {"a": {"type": "http://x/A"}}}"#);
        assert_eq!(
            config.resolve().unwrap_err(),
            ConfigError::ZeroLimit {
                at: "predicate_limit".to_owned()
            }
        );
        let config = parse(r#"{"targets": {"a": {"type": "http://x/A", "predicate_limit": 0}}}"#);
        assert_eq!(
            config.resolve().unwrap_err(),
            ConfigError::ZeroLimit {
                at: "targets.a.predicate_limit".to_owned()
            }
        );
    }

    #[test]
    fn a_placeholder_without_a_field_is_refused() {
        let config = parse(
            r#"{
              "profiles": {
                "p": {"type": "http://x/P", "template": "{id}: {name}",
                      "fields": {"id": "http://x/id"}}
              },
              "targets": {"a": {"type": "http://x/A"}}
            }"#,
        );
        assert_eq!(
            config.resolve().unwrap_err(),
            ConfigError::UnknownField {
                at: "profiles.p".to_owned(),
                field: "name".to_owned()
            }
        );
    }

    #[test]
    fn a_relative_iri_is_refused_with_its_key_path() {
        let config = parse(r#"{"targets": {"a": {"type": "/relative/A"}}}"#);
        match config.resolve().unwrap_err() {
            ConfigError::NotAnIri { at, iri, .. } => {
                assert_eq!(at, "targets.a.type");
                assert_eq!(iri, "/relative/A");
            }
            other => panic!("expected NotAnIri, got {other:?}"),
        }
    }

    #[test]
    fn two_profiles_for_one_class_are_refused() {
        let config = parse(
            r#"{
              "profiles": {
                "one": {"type": "http://x/P", "template": "a"},
                "two": {"type": "http://x/P", "template": "b"}
              },
              "targets": {"a": {"type": "http://x/A"}}
            }"#,
        );
        assert!(matches!(
            config.resolve().unwrap_err(),
            ConfigError::DuplicateProfileClass { .. }
        ));
    }

    #[test]
    fn unknown_keys_and_missing_targets_are_refused() {
        assert!(
            serde_json::from_str::<Config>(r#"{"expansion_limit": 1, "targets": {}}"#).is_err()
        );
        assert_eq!(
            parse(r#"{}"#).resolve().unwrap_err(),
            ConfigError::NoTargets
        );
    }
}
