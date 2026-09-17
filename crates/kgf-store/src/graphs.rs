//! Named-graph memberships: the mapped `data.hdt.graphs` sidecar and its
//! derived `data.hdt.graphs.idx`.
//!
//! `data.hdt` is the deduplicated union of every graph, and this pair records
//! which graphs each triple belongs to. The sidecar keeps one **layer** per
//! graph — the set of SPO positions the graph contains, with rank and select
//! over it — keyed by graph id, where id 0 is the **unnamed graph** (the
//! statements that carried no graph in the source) and ids `1..=G` are the
//! named graphs in the sorted order of the sidecar's own dictionary. The index
//! re-keys the same layers to POS and OPS positions, so a pattern that is
//! contiguous in one of those permutations can be scoped in its native
//! position space, and optionally carries a transpose — for each SPO position,
//! its graph ids — for the quad view's `g` column.
//!
//! # What is read at open, and what is not
//!
//! Open reads the two headers, checks each file's binding to the HDT and the
//! index's binding to the sidecar, locates the graph dictionary and the layer
//! directories as validated specs, and binds the transpose when present.
//! Nothing proportional to the graph count is read: a bundle may carry
//! thousands of graphs, and its layer entries, chunk directories, and
//! Elias–Fano headers are decoded on first touch. That is why every layer
//! operation returns a [`Result`] — the structure it walks was not validated
//! when the bundle opened, and a malformed entry is reported as such rather
//! than trusted.
//!
//! # One reader for three files' worth of layers
//!
//! A layer set inside the index has exactly the sidecar's layout, so
//! [`LayerSet`] is parameterised by the file and the directory offset and
//! nothing else; which permutation supplies a position is the caller's
//! business ([`Permutation`]). Each layer is one of three encodings, and
//! [`Layer`] presents the same five operations over all of them: `count`,
//! `rank`, `select`, `access`, and `next_member`.
//!
//! # Two reserved names
//!
//! [`UNION_GRAPH_IRI`] names the union and [`UNNAMED_GRAPH_IRI`] the unnamed
//! graph, everywhere in the API. Neither is a dictionary term: a build refuses
//! source quads that use them, and [`Graphs::open`] refuses a sidecar whose
//! dictionary holds either, so [`Graphs::resolve`] can answer the unnamed
//! constant before consulting the dictionary and the union constant never
//! reaches it.
//!
//! # Byte formats
//!
//! hdtc's `docs/graphs-sidecar-format.md` and `docs/graphs-index-format.md`
//! are normative. Record decoding is hdtc's ([`GraphLayerEntry::parse`],
//! [`GraphChunkEntry::parse`], [`EliasFanoHeader::parse`]); this module
//! addresses records and interprets their payloads.

use std::io::{Cursor, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use hdtc::format::{
    ELIAS_FANO_HEADER_SIZE, ELIAS_FANO_SUBBLOCK_BITS, ELIAS_FANO_SUPERBLOCK_BITS, EliasFanoHeader,
    GRAPH_ARRAY_CONTAINER_MAX, GRAPH_BITMAP_CONTAINER_BYTES, GRAPH_BITMAP_CONTAINER_SUBBLOCK_BITS,
    GRAPH_BITMAP_CONTAINER_SUBRANK_BYTES, GRAPH_CHUNK_ENTRY_SIZE, GRAPH_LAYER_ENTRY_SIZE,
    GRAPH_POSITION_CHUNK_SHIFT, GraphChunkContainer, GraphChunkEntry, GraphIndex,
    GraphIndexOpenError, GraphIndexSectionKind, GraphLayerEncoding, GraphLayerEntry,
    GraphSidecarDirectory, GraphSidecarOpenError,
};

use crate::dict::PfcLayout;
use crate::error::{Error, Result};
use crate::map::{BitmapSpec, BitmapView, BytesSpec, Mapping, PackedArray, PackedSpec};
use crate::pattern::Permutation;
use crate::rank::{RankedBitmap, RankedSpec, SUBRANK_WIDTH, SUPERRANK_WIDTH};

/// The reserved name of the union of every graph — what an unscoped request
/// reads. Accepted as a graph selector on every release and never a layer.
pub const UNION_GRAPH_IRI: &str = "urn:x-kgf:union";

/// The reserved name of the unnamed graph: the statements that carried no
/// graph in the source, held as layer 0. Accepted as a graph selector on every
/// release; on a bundle without memberships it selects every triple, since
/// every triple of such a bundle is unnamed.
pub const UNNAMED_GRAPH_IRI: &str = "urn:x-kgf:unnamed";

/// A membership layer: 0 for the unnamed graph, `1..=G` for the named graphs
/// in the sorted order of the sidecar's dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphId(pub u64);

impl GraphId {
    /// The unnamed graph's layer.
    pub const UNNAMED: GraphId = GraphId(0);

    /// Whether this is the unnamed graph.
    pub fn is_unnamed(self) -> bool {
        self.0 == 0
    }
}

/// Per-bundle membership facts, from the two headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphFacts {
    /// `N`, the triples of the union.
    pub triples: u64,
    /// `G`, the named graphs.
    pub named_graphs: u64,
    /// `M`, memberships summed over every layer; at least `N`.
    pub memberships: u64,
}

/// The mapped sidecar and index of one bundle, with specs validated at open.
#[derive(Debug)]
pub struct Graphs {
    sidecar: Mapping,
    index: Mapping,
    facts: GraphFacts,
    dictionary: PfcLayout,
    spo: LayerSetSpec,
    pos: LayerSetSpec,
    ops: LayerSetSpec,
    transpose: Option<TransposeSpec>,
}

/// Where a layer set's directory lives.
#[derive(Debug, Clone, Copy)]
struct LayerSetSpec {
    directory: BytesSpec,
}

/// The SPO transpose: `BitmapG` with its rank directory, and `ArrayG` when the
/// index carries graph ids as well as run boundaries.
#[derive(Debug, Clone, Copy)]
struct TransposeSpec {
    bitmap: RankedSpec,
    ids: Option<PackedSpec>,
}

impl Graphs {
    /// Open only the membership artifacts of a published bundle.
    ///
    /// [`Store::open`](crate::Store::open) is the ordinary door and requires a
    /// manifest. This one is for a caller that has to check a claim about a
    /// bundle's graphs *before* the manifest describing it exists — a build
    /// validating a declaration against the data it just wrote. `None` is a
    /// bundle that carries no memberships.
    pub fn open_bundle(bundle: &crate::map::PublishedBundle) -> Result<Option<Self>> {
        crate::store::ArtifactSet::resolve(bundle.path())?.open_graphs(bundle)
    }

    /// Bind a mapped sidecar and index to the HDT at `hdt`.
    ///
    /// hdtc parses both headers and checks the cheap bindings — each file's
    /// recorded suffix length, triple count, and digest fields against the
    /// HDT's and each other's. Full digests stay off the open path, where
    /// every other sidecar keeps them.
    ///
    /// Refuses an index without both POS and OPS layer sets: the three
    /// index-side patterns would otherwise need a per-candidate probe this
    /// crate does not implement. Refuses a sidecar whose dictionary holds a
    /// reserved graph name, because both names have fixed meanings that a
    /// stored layer could only contradict.
    pub fn open(hdt: &Path, sidecar: Mapping, index: Mapping) -> Result<Self> {
        // hdtc binds the index to the sidecar it finds beside the HDT, so the
        // mapping handed in must be that file.
        assert_eq!(
            sidecar.path(),
            hdtc::format::graph_sidecar_path(hdt),
            "the graph sidecar mapping is not the HDT's canonical sidecar"
        );
        let header = *GraphSidecarDirectory::read(sidecar.path(), hdt)
            .map_err(|error| match error {
                GraphSidecarOpenError::Binding { source } => Error::ArtifactBindingMismatch {
                    artifact: sidecar.path().to_path_buf(),
                    hdt: hdt.to_path_buf(),
                    detail: format!("{source:#}"),
                },
                GraphSidecarOpenError::Sidecar { source } => Error::Format(source.context(
                    format!("opening graph sidecar {}", sidecar.path().display()),
                )),
                GraphSidecarOpenError::Source { source } => Error::Format(
                    source.context(format!("validating source HDT {}", hdt.display())),
                ),
            })?
            .header();

        let directory = GraphIndex::directory(index.path(), hdt).map_err(|error| match error {
            GraphIndexOpenError::Binding { source } => Error::ArtifactBindingMismatch {
                artifact: index.path().to_path_buf(),
                hdt: hdt.to_path_buf(),
                detail: format!("{source:#}"),
            },
            GraphIndexOpenError::Index { source } => Error::Format(
                source.context(format!("opening graph index {}", index.path().display())),
            ),
            GraphIndexOpenError::Source { source } => {
                Error::Format(source.context(format!("validating source HDT {}", hdt.display())))
            }
            GraphIndexOpenError::Sidecar { source } => Error::Format(source.context(format!(
                "opening graph sidecar {}",
                sidecar.path().display()
            ))),
        })?;
        let index_header = directory.header();
        for (present, space) in [
            (index_header.has_pos_layers(), "pos"),
            (index_header.has_ops_layers(), "ops"),
        ] {
            if !present {
                return Err(Error::MissingRequiredArtifact {
                    bundle: index
                        .path()
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default(),
                    artifact: format!(
                        "{} with the {space} layer set",
                        index
                            .path()
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    remedy: format!("hdtc graphs-index {} --positions pos,ops", hdt.display()),
                });
            }
        }

        let facts = GraphFacts {
            triples: header.triples,
            named_graphs: header.named_graphs,
            memberships: header.memberships,
        };

        let dictionary = {
            let mut cursor = Cursor::new(sidecar.as_bytes());
            cursor
                .seek(SeekFrom::Start(header.dictionary_offset))
                .map_err(|error| malformed(&sidecar, format!("{error}")))?;
            let section = hdtc::format::scan_pfc_section(&mut cursor, "graph dictionary")
                .map_err(|error| malformed(&sidecar, format!("{error:#}")))?;
            if section.string_count != facts.named_graphs {
                return Err(malformed(
                    &sidecar,
                    format!(
                        "graph dictionary holds {} terms for {} named graphs",
                        section.string_count, facts.named_graphs
                    ),
                ));
            }
            with_artifact(&sidecar, PfcLayout::locate(&sidecar, &section))?
        };

        let layers = facts
            .named_graphs
            .checked_add(1)
            .and_then(|count| count.checked_mul(GRAPH_LAYER_ENTRY_SIZE as u64))
            .ok_or_else(|| malformed(&sidecar, "layer directory length overflows".to_owned()))?;
        let spo = LayerSetSpec {
            directory: with_artifact(
                &sidecar,
                BytesSpec::new(&sidecar, header.directory_offset, layers),
            )?,
        };
        let layer_set = |kind: GraphIndexSectionKind| -> Result<LayerSetSpec> {
            let section = directory
                .section(kind)
                .ok_or_else(|| malformed(&index, format!("missing {kind:?} section")))?;
            Ok(LayerSetSpec {
                directory: with_artifact(&index, BytesSpec::new(&index, section.offset, layers))?,
            })
        };
        let pos = layer_set(GraphIndexSectionKind::PosLayerDirectory)?;
        let ops = layer_set(GraphIndexSectionKind::OpsLayerDirectory)?;

        let transpose = if index_header.has_membership_ranks() {
            let section = |kind: GraphIndexSectionKind| {
                directory
                    .section(kind)
                    .ok_or_else(|| malformed(&index, format!("missing {kind:?} section")))
            };
            let bitmap = section(GraphIndexSectionKind::TransposeBitmap)?;
            let superrank = section(GraphIndexSectionKind::TransposeSuperrank)?;
            let subrank = section(GraphIndexSectionKind::TransposeSubrank)?;
            let bitmap = with_artifact(
                &index,
                BitmapSpec::new(&index, bitmap.offset, bitmap.entry_count).and_then(|bitmap| {
                    RankedSpec::new(
                        bitmap,
                        PackedSpec::new(
                            &index,
                            superrank.offset,
                            superrank.entry_count,
                            superrank.bits_per_entry,
                        )?,
                        PackedSpec::new(
                            &index,
                            subrank.offset,
                            subrank.entry_count,
                            subrank.bits_per_entry,
                        )?,
                        index_header.superblock_bits(),
                        index_header.subblock_bits(),
                    )
                }),
            )?;
            // One sentinel read: the transpose closes exactly one run per
            // position, so its population is the triple count. Without this a
            // short bitmap would fail inside a request's select rather than
            // here, with the path in hand.
            let runs = bitmap.view(&index, &index).count();
            if runs != facts.triples {
                return Err(malformed(
                    &index,
                    format!(
                        "the transpose closes {runs} runs for {} triples",
                        facts.triples
                    ),
                ));
            }
            let ids = if index_header.has_membership_ids() {
                let array = section(GraphIndexSectionKind::TransposeArray)?;
                let ids = with_artifact(
                    &index,
                    PackedSpec::new(
                        &index,
                        array.offset,
                        array.entry_count,
                        array.bits_per_entry,
                    ),
                )?;
                // Every graph id must fit the array's width, or an entry the
                // writer could not have stored is read as a smaller id.
                let needed = u64::BITS - facts.named_graphs.leading_zeros();
                if u32::from(ids.width()) < needed {
                    return Err(malformed(
                        &index,
                        format!(
                            "the transpose stores graph ids in {} bits, too few for {} graphs",
                            ids.width(),
                            facts.named_graphs
                        ),
                    ));
                }
                Some(ids)
            } else {
                None
            };
            Some(TransposeSpec { bitmap, ids })
        } else {
            None
        };

        let graphs = Self {
            sidecar,
            index,
            facts,
            dictionary,
            spo,
            pos,
            ops,
            transpose,
        };
        for reserved in [UNION_GRAPH_IRI, UNNAMED_GRAPH_IRI] {
            if graphs.named_graph_id(reserved.as_bytes())?.is_some() {
                return Err(malformed(
                    &graphs.sidecar,
                    format!(
                        "the graph dictionary names {reserved}, which is reserved for the \
                         graph selector and can never be a stored graph"
                    ),
                ));
            }
        }
        Ok(graphs)
    }

    /// Triples, named graphs, and memberships.
    pub fn facts(&self) -> GraphFacts {
        self.facts
    }

    /// Whether the index carries the SPO transpose's run boundaries, which
    /// make quad-view cardinality two selects rather than one rank per layer.
    pub fn has_transpose(&self) -> bool {
        self.transpose.is_some()
    }

    /// Whether the transpose also carries graph ids, which make the graph
    /// column an array read rather than a probe of every layer.
    pub fn has_transpose_ids(&self) -> bool {
        self.transpose
            .is_some_and(|transpose| transpose.ids.is_some())
    }

    /// The layer a graph name selects: the unnamed constant is layer 0, and
    /// any other name is looked up in the dictionary. `None` for a name this
    /// bundle does not hold.
    ///
    /// The union is not a layer, so its constant is not this function's to
    /// answer: a caller reads the union by not scoping at all, and decides
    /// that before asking for a layer. Asked anyway, the constant is simply a
    /// name no dictionary can hold, because opening refused any that did.
    pub fn resolve(&self, name: &[u8]) -> Result<Option<GraphId>> {
        if name == UNNAMED_GRAPH_IRI.as_bytes() {
            return Ok(Some(GraphId::UNNAMED));
        }
        self.named_graph_id(name)
    }

    /// Every layer, the unnamed graph first: `0..=G`.
    pub fn graph_ids(&self) -> impl Iterator<Item = GraphId> {
        (0..=self.facts.named_graphs).map(GraphId)
    }

    fn named_graph_id(&self, name: &[u8]) -> Result<Option<GraphId>> {
        Ok(self
            .dictionary
            .position_of(&self.sidecar, name)?
            .map(|position| GraphId(position + 1)))
    }

    /// The name of a layer, as the sidecar spells it: an IRI without brackets,
    /// or `_:label` for a graph named by a blank node. Layer 0 is spelled
    /// [`UNNAMED_GRAPH_IRI`].
    ///
    /// # Panics
    ///
    /// Panics if `id` is beyond the named-graph count.
    pub fn name<'b>(&self, id: GraphId, buf: &'b mut Vec<u8>) -> Result<&'b [u8]> {
        if id.is_unnamed() {
            buf.clear();
            buf.extend_from_slice(UNNAMED_GRAPH_IRI.as_bytes());
            return Ok(buf);
        }
        assert!(
            id.0 <= self.facts.named_graphs,
            "graph id {} out of range for {} named graphs",
            id.0,
            self.facts.named_graphs
        );
        self.dictionary.term_at(&self.sidecar, id.0 - 1, buf)
    }

    /// The layer set keyed to `space`'s positions.
    pub fn layers(&self, space: Permutation) -> LayerSet<'_> {
        let (file, spec) = match space {
            Permutation::Spo => (&self.sidecar, self.spo),
            Permutation::Pos => (&self.index, self.pos),
            Permutation::Ops => (&self.index, self.ops),
        };
        LayerSet {
            file,
            directory: spec.directory.view(file),
            triples: self.facts.triples,
            named_graphs: self.facts.named_graphs,
        }
    }

    /// One layer in one position space.
    pub fn layer(&self, space: Permutation, id: GraphId) -> Result<Layer<'_>> {
        self.layers(space).layer(id)
    }

    /// Members of a layer: `count(g)`, the same in every space.
    pub fn count(&self, id: GraphId) -> Result<u64> {
        Ok(self.layers(Permutation::Spo).entry(id)?.member_count)
    }

    /// The transpose, when the index carries one.
    fn transpose(&self) -> Option<Transpose<'_>> {
        self.transpose.map(|spec| Transpose {
            bitmap: spec.bitmap.view(&self.index, &self.index),
            ids: spec.ids.map(|ids| ids.view(&self.index)),
        })
    }

    /// Prepare to answer per-position questions in `space`: which graphs a
    /// position has, and how many memberships a position range holds.
    ///
    /// Opens every layer of the space once, so a page pays the decoding of
    /// `G + 1` directory entries once rather than per row. The SPO space
    /// answers from the transpose instead whenever the index carries it.
    pub fn memberships(&self, space: Permutation) -> Result<Memberships<'_>> {
        let transpose = match space {
            Permutation::Spo => self.transpose(),
            Permutation::Pos | Permutation::Ops => None,
        };
        let set = self.layers(space);
        let layers = if transpose
            .as_ref()
            .is_some_and(|transpose| transpose.ids.is_some())
        {
            Vec::new()
        } else {
            (0..=self.facts.named_graphs)
                .map(|id| set.layer(GraphId(id)))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(Memberships {
            layers,
            transpose,
            triples: self.facts.triples,
            named_graphs: self.facts.named_graphs,
            index: self.index.path(),
        })
    }
}

/// The transpose projected onto the index.
struct Transpose<'a> {
    bitmap: RankedBitmap<'a>,
    ids: Option<PackedArray<'a>>,
}

impl Transpose<'_> {
    /// The `ArrayG` index at which `position`'s run begins.
    fn offset(&self, position: u64) -> u64 {
        if position == 0 {
            0
        } else {
            self.bitmap.select1(position - 1) + 1
        }
    }
}

/// Per-position membership questions over one space, prepared once.
pub struct Memberships<'a> {
    layers: Vec<Layer<'a>>,
    transpose: Option<Transpose<'a>>,
    triples: u64,
    named_graphs: u64,
    /// The file the transpose lives in, for an error that names it.
    index: &'a Path,
}

impl Memberships<'_> {
    /// The graphs containing `position`, ascending, appended to `out`.
    ///
    /// An array read per graph with the transpose's ids, else one probe of
    /// every layer — bounded by the graph count, and amortised over a page by
    /// the locality of sequential positions.
    pub fn graphs_of(&self, position: u64, out: &mut Vec<GraphId>) -> Result<()> {
        assert!(
            position < self.triples,
            "position {position} out of range for {} triples",
            self.triples
        );
        if let Some(transpose) = &self.transpose
            && let Some(ids) = &transpose.ids
        {
            let start = transpose.offset(position);
            let end = transpose.bitmap.select1(position) + 1;
            for ordinal in start..end {
                let id = ids.get(ordinal);
                if id > self.named_graphs {
                    return Err(Error::Malformed {
                        artifact: self.index.to_path_buf(),
                        detail: format!(
                            "the transpose names graph {id} at position {position}, beyond \
                             the {} named graphs",
                            self.named_graphs
                        ),
                    });
                }
                out.push(GraphId(id));
            }
            return Ok(());
        }
        for layer in &self.layers {
            if layer.access(position)? {
                out.push(layer.id());
            }
        }
        Ok(())
    }

    /// An error for memberships that contradict the sidecar's own invariants,
    /// named after the index, for a caller that composes them.
    pub fn inconsistent(&self, detail: &str) -> Error {
        Error::Malformed {
            artifact: self.index.to_path_buf(),
            detail: detail.to_owned(),
        }
    }

    /// Memberships held by the positions in `range`: the quad-view
    /// cardinality of a pattern contiguous in this space.
    ///
    /// Two selects with the transpose, else one rank difference per layer.
    pub fn in_range(&self, range: Range<u64>) -> Result<u64> {
        assert!(
            range.start <= range.end && range.end <= self.triples,
            "range {}..{} out of range for {} triples",
            range.start,
            range.end,
            self.triples
        );
        if let Some(transpose) = &self.transpose {
            return Ok(transpose.offset(range.end) - transpose.offset(range.start));
        }
        let mut total = 0u64;
        for layer in &self.layers {
            total += layer.rank(range.end)? - layer.rank(range.start)?;
        }
        Ok(total)
    }
}

/// The `G + 1` layers of one position space, in one file.
#[derive(Debug, Clone, Copy)]
pub struct LayerSet<'a> {
    file: &'a Mapping,
    directory: &'a [u8],
    triples: u64,
    named_graphs: u64,
}

impl<'a> LayerSet<'a> {
    /// The directory entry of a layer.
    ///
    /// # Panics
    ///
    /// Panics if `id` is beyond the named-graph count.
    fn entry(&self, id: GraphId) -> Result<GraphLayerEntry> {
        assert!(
            id.0 <= self.named_graphs,
            "graph id {} out of range for {} named graphs",
            id.0,
            self.named_graphs
        );
        let start = (id.0 as usize) * GRAPH_LAYER_ENTRY_SIZE;
        let bytes: &[u8; GRAPH_LAYER_ENTRY_SIZE] = self.directory
            [start..start + GRAPH_LAYER_ENTRY_SIZE]
            .try_into()
            .expect("the directory spec holds G + 1 whole entries");
        let entry = GraphLayerEntry::parse(bytes);
        if entry.flags != 0 {
            return Err(self.malformed(id, "nonzero layer flags"));
        }
        if entry.member_count == 0 {
            return Ok(entry);
        }
        if entry.minimum_position >= entry.maximum_position_exclusive
            || entry.maximum_position_exclusive > self.triples
        {
            return Err(self.malformed(id, "layer range is outside the triple universe"));
        }
        // More members than positions in the range is impossible for a set,
        // and bounding the count here is what keeps every later sum over it
        // in range.
        if entry.member_count > entry.maximum_position_exclusive - entry.minimum_position {
            return Err(self.malformed(id, "layer holds more members than its range has positions"));
        }
        Ok(entry)
    }

    /// Open one layer: decode its directory entry and validate the extent of
    /// its primary structure against the file.
    ///
    /// # Panics
    ///
    /// Panics if `id` is beyond the named-graph count.
    pub fn layer(&self, id: GraphId) -> Result<Layer<'a>> {
        let entry = self.entry(id)?;
        let file = self.file.as_bytes();
        let body = if entry.member_count == 0 {
            LayerBody::Empty
        } else {
            match entry.layer_encoding() {
                Some(GraphLayerEncoding::DenseChunks | GraphLayerEncoding::SparseChunks) => {
                    let dense = entry.encoding == GraphLayerEncoding::DenseChunks as u32;
                    let expected = entry
                        .item_count_a
                        .checked_mul(GRAPH_CHUNK_ENTRY_SIZE as u64)
                        .ok_or_else(|| self.malformed(id, "chunk directory length overflows"))?;
                    if entry.primary_length != expected {
                        return Err(self.malformed(
                            id,
                            "chunk directory length disagrees with its entry count",
                        ));
                    }
                    if dense {
                        let universe_chunks =
                            self.triples.div_ceil(1 << GRAPH_POSITION_CHUNK_SHIFT);
                        if entry.item_count_a != universe_chunks {
                            return Err(self.malformed(
                                id,
                                "dense chunk directory does not cover the universe",
                            ));
                        }
                    }
                    let chunks = self
                        .region(file, entry.primary_offset, entry.primary_length)
                        .ok_or_else(|| self.malformed(id, "chunk directory runs past the file"))?;
                    LayerBody::Chunked {
                        chunks,
                        dense,
                        file,
                    }
                }
                Some(GraphLayerEncoding::EliasFano) => {
                    if entry.primary_length != ELIAS_FANO_HEADER_SIZE as u64 {
                        return Err(self.malformed(id, "Elias-Fano header has the wrong length"));
                    }
                    let raw = self
                        .region(file, entry.primary_offset, entry.primary_length)
                        .ok_or_else(|| {
                            self.malformed(id, "Elias-Fano header runs past the file")
                        })?;
                    let raw: &[u8; ELIAS_FANO_HEADER_SIZE] =
                        raw.try_into().expect("the region is exactly the header");
                    let header = EliasFanoHeader::parse(raw)
                        .map_err(|error| self.malformed(id, &format!("{error:#}")))?;
                    if header.universe != self.triples || header.members != entry.member_count {
                        return Err(
                            self.malformed(id, "Elias-Fano header disagrees with the directory")
                        );
                    }
                    if header.low_bits >= 64 {
                        return Err(self.malformed(id, "Elias-Fano low-bit width is not below 64"));
                    }
                    // `rank` walks buckets by their closing zero, so the bucket
                    // count must be what the universe and width imply and the
                    // upper bitmap must hold exactly one zero per bucket.
                    if header.high_buckets != 1 + ((header.universe - 1) >> header.low_bits)
                        || header.upper_bits != header.high_buckets + header.members
                    {
                        return Err(self.malformed(
                            id,
                            "Elias-Fano bucket count disagrees with the universe and width",
                        ));
                    }
                    let lower = if header.low_bits == 0 {
                        None
                    } else {
                        let bytes = self
                            .region(file, header.lower_offset, header.lower_length)
                            .ok_or_else(|| {
                                self.malformed(id, "Elias-Fano lower bits run past the file")
                            })?;
                        Some(
                            PackedArray::new(bytes, header.members, header.low_bits as u8)
                                .map_err(|error| self.malformed(id, &error.to_string()))?,
                        )
                    };
                    let region = |offset, length, what: &str| {
                        self.region(file, offset, length).ok_or_else(|| {
                            self.malformed(id, &format!("Elias-Fano {what} runs past the file"))
                        })
                    };
                    let upper = RankedBitmap::new(
                        BitmapView::new(
                            region(header.upper_offset, header.upper_length, "upper bitmap")?,
                            header.upper_bits,
                        )
                        .map_err(|error| self.malformed(id, &error.to_string()))?,
                        PackedArray::new(
                            region(
                                header.superrank_offset,
                                header.superrank_length,
                                "superranks",
                            )?,
                            header.superrank_count,
                            SUPERRANK_WIDTH,
                        )
                        .map_err(|error| self.malformed(id, &error.to_string()))?,
                        PackedArray::new(
                            region(header.subrank_offset, header.subrank_length, "subranks")?,
                            header.subrank_count,
                            SUBRANK_WIDTH,
                        )
                        .map_err(|error| self.malformed(id, &error.to_string()))?,
                        ELIAS_FANO_SUPERBLOCK_BITS,
                        ELIAS_FANO_SUBBLOCK_BITS,
                    )
                    .map_err(|error| self.malformed(id, &error.to_string()))?;
                    if upper.count() != header.members {
                        return Err(self.malformed(
                            id,
                            "Elias-Fano upper bitmap does not hold one bit per member",
                        ));
                    }
                    LayerBody::EliasFano(EliasFanoLayer {
                        low_bits: header.low_bits,
                        universe: header.universe,
                        high_buckets: header.high_buckets,
                        lower,
                        upper,
                    })
                }
                None => {
                    return Err(
                        self.malformed(id, &format!("unknown layer encoding {}", entry.encoding))
                    );
                }
            }
        };
        Ok(Layer {
            id,
            entry,
            triples: self.triples,
            body,
            path: self.file.path(),
        })
    }

    fn region<'b>(&self, file: &'b [u8], offset: u64, length: u64) -> Option<&'b [u8]> {
        let end = offset.checked_add(length)?;
        file.get(offset as usize..end as usize)
    }

    fn malformed(&self, id: GraphId, detail: &str) -> Error {
        Error::Malformed {
            artifact: self.file.path().to_path_buf(),
            detail: format!("layer {}: {detail}", id.0),
        }
    }
}

/// Positions per chunk.
const CHUNK_POSITIONS: u64 = 1 << GRAPH_POSITION_CHUNK_SHIFT;

/// One graph's positions in one space: a set over `[0, N)` with rank and select.
///
/// The five operations are what scoping a pattern needs: a scoped count is two
/// ranks, a scoped page is a run of selects, `s ? o` filters its probe with
/// `access`, and `next_member` skips from any position to the next member.
#[derive(Debug, Clone, Copy)]
pub struct Layer<'a> {
    id: GraphId,
    entry: GraphLayerEntry,
    triples: u64,
    body: LayerBody<'a>,
    path: &'a Path,
}

#[derive(Debug, Clone, Copy)]
enum LayerBody<'a> {
    Empty,
    Chunked {
        chunks: &'a [u8],
        dense: bool,
        file: &'a [u8],
    },
    EliasFano(EliasFanoLayer<'a>),
}

/// The projected regions of an Elias–Fano layer and the three header fields
/// its arithmetic needs; the header's region offsets and CRCs are consumed
/// while projecting and not carried.
#[derive(Debug, Clone, Copy)]
struct EliasFanoLayer<'a> {
    low_bits: u32,
    universe: u64,
    high_buckets: u64,
    lower: Option<PackedArray<'a>>,
    upper: RankedBitmap<'a>,
}

impl Layer<'_> {
    /// The graph this layer holds.
    pub fn id(&self) -> GraphId {
        self.id
    }

    /// Members: `count(g)`.
    pub fn count(&self) -> u64 {
        self.entry.member_count
    }

    /// Members strictly before `position`. `position` may equal the universe.
    pub fn rank(&self, position: u64) -> Result<u64> {
        assert!(
            position <= self.triples,
            "rank({position}) out of range for {} triples",
            self.triples
        );
        let entry = &self.entry;
        if position == 0 || entry.member_count == 0 {
            return Ok(0);
        }
        if position >= entry.maximum_position_exclusive {
            return Ok(entry.member_count);
        }
        if position <= entry.minimum_position {
            return Ok(0);
        }
        match &self.body {
            LayerBody::Empty => Ok(0),
            LayerBody::Chunked {
                chunks,
                dense,
                file,
            } => {
                let key = position >> GRAPH_POSITION_CHUNK_SHIFT;
                let offset = (position & (CHUNK_POSITIONS - 1)) as u16;
                let (insertion, chunk) = self.find_chunk(chunks, *dense, key)?;
                match chunk {
                    Some(chunk) if chunk.cardinality == 0 => Ok(chunk.rank_before),
                    Some(chunk) => {
                        Ok(chunk.rank_before + self.container_rank(file, chunk, offset)?)
                    }
                    None if insertion == self.chunk_count(chunks) => Ok(entry.member_count),
                    None => Ok(self.chunk(chunks, insertion)?.rank_before),
                }
            }
            LayerBody::EliasFano(EliasFanoLayer {
                low_bits,
                universe,
                high_buckets,
                lower,
                upper,
            }) => {
                let members = entry.member_count;
                if position == *universe {
                    return Ok(members);
                }
                let high = position >> low_bits;
                let low_mask = if *low_bits == 0 {
                    0
                } else {
                    (1u64 << low_bits) - 1
                };
                let low = position & low_mask;
                if high >= *high_buckets {
                    return Ok(members);
                }
                // Bucket `h` is closed by the `h`-th clear bit, and exactly `j`
                // clear bits precede the `j`-th, so the members with a smaller
                // high part number `select0(h - 1) - (h - 1)`.
                let start = if high == 0 {
                    0
                } else {
                    upper.select0(high - 1) - (high - 1)
                };
                let end = upper.select0(high) - high;
                let (mut left, mut right) = (start, end);
                while left < right {
                    let middle = left + (right - left) / 2;
                    if ef_lower(lower, middle) < low {
                        left = middle + 1;
                    } else {
                        right = middle;
                    }
                }
                Ok(left)
            }
        }
    }

    /// The position of the zero-based `ordinal`-th member.
    ///
    /// # Panics
    ///
    /// Panics if `ordinal >= count()`.
    pub fn select(&self, ordinal: u64) -> Result<u64> {
        assert!(
            ordinal < self.entry.member_count,
            "select({ordinal}) out of range for {} members",
            self.entry.member_count
        );
        match &self.body {
            LayerBody::Empty => unreachable!("an empty layer has no member to select"),
            LayerBody::Chunked { chunks, file, .. } => {
                // The first chunk whose members extend past the ordinal. An
                // empty dense chunk extends no further than its predecessor,
                // so the search passes over it to the chunk that holds the
                // member.
                let count = self.chunk_count(chunks);
                let (mut low, mut high) = (0u64, count);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let chunk = self.chunk(chunks, middle)?;
                    if chunk.rank_before + u64::from(chunk.cardinality) > ordinal {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                if low >= count {
                    return Err(self.malformed("chunk directory does not hold its member count"));
                }
                let chunk = self.chunk(chunks, low)?;
                let local = self.container_select(file, chunk, ordinal - chunk.rank_before)?;
                Ok((chunk.key << GRAPH_POSITION_CHUNK_SHIFT) | u64::from(local))
            }
            LayerBody::EliasFano(EliasFanoLayer {
                low_bits,
                universe,
                lower,
                upper,
                ..
            }) => {
                let high = upper.select1(ordinal) - ordinal;
                let value = (high << low_bits) | ef_lower(lower, ordinal);
                if value >= *universe {
                    return Err(
                        self.malformed("decoded Elias-Fano position is outside the universe")
                    );
                }
                Ok(value)
            }
        }
    }

    /// Whether `position` is a member.
    pub fn access(&self, position: u64) -> Result<bool> {
        assert!(
            position < self.triples,
            "access({position}) out of range for {} triples",
            self.triples
        );
        let entry = &self.entry;
        if entry.member_count == 0
            || position < entry.minimum_position
            || position >= entry.maximum_position_exclusive
        {
            return Ok(false);
        }
        match &self.body {
            LayerBody::Empty => Ok(false),
            LayerBody::Chunked {
                chunks,
                dense,
                file,
            } => {
                let key = position >> GRAPH_POSITION_CHUNK_SHIFT;
                let offset = (position & (CHUNK_POSITIONS - 1)) as u16;
                let Some(chunk) = self.find_chunk(chunks, *dense, key)?.1 else {
                    return Ok(false);
                };
                self.container_access(file, chunk, offset)
            }
            LayerBody::EliasFano(_) => Ok(self.rank(position + 1)? != self.rank(position)?),
        }
    }

    /// The first member at or after `position`, if any.
    pub fn next_member(&self, position: u64) -> Result<Option<u64>> {
        let rank = self.rank(position)?;
        if rank == self.entry.member_count {
            Ok(None)
        } else {
            self.select(rank).map(Some)
        }
    }

    fn chunk_count(&self, chunks: &[u8]) -> u64 {
        (chunks.len() / GRAPH_CHUNK_ENTRY_SIZE) as u64
    }

    fn chunk(&self, chunks: &[u8], index: u64) -> Result<GraphChunkEntry> {
        let start = (index as usize)
            .checked_mul(GRAPH_CHUNK_ENTRY_SIZE)
            .filter(|start| start + GRAPH_CHUNK_ENTRY_SIZE <= chunks.len())
            .ok_or_else(|| self.malformed("chunk index is outside the chunk directory"))?;
        let bytes: &[u8; GRAPH_CHUNK_ENTRY_SIZE] = chunks[start..start + GRAPH_CHUNK_ENTRY_SIZE]
            .try_into()
            .expect("a whole chunk entry");
        Ok(GraphChunkEntry::parse(bytes))
    }

    /// The chunk holding `key`, or where it would be inserted.
    ///
    /// A dense directory is indexed by key. A sparse one is binary-searched by
    /// key: the directory is sorted, so the access hash the format also stores
    /// is an alternative route to the same entry and stays unread.
    fn find_chunk(
        &self,
        chunks: &[u8],
        dense: bool,
        key: u64,
    ) -> Result<(u64, Option<GraphChunkEntry>)> {
        let count = self.chunk_count(chunks);
        if dense {
            if key >= count {
                return Ok((count, None));
            }
            let chunk = self.chunk(chunks, key)?;
            if chunk.key != key {
                return Err(self.malformed("dense chunk directory is out of key order"));
            }
            return Ok((key, Some(chunk)));
        }
        let (mut low, mut high) = (0u64, count);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.chunk(chunks, middle)?.key < key {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low < count {
            let chunk = self.chunk(chunks, low)?;
            if chunk.key == key {
                return Ok((low, Some(chunk)));
            }
        }
        Ok((low, None))
    }

    fn payload<'b>(&self, file: &'b [u8], chunk: GraphChunkEntry) -> Result<&'b [u8]> {
        let expected = match chunk.container() {
            Some(GraphChunkContainer::Array) if chunk.cardinality <= GRAPH_ARRAY_CONTAINER_MAX => {
                chunk.cardinality * 2
            }
            Some(GraphChunkContainer::Bitmap) if chunk.cardinality > GRAPH_ARRAY_CONTAINER_MAX => {
                GRAPH_BITMAP_CONTAINER_BYTES
            }
            _ => return Err(self.malformed("chunk container disagrees with its cardinality")),
        };
        if chunk.payload_length != expected {
            return Err(self.malformed("chunk payload has the wrong length for its container"));
        }
        chunk
            .payload_offset
            .checked_add(u64::from(chunk.payload_length))
            .and_then(|end| file.get(chunk.payload_offset as usize..end as usize))
            .ok_or_else(|| self.malformed("chunk payload runs past the file"))
    }

    fn container_access(&self, file: &[u8], chunk: GraphChunkEntry, offset: u16) -> Result<bool> {
        if chunk.cardinality == 0 {
            return Ok(false);
        }
        let payload = self.payload(file, chunk)?;
        Ok(match chunk.container() {
            Some(GraphChunkContainer::Bitmap) => {
                payload[BITMAP_CONTAINER_BITS_AT + usize::from(offset) / 8] >> (offset % 8) & 1 == 1
            }
            _ => {
                let index = array_lower_bound(payload, offset);
                index < chunk.cardinality as usize && array_value(payload, index) == offset
            }
        })
    }

    fn container_rank(&self, file: &[u8], chunk: GraphChunkEntry, offset: u16) -> Result<u64> {
        let payload = self.payload(file, chunk)?;
        Ok(match chunk.container() {
            Some(GraphChunkContainer::Bitmap) => {
                let subblock = usize::from(offset) / BITMAP_CONTAINER_SUBBLOCK_BITS;
                let base = u64::from(u16::from_le_bytes([
                    payload[subblock * 2],
                    payload[subblock * 2 + 1],
                ]));
                let bits = BitmapView::new(&payload[BITMAP_CONTAINER_BITS_AT..], CHUNK_POSITIONS)
                    .expect("a bitmap container holds exactly one chunk of bits");
                base + bits.count_ones_in(
                    (subblock * BITMAP_CONTAINER_SUBBLOCK_BITS) as u64..u64::from(offset),
                )
            }
            _ => array_lower_bound(payload, offset) as u64,
        })
    }

    fn container_select(&self, file: &[u8], chunk: GraphChunkEntry, ordinal: u64) -> Result<u16> {
        debug_assert!(ordinal < u64::from(chunk.cardinality));
        let payload = self.payload(file, chunk)?;
        match chunk.container() {
            Some(GraphChunkContainer::Bitmap) => {
                // The last subblock whose starting rank is at or below the
                // ordinal, then a bounded scan inside it.
                let subrank =
                    |j: usize| u64::from(u16::from_le_bytes([payload[j * 2], payload[j * 2 + 1]]));
                let (mut low, mut high) = (0usize, BITMAP_CONTAINER_SUBBLOCKS - 1);
                while low < high {
                    let middle = low + (high - low).div_ceil(2);
                    if subrank(middle) <= ordinal {
                        low = middle;
                    } else {
                        high = middle - 1;
                    }
                }
                let bits = BitmapView::new(&payload[BITMAP_CONTAINER_BITS_AT..], CHUNK_POSITIONS)
                    .expect("a bitmap container holds exactly one chunk of bits");
                bits.select_from(
                    (low * BITMAP_CONTAINER_SUBBLOCK_BITS) as u64,
                    ordinal - subrank(low),
                )
                .and_then(|position| u16::try_from(position).ok())
                .ok_or_else(|| {
                    self.malformed("bitmap container holds fewer members than its subranks say")
                })
            }
            _ => Ok(array_value(payload, ordinal as usize)),
        }
    }

    fn malformed(&self, detail: &str) -> Error {
        Error::Malformed {
            artifact: self.path.to_path_buf(),
            detail: format!("layer {}: {detail}", self.id.0),
        }
    }

    /// An error for a layer whose operations contradict each other, named
    /// after the file, for a caller that composes them.
    pub fn inconsistent(&self, detail: &str) -> Error {
        self.malformed(detail)
    }
}

/// The `i`-th packed low part, or zero when the layer stores none.
fn ef_lower(lower: &Option<PackedArray<'_>>, index: u64) -> u64 {
    lower.as_ref().map_or(0, |lower| lower.get(index))
}

/// Byte offset of the bitmap inside a bitmap container, after its subranks.
const BITMAP_CONTAINER_BITS_AT: usize = GRAPH_BITMAP_CONTAINER_SUBRANK_BYTES;
/// Subranks per bitmap container, one per subblock.
const BITMAP_CONTAINER_SUBBLOCKS: usize = GRAPH_BITMAP_CONTAINER_SUBRANK_BYTES / 2;
const BITMAP_CONTAINER_SUBBLOCK_BITS: usize = GRAPH_BITMAP_CONTAINER_SUBBLOCK_BITS as usize;

/// The `index`-th offset of an array container.
fn array_value(payload: &[u8], index: usize) -> u16 {
    u16::from_le_bytes([payload[index * 2], payload[index * 2 + 1]])
}

/// The first index of an array container whose offset is not below `offset`.
fn array_lower_bound(payload: &[u8], offset: u16) -> usize {
    let (mut low, mut high) = (0usize, payload.len() / 2);
    while low < high {
        let middle = low + (high - low) / 2;
        if array_value(payload, middle) < offset {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn malformed(mapping: &Mapping, detail: String) -> Error {
    Error::Malformed {
        artifact: mapping.path().to_path_buf(),
        detail,
    }
}

fn with_artifact<T>(mapping: &Mapping, result: Result<T>) -> Result<T> {
    result.map_err(|error| match error {
        Error::Region(detail) => malformed(mapping, detail),
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{IdPattern, resolve};
    use crate::perm::Permutations;
    use crate::testing::{
        Fixture, TINY_NQ, Transpose as Built, WORKED_EXAMPLE_NQ, parse_hdtc_row, synthetic_quads,
    };
    use crate::{IdTriple, Role};
    use std::collections::BTreeMap;

    /// Every membership of a bundle in every space, from hdtc's own
    /// four-position search and the permutations' positions: the oracle for
    /// the layers, which shares nothing with the code it checks.
    struct Oracle {
        /// For each space, for each layer, the sorted member positions.
        members: BTreeMap<Permutation, Vec<Vec<u64>>>,
        triples: u64,
    }

    impl Oracle {
        fn new(fixture: &Fixture, perms: &Permutations, graphs: &Graphs) -> Self {
            let dictionary = perms.dict();
            let positions = positions_by_space(perms);
            let named = graphs.facts().named_graphs;
            let mut members: BTreeMap<Permutation, Vec<Vec<u64>>> = BTreeMap::new();
            for space in [Permutation::Spo, Permutation::Pos, Permutation::Ops] {
                members.insert(space, vec![Vec::new(); named as usize + 1]);
            }
            let mut buf = Vec::new();
            for graph in 0..=named {
                let query = if graph == 0 {
                    "? ? ? default".to_owned()
                } else {
                    let name = graphs.name(GraphId(graph), &mut buf).unwrap();
                    format!("? ? ? <{}>", String::from_utf8_lossy(name))
                };
                for row in fixture.search(&query) {
                    let triple = parse_hdtc_row(&dictionary, &row);
                    for (space, layers) in members.iter_mut() {
                        layers[graph as usize].push(positions[space][&triple]);
                    }
                }
            }
            for layers in members.values_mut() {
                for layer in layers {
                    layer.sort_unstable();
                }
            }
            Self {
                members,
                triples: perms.triples(),
            }
        }

        fn layer(&self, space: Permutation, graph: u64) -> &[u64] {
            &self.members[&space][graph as usize]
        }

        fn rank(&self, space: Permutation, graph: u64, position: u64) -> u64 {
            self.layer(space, graph)
                .partition_point(|member| *member < position) as u64
        }
    }

    /// Each triple's position in each permutation, by enumerating every
    /// root group of that permutation with the running position.
    fn positions_by_space(perms: &Permutations) -> BTreeMap<Permutation, BTreeMap<IdTriple, u64>> {
        let counts = perms.dict_counts();
        let mut by_space = BTreeMap::new();
        for space in [Permutation::Spo, Permutation::Pos, Permutation::Ops] {
            let mut positions = BTreeMap::new();
            let role = match space {
                Permutation::Spo => Role::Subject,
                Permutation::Pos => Role::Predicate,
                Permutation::Ops => Role::Object,
            };
            let make = |id| match space {
                Permutation::Spo => IdPattern {
                    subject: Some(id),
                    predicate: None,
                    object: None,
                },
                Permutation::Pos => IdPattern {
                    subject: None,
                    predicate: Some(id),
                    object: None,
                },
                Permutation::Ops => IdPattern {
                    subject: None,
                    predicate: None,
                    object: Some(id),
                },
            };
            let mut next = 0u64;
            for id in 1..=counts.len(role) {
                let selection = resolve(perms, make(id)).unwrap();
                assert_eq!(selection.permutation(), space);
                for triple in selection.page(0, usize::MAX) {
                    positions.insert(triple, next);
                    next += 1;
                }
            }
            assert_eq!(next, perms.triples());
            by_space.insert(space, positions);
        }
        by_space
    }

    fn open(fixture: &Fixture) -> (Permutations, Graphs) {
        let perms = Permutations::open(fixture.map_hdt(), fixture.map_perm()).unwrap();
        let graphs = Graphs::open(
            &fixture.hdt_path(),
            fixture.map_graphs(),
            fixture.map_graph_index(),
        )
        .unwrap();
        (perms, graphs)
    }

    #[test]
    fn the_worked_example_has_the_counts_its_contract_states() {
        let fixture = Fixture::build_quads(WORKED_EXAMPLE_NQ);
        let (_, graphs) = open(&fixture);
        let facts = graphs.facts();
        assert_eq!(facts.triples, 3);
        assert_eq!(facts.named_graphs, 2);
        assert_eq!(facts.memberships, 5);

        let g1 = graphs.resolve(b"http://example.org/g1").unwrap().unwrap();
        let g2 = graphs.resolve(b"http://example.org/g2").unwrap().unwrap();
        assert_eq!((g1, g2), (GraphId(1), GraphId(2)));
        assert_eq!(
            graphs.resolve(UNNAMED_GRAPH_IRI.as_bytes()).unwrap(),
            Some(GraphId::UNNAMED)
        );
        assert_eq!(graphs.resolve(UNION_GRAPH_IRI.as_bytes()).unwrap(), None);
        assert_eq!(graphs.resolve(b"http://example.org/g3").unwrap(), None);

        assert_eq!(graphs.count(GraphId::UNNAMED).unwrap(), 2);
        assert_eq!(graphs.count(g1).unwrap(), 2);
        assert_eq!(graphs.count(g2).unwrap(), 1);

        let mut buf = Vec::new();
        assert_eq!(graphs.name(g1, &mut buf).unwrap(), b"http://example.org/g1");
        assert_eq!(
            graphs.name(GraphId::UNNAMED, &mut buf).unwrap(),
            UNNAMED_GRAPH_IRI.as_bytes()
        );
    }

    #[test]
    fn every_layer_operation_agrees_with_hdtc_in_every_space() {
        for (source, transpose) in [
            (TINY_NQ, Built::None),
            (WORKED_EXAMPLE_NQ, Built::None),
            (WORKED_EXAMPLE_NQ, Built::Ranks),
            (WORKED_EXAMPLE_NQ, Built::Ids),
            (synthetic_quads().as_str(), Built::None),
            (synthetic_quads().as_str(), Built::Ranks),
            (synthetic_quads().as_str(), Built::Ids),
        ] {
            let fixture = Fixture::build_quads_with(source, transpose);
            let (perms, graphs) = open(&fixture);
            assert_eq!(graphs.has_transpose(), transpose != Built::None);
            assert_eq!(graphs.has_transpose_ids(), transpose == Built::Ids);
            let oracle = Oracle::new(&fixture, &perms, &graphs);
            let n = oracle.triples;
            let mut encodings = Vec::new();

            for space in [Permutation::Spo, Permutation::Pos, Permutation::Ops] {
                for graph in 0..=graphs.facts().named_graphs {
                    let layer = graphs.layer(space, GraphId(graph)).unwrap();
                    encodings.push(layer.entry.encoding);
                    let members = oracle.layer(space, graph);
                    assert_eq!(
                        layer.count(),
                        members.len() as u64,
                        "{space:?} layer {graph}"
                    );
                    for (ordinal, member) in members.iter().enumerate() {
                        assert_eq!(
                            layer.select(ordinal as u64).unwrap(),
                            *member,
                            "{space:?} layer {graph} select({ordinal})"
                        );
                    }
                    // Every position for the small fixtures; for the wide
                    // one, a stride, the neighbourhood of a sample of members
                    // (where rank and access change value), and every chunk
                    // and subblock boundary (where the directories hand over).
                    let probes: Vec<u64> = if n <= 64 {
                        (0..=n).collect()
                    } else {
                        (0..=n)
                            .step_by(997)
                            .chain(
                                members
                                    .iter()
                                    .step_by(97)
                                    .flat_map(|m| [m.saturating_sub(1), *m, (*m + 1).min(n)]),
                            )
                            .chain((0..=n).step_by(512))
                            .chain((0..=n).step_by(CHUNK_POSITIONS as usize))
                            .collect()
                    };
                    for position in probes {
                        assert_eq!(
                            layer.rank(position).unwrap(),
                            oracle.rank(space, graph, position),
                            "{space:?} layer {graph} rank({position})"
                        );
                        if position < n {
                            assert_eq!(
                                layer.access(position).unwrap(),
                                members.binary_search(&position).is_ok(),
                                "{space:?} layer {graph} access({position})"
                            );
                            let next = members
                                .get(members.partition_point(|m| *m < position))
                                .copied();
                            assert_eq!(
                                layer.next_member(position).unwrap(),
                                next,
                                "{space:?} layer {graph} next_member({position})"
                            );
                        }
                    }
                }

                // The per-position questions, against the same oracle.
                let memberships = graphs.memberships(space).unwrap();
                let mut out = Vec::new();
                let sample: Vec<u64> = if n <= 64 {
                    (0..n).collect()
                } else {
                    (0..n).step_by(4093).collect()
                };
                for position in sample {
                    out.clear();
                    memberships.graphs_of(position, &mut out).unwrap();
                    let expected: Vec<GraphId> = (0..=graphs.facts().named_graphs)
                        .filter(|graph| {
                            oracle.layer(space, *graph).binary_search(&position).is_ok()
                        })
                        .map(GraphId)
                        .collect();
                    assert_eq!(out, expected, "{space:?} graphs_of({position})");
                }
                let total: u64 = (0..=graphs.facts().named_graphs)
                    .map(|graph| oracle.layer(space, graph).len() as u64)
                    .sum();
                assert_eq!(
                    memberships.in_range(0..n).unwrap(),
                    total,
                    "{space:?} whole range"
                );
                assert_eq!(memberships.in_range(0..0).unwrap(), 0);
                let middle = n / 3..(2 * n / 3).max(n / 3);
                let expected: u64 = (0..=graphs.facts().named_graphs)
                    .map(|graph| {
                        oracle.rank(space, graph, middle.end)
                            - oracle.rank(space, graph, middle.start)
                    })
                    .sum();
                assert_eq!(
                    memberships.in_range(middle).unwrap(),
                    expected,
                    "{space:?} middle range"
                );
            }

            if source.len() > 1000 {
                encodings.sort_unstable();
                encodings.dedup();
                assert_eq!(
                    encodings,
                    vec![1, 2, 3],
                    "the synthetic bundle must reach every encoding"
                );
            }
        }
    }

    #[test]
    fn an_index_without_both_layer_sets_is_refused_by_name() {
        let fixture = Fixture::build_quads(WORKED_EXAMPLE_NQ);
        let hdtc = crate::testing::hdtc_binary();
        let status = std::process::Command::new(&hdtc)
            .arg("graphs-index")
            .arg(fixture.hdt_path())
            .args(["--positions", "pos", "--memory-limit", "64M"])
            .status()
            .unwrap();
        assert!(status.success());
        let error = Graphs::open(
            &fixture.hdt_path(),
            fixture.map_graphs(),
            fixture.map_graph_index(),
        )
        .expect_err("an index missing the OPS layer set must be refused");
        match error {
            Error::MissingRequiredArtifact {
                artifact, remedy, ..
            } => {
                assert!(artifact.contains("ops"), "{artifact}");
                assert!(remedy.contains("--positions pos,ops"), "{remedy}");
            }
            other => panic!("unexpected error: {other:#}"),
        }
    }

    /// The structures a layer walks are decoded on first touch, so a
    /// malformed one must come back as an error naming the file rather than
    /// a panic inside a request. Corrupt each field the reader trusts and ask
    /// for the layer.
    #[test]
    fn a_malformed_layer_is_reported_rather_than_trusted() {
        let fixture = Fixture::build_quads(&synthetic_quads());
        let (_, graphs) = open(&fixture);
        let header = GraphSidecarDirectory::read(
            &fixture.bundle_path().join(crate::store::artifact::GRAPHS),
            &fixture.hdt_path(),
        )
        .unwrap();
        let directory_offset = header.header().directory_offset as usize;
        let original =
            std::fs::read(fixture.bundle_path().join(crate::store::artifact::GRAPHS)).unwrap();

        // Layer 1 is the dense one, layer 3 the Elias–Fano one; every graph
        // id here is 1-based, so the entry offsets follow.
        let corruptions: Vec<(&str, usize, Vec<u8>)> = vec![
            ("nonzero layer flags", 76, vec![1, 0, 0, 0]),
            ("unknown encoding", 72, vec![9, 0, 0, 0]),
            ("chunk directory length", 8, vec![1, 0, 0, 0, 0, 0, 0, 0]),
            ("range past the universe", 64, vec![0xff; 8]),
        ];
        for (what, field, bytes) in corruptions {
            for graph in graphs.graph_ids().filter(|id| !id.is_unnamed()) {
                let mut corrupt = original.clone();
                let at = directory_offset + graph.0 as usize * GRAPH_LAYER_ENTRY_SIZE + field;
                corrupt[at..at + bytes.len()].copy_from_slice(&bytes);
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("data.hdt.graphs");
                std::fs::write(&path, &corrupt).unwrap();
                let file = crate::testing::map_fixture(&path);
                let set = LayerSet {
                    file: &file,
                    directory: &file.as_bytes()[directory_offset..],
                    triples: graphs.facts().triples,
                    named_graphs: graphs.facts().named_graphs,
                };
                match set.layer(graph) {
                    Err(Error::Malformed { artifact, detail }) => {
                        assert_eq!(artifact, path, "{what} on layer {}", graph.0);
                        assert!(
                            detail.starts_with(&format!("layer {}", graph.0)),
                            "{detail}"
                        );
                    }
                    Ok(_) => panic!("{what} on layer {} was accepted", graph.0),
                    Err(other) => panic!("{what} on layer {}: {other:#}", graph.0),
                }
            }
        }
    }

    /// The transpose's run count is checked at open with one sentinel read; a
    /// bitmap that closes the wrong number of runs would otherwise fail
    /// inside a request's select.
    #[test]
    fn a_transpose_closing_the_wrong_number_of_runs_is_refused_at_open() {
        let fixture = Fixture::build_quads_with(WORKED_EXAMPLE_NQ, Built::Ids);
        let index_path = fixture
            .bundle_path()
            .join(crate::store::artifact::GRAPHS_IDX);
        let directory = GraphIndex::directory(&index_path, &fixture.hdt_path()).unwrap();
        let bitmap = directory
            .section(GraphIndexSectionKind::TransposeBitmap)
            .expect("the fixture carries a transpose");
        // The rank directory is read at open and the bitmap is not, so a
        // sentinel that disagrees with the header is the case to catch:
        // rewrite the superrank sentinel to claim one run fewer.
        let superrank = directory
            .section(GraphIndexSectionKind::TransposeSuperrank)
            .unwrap();
        let mut bytes = std::fs::read(&index_path).unwrap();
        let sentinel = (superrank.offset + (superrank.entry_count - 1) * 8) as usize;
        bytes[sentinel..sentinel + 8].copy_from_slice(&(bitmap.entry_count - 1).to_le_bytes());
        std::fs::write(&index_path, &bytes).unwrap();
        // The sentinel is under the section's CRC; hdtc's open verifies the
        // header and footer only, so the store's own check is what fires.
        let error = Graphs::open(
            &fixture.hdt_path(),
            fixture.map_graphs(),
            fixture.map_graph_index(),
        )
        .expect_err("a transpose with the wrong run count must be refused");
        match error {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, index_path);
                assert!(detail.contains("runs"), "{detail}");
            }
            other => panic!("unexpected error: {other:#}"),
        }
    }

    #[test]
    fn a_sidecar_naming_a_reserved_graph_is_refused_at_open() {
        for reserved in [UNION_GRAPH_IRI, UNNAMED_GRAPH_IRI] {
            let fixture = Fixture::build_quads(&format!(
                "<http://example.org/s> <http://example.org/p> <http://example.org/o> <{reserved}> .\n"
            ));
            let error = Graphs::open(
                &fixture.hdt_path(),
                fixture.map_graphs(),
                fixture.map_graph_index(),
            )
            .expect_err("a reserved graph name must be refused");
            match error {
                Error::Malformed { artifact, detail } => {
                    assert_eq!(
                        artifact,
                        fixture.bundle_path().join(crate::store::artifact::GRAPHS)
                    );
                    assert!(detail.contains(reserved), "{detail}");
                }
                other => panic!("unexpected error: {other:#}"),
            }
        }
    }

    #[test]
    fn a_sidecar_for_another_hdt_is_refused_as_a_binding_failure() {
        // The shape a real bundle can have: one directory whose sidecar pair
        // was built against a different HDT than the one beside them.
        let first = Fixture::build_quads(WORKED_EXAMPLE_NQ);
        let second = Fixture::build_quads(TINY_NQ);
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("mixed");
        second.copy_bundle_to(&bundle);
        for name in [
            crate::store::artifact::GRAPHS,
            crate::store::artifact::GRAPHS_IDX,
        ] {
            std::fs::copy(first.bundle_path().join(name), bundle.join(name)).unwrap();
        }
        let error = Graphs::open(
            &bundle.join(crate::store::artifact::HDT),
            crate::testing::map_fixture(&bundle.join(crate::store::artifact::GRAPHS)),
            crate::testing::map_fixture(&bundle.join(crate::store::artifact::GRAPHS_IDX)),
        )
        .expect_err("a foreign sidecar must be refused");
        match error {
            Error::ArtifactBindingMismatch { artifact, hdt, .. } => {
                assert_eq!(artifact, bundle.join(crate::store::artifact::GRAPHS));
                assert_eq!(hdt, bundle.join(crate::store::artifact::HDT));
            }
            other => panic!("expected a binding mismatch, got {other:#}"),
        }
    }
}
