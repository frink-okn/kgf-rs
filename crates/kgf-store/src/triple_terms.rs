//! RDF 1.2 triple terms: `data.hdt.tt` and the propositions HDT beside it.
//!
//! hdtc stores a triple term without changing what `data.hdt` is. Each
//! distinct triple term is an ordinary object IRI under a reserved namespace,
//! minted from the term's content (`hdtc/docs/triple-terms-format.md` §2), so
//! every pattern that does not look inside a triple term — `?e rdf:reifies ?t`,
//! or a ground `<<( s p o )>>`, which is its minted IRI — is an ordinary
//! pattern over `data.hdt` with ordinary counts. Two artifacts say what those
//! IRIs mean:
//!
//! - **`data.hdt.tt`**, an index. Its *runs* give each triple-term IRI's
//!   proposition as three identifiers, in the HDT that holds the proposition.
//!   Its *layers*, one per HDT and permutation, mark the positions whose triple
//!   is a proposition named by an object of `data.hdt`, and a *reverse* array
//!   gives that object's identifier for each marked position, in position order.
//! - **`data.hdt.props.hdt`**, with its own permutation index: an ordinary HDT
//!   of the **unasserted** propositions only. An asserted triple term's
//!   proposition is already a triple of `data.hdt`, so a graph with provenance
//!   on every triple pays one layer bit per triple rather than a second copy of
//!   itself.
//!
//! A proposition therefore lives in exactly one of two HDTs — [`Source`] —
//! and its identifiers address that HDT's dictionary.
//!
//! # Matching inside a triple term
//!
//! A pattern with variables inside a triple term, `S P <<( s p o )>>`, is
//! answered by [`TripleTermSelection`] with one of three plans, chosen from
//! the bundle alone so that a resumed page takes the plan that issued it:
//!
//! - **Any triple term** (no inner constraint, no outer subject): the triples
//!   whose object is a triple term are one contiguous range of OPS, or of POS
//!   under a bound outer predicate, because the triple-term IRIs are one
//!   contiguous run of the object dictionary. An ordinary [`Selection`].
//! - **Inner first**: the inner pattern is a range of SPO, POS or OPS in each
//!   HDT that holds propositions, and the layer's set bits in that range are
//!   exactly the matching triple terms named in `data.hdt`. Each is then one
//!   OPS group of `data.hdt`. Work is proportional to the matching terms, not
//!   to the range — except for an inner `s ? o`, which no permutation makes
//!   contiguous: there each predicate group of `s` is probed for `o`, so the
//!   candidates are at most the subject's predicates.
//! - **Outer subject first**, when the outer subject's triple-term objects are
//!   fewer than the inner matches: each is decoded and tested.
//!
//! The last two are budgeted on candidates examined, like a text constraint,
//! because a matching term need not contribute a row: one that is named only
//! under `:claims` contributes nothing to `? rdf:reifies <<( … )>>`.

use std::ops::Range;

use hdtc::format::{
    TRIPLE_TERM_NAMESPACE, TripleTermSection, read_triple_term_directory,
    triple_term_component as component, triple_term_section_kind as kind,
    triple_term_section_type as section_type,
};

use crate::error::{Error, Result};
use crate::hdt::BitmapTriples;
use crate::indexed::IndexedHdt;
use crate::map::{BitmapSpec, Mapping, PackedArray, PackedSpec};
use crate::pattern::{IdPattern, Permutation, Selection};
use crate::rank::{RankedBitmap, RankedSpec};
use crate::{IdTriple, Role, TermId};

/// The namespace every triple-term IRI is minted under. Reserved whole: hdtc
/// refuses it in source RDF, so a dictionary term under it is a triple term.
pub const NAMESPACE: &str = TRIPLE_TERM_NAMESPACE;

/// Superblock and subblock widths every layer directory uses: the
/// permutation index's geometry, which the triple-term index adopts
/// (`triple-terms-format.md` §6) and hdtc checks on read.
const SUPERBLOCK_BITS: u32 = 4096;
const SUBBLOCK_BITS: u32 = 512;

/// Which HDT holds a proposition, and so which dictionary its identifiers
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// `data.hdt`: the triple term is asserted.
    Data,
    /// The propositions HDT: the triple term is not asserted.
    Propositions,
}

/// A triple term's content: the triple it denotes, in the identifier spaces of
/// the HDT that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proposition {
    /// Where the identifiers live.
    pub source: Source,
    /// Subject, predicate and object identifiers in that HDT. An object may
    /// itself be a triple-term IRI of that HDT, decoded through
    /// [`TripleTerms::decode_in`].
    pub triple: IdTriple,
}

#[derive(Debug, Clone, Copy)]
struct RunSpec {
    /// First object identifier of the run; meaningful only when the run is
    /// non-empty.
    start: u64,
    len: u64,
    subject: PackedSpec,
    predicate: PackedSpec,
    object: PackedSpec,
}

impl RunSpec {
    fn range(&self) -> Range<u64> {
        self.start..self.start + self.len
    }
}

#[derive(Debug, Clone, Copy)]
struct LayerSpec {
    /// The bitmap the rank directory indexes: the layer itself, or a sparse
    /// layer's Elias–Fano high bits (`triple-terms-format.md` §8.1).
    bitmap: RankedSpec,
    /// A sparse layer's low bits.
    low: Option<PackedSpec>,
    /// Positions covered: the HDT's triples.
    positions: u64,
    reverse: PackedSpec,
}

/// The propositions HDT, with the layers over its three permutations.
#[derive(Debug)]
struct Propositions {
    hdt: IndexedHdt,
    layers: [LayerSpec; 3],
}

/// The triple-term artifacts of one bundle, mapped and bound.
#[derive(Debug)]
pub struct TripleTerms {
    index: Mapping,
    data_run: RunSpec,
    props_run: RunSpec,
    data_layers: [LayerSpec; 3],
    propositions: Option<Propositions>,
}

/// One layer, projected: which positions of one permutation of one HDT are
/// propositions named in `data.hdt`, and the object identifier naming each.
///
/// Stored dense or sparse as its counts chose (`triple-terms-format.md`
/// §8.1); every query answers the same either way. Sparse, the bitmap is the
/// Elias–Fano high bits: a mark's place there is its bucket plus the marks
/// before it, and each bucket is closed by a clear bit.
#[derive(Debug, Clone, Copy)]
pub struct Layer<'a> {
    bitmap: RankedBitmap<'a>,
    low: Option<PackedArray<'a>>,
    positions: u64,
    reverse: PackedArray<'a>,
}

impl Layer<'_> {
    /// Marked positions strictly before `position`.
    ///
    /// Panics if `position` exceeds the positions covered.
    pub fn rank(&self, position: u64) -> u64 {
        match &self.low {
            None => self.bitmap.rank1(position),
            Some(low) => self.locate(low, position).0,
        }
    }

    /// The rank of `position` if it is marked: [`contains`](Self::contains)
    /// and [`rank`](Self::rank) at once, as a probe needs them.
    pub fn rank_if_marked(&self, position: u64) -> Option<u64> {
        match &self.low {
            None => self
                .bitmap
                .bitmap()
                .get(position)
                .then(|| self.bitmap.rank1(position)),
            Some(low) => {
                let (rank, marked) = self.locate(low, position);
                marked.then_some(rank)
            }
        }
    }

    /// For a sparse layer: the marks before `position`, and whether it is one.
    ///
    /// One select in the high bits finds where `position`'s bucket begins;
    /// its marks are the set bits from there to the next clear bit, and a
    /// binary search of their ascending low bits places `position` among
    /// them.
    fn locate(&self, low: &PackedArray<'_>, position: u64) -> (u64, bool) {
        assert!(
            position <= self.positions,
            "rank({position}) out of range for {} positions",
            self.positions
        );
        if position == self.positions {
            return (self.reverse.len(), false);
        }
        let bits = low.width();
        let bucket = position >> bits;
        let first = match bucket {
            0 => 0,
            _ => self.bitmap.select0(bucket - 1) + 1,
        };
        // Scans start on a byte: skip the clear bits of that byte before
        // `first`.
        let high = self.bitmap.bitmap();
        let byte = first & !7;
        let skipped = (first - byte) - high.count_ones_in(byte..first);
        let close = high
            .select_zero_from(byte, skipped)
            .expect("every bucket of a sparse layer is closed by a clear bit");
        let (start, end) = (first - bucket, close - bucket);
        let target = position & ((1u64 << bits) - 1);
        let (mut lo, mut hi) = (start, end);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if low.get(mid) < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo, lo < end && low.get(lo) == target)
    }

    /// Marked positions in `range`: the matching triple terms, without
    /// enumerating them.
    pub fn count_in(&self, range: &Range<u64>) -> u64 {
        self.rank(range.end) - self.rank(range.start)
    }

    /// The position of the `rank`-th marked triple.
    pub fn select(&self, rank: u64) -> u64 {
        let place = self.bitmap.select1(rank);
        match &self.low {
            Some(low) => (place - rank) << low.width() | low.get(rank),
            None => place,
        }
    }

    /// Whether the layer is stored as Elias–Fano high and low bits.
    pub fn is_sparse(&self) -> bool {
        self.low.is_some()
    }

    /// The `data.hdt` object identifier of the triple-term IRI naming the
    /// `rank`-th marked triple.
    pub fn term(&self, rank: u64) -> u64 {
        self.reverse.get(rank)
    }

    /// Whether the triple at `position` is marked.
    pub fn contains(&self, position: u64) -> bool {
        match &self.low {
            None => self.bitmap.bitmap().get(position),
            Some(low) => self.locate(low, position).1,
        }
    }
}

impl TripleTerms {
    /// Map and bind the triple-term index beside `data`, and the propositions
    /// HDT when the index names one.
    ///
    /// Bound the way the permutation index is: by counts and by the identity
    /// digest each sidecar records. The index records the digest of each HDT it
    /// describes, and each HDT's permutation index records the same digest of
    /// the same bytes, so comparing the two is a cryptographic binding at the
    /// price of reading two headers — transitively through the permutation
    /// index, whose own binding publication verifies.
    ///
    /// Also checks each run against its dictionary: the run must be exactly
    /// the dictionary's terms under [`NAMESPACE`], and none of them may be a
    /// subject or a predicate. Two prefix searches per role.
    pub(crate) fn open(
        data: &IndexedHdt,
        index: Mapping,
        propositions: Option<(Mapping, Mapping)>,
    ) -> Result<Self> {
        let (header, sections) = read_triple_term_directory(index.path()).map_err(|error| {
            Error::Format(error.context(format!(
                "opening triple-term index {}",
                index.path().display()
            )))
        })?;
        bind(&index, data, &header.data, "data")?;
        let data_run = run_spec(&index, &sections, component::DATA_RUN, &header.data)?;
        check_run(&index, data, &data_run)?;
        let data_layers = layer_specs(
            &index,
            &sections,
            [
                component::DATA_SPO,
                component::DATA_POS,
                component::DATA_OPS,
            ],
            header.data.triples,
        )?;

        let props_run = run_spec(&index, &sections, component::PROPS_RUN, &header.props)?;
        let propositions = match (header.has_propositions(), propositions) {
            (true, Some((hdt, perm))) => {
                let hdt = IndexedHdt::open(hdt, perm)?;
                bind(&index, &hdt, &header.props, "propositions")?;
                check_run(&index, &hdt, &props_run)?;
                let layers = layer_specs(
                    &index,
                    &sections,
                    [
                        component::PROPS_SPO,
                        component::PROPS_POS,
                        component::PROPS_OPS,
                    ],
                    header.props.triples,
                )?;
                Some(Propositions { hdt, layers })
            }
            (false, None) => None,
            (true, None) => {
                return Err(Error::MissingRequiredArtifact {
                    bundle: bundle_dir(&index),
                    artifact: file_name(&hdtc::format::propositions_hdt_path(data.path())),
                    remedy: format!("hdtc create, rebuilding {}", data.path().display()),
                });
            }
            (false, Some((hdt, _))) => {
                return Err(Error::Malformed {
                    artifact: hdt.path().to_path_buf(),
                    detail: format!(
                        "a propositions HDT is present, but {} says the bundle has none",
                        index.path().display()
                    ),
                });
            }
        };
        check_layers(
            &index,
            &data_run,
            &data_layers,
            propositions.as_ref().map(|p| &p.layers),
        )?;
        check_run_widths(
            &index,
            [&data_run, &props_run],
            [Some(data), propositions.as_ref().map(|p| &p.hdt)],
        )?;
        Ok(Self {
            index,
            data_run,
            props_run,
            data_layers,
            propositions,
        })
    }

    /// The object identifiers of the triple-term IRIs in one HDT: always one
    /// contiguous range, because they share a prefix and a dictionary section
    /// is sorted.
    pub fn run(&self, source: Source) -> Range<u64> {
        match source {
            Source::Data => self.data_run.range(),
            Source::Propositions => self.props_run.range(),
        }
    }

    /// Whether `object` in `data.hdt` is a triple term.
    pub fn is_triple_term(&self, object: u64) -> bool {
        self.data_run.range().contains(&object)
    }

    /// The proposition of `data.hdt` object `object`, or `None` if it is not a
    /// triple term. Three packed reads.
    pub fn decode(&self, object: u64) -> Option<Proposition> {
        self.decode_in(Source::Data, object)
    }

    /// The proposition of object `object` of the HDT `source` names, for a
    /// triple term nested inside another, whose IRI is an object of the HDT
    /// holding the outer proposition.
    pub fn decode_in(&self, source: Source, object: u64) -> Option<Proposition> {
        let run = match source {
            Source::Data => &self.data_run,
            Source::Propositions => &self.props_run,
        };
        if !run.range().contains(&object) {
            return None;
        }
        let at = object - run.start;
        let subject = run.subject.view(&self.index).get(at);
        Some(Proposition {
            // The low bit of the subject entry says which HDT holds it
            // (`triple-terms-format.md` §7).
            source: if subject & 1 == 0 {
                Source::Data
            } else {
                Source::Propositions
            },
            triple: IdTriple {
                subject: subject >> 1,
                predicate: run.predicate.view(&self.index).get(at),
                object: run.object.view(&self.index).get(at),
            },
        })
    }

    /// One layer of one HDT, or `None` for the propositions HDT of a bundle
    /// that has none.
    pub fn layer(&self, source: Source, permutation: Permutation) -> Option<Layer<'_>> {
        let layers = match source {
            Source::Data => &self.data_layers,
            Source::Propositions => &self.propositions.as_ref()?.layers,
        };
        let spec = layers[permutation_index(permutation)];
        Some(Layer {
            bitmap: spec.bitmap.view(&self.index, &self.index),
            low: spec.low.map(|low| low.view(&self.index)),
            positions: spec.positions,
            reverse: spec.reverse.view(&self.index),
        })
    }

    /// The propositions HDT's dictionary, if the bundle has one.
    pub fn propositions_dict(&self) -> Option<crate::dict::Dictionary<'_>> {
        Some(self.propositions.as_ref()?.hdt.dict())
    }

    /// SHA-256 identity of the propositions HDT, which scopes the published
    /// names of blank nodes that occur only there.
    pub fn propositions_identity_digest(&self) -> Option<[u8; 32]> {
        Some(
            self.propositions
                .as_ref()?
                .hdt
                .permutations()
                .hdt_identity_digest(),
        )
    }

    /// Resolve a pattern inside triple terms against the HDTs that hold
    /// propositions. `data` must be the HDT this index was opened against.
    ///
    /// Enumerates nothing. Costs `O(log N)` per HDT, plus one binary search per
    /// predicate group of the outer subject when one is bound; the plan it
    /// picks is a function of the bundle and the pattern alone.
    pub(crate) fn select<'a>(
        &'a self,
        data: &'a IndexedHdt,
        pattern: TripleTermPattern,
    ) -> Result<TripleTermSelection<'a>> {
        let unconstrained = pattern.data.is_some_and(|inner| inner.is_unconstrained())
            && (self.propositions.is_none()
                || pattern
                    .propositions
                    .is_some_and(|inner| inner.is_unconstrained()));
        let run = self.data_run.range();
        let perms = data.permutations();

        if unconstrained && pattern.subject.is_none() {
            let range = match pattern.predicate {
                // The triple-term IRIs are one run of the object identifiers,
                // so their OPS groups are one range.
                None if run.is_empty() => 0..0,
                None => {
                    let ops = perms.ops();
                    ops.z_span(ops.level2_range(run.start).start..ops.level2_range(run.end - 1).end)
                }
                // And within a predicate's POS group, objects are sorted.
                Some(predicate) => {
                    let pos = perms.pos();
                    let y = pos.level2_range(predicate);
                    pos.z_span(
                        pos.level2_lower_bound(y.clone(), run.start)
                            ..pos.level2_lower_bound(y, run.end),
                    )
                }
            };
            let (permutation, triples) = match pattern.predicate {
                None => (Permutation::Ops, perms.ops()),
                Some(_) => (Permutation::Pos, perms.pos()),
            };
            return Ok(TripleTermSelection::Range(crate::pattern::contiguous(
                permutation,
                triples,
                range,
            )));
        }

        // Each HDT that holds propositions, with its candidates.
        let mut sources = Vec::with_capacity(2);
        let mut inner_candidates = 0u64;
        for (source, inner) in [
            (Source::Data, pattern.data),
            (Source::Propositions, pattern.propositions),
        ] {
            let (Some(inner), Some(hdt)) = (inner, self.hdt(data, source)) else {
                continue;
            };
            let Some(source) = InnerSource::new(self, source, hdt, inner)? else {
                continue;
            };
            inner_candidates += source.count;
            sources.push(source);
        }

        if let Some(subject) = pattern.subject {
            let groups = subject_groups(perms.spo(), subject, pattern.predicate, &run);
            let candidates: u64 = groups.iter().map(range_len).sum();
            if unconstrained || candidates <= inner_candidates {
                return Ok(TripleTermSelection::Budgeted(BudgetedSelection {
                    plan: Budgeted::OuterFirst(OuterFirst {
                        terms: self,
                        spo: perms.spo(),
                        groups,
                        candidates,
                        inner: if unconstrained {
                            None
                        } else {
                            Some([pattern.data, pattern.propositions])
                        },
                    }),
                }));
            }
        }

        Ok(TripleTermSelection::Budgeted(BudgetedSelection {
            plan: Budgeted::InnerFirst(InnerFirst {
                ops: perms.ops(),
                subject: pattern.subject,
                predicate: pattern.predicate,
                sources,
                candidates: inner_candidates,
            }),
        }))
    }

    fn hdt<'a>(&'a self, data: &'a IndexedHdt, source: Source) -> Option<&'a IndexedHdt> {
        match source {
            Source::Data => Some(data),
            Source::Propositions => self.propositions.as_ref().map(|p| &p.hdt),
        }
    }
}

/// A pattern inside triple terms: `S P <<( s p o )>>`, with the outer
/// subject and predicate in `data.hdt`'s identifier spaces and the inner
/// pattern resolved separately in each HDT that holds propositions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TripleTermPattern {
    /// Outer subject, or `None` for a variable.
    pub subject: Option<u64>,
    /// Outer predicate, or `None` for a variable.
    pub predicate: Option<u64>,
    /// The inner pattern in `data.hdt`'s identifiers; `None` when a bound
    /// inner term does not occur there, so no asserted triple term matches.
    pub data: Option<InnerPattern>,
    /// The same in the propositions HDT's identifiers; `None` when a bound
    /// inner term does not occur there or the bundle has no propositions HDT.
    pub propositions: Option<InnerPattern>,
}

/// The inner positions of a triple-term pattern, as identifiers of one HDT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InnerPattern {
    /// Subject, or `None` for a variable.
    pub subject: Option<u64>,
    /// Predicate, or `None` for a variable.
    pub predicate: Option<u64>,
    /// Object, or `None` for a variable. A nested triple term is its IRI.
    pub object: Option<u64>,
}

impl InnerPattern {
    fn is_unconstrained(&self) -> bool {
        self.subject.is_none() && self.predicate.is_none() && self.object.is_none()
    }

    fn matches(&self, triple: IdTriple) -> bool {
        self.subject.is_none_or(|s| s == triple.subject)
            && self.predicate.is_none_or(|p| p == triple.predicate)
            && self.object.is_none_or(|o| o == triple.object)
    }
}

/// Where a triple-term enumeration stands: the position of a candidate, and
/// how many of that candidate's rows a page already delivered.
///
/// What `term` counts depends on the plan, which the bundle fixes: the index
/// of a candidate triple term among all of them (inner first), or an SPO
/// position (outer subject first). The plan an enumeration resumes on is the
/// plan that issued its position, because both are functions of the same
/// immutable bundle and the same request. The default is the start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TripleTermPosition {
    /// The candidate.
    pub term: u64,
    /// Rows of that candidate already delivered.
    pub row: u64,
}

/// How a [`TripleTermSelection`] will be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripleTermPlan {
    /// One contiguous range of a permutation: an ordinary [`Selection`].
    Range,
    /// Candidate triple terms from each layer, then their OPS groups.
    InnerFirst,
    /// The outer subject's triple-term objects, each decoded and tested.
    OuterFirst,
}

/// A resolved pattern inside triple terms.
#[derive(Debug)]
pub enum TripleTermSelection<'a> {
    /// Any triple term, without an outer subject: an ordinary selection, paged
    /// and counted like any other.
    Range(Selection<'a>),
    /// A plan whose work is budgeted on candidates examined.
    Budgeted(BudgetedSelection<'a>),
}

impl TripleTermSelection<'_> {
    /// The plan.
    pub fn plan(&self) -> TripleTermPlan {
        match self {
            Self::Range(_) => TripleTermPlan::Range,
            Self::Budgeted(budgeted) => budgeted.plan(),
        }
    }
}

/// A pattern inside triple terms whose enumeration examines candidates: the
/// inner pattern's matching triple terms, or the outer subject's triple-term
/// objects. A candidate need not contribute a row, so the work of a page is
/// bounded by a candidate budget rather than by its rows.
#[derive(Debug)]
pub struct BudgetedSelection<'a> {
    plan: Budgeted<'a>,
}

#[derive(Debug)]
enum Budgeted<'a> {
    InnerFirst(InnerFirst<'a>),
    OuterFirst(OuterFirst<'a>),
}

#[derive(Debug)]
struct InnerFirst<'a> {
    ops: BitmapTriples<'a>,
    subject: Option<u64>,
    predicate: Option<u64>,
    sources: Vec<InnerSource<'a>>,
    candidates: u64,
}

/// One HDT's candidate triple terms for an inner pattern.
#[derive(Debug)]
struct InnerSource<'a> {
    candidates: Candidates<'a>,
    count: u64,
    layer: Layer<'a>,
}

// Unboxed although the variants differ in size: a selection holds at most two
// of these, one per HDT, so the padding is a few hundred bytes per request.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Candidates<'a> {
    /// The marked positions of the pattern's range in its permutation:
    /// candidate `i` is the layer's `first + i`-th marked position. Every
    /// candidate matches.
    Marked { first: u64 },
    /// `s ? o`, which no permutation makes contiguous: candidate `i` probes
    /// the subject's `i`-th predicate group for the object, and matches when
    /// the triple found there is marked. At most one candidate per
    /// predicate of the subject.
    Probed {
        spo: BitmapTriples<'a>,
        groups: Range<u64>,
        object: u64,
    },
}

impl<'a> InnerSource<'a> {
    fn new(
        terms: &'a TripleTerms,
        source: Source,
        hdt: &'a IndexedHdt,
        inner: InnerPattern,
    ) -> Result<Option<Self>> {
        let spo = hdt.permutations().spo();
        if let (Some(subject), None, Some(object)) = (inner.subject, inner.predicate, inner.object)
        {
            let Some(layer) = terms.layer(source, Permutation::Spo) else {
                return Ok(None);
            };
            hdt.resolve(IdPattern {
                subject: Some(subject),
                predicate: None,
                object: Some(object),
            })?; // validates both ids
            let groups = spo.level2_range(subject);
            return Ok(Some(Self {
                count: range_len(&groups),
                candidates: Candidates::Probed {
                    spo,
                    groups,
                    object,
                },
                layer,
            }));
        }
        let ids = IdPattern {
            subject: inner.subject,
            predicate: inner.predicate,
            object: inner.object,
        };
        let (permutation, range) = if inner.is_unconstrained() {
            (Permutation::Spo, 0..hdt.triples())
        } else {
            let selection = hdt.resolve(ids)?;
            let range = selection
                .contiguous_range()
                .expect("every pattern but `s ? o` is contiguous");
            (selection.permutation(), range)
        };
        let Some(layer) = terms.layer(source, permutation) else {
            return Ok(None);
        };
        Ok(Some(Self {
            count: layer.count_in(&range),
            candidates: Candidates::Marked {
                first: layer.rank(range.start),
            },
            layer,
        }))
    }

    /// The `data.hdt` object identifier of candidate `i`, or `None` if it
    /// does not match. `O(log N)`.
    fn term(&self, i: u64) -> Option<u64> {
        match &self.candidates {
            Candidates::Marked { first } => Some(self.layer.term(first + i)),
            Candidates::Probed {
                spo,
                groups,
                object,
            } => {
                let z = spo.find_level3(spo.level3_range(groups.start + i), *object)?;
                self.layer
                    .rank_if_marked(z)
                    .map(|rank| self.layer.term(rank))
            }
        }
    }
}

#[derive(Debug)]
struct OuterFirst<'a> {
    terms: &'a TripleTerms,
    spo: BitmapTriples<'a>,
    /// The SPO ranges, one per predicate group of the subject, whose objects
    /// are triple terms; ascending.
    groups: Vec<Range<u64>>,
    candidates: u64,
    /// The inner pattern per source, or `None` when any triple term matches.
    inner: Option<[Option<InnerPattern>; 2]>,
}

/// One step of a budgeted triple-term enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripleTermStep {
    /// A matching triple, and the position that resumes an enumeration at it.
    Row {
        /// The triple of `data.hdt`.
        triple: IdTriple,
        /// Where a page that stops before this row resumes.
        at: TripleTermPosition,
    },
    /// The candidate budget ran out here; nothing at or after this position has
    /// been examined.
    Spent(TripleTermPosition),
}

/// Rows found so far by a budgeted count, and where it stopped if it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TripleTermCount {
    /// Rows counted from the start position.
    pub counted: u64,
    /// The first unexamined candidate, when the budget ran out.
    pub next: Option<TripleTermPosition>,
}

impl BudgetedSelection<'_> {
    /// The plan.
    pub fn plan(&self) -> TripleTermPlan {
        match self.plan {
            Budgeted::InnerFirst(_) => TripleTermPlan::InnerFirst,
            Budgeted::OuterFirst(_) => TripleTermPlan::OuterFirst,
        }
    }

    /// Candidates the whole enumeration examines.
    pub fn candidates(&self) -> u64 {
        match &self.plan {
            Budgeted::InnerFirst(plan) => plan.candidates,
            Budgeted::OuterFirst(plan) => plan.candidates,
        }
    }

    /// The exact row count when it is arithmetic: the outer subject's
    /// triple-term objects, when nothing constrains the inner pattern.
    pub fn exact_count(&self) -> Option<u64> {
        match &self.plan {
            Budgeted::OuterFirst(plan) if plan.inner.is_none() => Some(plan.candidates),
            _ => None,
        }
    }

    /// Enumerate from `from`, examining at most `budget` candidates.
    ///
    /// Yields rows in the plan's order and, if the budget runs out first, one
    /// final [`TripleTermStep::Spent`] naming the first unexamined candidate —
    /// `from` itself, rows included, when the budget is zero.
    ///
    /// Fails with [`Error::ResumePositionOutOfRange`] when `from` names no
    /// candidate of this enumeration, or no row of the candidate it names.
    pub fn steps(
        &self,
        from: TripleTermPosition,
        budget: u64,
    ) -> Result<Box<dyn Iterator<Item = TripleTermStep> + '_>> {
        let steps = match &self.plan {
            Budgeted::InnerFirst(plan) => plan.steps(from, budget)?,
            Budgeted::OuterFirst(plan) => plan.steps(from, budget)?,
        };
        if budget == 0 {
            return Ok(Box::new(std::iter::once(TripleTermStep::Spent(from))));
        }
        Ok(steps)
    }

    /// Count rows from `from`, examining at most `budget` candidates.
    pub fn count(&self, from: TripleTermPosition, budget: u64) -> Result<TripleTermCount> {
        match &self.plan {
            Budgeted::InnerFirst(plan) => {
                plan.check(from)?;
                if budget == 0 {
                    return Ok(TripleTermCount {
                        counted: 0,
                        next: Some(from),
                    });
                }
                plan.count(from, budget)
            }
            Budgeted::OuterFirst(_) => {
                let mut counted = 0;
                for step in self.steps(from, budget)? {
                    match step {
                        TripleTermStep::Row { .. } => counted += 1,
                        TripleTermStep::Spent(next) => {
                            return Ok(TripleTermCount {
                                counted,
                                next: Some(next),
                            });
                        }
                    }
                }
                Ok(TripleTermCount {
                    counted,
                    next: None,
                })
            }
        }
    }
}

impl InnerFirst<'_> {
    /// Candidate `index`'s `data.hdt` object identifier, or `None` when it
    /// does not match.
    fn term(&self, index: u64) -> Option<u64> {
        let mut index = index;
        for source in &self.sources {
            if index < source.count {
                return source.term(index);
            }
            index -= source.count;
        }
        unreachable!("a candidate index within the candidates")
    }

    /// The rows of triple term `term` in `data.hdt` under the outer pattern:
    /// OPS ranges in ascending order, one per predicate group (or a single
    /// position per group when the outer subject is bound). At most one range
    /// per distinct predicate, so this is bounded by the predicate count
    /// however many rows the term has.
    fn rows(&self, term: u64) -> Vec<Range<u64>> {
        let y = self.ops.level2_range(term);
        let groups: Vec<Range<u64>> = match self.predicate {
            Some(predicate) => self
                .ops
                .find_level2(y, predicate)
                .map(|y| self.ops.level3_range(y))
                .into_iter()
                .collect(),
            None => y.map(|y| self.ops.level3_range(y)).collect(),
        };
        match self.subject {
            None => groups,
            Some(subject) => groups
                .into_iter()
                .filter_map(|z| self.ops.find_level3(z, subject))
                .map(|z| z..z + 1)
                .collect(),
        }
    }

    /// The number of those rows, without listing them when no outer subject
    /// is bound.
    fn row_count(&self, term: u64) -> u64 {
        if self.subject.is_some() {
            return self.rows(term).len() as u64;
        }
        let y = self.ops.level2_range(term);
        match self.predicate {
            Some(predicate) => self
                .ops
                .find_level2(y, predicate)
                .map_or(0, |y| range_len(&self.ops.level3_range(y))),
            None => range_len(&self.ops.z_span(y)),
        }
    }

    fn check(&self, from: TripleTermPosition) -> Result<()> {
        if from == TripleTermPosition::default() {
            return Ok(());
        }
        if from.term >= self.candidates {
            return Err(Error::ResumePositionOutOfRange {
                position: from.term,
                length: self.candidates,
            });
        }
        let rows = self.term(from.term).map_or(0, |term| self.row_count(term));
        if from.row > 0 && from.row >= rows {
            return Err(Error::ResumePositionOutOfRange {
                position: from.row,
                length: rows,
            });
        }
        Ok(())
    }

    fn steps(
        &self,
        from: TripleTermPosition,
        budget: u64,
    ) -> Result<Box<dyn Iterator<Item = TripleTermStep> + '_>> {
        self.check(from)?;
        let mut index = from.term;
        let mut skip = from.row;
        let mut examined = 0u64;
        // The current term's remaining OPS positions, its identifier, its
        // candidate index, and the row number of the next one.
        let mut pending: Box<dyn Iterator<Item = u64>> = Box::new(std::iter::empty());
        let (mut term_id, mut term_index, mut row) = (0u64, 0u64, 0u64);
        let mut done = false;
        Ok(Box::new(std::iter::from_fn(move || {
            loop {
                if done {
                    return None;
                }
                if let Some(z) = pending.next() {
                    let at = TripleTermPosition {
                        term: term_index,
                        row,
                    };
                    row += 1;
                    return Some(TripleTermStep::Row {
                        triple: IdTriple {
                            subject: self.ops.level3_at(z),
                            predicate: self.ops.level2_at(self.ops.level2_of(z)),
                            object: term_id,
                        },
                        at,
                    });
                }
                if index >= self.candidates {
                    done = true;
                    return None;
                }
                if examined >= budget {
                    done = true;
                    return Some(TripleTermStep::Spent(TripleTermPosition {
                        term: index,
                        row: 0,
                    }));
                }
                examined += 1;
                term_index = index;
                row = std::mem::take(&mut skip);
                index += 1;
                if let Some(term) = self.term(term_index) {
                    term_id = term;
                    pending = Box::new(skip_rows(self.rows(term), row));
                }
            }
        })))
    }

    fn count(&self, from: TripleTermPosition, budget: u64) -> Result<TripleTermCount> {
        let mut counted = 0;
        let mut skip = from.row;
        for (examined, index) in (from.term..self.candidates).enumerate() {
            if examined as u64 >= budget {
                return Ok(TripleTermCount {
                    counted,
                    next: Some(TripleTermPosition {
                        term: index,
                        row: 0,
                    }),
                });
            }
            if let Some(term) = self.term(index) {
                counted += self
                    .row_count(term)
                    .saturating_sub(std::mem::take(&mut skip));
            }
            skip = 0;
        }
        Ok(TripleTermCount {
            counted,
            next: None,
        })
    }
}

impl OuterFirst<'_> {
    fn check(&self, from: TripleTermPosition) -> Result<()> {
        if from == TripleTermPosition::default() {
            return Ok(());
        }
        if from.row != 0 || !self.groups.iter().any(|group| group.contains(&from.term)) {
            return Err(Error::ResumePositionOutOfRange {
                position: from.term,
                length: self.groups.last().map_or(0, |group| group.end),
            });
        }
        Ok(())
    }

    fn matches(&self, object: u64) -> bool {
        let Some(inner) = &self.inner else {
            return true;
        };
        let Some(proposition) = self.terms.decode(object) else {
            return false;
        };
        let pattern = match proposition.source {
            Source::Data => inner[0],
            Source::Propositions => inner[1],
        };
        pattern.is_some_and(|pattern| pattern.matches(proposition.triple))
    }

    fn steps(
        &self,
        from: TripleTermPosition,
        budget: u64,
    ) -> Result<Box<dyn Iterator<Item = TripleTermStep> + '_>> {
        self.check(from)?;
        // Straight to the group holding `from`: resuming costs a pass over the
        // groups, never over the positions before it.
        let start = from.term;
        let mut positions = self
            .groups
            .iter()
            .filter(move |group| group.end > start)
            .flat_map(move |group| group.start.max(start)..group.end);
        let mut examined = 0u64;
        let mut done = false;
        Ok(Box::new(std::iter::from_fn(move || {
            loop {
                if done {
                    return None;
                }
                let z = positions.next()?;
                if examined >= budget {
                    done = true;
                    return Some(TripleTermStep::Spent(TripleTermPosition {
                        term: z,
                        row: 0,
                    }));
                }
                examined += 1;
                let object = self.spo.level3_at(z);
                if self.matches(object) {
                    let y = self.spo.level2_of(z);
                    return Some(TripleTermStep::Row {
                        triple: IdTriple {
                            subject: self.spo.level1_of(y),
                            predicate: self.spo.level2_at(y),
                            object,
                        },
                        at: TripleTermPosition { term: z, row: 0 },
                    });
                }
            }
        })))
    }
}

/// The positions of `ranges`, in order, after the first `skip`.
fn skip_rows(ranges: Vec<Range<u64>>, mut skip: u64) -> impl Iterator<Item = u64> {
    ranges.into_iter().flat_map(move |range| {
        let taken = skip.min(range_len(&range));
        skip -= taken;
        range.start + taken..range.end
    })
}

/// The SPO ranges of `subject`'s triples (under `predicate`, if bound) whose
/// object lies in `run`: one per predicate group, because objects are sorted
/// within a predicate group and the run is a contiguous identifier range.
fn subject_groups(
    spo: BitmapTriples<'_>,
    subject: u64,
    predicate: Option<u64>,
    run: &Range<u64>,
) -> Vec<Range<u64>> {
    if run.is_empty() {
        return Vec::new();
    }
    let y = spo.level2_range(subject);
    let ys: Vec<u64> = match predicate {
        Some(predicate) => spo.find_level2(y, predicate).into_iter().collect(),
        None => y.collect(),
    };
    ys.into_iter()
        .filter_map(|y| {
            let z = spo.level3_range(y);
            let first = spo.level3_lower_bound(z.clone(), run.start);
            let last = spo.level3_lower_bound(z, run.end);
            (first < last).then_some(first..last)
        })
        .collect()
}

fn range_len(range: &Range<u64>) -> u64 {
    range.end - range.start
}

fn permutation_index(permutation: Permutation) -> usize {
    match permutation {
        Permutation::Spo => 0,
        Permutation::Pos => 1,
        Permutation::Ops => 2,
    }
}

fn bundle_dir(index: &Mapping) -> std::path::PathBuf {
    index
        .path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Check the index's record of one HDT against that HDT.
fn bind(
    index: &Mapping,
    hdt: &IndexedHdt,
    facts: &hdtc::format::TripleTermSourceFacts,
    which: &str,
) -> Result<()> {
    let mismatch = |detail: String| Error::ArtifactBindingMismatch {
        artifact: index.path().to_path_buf(),
        hdt: hdt.path().to_path_buf(),
        detail,
    };
    if facts.digest != hdt.permutations().hdt_identity_digest() {
        return Err(mismatch(format!(
            "the {which} HDT's identity digest differs from the one its permutation index records"
        )));
    }
    let objects = hdt.dict_counts().len(Role::Object);
    if facts.triples != hdt.triples() || facts.objects != objects {
        return Err(mismatch(format!(
            "the index records {} triples and {} objects for the {which} HDT, which has {} and {}",
            facts.triples,
            facts.objects,
            hdt.triples(),
            objects
        )));
    }
    Ok(())
}

/// Check a run against its dictionary: exactly the terms under
/// [`NAMESPACE`], all of them objects only.
fn check_run(index: &Mapping, hdt: &IndexedHdt, run: &RunSpec) -> Result<()> {
    let dict = hdt.dict();
    let malformed = |detail: String| Error::Malformed {
        artifact: index.path().to_path_buf(),
        detail,
    };
    for role in [Role::Subject, Role::Predicate] {
        if dict.prefix_bounds(role, NAMESPACE.as_bytes())?.count() != 0 {
            return Err(malformed(format!(
                "{} holds a triple-term IRI in the {role:?} position",
                hdt.path().display()
            )));
        }
    }
    let bounds = dict.prefix_bounds(Role::Object, NAMESPACE.as_bytes())?;
    let found: Vec<Range<TermId>> = bounds.ranges().cloned().collect();
    let expected: Vec<Range<TermId>> = if run.len == 0 {
        Vec::new()
    } else {
        vec![TermId(run.start)..TermId(run.start + run.len)]
    };
    if found != expected {
        return Err(malformed(format!(
            "the run of triple-term IRIs it records is not the run {} holds",
            hdt.path().display()
        )));
    }
    Ok(())
}

/// Check what the layers' payloads promise without reading them: each
/// reverse array has one entry per marked position, the three orders of an
/// HDT mark the same triples, and every triple-term IRI of `data.hdt` names
/// exactly one marked triple, in one HDT or the other
/// (`triple-terms-format.md` §8). Sentinel loads only.
fn check_layers(
    index: &Mapping,
    data_run: &RunSpec,
    data: &[LayerSpec; 3],
    props: Option<&[LayerSpec; 3]>,
) -> Result<()> {
    let malformed = |detail: String| Error::Malformed {
        artifact: index.path().to_path_buf(),
        detail,
    };
    let mut marked = 0;
    for (which, layers) in [("data", Some(data)), ("propositions", props)] {
        let Some(layers) = layers else {
            continue;
        };
        let counts: Vec<u64> = layers
            .iter()
            .map(|layer| {
                let count = layer.bitmap.view(index, index).count();
                (layer.reverse.len() == count).then_some(count).ok_or(count)
            })
            .collect::<std::result::Result<_, u64>>()
            .map_err(|count| {
                malformed(format!(
                    "a {which} layer marks {count} triples but its reverse array has a \
                     different number of entries"
                ))
            })?;
        if counts.windows(2).any(|pair| pair[0] != pair[1]) {
            return Err(malformed(format!(
                "the {which} layers mark different numbers of triples in different orders"
            )));
        }
        marked += counts[0];
    }
    if marked != data_run.len {
        return Err(malformed(format!(
            "the layers mark {marked} triples for {} triple-term IRIs",
            data_run.len
        )));
    }
    Ok(())
}

/// Check the run arrays' widths, which the bound HDTs' identifier spaces fix
/// (`triple-terms-format.md` §7). A wrong width decodes every proposition
/// wrongly without failing.
fn check_run_widths(
    index: &Mapping,
    runs: [&RunSpec; 2],
    hdts: [Option<&IndexedHdt>; 2],
) -> Result<()> {
    let width = |maximum: u64| (u64::BITS - maximum.leading_zeros()) as u8;
    let largest = |role: Role| {
        hdts.iter()
            .flatten()
            .map(|hdt| hdt.dict_counts().len(role))
            .max()
            .unwrap_or(0)
    };
    let (subject, predicate, object) = (
        width(largest(Role::Subject)) + 1,
        width(largest(Role::Predicate)),
        width(largest(Role::Object)),
    );
    for run in runs {
        if run.subject.width() != subject
            || run.predicate.width() != predicate
            || run.object.width() != object
        {
            return Err(Error::Malformed {
                artifact: index.path().to_path_buf(),
                detail: "the run arrays' widths do not fit the HDTs they describe".to_owned(),
            });
        }
    }
    Ok(())
}

fn section<'s>(
    index: &Mapping,
    sections: &'s [TripleTermSection],
    component: u32,
    section_kind: u32,
) -> Result<&'s TripleTermSection> {
    let wanted = section_type(component, section_kind);
    sections
        .iter()
        .find(|section| section.section_type == wanted)
        .ok_or_else(|| Error::Malformed {
            artifact: index.path().to_path_buf(),
            detail: format!("missing triple-term index section {wanted:#06x}"),
        })
}

fn packed(index: &Mapping, section: &TripleTermSection) -> Result<PackedSpec> {
    PackedSpec::new(
        index,
        section.offset,
        section.entry_count,
        section.bits_per_entry,
    )
    .map_err(|error| Error::Malformed {
        artifact: index.path().to_path_buf(),
        detail: error.to_string(),
    })
}

fn run_spec(
    index: &Mapping,
    sections: &[TripleTermSection],
    run: u32,
    facts: &hdtc::format::TripleTermSourceFacts,
) -> Result<RunSpec> {
    if facts.run_start.checked_add(facts.run_length).is_none() {
        return Err(Error::Malformed {
            artifact: index.path().to_path_buf(),
            detail: "a run of triple-term IRIs ends past the identifier space".to_owned(),
        });
    }
    Ok(RunSpec {
        start: facts.run_start,
        len: facts.run_length,
        subject: packed(index, section(index, sections, run, kind::TARGET_SUBJECT)?)?,
        predicate: packed(
            index,
            section(index, sections, run, kind::TARGET_PREDICATE)?,
        )?,
        object: packed(index, section(index, sections, run, kind::TARGET_OBJECT)?)?,
    })
}

/// The three layers of one HDT of `positions` triples, each in the encoding
/// its counts chose. hdtc's directory check has already required that
/// encoding and the sections' shapes (`triple-terms-format.md` §6, §8.1).
fn layer_specs(
    index: &Mapping,
    sections: &[TripleTermSection],
    components: [u32; 3],
    positions: u64,
) -> Result<[LayerSpec; 3]> {
    let spec = |component: u32| -> Result<LayerSpec> {
        let present = |k: u32| {
            sections
                .iter()
                .any(|s| s.section_type == section_type(component, k))
        };
        let (layer, low) = if present(kind::LAYER) {
            (section(index, sections, component, kind::LAYER)?, None)
        } else {
            (
                section(index, sections, component, kind::LAYER_HIGH)?,
                Some(packed(
                    index,
                    section(index, sections, component, kind::LAYER_LOW)?,
                )?),
            )
        };
        let bitmap = BitmapSpec::new(index, layer.offset, layer.entry_count).map_err(|error| {
            Error::Malformed {
                artifact: index.path().to_path_buf(),
                detail: error.to_string(),
            }
        })?;
        let ranked = RankedSpec::new(
            bitmap,
            packed(
                index,
                section(index, sections, component, kind::LAYER_SUPERRANK)?,
            )?,
            packed(
                index,
                section(index, sections, component, kind::LAYER_SUBRANK)?,
            )?,
            SUPERBLOCK_BITS,
            SUBBLOCK_BITS,
        )?;
        Ok(LayerSpec {
            bitmap: ranked,
            low,
            positions,
            reverse: packed(index, section(index, sections, component, kind::REVERSE)?)?,
        })
    };
    Ok([
        spec(components[0])?,
        spec(components[1])?,
        spec(components[2])?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::dict::Dictionary;
    use crate::store::OpenOptions;
    use crate::testing::{Fixture, TINY_NT, TRIPLE_TERMS_NT, published_bundle};

    fn open(fixture: &Fixture) -> Store {
        Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .expect("open fixture")
    }

    fn text(dict: &Dictionary<'_>, role: Role, id: u64) -> String {
        let mut buf = Vec::new();
        String::from_utf8(dict.extract(role, TermId(id), &mut buf).unwrap().to_vec()).unwrap()
    }

    /// A proposition spelled out, nested triple terms included.
    fn spelled(store: &Store, proposition: Proposition) -> String {
        let terms = store.triple_terms().unwrap();
        let dict = match proposition.source {
            Source::Data => store.dict(),
            Source::Propositions => terms.propositions_dict().unwrap(),
        };
        let triple = proposition.triple;
        let object = match terms.decode_in(proposition.source, triple.object) {
            Some(nested) => spelled(store, nested),
            None => text(&dict, Role::Object, triple.object),
        };
        format!(
            "<<( {} {} {object} )>>",
            text(&dict, Role::Subject, triple.subject),
            text(&dict, Role::Predicate, triple.predicate)
        )
    }

    /// Every triple of `data.hdt` whose object is a triple term.
    fn reifying(store: &Store) -> Vec<IdTriple> {
        let terms = store.triple_terms().unwrap();
        let all = store
            .resolve(IdPattern {
                subject: None,
                predicate: None,
                object: None,
            })
            .unwrap();
        all.page(0, usize::MAX)
            .filter(|triple| terms.is_triple_term(triple.object))
            .collect()
    }

    fn all_rows(selection: &TripleTermSelection<'_>) -> Vec<IdTriple> {
        let selection = match selection {
            TripleTermSelection::Range(selection) => {
                return selection.page(0, usize::MAX).collect();
            }
            TripleTermSelection::Budgeted(selection) => selection,
        };
        selection
            .steps(TripleTermPosition::default(), u64::MAX)
            .unwrap()
            .map(|step| match step {
                TripleTermStep::Row { triple, .. } => triple,
                TripleTermStep::Spent(_) => panic!("an unlimited budget ran out"),
            })
            .collect()
    }

    /// A few triple terms among many triples, some clustered in one
    /// predicate: the data layers are stored sparse and the propositions
    /// HDT's dense, and in every order every position answers `rank`,
    /// `select` and `contains` as the marks — the targets the run decodes to,
    /// asserted for `data.hdt`, unasserted for the propositions HDT — do.
    #[test]
    fn layers_answer_as_their_marks_do_dense_or_sparse() {
        let iri = |kind: &str, n: u64| format!("<http://ex/{kind}{n}>");
        let reifies = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies>";
        let mut source = String::new();
        let mut triples = Vec::new();
        for i in 0..3_000u64 {
            let triple = (iri("s", i % 300), iri("p", i % 10), iri("o", (i * 7) % 200));
            source.push_str(&format!("{} {} {} .\n", triple.0, triple.1, triple.2));
            triples.push(triple);
        }
        let mut reified: Vec<_> = (0..40u64)
            .map(|i| triples[(i * 73 % 3_000) as usize].clone())
            .collect();
        // Every triple of one predicate: a dense run in POS.
        reified.extend(
            triples
                .iter()
                .filter(|t| t.1 == iri("p", 3))
                .take(60)
                .cloned(),
        );
        reified.extend((0..15u64).map(|i| (iri("s", i), iri("p", i % 10), iri("u", i))));
        for (i, (s, p, o)) in reified.iter().enumerate() {
            source.push_str(&format!(
                "{} {reifies} <<( {s} {p} {o} )>> .\n",
                iri("r", i as u64)
            ));
        }
        let fixture = Fixture::build(&source);
        let store = open(&fixture);
        let terms = store.triple_terms().expect("the fixture has triple terms");
        let mut marks: [std::collections::HashSet<(u64, u64, u64)>; 2] = Default::default();
        for object in terms.run(Source::Data) {
            let proposition = terms.decode(object).unwrap();
            let t = proposition.triple;
            marks[usize::from(proposition.source == Source::Propositions)].insert((
                t.subject,
                t.predicate,
                t.object,
            ));
        }
        let props = terms.propositions.as_ref().expect("unasserted ones too");
        for (source, perms, marked) in [
            (Source::Data, store.perms(), &marks[0]),
            (Source::Propositions, props.hdt.permutations(), &marks[1]),
        ] {
            for permutation in [Permutation::Spo, Permutation::Pos, Permutation::Ops] {
                let layer = terms.layer(source, permutation).unwrap();
                assert_eq!(
                    layer.is_sparse(),
                    source == Source::Data,
                    "{source:?} {permutation:?}"
                );
                let order = match permutation {
                    Permutation::Spo => perms.spo(),
                    Permutation::Pos => perms.pos(),
                    Permutation::Ops => perms.ops(),
                };
                let n = perms.triples();
                let mut rank = 0;
                for z in 0..n {
                    let y = order.level2_of(z);
                    let (a, b, c) = (order.level1_of(y), order.level2_at(y), order.level3_at(z));
                    let triple = match permutation {
                        Permutation::Spo => (a, b, c),
                        Permutation::Pos => (c, a, b),
                        Permutation::Ops => (c, b, a),
                    };
                    assert_eq!(layer.rank(z), rank, "{source:?} {permutation:?} rank({z})");
                    let is_marked = marked.contains(&triple);
                    assert_eq!(
                        layer.contains(z),
                        is_marked,
                        "{source:?} {permutation:?} {z}"
                    );
                    assert_eq!(layer.rank_if_marked(z), is_marked.then_some(rank));
                    if is_marked {
                        assert_eq!(layer.select(rank), z);
                        let named = terms.decode(layer.term(rank)).unwrap().triple;
                        assert_eq!((named.subject, named.predicate, named.object), triple);
                        rank += 1;
                    }
                }
                assert_eq!(rank, marked.len() as u64, "{source:?} {permutation:?}");
                assert_eq!(layer.rank(n), rank);
                assert_eq!(layer.count_in(&(0..n)), rank);
            }
        }
    }

    #[test]
    fn every_triple_term_decodes_to_the_proposition_it_names() {
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        let store = open(&fixture);
        let terms = store.triple_terms().expect("the fixture has triple terms");
        assert!(terms.propositions_dict().is_some(), "and unasserted ones");
        let mut decoded: Vec<String> = terms
            .run(Source::Data)
            .map(|object| spelled(&store, terms.decode(object).unwrap()))
            .collect();
        decoded.sort();
        let ex = |local: &str| format!("http://example.org/{local}");
        let mut expected = vec![
            format!("<<( {} {} {} )>>", ex("alice"), ex("knows"), ex("bob")),
            format!("<<( {} {} {} )>>", ex("bob"), ex("knows"), ex("carol")),
            format!("<<( {} {} {} )>>", ex("bob"), ex("likes"), ex("alice")),
            format!(
                "<<( {} {} <<( {} {} {} )>> )>>",
                ex("dan"),
                ex("says"),
                ex("bob"),
                ex("knows"),
                ex("erin")
            ),
            format!("<<( {} {} \"Alice\"@en--ltr )>>", ex("alice"), ex("name")),
            format!("<<( _:n {} {} )>>", ex("knows"), ex("carol")),
            format!("<<( _:m {} \"a literal\" )>>", ex("knows")),
        ];
        expected.sort();
        assert_eq!(decoded, expected);
        // The nested proposition is named only inside another, so it is in the
        // propositions HDT's run and in no data object.
        assert_eq!(terms.run(Source::Propositions).count(), 1);
        // Asserted ones decode through data.hdt.
        let asserted = terms
            .run(Source::Data)
            .filter(|&object| terms.decode(object).unwrap().source == Source::Data)
            .count();
        assert_eq!(asserted, 3);
    }

    /// Every pattern inside triple terms answers what a scan of the
    /// reifying triples answers, in every plan.
    #[test]
    fn patterns_inside_triple_terms_match_a_scan() {
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        let store = open(&fixture);
        let terms = store.triple_terms().unwrap();
        let data_dict = store.dict();
        let props_dict = terms.propositions_dict().unwrap();
        let rows = reifying(&store);

        // Inner patterns from every proposition and every subset of its
        // positions, spelled out and resolved in each HDT.
        let mut inners: Vec<[Option<String>; 3]> = vec![[None, None, None]];
        for object in terms.run(Source::Data) {
            let proposition = terms.decode(object).unwrap();
            let dict = match proposition.source {
                Source::Data => data_dict,
                Source::Propositions => props_dict,
            };
            let t = proposition.triple;
            let spelled = [
                text(&dict, Role::Subject, t.subject),
                text(&dict, Role::Predicate, t.predicate),
                text(&dict, Role::Object, t.object),
            ];
            for mask in 1..8u8 {
                let pick = |i: usize| (mask >> i & 1 == 1).then(|| spelled[i].clone());
                inners.push([pick(0), pick(1), pick(2)]);
            }
        }
        inners.sort();
        inners.dedup();
        let resolve = |dict: &Dictionary<'_>, inner: &[Option<String>; 3]| {
            let id = |role: Role, term: &Option<String>| -> Option<Option<u64>> {
                match term {
                    None => Some(None),
                    Some(term) => dict
                        .locate(role, term.as_bytes())
                        .unwrap()
                        .map(|id| Some(id.0)),
                }
            };
            Some(InnerPattern {
                subject: id(Role::Subject, &inner[0])?,
                predicate: id(Role::Predicate, &inner[1])?,
                object: id(Role::Object, &inner[2])?,
            })
        };

        let mut outers = vec![(None, None)];
        for row in &rows {
            outers.push((Some(row.subject), None));
            outers.push((None, Some(row.predicate)));
            outers.push((Some(row.subject), Some(row.predicate)));
        }
        outers.sort();
        outers.dedup();

        let mut plans = std::collections::BTreeSet::new();
        for inner in &inners {
            let pattern_data = resolve(&data_dict, inner);
            let pattern_props = resolve(&props_dict, inner);
            for &(subject, predicate) in &outers {
                let pattern = TripleTermPattern {
                    subject,
                    predicate,
                    data: pattern_data,
                    propositions: pattern_props,
                };
                let selection = store.resolve_triple_terms(pattern).unwrap().unwrap();
                plans.insert(format!("{:?}", selection.plan()));
                let mut found = all_rows(&selection);
                let mut expected: Vec<IdTriple> = rows
                    .iter()
                    .copied()
                    .filter(|row| {
                        subject.is_none_or(|s| s == row.subject)
                            && predicate.is_none_or(|p| p == row.predicate)
                            && {
                                let proposition = terms.decode(row.object).unwrap();
                                let inner = match proposition.source {
                                    Source::Data => pattern_data,
                                    Source::Propositions => pattern_props,
                                };
                                inner.is_some_and(|inner| {
                                    let t = proposition.triple;
                                    inner.subject.is_none_or(|s| s == t.subject)
                                        && inner.predicate.is_none_or(|p| p == t.predicate)
                                        && inner.object.is_none_or(|o| o == t.object)
                                })
                            }
                    })
                    .collect();
                assert_eq!(
                    found.len(),
                    {
                        let mut unique = found.clone();
                        unique.sort();
                        unique.dedup();
                        unique.len()
                    },
                    "duplicates for {inner:?} under {subject:?} {predicate:?}"
                );
                found.sort();
                expected.sort();
                assert_eq!(found, expected, "{inner:?} under {subject:?} {predicate:?}");
                match &selection {
                    TripleTermSelection::Range(range) => {
                        assert_eq!(range.count().value, expected.len() as u64);
                    }
                    TripleTermSelection::Budgeted(budgeted) => {
                        let count = budgeted
                            .count(TripleTermPosition::default(), u64::MAX)
                            .unwrap();
                        assert_eq!(count.counted, expected.len() as u64);
                        assert_eq!(count.next, None);
                        if let Some(exact) = budgeted.exact_count() {
                            assert_eq!(exact, expected.len() as u64);
                        }
                        check_resumes(budgeted, &expected);
                    }
                }
            }
        }
        // Not vacuous: every plan was exercised.
        assert_eq!(plans.len(), 3, "{plans:?}");
    }

    /// Resuming at any row, or after any budget, continues the enumeration
    /// exactly; a budget of zero spends nothing and loses nothing; and a
    /// position past the end names nothing.
    fn check_resumes(selection: &BudgetedSelection<'_>, expected: &[IdTriple]) {
        let full: Vec<(IdTriple, TripleTermPosition)> = selection
            .steps(TripleTermPosition::default(), u64::MAX)
            .unwrap()
            .map(|step| match step {
                TripleTermStep::Row { triple, at } => (triple, at),
                TripleTermStep::Spent(_) => unreachable!(),
            })
            .collect();
        let mut sorted: Vec<IdTriple> = full.iter().map(|(t, _)| *t).collect();
        sorted.sort();
        assert_eq!(sorted, expected);
        for (i, (_, at)) in full.iter().enumerate() {
            let rest: Vec<IdTriple> = selection
                .steps(*at, u64::MAX)
                .unwrap()
                .filter_map(|step| match step {
                    TripleTermStep::Row { triple, .. } => Some(triple),
                    TripleTermStep::Spent(_) => None,
                })
                .collect();
            let suffix: Vec<IdTriple> = full[i..].iter().map(|(t, _)| *t).collect();
            assert_eq!(rest, suffix, "resumed at {at:?}");
            // Nothing is examined under a zero budget, and the position,
            // row included, comes back as it went in.
            let zero: Vec<TripleTermStep> = selection.steps(*at, 0).unwrap().collect();
            assert_eq!(zero, vec![TripleTermStep::Spent(*at)]);
            assert_eq!(selection.count(*at, 0).unwrap().next, Some(*at));
        }
        for budget in 1..=3 {
            let mut collected = Vec::new();
            let mut from = TripleTermPosition::default();
            loop {
                let mut next = None;
                for step in selection.steps(from, budget).unwrap() {
                    match step {
                        TripleTermStep::Row { triple, .. } => collected.push(triple),
                        TripleTermStep::Spent(at) => next = Some(at),
                    }
                }
                match next {
                    Some(at) => from = at,
                    None => break,
                }
            }
            let in_order: Vec<IdTriple> = full.iter().map(|(t, _)| *t).collect();
            assert_eq!(collected, in_order, "budget {budget}");
            let mut counted = 0;
            let mut from = TripleTermPosition::default();
            loop {
                let count = selection.count(from, budget).unwrap();
                counted += count.counted;
                match count.next {
                    Some(at) => from = at,
                    None => break,
                }
            }
            assert_eq!(counted, expected.len() as u64, "budget {budget}");
        }
        let past = match selection.plan() {
            TripleTermPlan::InnerFirst => TripleTermPosition {
                term: selection.candidates(),
                row: 0,
            },
            _ => TripleTermPosition {
                term: u64::MAX,
                row: 0,
            },
        };
        assert!(matches!(
            selection.steps(past, 1),
            Err(Error::ResumePositionOutOfRange { .. })
        ));
        // A row past the end of the candidate it names.
        if let Some((_, at)) = full.last() {
            let forged = TripleTermPosition {
                term: at.term,
                row: at.row + 1_000,
            };
            assert!(selection.steps(forged, 1).is_err(), "{forged:?}");
        }
    }

    #[test]
    fn a_bundle_without_triple_terms_has_none_to_answer() {
        let fixture = Fixture::build(TINY_NT);
        let store = open(&fixture);
        assert!(store.triple_terms().is_none());
        assert!(
            store
                .resolve_triple_terms(TripleTermPattern::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn triple_terms_without_their_artifacts_are_refused() {
        // The index missing: the propositions beside it name nothing.
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        std::fs::remove_file(
            fixture
                .bundle_path()
                .join(crate::store::artifact::TRIPLE_TERMS),
        )
        .unwrap();
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(&error, Error::Malformed { .. }), "{error}");

        // All three missing: triple-term IRIs would be opaque strings.
        for name in [
            crate::store::artifact::PROPOSITIONS,
            crate::store::artifact::PROPOSITIONS_PERM,
        ] {
            std::fs::remove_file(fixture.bundle_path().join(name)).unwrap();
        }
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::MissingRequiredArtifact { artifact, .. } if artifact == crate::store::artifact::TRIPLE_TERMS),
            "{error}"
        );

        // The propositions HDT missing, though the index names one.
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        for name in [
            crate::store::artifact::PROPOSITIONS,
            crate::store::artifact::PROPOSITIONS_PERM,
        ] {
            std::fs::remove_file(fixture.bundle_path().join(name)).unwrap();
        }
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::MissingRequiredArtifact { artifact, .. } if artifact == crate::store::artifact::PROPOSITIONS),
            "{error}"
        );

        // Its permutation index missing.
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        std::fs::remove_file(
            fixture
                .bundle_path()
                .join(crate::store::artifact::PROPOSITIONS_PERM),
        )
        .unwrap();
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::MissingRequiredArtifact { artifact, .. } if artifact == crate::store::artifact::PROPOSITIONS_PERM),
            "{error}"
        );

        // An index from another bundle.
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        let other = Fixture::build(&TRIPLE_TERMS_NT.replace("survey", "census"));
        std::fs::copy(
            other
                .bundle_path()
                .join(crate::store::artifact::TRIPLE_TERMS),
            fixture
                .bundle_path()
                .join(crate::store::artifact::TRIPLE_TERMS),
        )
        .unwrap();
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::ArtifactBindingMismatch { .. }),
            "{error}"
        );
    }

    /// Every triple term asserted: the index names no propositions HDT, and
    /// every plan still answers.
    #[test]
    fn a_bundle_whose_triple_terms_are_all_asserted_needs_no_propositions_hdt() {
        let source = concat!(
            "<http://example.org/a> <http://example.org/p> <http://example.org/b> .\n",
            "<http://example.org/e> <http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies> ",
            "<<( <http://example.org/a> <http://example.org/p> <http://example.org/b> )>> .\n",
        );
        let fixture = Fixture::build(source);
        assert!(
            !fixture
                .bundle_path()
                .join(crate::store::artifact::PROPOSITIONS)
                .exists()
        );
        let store = open(&fixture);
        let terms = store.triple_terms().unwrap();
        assert!(terms.propositions_dict().is_none());
        assert!(
            terms
                .layer(Source::Propositions, Permutation::Spo)
                .is_none()
        );
        let rows = reifying(&store);
        assert_eq!(rows.len(), 1);
        let any = store
            .resolve_triple_terms(TripleTermPattern {
                data: Some(InnerPattern::default()),
                ..TripleTermPattern::default()
            })
            .unwrap()
            .unwrap();
        assert_eq!(any.plan(), TripleTermPlan::Range);
        assert_eq!(all_rows(&any), rows);
        let p = store
            .dict()
            .locate(Role::Predicate, b"http://example.org/p")
            .unwrap()
            .unwrap()
            .0;
        let inner = store
            .resolve_triple_terms(TripleTermPattern {
                data: Some(InnerPattern {
                    predicate: Some(p),
                    ..InnerPattern::default()
                }),
                ..TripleTermPattern::default()
            })
            .unwrap()
            .unwrap();
        assert_eq!(inner.plan(), TripleTermPlan::InnerFirst);
        assert_eq!(all_rows(&inner), rows);

        // A propositions HDT the index does not name is refused.
        let other = Fixture::build(TRIPLE_TERMS_NT);
        for name in [
            crate::store::artifact::PROPOSITIONS,
            crate::store::artifact::PROPOSITIONS_PERM,
        ] {
            std::fs::copy(
                other.bundle_path().join(name),
                fixture.bundle_path().join(name),
            )
            .unwrap();
        }
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(&error, Error::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_layer_whose_reverse_array_disagrees_with_its_bitmap_is_refused_at_open() {
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        let path = fixture
            .bundle_path()
            .join(crate::store::artifact::TRIPLE_TERMS);
        // Found through hdtc's own reading of the directory, so that only the
        // tampering below is this test's.
        let (_, sections) = read_triple_term_directory(&path).unwrap();
        let wanted = section_type(component::DATA_POS, kind::LAYER_SUPERRANK);
        let directory = sections
            .iter()
            .find(|section| section.section_type == wanted)
            .expect("the POS rank directory");
        assert_eq!(
            directory.bits_per_entry, 64,
            "a superblock rank is a whole little-endian word"
        );
        // Raise the population the directory's sentinel — its last entry —
        // records: the directory's shape, and every count hdtc checks from it,
        // still hold, but the layer now claims one mark its reverse array
        // lacks.
        let sentinel = (directory.offset + (directory.entry_count - 1) * 8) as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        let word: [u8; 8] = bytes[sentinel..sentinel + 8].try_into().unwrap();
        let marks = u64::from_le_bytes(word) + 1;
        bytes[sentinel..sentinel + 8].copy_from_slice(&marks.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("reverse array"),
            "refused for the reason: {error}"
        );
    }
}
