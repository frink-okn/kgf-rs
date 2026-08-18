//! Stable RDF identities for blank nodes in immutable HDT content.
//!
//! RDF blank-node labels are local to one document. A fragment protocol emits
//! many documents for one graph, so publishing HDT blank nodes directly would
//! give the same stored node a different RDF identity on every page. KGF uses
//! a content-addressed FDC URN instead and reverses it when that IRI returns in
//! a later fragment request.

use std::fmt::Write as _;

use kgf_store::dict::{DictCounts, Section, SectionTermId};
use kgf_store::{Role, TermId};

const FDC_PREFIX: &str = "urn:fdc:frink-okn.github.io:20260818:kgf:bnode:v1:sha256:";

/// The immutable HDT scope in which stored blank-node ids have identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkolemScope {
    iri_prefix: String,
    counts: DictCounts,
}

impl SkolemScope {
    /// Bind blank-node identities to the HDT dictionary-and-triples digest.
    pub(crate) fn new(hdt_identity_digest: [u8; 32], counts: DictCounts) -> Self {
        let mut iri_prefix = String::with_capacity(FDC_PREFIX.len() + 65);
        iri_prefix.push_str(FDC_PREFIX);
        for byte in hdt_identity_digest {
            write!(&mut iri_prefix, "{byte:02x}").expect("writing to a String cannot fail");
        }
        iri_prefix.push(':');
        Self { iri_prefix, counts }
    }

    /// Namespace bound to `kgfbn:` in RDF syntaxes that support prefixes.
    pub(crate) fn iri_prefix(&self) -> &str {
        &self.iri_prefix
    }

    /// Replace a dictionary blank node with its stable RDF IRI.
    pub(crate) fn iri(&self, role: Role, id: TermId, dictionary_term: &str) -> Option<String> {
        dictionary_term.strip_prefix("_:")?;
        let section_id = self.counts.section_id(role, id).ok()?;
        let section = match section_id.section() {
            Section::Shared => "sh",
            Section::Subjects => "s",
            Section::Objects => "o",
            Section::Predicates => return None,
        };
        Some(format!(
            "{}{section}-{}",
            self.iri_prefix,
            section_id.local_id()
        ))
    }

    /// Recover the role-scoped id named by one of this HDT's blank-node URNs.
    ///
    /// The caller still verifies that the id names a dictionary blank node.
    /// Foreign, malformed, out-of-range, and wrong-role URNs remain ordinary
    /// named nodes.
    pub(crate) fn role_id(&self, role: Role, term: &str) -> Option<TermId> {
        let local = term.strip_prefix(&self.iri_prefix)?;
        let (section, id) = local.split_once('-')?;
        if id.starts_with('0') || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let section = match section {
            "sh" => Section::Shared,
            "s" => Section::Subjects,
            "o" => Section::Objects,
            _ => return None,
        };
        self.counts
            .role_id(role, SectionTermId::new(section, id.parse().ok()?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts() -> DictCounts {
        DictCounts {
            shared: 10,
            subjects: 20,
            objects: 30,
            predicates: 5,
        }
    }

    #[test]
    fn blank_nodes_use_section_scoped_dictionary_integers() {
        let scope = SkolemScope::new([0xab; 32], counts());
        let prefix = concat!(
            "urn:fdc:frink-okn.github.io:20260818:kgf:bnode:v1:sha256:",
            "abababababababababababababababababababababababababababababababab:"
        );
        let shared = scope.iri(Role::Subject, TermId(7), "_:shared").unwrap();
        let subject = scope.iri(Role::Subject, TermId(12), "_:subject").unwrap();
        let object = scope.iri(Role::Object, TermId(13), "_:object").unwrap();

        assert_eq!(shared, format!("{prefix}sh-7"));
        assert_eq!(subject, format!("{prefix}s-2"));
        assert_eq!(object, format!("{prefix}o-3"));
        assert_eq!(scope.role_id(Role::Subject, &shared), Some(TermId(7)));
        assert_eq!(scope.role_id(Role::Object, &shared), Some(TermId(7)));
        assert_eq!(scope.role_id(Role::Subject, &subject), Some(TermId(12)));
        assert_eq!(scope.role_id(Role::Object, &object), Some(TermId(13)));
    }

    #[test]
    fn only_canonical_ids_from_the_same_hdt_and_role_are_reversed() {
        let first = SkolemScope::new([1; 32], counts());
        let second = SkolemScope::new([2; 32], counts());
        let subject = first.iri(Role::Subject, TermId(12), "_:subject").unwrap();

        assert_eq!(first.role_id(Role::Subject, &subject), Some(TermId(12)));
        assert_eq!(first.role_id(Role::Object, &subject), None);
        assert_eq!(second.role_id(Role::Subject, &subject), None);
        assert_eq!(
            first.role_id(Role::Subject, "http://example.org/named"),
            None
        );
        assert_eq!(
            first.role_id(Role::Subject, &format!("{}s-02", first.iri_prefix)),
            None
        );
        assert_eq!(
            first.role_id(Role::Subject, &format!("{}s-not-an-id", first.iri_prefix)),
            None
        );
        assert!(
            first
                .iri(Role::Subject, TermId(12), "http://example.org/named")
                .is_none()
        );
    }
}
