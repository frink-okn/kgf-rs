//! The string rules: how an IRI becomes a readable word, and how text is keyed.
//!
//! Small on purpose. Everything a verbalized record's *text* depends on beyond
//! the graph itself is here, so that "why does this line read the way it does"
//! has one place to look.
//!
//! Two hashes live here because two things are keyed on text:
//!
//! - [`stable_score`] picks which values of a high-fanout predicate survive
//!   `predicate_limit`. It hashes the *strings* of the triple, never the ids,
//!   so an unchanged node keeps the same sample across builds even though every
//!   build assigns ids afresh. A uniform sample is the point: string order
//!   would cluster the sample on whatever sorts first.
//! - [`text_digest`] groups roots whose verbalized text is identical into one
//!   record, so measurement-style nodes that differ only in IRI are embedded
//!   once.

use sha2::{Digest, Sha256};

/// `rdfs:label`, the one predicate every cascade is assumed to want.
pub const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";

/// `rdf:type`, the predicate that selects roots.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Make an identifier read as words.
///
/// Underscores and hyphens become spaces, a lower-to-upper case change splits
/// a camelCase run, and whitespace collapses. `hasExactSynonym` reads as
/// `has Exact Synonym`; the caller lowercases where a predicate name wants it.
pub fn humanize(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut previous_lower = false;
    for c in text.chars() {
        let c = match c {
            '_' | '-' => ' ',
            c => c,
        };
        if previous_lower && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
        previous_lower = c.is_lowercase();
    }
    normalize_label(&out)
}

/// The part of an IRI after its last `#`, else after its last `/`, else the
/// IRI itself.
pub fn iri_fragment(iri: &str) -> &str {
    if let Some((_, fragment)) = iri.rsplit_once('#') {
        return fragment;
    }
    if let Some((_, fragment)) = iri.rsplit_once('/') {
        return fragment;
    }
    iri
}

/// The label an IRI gets when the graph offers none: its fragment, humanized.
pub fn fallback_label(iri: &str) -> String {
    humanize(iri_fragment(iri))
}

/// Collapse runs of whitespace to one space and trim the ends.
pub fn normalize_label(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// The sort key that decides which values of one predicate survive a limit.
///
/// Keyed on the three terms' strings — the root and predicate IRIs and the
/// object's lexical form or IRI — so the same triple scores the same in every
/// build. Lower sorts first; the smallest `predicate_limit` scores are kept.
pub fn stable_score(root: &str, predicate: &str, object: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(root.as_bytes());
    hasher.update(b"\t");
    hasher.update(predicate.as_bytes());
    hasher.update(b"\t");
    hasher.update(object.as_bytes());
    hasher.finalize().into()
}

/// The grouping key of a verbalized text.
pub fn text_digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// A digest as lowercase hex, the form records carry.
pub fn hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in digest {
        write!(&mut out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_splits_identifiers_into_words() {
        assert_eq!(humanize("hasExactSynonym"), "has Exact Synonym");
        assert_eq!(humanize("has_exact-synonym"), "has exact synonym");
        assert_eq!(humanize("  already   spaced "), "already spaced");
        // An upper-to-upper run is an acronym, not a word boundary.
        assert_eq!(humanize("NCBITaxon"), "NCBITaxon");
        // A digit is neither case, so it neither splits nor is split.
        assert_eq!(humanize("IAO_0000115"), "IAO 0000115");
        assert_eq!(humanize(""), "");
    }

    #[test]
    fn iri_fragment_prefers_the_hash_over_the_slash() {
        assert_eq!(iri_fragment("http://example.org/a/b#Thing"), "Thing");
        assert_eq!(iri_fragment("http://example.org/a/b"), "b");
        assert_eq!(iri_fragment("http://example.org/a#x/y"), "x/y");
        assert_eq!(iri_fragment("urn:isbn:123"), "urn:isbn:123");
        assert_eq!(iri_fragment("http://example.org/trailing/"), "");
    }

    #[test]
    fn fallback_label_humanizes_the_fragment() {
        assert_eq!(
            fallback_label("https://idir.uta.edu/sockg-ontology#hasMeasurement"),
            "has Measurement"
        );
        assert_eq!(
            fallback_label("http://purl.obolibrary.org/obo/GO_0006915"),
            "GO 0006915"
        );
    }

    /// Pinned to a known answer — the SHA-256 of `a\tb\tc` — because the
    /// sample a limit keeps must not change between builds: a different
    /// digest is a different sample and a different text for every unchanged
    /// node.
    #[test]
    fn stable_score_is_sha256_of_the_tab_joined_strings() {
        assert_eq!(
            hex(&stable_score("a", "b", "c")),
            "8b4e84e4e5d1c12e856a9229ecb3a0b1877bb4e6ab726378b98a2fe3d2357ad3"
        );
        assert_ne!(stable_score("a", "b", "c"), stable_score("a", "b", "d"));
    }

    #[test]
    fn text_digest_is_sha256_of_the_bytes() {
        // sha256("") is the well-known empty digest.
        assert_eq!(
            hex(&text_digest("")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // The SHA-256 of the two-line text, computed independently.
        assert_eq!(
            hex(&text_digest("label: Alice\nknows: Bob")),
            "50a9aa87353773c63951909f2a2ed53d9ba3db0ae2465d675869424fb569b90a"
        );
    }
}
