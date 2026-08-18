//! Stable RDF identities for blank nodes in immutable HDT content.
//!
//! RDF blank-node labels are local to one document. A fragment protocol emits
//! many documents for one graph, so publishing HDT blank nodes directly would
//! give the same stored node a different RDF identity on every page. KGF uses
//! a content-addressed FDC URN instead and reverses it when that IRI returns in
//! a later fragment request.

use std::borrow::Cow;
use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const FDC_PREFIX: &str = "urn:fdc:frink-okn.github.io:20260818:kgf:bnode:v1:sha256:";

/// The immutable HDT scope in which a stored blank-node label has identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkolemScope {
    iri_prefix: String,
}

impl SkolemScope {
    /// Bind blank-node identities to the HDT dictionary-and-triples digest.
    pub(crate) fn new(hdt_identity_digest: [u8; 32]) -> Self {
        let mut iri_prefix = String::with_capacity(FDC_PREFIX.len() + 65);
        iri_prefix.push_str(FDC_PREFIX);
        for byte in hdt_identity_digest {
            write!(&mut iri_prefix, "{byte:02x}").expect("writing to a String cannot fail");
        }
        iri_prefix.push(':');
        Self { iri_prefix }
    }

    /// Replace a dictionary blank node with its stable RDF IRI.
    pub(crate) fn iri(&self, dictionary_term: &str) -> Option<String> {
        dictionary_term.strip_prefix("_:")?;
        let encoded = URL_SAFE_NO_PAD.encode(dictionary_term.as_bytes());
        Some(format!("{}{encoded}", self.iri_prefix))
    }

    /// Recover this HDT's dictionary spelling when one of its URNs returns.
    ///
    /// A URN from another immutable HDT remains an ordinary named node and
    /// therefore does not accidentally match a local blank node.
    pub(crate) fn dictionary_term<'a>(&self, term: &'a str) -> Cow<'a, str> {
        let Some(encoded) = term.strip_prefix(&self.iri_prefix) else {
            return Cow::Borrowed(term);
        };
        let Some(decoded) = URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .filter(|decoded| decoded.starts_with("_:") && decoded.len() > 2)
        else {
            return Cow::Borrowed(term);
        };
        Cow::Owned(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_node_round_trips_through_its_hdt_scoped_urn() {
        let scope = SkolemScope::new([0xab; 32]);
        let iri = scope.iri("_:b1").unwrap();
        assert_eq!(
            iri,
            concat!(
                "urn:fdc:frink-okn.github.io:20260818:kgf:bnode:v1:sha256:",
                "abababababababababababababababababababababababababababababababab:",
                "XzpiMQ"
            )
        );
        assert_eq!(scope.dictionary_term(&iri), "_:b1");
    }

    #[test]
    fn only_blank_nodes_from_the_same_hdt_are_reversed() {
        let first = SkolemScope::new([1; 32]);
        let second = SkolemScope::new([2; 32]);
        let iri = first.iri("_:same label").unwrap();

        assert_eq!(second.dictionary_term(&iri), iri);
        assert_eq!(
            first.dictionary_term("http://example.org/named"),
            "http://example.org/named"
        );
        assert!(first.iri("http://example.org/named").is_none());
        assert_eq!(
            first.dictionary_term(&format!("{}not-base64!", first.iri_prefix)),
            format!("{}not-base64!", first.iri_prefix)
        );
    }
}
