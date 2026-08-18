//! RDF document serialization shared by every HTTP resource.
//!
//! KGF constructs the graph it means to publish as [`oxrdf`] statements. Wire
//! syntax belongs to [`oxrdfio`]: keeping it here prevents each resource from
//! growing its own escaping rules or partial JSON-LD implementation.

use std::io;

use oxrdf::Triple;
use oxrdfio::{JsonLdProfile, RdfFormat, RdfSerializer};

/// The graph syntaxes currently offered by triple-valued resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphFormat {
    Turtle,
    JsonLd,
}

impl GraphFormat {
    fn rdf_format(self) -> RdfFormat {
        match self {
            Self::Turtle => RdfFormat::Turtle,
            Self::JsonLd => RdfFormat::JsonLd {
                profile: JsonLdProfile::Streaming | JsonLdProfile::Expanded,
            },
        }
    }
}

/// Serialize a complete RDF graph document.
///
/// `finish` is part of the operation: JSON-LD and the grouped Turtle writer
/// may retain closing syntax or the final statement. Callers fit byte budgets
/// by trying complete prefixes, never by interrupting this writer.
pub(crate) fn serialize_graph(format: GraphFormat, triples: &[Triple]) -> io::Result<Vec<u8>> {
    let mut serializer = RdfSerializer::from_format(format.rdf_format()).for_writer(Vec::new());
    for triple in triples {
        serializer.serialize_triple(triple)?;
    }
    serializer.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use oxrdf::{BlankNode, Literal, NamedNode, Quad};
    use oxrdfio::RdfParser;

    use super::*;

    fn graph() -> Vec<Triple> {
        vec![
            Triple::new(
                NamedNode::new("https://example.org/s").unwrap(),
                NamedNode::new("https://example.org/p").unwrap(),
                Literal::new_language_tagged_literal("a \"quote\"\nand newline", "tr").unwrap(),
            ),
            Triple::new(
                BlankNode::new("subject").unwrap(),
                NamedNode::new("https://example.org/link").unwrap(),
                BlankNode::new("object").unwrap(),
            ),
        ]
    }

    #[test]
    fn every_graph_format_round_trips_through_the_independent_parser() {
        let expected: HashSet<Quad> = graph()
            .into_iter()
            .map(|triple| triple.in_graph(oxrdf::GraphName::DefaultGraph))
            .collect();

        for format in [GraphFormat::Turtle, GraphFormat::JsonLd] {
            let bytes = serialize_graph(format, &graph()).unwrap();
            let parsed: HashSet<Quad> = RdfParser::from_format(format.rdf_format())
                .for_reader(bytes.as_slice())
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                parsed,
                expected,
                "{format:?}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }
}
