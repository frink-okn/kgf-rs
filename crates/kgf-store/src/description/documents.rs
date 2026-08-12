//! Typed access to the static description documents.
//!
//! Namespace inventory fields have a published shape and are parsed into
//! borrowed domain types on demand. Summary JSON deliberately has no field
//! model yet in doc 04, so publication verifies only that it is a JSON object;
//! serving preserves its exact bytes rather than inventing a schema here.

use std::borrow::Cow;
use std::path::Path;

use serde::Deserialize;

use crate::error::Result;

use super::malformed;

/// One parsed `stats/namespaces.json` inventory.
#[derive(Debug, Deserialize)]
pub struct NamespaceInventory<'a> {
    #[serde(borrow)]
    prefix_table: PrefixTableIdentity<'a>,
    roles: NamespaceRoles,
    #[serde(borrow)]
    namespaces: Vec<NamespaceEntry<'a>>,
}

impl<'a> NamespaceInventory<'a> {
    /// Identity of the merged prefix table that was counted.
    pub fn prefix_table(&self) -> &PrefixTableIdentity<'a> {
        &self.prefix_table
    }

    /// Whole-dictionary IRI coverage by RDF role.
    pub fn roles(&self) -> NamespaceRoles {
        self.roles
    }

    /// Nonempty namespace counts in deterministic prefix order.
    pub fn namespaces(&self) -> &[NamespaceEntry<'a>] {
        &self.namespaces
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if !is_sha256(self.prefix_table.version()) {
            return Err(malformed(
                path,
                format!(
                    "prefix_table.version {:?} is not a lowercase SHA-256 identity",
                    self.prefix_table.version()
                ),
            ));
        }
        for (role, counts) in [
            ("subject", self.roles.subject),
            ("predicate", self.roles.predicate),
            ("object", self.roles.object),
        ] {
            counts.validate(path, role)?;
        }

        let mut previous: Option<&str> = None;
        for entry in &self.namespaces {
            if entry.prefix().is_empty() {
                return Err(malformed(
                    path,
                    "namespace prefix must not be empty".to_owned(),
                ));
            }
            if let Some(previous) = previous
                && previous.as_bytes() >= entry.prefix().as_bytes()
            {
                return Err(malformed(
                    path,
                    format!(
                        "namespace prefix {:?} is not strictly ordered after {previous:?}",
                        entry.prefix()
                    ),
                ));
            }
            previous = Some(entry.prefix());

            oxiri::Iri::parse(entry.namespace()).map_err(|error| {
                malformed(
                    path,
                    format!(
                        "namespace {:?} for prefix {:?} is not an absolute IRI: {error}",
                        entry.namespace(),
                        entry.prefix()
                    ),
                )
            })?;
            let role_sum = entry
                .subject
                .checked_add(entry.predicate)
                .and_then(|sum| sum.checked_add(entry.object))
                .ok_or_else(|| {
                    malformed(
                        path,
                        format!("role counts for prefix {:?} overflow u64", entry.prefix()),
                    )
                })?;
            let role_max = entry.subject.max(entry.predicate).max(entry.object);
            if role_sum == 0 {
                return Err(malformed(
                    path,
                    format!("prefix {:?} has no role count", entry.prefix()),
                ));
            }
            if !(role_max..=role_sum).contains(&entry.distinct_iris) {
                return Err(malformed(
                    path,
                    format!(
                        "prefix {:?} distinct_iris {} is outside the role-union bounds {role_max}..={role_sum}",
                        entry.prefix(),
                        entry.distinct_iris
                    ),
                ));
            }
            if let Some(example) = entry.example() {
                if !example.starts_with(entry.namespace()) {
                    return Err(malformed(
                        path,
                        format!(
                            "example {example:?} does not start with namespace {:?}",
                            entry.namespace()
                        ),
                    ));
                }
                oxiri::Iri::parse(example).map_err(|error| {
                    malformed(
                        path,
                        format!("example {example:?} is not an absolute IRI: {error}"),
                    )
                })?;
            }
        }
        Ok(())
    }
}

/// Stable content identity of the counted prefix map.
#[derive(Debug, Deserialize)]
pub struct PrefixTableIdentity<'a> {
    #[serde(borrow)]
    version: Cow<'a, str>,
}

impl PrefixTableIdentity<'_> {
    /// Lowercase `sha256:` identity of the fully merged prefix map.
    pub fn version(&self) -> &str {
        &self.version
    }
}

/// Whole-dictionary namespace coverage for each RDF role.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct NamespaceRoles {
    subject: NamespaceRoleCounts,
    predicate: NamespaceRoleCounts,
    object: NamespaceRoleCounts,
}

impl NamespaceRoles {
    /// Subject-position IRI coverage.
    pub fn subject(self) -> NamespaceRoleCounts {
        self.subject
    }

    /// Predicate-position IRI coverage.
    pub fn predicate(self) -> NamespaceRoleCounts {
        self.predicate
    }

    /// Object-position IRI coverage, excluding literals.
    pub fn object(self) -> NamespaceRoleCounts {
        self.object
    }
}

/// Coverage of one role by the counted prefix table.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct NamespaceRoleCounts {
    distinct_iris: u64,
    matched: u64,
    residual: u64,
}

impl NamespaceRoleCounts {
    /// All distinct IRIs in this dictionary role.
    pub fn distinct_iris(self) -> u64 {
        self.distinct_iris
    }

    /// Distinct IRIs matched by at least one configured namespace.
    pub fn matched(self) -> u64 {
        self.matched
    }

    /// Distinct IRIs not matched by the prefix table.
    pub fn residual(self) -> u64 {
        self.residual
    }

    fn validate(self, path: &Path, role: &str) -> Result<()> {
        let total = self.matched.checked_add(self.residual).ok_or_else(|| {
            malformed(
                path,
                format!("{role} matched and residual counts overflow u64"),
            )
        })?;
        if total != self.distinct_iris {
            return Err(malformed(
                path,
                format!(
                    "{role} matched {} plus residual {} does not equal distinct_iris {}",
                    self.matched, self.residual, self.distinct_iris
                ),
            ));
        }
        Ok(())
    }
}

/// Counts for one configured namespace.
#[derive(Debug, Deserialize)]
pub struct NamespaceEntry<'a> {
    #[serde(borrow)]
    prefix: Cow<'a, str>,
    #[serde(borrow)]
    namespace: Cow<'a, str>,
    distinct_iris: u64,
    subject: u64,
    predicate: u64,
    object: u64,
    #[serde(borrow)]
    example: Option<Cow<'a, str>>,
}

impl NamespaceEntry<'_> {
    /// Prefix name used by the shared table.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Namespace IRI matched lexically against dictionary terms.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Distinct matching IRIs across the union of all three roles.
    pub fn distinct_iris(&self) -> u64 {
        self.distinct_iris
    }

    /// Distinct matching subject IRIs.
    pub fn subject(&self) -> u64 {
        self.subject
    }

    /// Distinct matching predicate IRIs.
    pub fn predicate(&self) -> u64 {
        self.predicate
    }

    /// Distinct matching object IRIs, excluding literals.
    pub fn object(&self) -> u64 {
        self.object
    }

    /// Lexically first matching IRI, when examples were requested.
    pub fn example(&self) -> Option<&str> {
        self.example.as_deref()
    }
}

pub(super) fn parse_namespace_inventory<'a>(
    bytes: &'a [u8],
    path: &Path,
) -> Result<NamespaceInventory<'a>> {
    let inventory = serde_json::from_slice(bytes).map_err(|error| {
        malformed(
            path,
            format!("namespace inventory is not valid JSON: {error}"),
        )
    })?;
    NamespaceInventory::validate(&inventory, path)?;
    Ok(inventory)
}

pub(super) fn verify_summary_json(bytes: &[u8], path: &Path) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| malformed(path, format!("summary is not valid JSON: {error}")))?;
    if !value.is_object() {
        return Err(malformed(path, "summary JSON must be an object".to_owned()));
    }
    Ok(())
}

pub(super) fn summary_markdown<'a>(bytes: &'a [u8], path: &Path) -> Result<&'a str> {
    std::str::from_utf8(bytes)
        .map_err(|error| malformed(path, format!("summary Markdown is not UTF-8: {error}")))
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "stats/namespaces.json";
    const ONE_NAMESPACE: &str = concat!(
        "{",
        "\"prefix_table\":{",
        "\"version\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},",
        "\"roles\":{",
        "\"subject\":{\"distinct_iris\":2,\"matched\":2,\"residual\":0},",
        "\"predicate\":{\"distinct_iris\":1,\"matched\":1,\"residual\":0},",
        "\"object\":{\"distinct_iris\":2,\"matched\":1,\"residual\":1}},",
        "\"namespaces\":[{\"prefix\":\"ex\",",
        "\"namespace\":\"https://example.org/\",\"distinct_iris\":3,",
        "\"subject\":2,\"predicate\":1,\"object\":1,",
        "\"example\":\"https://example.org/A\"}]",
        "}",
    );

    #[test]
    fn namespace_shape_parses_into_borrowed_domain_values() {
        let inventory =
            parse_namespace_inventory(ONE_NAMESPACE.as_bytes(), Path::new(PATH)).unwrap();
        assert_eq!(
            inventory.prefix_table().version(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(inventory.roles().subject().matched(), 2);
        assert_eq!(inventory.roles().object().residual(), 1);
        let entry = &inventory.namespaces()[0];
        assert_eq!(entry.namespace(), "https://example.org/");
        assert_eq!(entry.distinct_iris(), 3);
        assert_eq!(entry.example(), Some("https://example.org/A"));
    }

    #[test]
    fn namespace_count_identities_are_proved_at_publication() {
        for (document, expected) in [
            (
                ONE_NAMESPACE.replace(
                    "\"matched\":1,\"residual\":1",
                    "\"matched\":1,\"residual\":2",
                ),
                "does not equal distinct_iris",
            ),
            (
                ONE_NAMESPACE.replace(
                    "\"distinct_iris\":3,\"subject\":2",
                    "\"distinct_iris\":5,\"subject\":2",
                ),
                "outside the role-union bounds",
            ),
            (
                ONE_NAMESPACE.replace("sha256:aaaaaaaa", "release-aaaaaaaa"),
                "is not a lowercase SHA-256 identity",
            ),
            (
                ONE_NAMESPACE.replace("https://example.org/A", "https://other.example/A"),
                "does not start with namespace",
            ),
        ] {
            let error = parse_namespace_inventory(document.as_bytes(), Path::new(PATH))
                .expect_err("invalid namespace document");
            assert!(
                error.to_string().contains(expected),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn summary_documents_keep_a_deliberately_small_contract() {
        verify_summary_json(b"{\"future_field\":true}\n", Path::new("summary.json")).unwrap();
        let error = verify_summary_json(b"[]\n", Path::new("summary.json"))
            .expect_err("summary must be a JSON object");
        assert!(error.to_string().contains("must be an object"));

        assert_eq!(
            summary_markdown(b"# Summary\n", Path::new("summary.md")).unwrap(),
            "# Summary\n"
        );
        let error = summary_markdown(&[0xff], Path::new("summary.md"))
            .expect_err("summary Markdown must be UTF-8");
        assert!(error.to_string().contains("not UTF-8"));
    }
}
