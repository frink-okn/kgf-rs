//! The bundle's prefix map: shared tables layered under the dataset's own
//! bindings, built once and handed to everything that reads it.
//!
//! One map has three readers. It is written to the manifest, where it resolves
//! CURIEs in request parameters and picks the compact spelling of every IRI a
//! page shows. It is the table the namespace inventory counts against, whose
//! digest the bundle publishes as the map's identity. Building it in one place
//! is what keeps those readers in agreement. Before this module the tables
//! reached only the inventory, so an IRI it had counted under `obo:` was still
//! rendered in full and could not be written as `obo:…` in a request.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// Layer `tables` in order, later files winning, then `declared` over all.
///
/// `declared` is the resolved plan's own map: the well-known four plus the
/// config's `semantics.prefixes`. It wins so that a dataset's binding beats the
/// federation's, and so that no shared table can quietly rebind `rdf:`.
pub(crate) fn layered(
    tables: &[PathBuf],
    declared: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut merged = BTreeMap::new();
    for table in tables {
        merged.extend(load(table)?);
    }
    merged.extend(
        declared
            .iter()
            .map(|(prefix, namespace)| (prefix.clone(), namespace.clone())),
    );
    Ok(merged)
}

/// Hold one binding to the manifest's contract.
///
/// Both halves are published as what a client may send, so both are checked
/// wherever a binding enters: a prefix name must be spellable in a CURIE and a
/// namespace must be an IRI. The name rule is stricter than the table files'
/// own format demands, and deliberately so: an entry the inventory could count
/// but no request could ever use would be a declaration with no meaning.
pub(crate) fn validate_binding(prefix: &str, namespace: &str) -> Result<()> {
    ensure!(
        !prefix.is_empty()
            && prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "prefix name {prefix:?} is not a usable CURIE prefix"
    );
    oxiri::Iri::parse(namespace).map_err(|error| {
        anyhow::anyhow!("prefix {prefix:?} expands to {namespace:?}, which is not an IRI: {error}")
    })?;
    Ok(())
}

/// One table file: a flat map of prefix to namespace, JSON or YAML by
/// extension.
///
/// The same shapes, told apart the same way, as the tables `hdtc namespaces`
/// reads. The registry's shared table is the base of every bundle, and a file
/// that one tool accepted and the other refused would leave the manifest and
/// the inventory disagreeing about which prefixes exist.
fn load(path: &Path) -> Result<BTreeMap<String, String>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading prefix table {}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    let table: BTreeMap<String, String> = match extension.as_deref() {
        Some("json") => serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "prefix table {} is not a JSON object of prefix to namespace",
                path.display()
            )
        })?,
        Some("yaml" | "yml") => serde_norway::from_slice(&bytes).with_context(|| {
            format!(
                "prefix table {} is not a YAML map of prefix to namespace",
                path.display()
            )
        })?,
        _ => bail!(
            "prefix table {} must be named .json, .yaml or .yml; the extension is how its \
             format is chosen",
            path.display()
        ),
    };
    for (prefix, namespace) in &table {
        validate_binding(prefix, namespace)
            .with_context(|| format!("in prefix table {}", path.display()))?;
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(prefix, namespace)| ((*prefix).to_owned(), (*namespace).to_owned()))
            .collect()
    }

    #[test]
    fn later_tables_win_and_the_declared_map_wins_over_every_table() {
        let dir = tempfile::tempdir().unwrap();
        let first = table(
            dir.path(),
            "shared.yaml",
            "# comments and quoting as the registry writes them\n\
             obo: \"http://purl.obolibrary.org/obo/\"\n\
             chebi: \"http://old.example/CHEBI_\"\n\
             ex: \"http://table.example/\"\n",
        );
        let second = table(
            dir.path(),
            "local.json",
            r#"{"chebi": "http://purl.obolibrary.org/obo/CHEBI_"}"#,
        );
        let merged = layered(
            &[first, second],
            &declared(&[("ex", "http://example.org/"), ("rdf", "http://r/")]),
        )
        .unwrap();
        assert_eq!(merged["obo"], "http://purl.obolibrary.org/obo/");
        assert_eq!(merged["chebi"], "http://purl.obolibrary.org/obo/CHEBI_");
        assert_eq!(merged["ex"], "http://example.org/");
        assert_eq!(merged["rdf"], "http://r/");
        assert_eq!(merged.len(), 4);
    }

    #[test]
    fn no_tables_is_just_the_declared_map() {
        let map = declared(&[("ex", "http://example.org/")]);
        assert_eq!(layered(&[], &map).unwrap(), map);
    }

    #[test]
    fn a_table_entry_is_held_to_the_manifest_contract_and_names_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let bad_name = table(
            dir.path(),
            "bad.yaml",
            "\"has space\": \"http://x.example/\"\n",
        );
        let error = format!("{:#}", layered(&[bad_name], &BTreeMap::new()).unwrap_err());
        assert!(error.contains("bad.yaml"), "{error}");
        assert!(error.contains("has space"), "{error}");

        let bad_iri = table(dir.path(), "iri.json", r#"{"ok": "not an iri"}"#);
        let error = format!("{:#}", layered(&[bad_iri], &BTreeMap::new()).unwrap_err());
        assert!(error.contains("iri.json"), "{error}");
        assert!(error.contains("not an IRI"), "{error}");
    }

    #[test]
    fn the_format_comes_from_the_extension_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = table(dir.path(), "prefixes.txt", "ex: http://example.org/\n");
        let error = format!("{:#}", layered(&[unknown], &BTreeMap::new()).unwrap_err());
        assert!(error.contains("prefixes.txt"), "{error}");
        assert!(error.contains(".yaml"), "{error}");

        let missing = dir.path().join("nope.yaml");
        let error = format!("{:#}", layered(&[missing], &BTreeMap::new()).unwrap_err());
        assert!(error.contains("nope.yaml"), "{error}");
    }
}
