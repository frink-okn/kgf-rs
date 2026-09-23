//! Stable RDF identities for blank nodes in immutable HDT content.
//!
//! RDF blank-node labels are local to one document. A fragment protocol emits
//! many documents for one graph, so publishing HDT blank nodes directly would
//! give the same stored node a different RDF identity on every page. KGF uses
//! a content-addressed FDC URN instead and reverses it when that IRI returns in
//! a later fragment request.
//!
//! The digest in a URN names the content its local id is scoped by. A blank
//! node of the data is scoped by the HDT's dictionary and triples, and keeps
//! its IRI across every bundle that publishes those bytes. A graph named by a
//! blank node that occurs nowhere in the data has no dictionary id at all: its
//! id is a layer of the membership sidecar, and two sidecars over one HDT can
//! give the same layer id to different graphs, so it is scoped by the
//! sidecar's digest instead.

use std::fmt::Write as _;

use kgf_store::Store;
use kgf_store::dict::{DictCounts, Section, SectionTermId};
use kgf_store::graphs::GraphId;
use kgf_store::{Role, TermId};

const FDC_PREFIX: &str = "urn:fdc:frink-okn.github.io:20260818:kgf:bnode:v1:sha256:";

/// The immutable content in which stored blank-node ids have identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkolemScope {
    iri_prefix: String,
    /// Where graph-only blank names are minted; `None` for a bundle without
    /// memberships, which has no such graph to name.
    graph_prefix: Option<String>,
    counts: DictCounts,
}

impl SkolemScope {
    /// Bind blank-node identities to the HDT dictionary-and-triples digest.
    pub(crate) fn new(hdt_identity_digest: [u8; 32], counts: DictCounts) -> Self {
        Self {
            iri_prefix: scoped_prefix(hdt_identity_digest),
            graph_prefix: None,
            counts,
        }
    }

    /// The scope of every blank node one open bundle publishes.
    pub(crate) fn of(store: &Store) -> Self {
        let scope = Self::new(store.hdt_identity_digest(), *store.dict().counts());
        match store.graphs() {
            Some(graphs) => scope.with_graph_sidecar(graphs.sidecar_identity_digest()),
            None => scope,
        }
    }

    /// Bind graph-only blank names to the membership sidecar's digest.
    ///
    /// That digest covers the sidecar's binding to the HDT as well as its
    /// layers and graph dictionary, so a layer id reverses only against the
    /// memberships it was minted from.
    pub(crate) fn with_graph_sidecar(self, sidecar_identity_digest: [u8; 32]) -> Self {
        Self {
            graph_prefix: Some(scoped_prefix(sidecar_identity_digest)),
            ..self
        }
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

    /// The IRI of a graph named by a blank node that fills no triple position.
    ///
    /// Keyed by layer id, because such a node has no dictionary id to key it
    /// by. A blank graph name that *does* occur in the data is that node, and
    /// is published under the IRI [`Self::iri`] gives it there; telling the
    /// two apart takes the dictionary, so it is the caller's to decide.
    /// `None` in a scope without memberships.
    pub(crate) fn graph_iri(&self, graph: GraphId) -> Option<String> {
        let prefix = self.graph_prefix.as_deref()?;
        Some(format!("{prefix}g-{}", graph.0))
    }

    /// Recover the layer named by one of this sidecar's graph-only URNs.
    ///
    /// The caller still verifies that the id names a layer of this bundle,
    /// that the layer's stored name is a blank node, and that the node occurs
    /// nowhere in the data.
    pub(crate) fn graph_id(&self, term: &str) -> Option<GraphId> {
        let local = term.strip_prefix(self.graph_prefix.as_deref()?)?;
        canonical_id(local.strip_prefix("g-")?).map(GraphId)
    }

    /// Recover the role-scoped id named by one of this HDT's blank-node URNs.
    ///
    /// The caller still verifies that the id names a dictionary blank node.
    /// Foreign, malformed, out-of-range, and wrong-role URNs remain ordinary
    /// named nodes.
    pub(crate) fn role_id(&self, role: Role, term: &str) -> Option<TermId> {
        let (section, local_id) = self.suffix(term)?;
        self.counts
            .role_id(role, SectionTermId::new(section, local_id)?)
    }

    /// The `{section}-{local-id}` tail of one of this scope's blank-node URNs.
    ///
    /// A human-facing spelling for a term the wire names by its full IRI: a
    /// page shows `_:sh-7` because `_:` is what an RDF reader recognizes, while
    /// the IRI it abbreviates stays in the link and the tooltip. Display only —
    /// nothing expands it, and it is not a spelling any parameter accepts.
    pub(crate) fn display_label<'a>(&self, term: &'a str) -> Option<&'a str> {
        // Parsed rather than merely stripped, so a malformed or out-of-range
        // tail is shown as the ordinary IRI it is treated as everywhere else.
        if self.suffix(term).is_some() {
            return term.strip_prefix(&self.iri_prefix);
        }
        self.graph_id(term)?;
        term.strip_prefix(self.graph_prefix.as_deref()?)
    }

    /// Split a URN of this scope into its section and canonical local id.
    fn suffix(&self, term: &str) -> Option<(Section, u64)> {
        let local = term.strip_prefix(&self.iri_prefix)?;
        let (section, id) = local.split_once('-')?;
        let section = match section {
            "sh" => Section::Shared,
            "s" => Section::Subjects,
            "o" => Section::Objects,
            _ => return None,
        };
        Some((section, canonical_id(id)?))
    }
}

/// `{FDC_PREFIX}{hex digest}:`, the namespace one content digest scopes.
fn scoped_prefix(digest: [u8; 32]) -> String {
    let mut prefix = String::with_capacity(FDC_PREFIX.len() + 65);
    prefix.push_str(FDC_PREFIX);
    for byte in digest {
        write!(&mut prefix, "{byte:02x}").expect("writing to a String cannot fail");
    }
    prefix.push(':');
    prefix
}

/// A local id in its one canonical spelling: decimal digits, no leading zero.
fn canonical_id(text: &str) -> Option<u64> {
    if text.starts_with('0') || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
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
    fn a_graph_only_blank_name_is_scoped_by_the_sidecar_and_reversed_only_from_it() {
        let hdt = [1; 32];
        let first = SkolemScope::new(hdt, counts()).with_graph_sidecar([7; 32]);
        // The same triples under different memberships: a layer id says
        // nothing about which graph it holds once the sidecar changes.
        let regrouped = SkolemScope::new(hdt, counts()).with_graph_sidecar([8; 32]);
        let iri = first.graph_iri(GraphId(3)).unwrap();
        assert!(iri.ends_with(&format!("{}:g-3", "07".repeat(32))), "{iri}");
        assert!(!iri.starts_with(first.iri_prefix()), "{iri}");
        assert_eq!(first.graph_id(&iri), Some(GraphId(3)));
        assert_eq!(regrouped.graph_id(&iri), None);
        assert_eq!(first.display_label(&iri), Some("g-3"));

        // Not a data blank node's spelling, and no data blank node's is one.
        assert_eq!(first.role_id(Role::Subject, &iri), None);
        let data = first.iri(Role::Subject, TermId(7), "_:shared").unwrap();
        assert_eq!(first.graph_id(&data), None);
        assert_eq!(
            first.graph_id(&format!("{}g-3", first.iri_prefix())),
            None,
            "the HDT's own namespace mints no graph ids"
        );

        let prefix = iri.strip_suffix("g-3").unwrap();
        assert_eq!(first.graph_id(&format!("{prefix}g-03")), None);
        assert_eq!(first.graph_id(&format!("{prefix}s-3")), None);

        // A bundle without memberships has no graph to mint a name for.
        let triples = SkolemScope::new(hdt, counts());
        assert_eq!(triples.graph_iri(GraphId(3)), None);
        assert_eq!(triples.graph_id(&iri), None);
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
