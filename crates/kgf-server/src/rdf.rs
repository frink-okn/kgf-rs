//! RDF document serialization shared by every HTTP resource.
//!
//! KGF constructs the graph it means to publish as [`oxrdf`] statements. Wire
//! syntax belongs to [`oxrdfio`]: keeping it here prevents each resource from
//! growing its own escaping rules or partial JSON-LD implementation.

use std::io;

use oxrdf::{Quad, Triple};
use oxrdfio::{JsonLdProfile, RdfFormat, RdfSerializer};

/// The graph syntaxes currently offered by triple-valued resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphFormat {
    Turtle,
    JsonLd,
}

/// The dataset syntaxes offered by quad-valued resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatasetFormat {
    NQuads,
    TriG,
    JsonLd,
}

impl DatasetFormat {
    fn rdf_format(self) -> RdfFormat {
        match self {
            Self::NQuads => RdfFormat::NQuads,
            Self::TriG => RdfFormat::TriG,
            Self::JsonLd => RdfFormat::JsonLd {
                profile: JsonLdProfile::Streaming | JsonLdProfile::Expanded,
            },
        }
    }
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
pub(crate) fn serialize_graph(
    format: GraphFormat,
    triples: &[Triple],
    prefixes: &[(&str, &str)],
) -> io::Result<Vec<u8>> {
    let mut serializer = RdfSerializer::from_format(format.rdf_format());
    for &(name, iri) in prefixes {
        serializer = serializer
            .with_prefix(name, iri)
            .map_err(io::Error::other)?;
    }
    let mut serializer = serializer.for_writer(Vec::new());
    for triple in triples {
        serializer.serialize_triple(triple)?;
    }
    serializer.finish()
}

/// Serialize a complete RDF dataset document, preserving graph names.
///
/// JSON-LD is a dataset syntax: a named graph is represented with `@graph`, so
/// control quads must pass through this path rather than the graph-only helper.
pub(crate) fn serialize_dataset(
    format: DatasetFormat,
    quads: &[Quad],
    prefixes: &[(&str, &str)],
) -> io::Result<Vec<u8>> {
    let mut serializer = RdfSerializer::from_format(format.rdf_format());
    for &(name, iri) in prefixes {
        serializer = serializer
            .with_prefix(name, iri)
            .map_err(io::Error::other)?;
    }
    let mut serializer = serializer.for_writer(Vec::new());
    for quad in quads {
        serializer.serialize_quad(quad)?;
    }
    serializer.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad};
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
            let bytes = serialize_graph(format, &graph(), &[]).unwrap();
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

    #[test]
    fn turtle_declares_and_uses_supplied_prefixes() {
        let namespace = "urn:fdc:frink-okn.github.io:20260818:kgf:bnode:v1:sha256:abc:";
        let graph = [Triple::new(
            NamedNode::new(format!("{namespace}sh-7")).unwrap(),
            NamedNode::new("https://example.org/p").unwrap(),
            NamedNode::new(format!("{namespace}o-2")).unwrap(),
        )];
        let bytes = serialize_graph(GraphFormat::Turtle, &graph, &[("kgfbn", namespace)]).unwrap();
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains(&format!("@prefix kgfbn: <{namespace}> .")));
        assert!(text.contains("kgfbn:sh-7"));
        assert!(text.contains("kgfbn:o-2"));
    }

    #[test]
    fn every_dataset_format_preserves_a_named_graph() {
        let graph_name = NamedNode::new("https://example.org/metadata").unwrap();
        let quads = vec![
            graph()[0].clone().in_graph(GraphName::DefaultGraph),
            Triple::new(
                graph_name.clone(),
                NamedNode::new("https://example.org/primaryTopic").unwrap(),
                NamedNode::new("https://example.org/page").unwrap(),
            )
            .in_graph(graph_name),
        ];
        let expected: HashSet<_> = quads.iter().cloned().collect();

        for format in [
            DatasetFormat::NQuads,
            DatasetFormat::TriG,
            DatasetFormat::JsonLd,
        ] {
            let bytes = serialize_dataset(format, &quads, &[]).unwrap();
            let parsed: HashSet<Quad> = RdfParser::from_format(format.rdf_format())
                .for_reader(bytes.as_slice())
                .map(Result::unwrap)
                .collect();
            assert_eq!(parsed, expected, "{format:?}");
        }
    }
}
