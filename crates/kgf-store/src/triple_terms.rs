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
//!   to the range.
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
    bitmap: RankedSpec,
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
#[derive(Debug, Clone, Copy)]
pub struct Layer<'a> {
    bitmap: RankedBitmap<'a>,
    reverse: PackedArray<'a>,
}

impl Layer<'_> {
    /// Marked positions strictly before `position`.
    pub fn rank(&self, position: u64) -> u64 {
        self.bitmap.rank1(position)
    }

    /// Marked positions in `range`: the matching triple terms, without
    /// enumerating them.
    pub fn count_in(&self, range: &Range<u64>) -> u64 {
        self.bitmap.rank1(range.end) - self.bitmap.rank1(range.start)
    }

    /// The position of the `rank`-th marked triple.
    pub fn select(&self, rank: u64) -> u64 {
        self.bitmap.select1(rank)
    }

    /// The `data.hdt` object identifier of the triple-term IRI naming the
    /// `rank`-th marked triple.
    pub fn term(&self, rank: u64) -> u64 {
        self.reverse.get(rank)
    }

    /// Whether the triple at `position` is marked.
    pub fn contains(&self, position: u64) -> bool {
        self.bitmap.bitmap().get(position)
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
    /// `O(log N)` per source and enumerates nothing; the plan it picks is a
    /// function of the bundle and the pattern alone.
    pub(crate) fn select<'a>(
        &'a self,
        data: &'a IndexedHdt,
        pattern: TripleTermPattern,
    ) -> Result<TripleTermSelection<'a>> {
        let outer = IdPattern {
            subject: pattern.subject,
            predicate: pattern.predicate,
            object: None,
        };
        data.resolve(outer)?; // validates the outer ids
        let unconstrained = pattern.data.is_some_and(|inner| inner.is_unconstrained())
            && (self.propositions.is_none()
                || pattern
                    .propositions
                    .is_some_and(|inner| inner.is_unconstrained()));
        let run = self.data_run.range();
        let perms = data.permutations();

        if unconstrained && pattern.subject.is_none() {
            let selection = match pattern.predicate {
                None => {
                    let ops = perms.ops();
                    let range = if run.is_empty() {
                        0..0
                    } else {
                        z_span(
                            ops,
                            ops.level2_range(run.start).start..ops.level2_range(run.end - 1).end,
                        )
                    };
                    crate::pattern::contiguous(Permutation::Ops, ops, range)
                }
                Some(predicate) => {
                    let pos = perms.pos();
                    let y = pos.level2_range(predicate);
                    let first = lower_bound_level2(pos, y.clone(), run.start);
                    let last = lower_bound_level2(pos, y, run.end);
                    let range = if first == last {
                        0..0
                    } else {
                        z_span(pos, first..last)
                    };
                    crate::pattern::contiguous(Permutation::Pos, pos, range)
                }
            };
            return Ok(TripleTermSelection {
                plan: Plan::Range(selection),
            });
        }

        // Each HDT that holds propositions, with the inner pattern's range.
        let mut sources = Vec::with_capacity(2);
        let mut inner_matches = 0u64;
        for (source, inner) in [
            (Source::Data, pattern.data),
            (Source::Propositions, pattern.propositions),
        ] {
            let (Some(inner), Some(hdt)) = (inner, self.hdt(data, source)) else {
                continue;
            };
            let layer_permutation = inner.permutation();
            let Some(layer) = self.layer(source, layer_permutation) else {
                continue;
            };
            let range = inner.range(hdt)?;
            let matches = layer.count_in(&range);
            inner_matches += matches;
            sources.push(InnerSource {
                source,
                inner,
                permutation: layer_permutation,
                range,
                matches,
                hdt,
                layer,
            });
        }

        if let Some(subject) = pattern.subject {
            let groups = subject_groups(perms.spo(), subject, pattern.predicate, &run);
            let candidates: u64 = groups.iter().map(range_len).sum();
            if unconstrained || candidates <= inner_matches {
                return Ok(TripleTermSelection {
                    plan: Plan::OuterFirst(OuterFirst {
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
                });
            }
        }

        Ok(TripleTermSelection {
            plan: Plan::InnerFirst(InnerFirst {
                ops: perms.ops(),
                subject: pattern.subject,
                predicate: pattern.predicate,
                sources,
                matches: inner_matches,
            }),
        })
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

    /// The permutation whose layer serves this pattern: the one in which its
    /// bound positions form a contiguous range, with `s ? o` reading the
    /// subject's group and probing each marked position's object.
    fn permutation(&self) -> Permutation {
        match (self.subject, self.predicate, self.object) {
            (Some(_), _, _) | (None, None, None) => Permutation::Spo,
            (None, Some(_), _) => Permutation::Pos,
            (None, None, Some(_)) => Permutation::Ops,
        }
    }

    /// The pattern's range in [`permutation`](Self::permutation).
    fn range(&self, hdt: &IndexedHdt) -> Result<Range<u64>> {
        let probe_free = IdPattern {
            subject: self.subject,
            predicate: self.predicate,
            // `s ? o` has no contiguous range; its subject's group stands in,
            // and each marked position's object is probed.
            object: if self.subject.is_some() && self.predicate.is_none() {
                None
            } else {
                self.object
            },
        };
        if probe_free.subject.is_none()
            && probe_free.predicate.is_none()
            && probe_free.object.is_none()
        {
            return Ok(0..hdt.triples());
        }
        Ok(hdt
            .resolve(probe_free)?
            .contiguous_range()
            .expect("an `s ? o`-free pattern is contiguous"))
    }

    /// Whether a marked position of the range still needs its object probed.
    fn probes_object(&self) -> Option<u64> {
        match (self.subject, self.predicate, self.object) {
            (Some(_), None, Some(object)) => Some(object),
            _ => None,
        }
    }
}

/// Where a triple-term enumeration stands: the position of a candidate, and
/// how many of that candidate's rows a page already delivered.
///
/// What `term` counts depends on the plan, which the bundle fixes: the index
/// of a matching triple term among all of them (inner first), or an SPO
/// position (outer subject first). The plan an enumeration resumes on is the
/// plan that issued its position, because both are functions of the same
/// immutable bundle and the same request.
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
    /// Matching triple terms from each layer, then their OPS groups.
    InnerFirst,
    /// The outer subject's triple-term objects, each decoded and tested.
    OuterFirst,
}

/// A resolved pattern inside triple terms.
#[derive(Debug)]
pub struct TripleTermSelection<'a> {
    plan: Plan<'a>,
}

#[derive(Debug)]
enum Plan<'a> {
    Range(Selection<'a>),
    InnerFirst(InnerFirst<'a>),
    OuterFirst(OuterFirst<'a>),
}

#[derive(Debug)]
struct InnerFirst<'a> {
    ops: BitmapTriples<'a>,
    subject: Option<u64>,
    predicate: Option<u64>,
    sources: Vec<InnerSource<'a>>,
    matches: u64,
}

#[derive(Debug)]
struct InnerSource<'a> {
    #[allow(dead_code)]
    source: Source,
    inner: InnerPattern,
    permutation: Permutation,
    range: Range<u64>,
    matches: u64,
    hdt: &'a IndexedHdt,
    layer: Layer<'a>,
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

impl<'a> TripleTermSelection<'a> {
    /// The plan.
    pub fn plan(&self) -> TripleTermPlan {
        match self.plan {
            Plan::Range(_) => TripleTermPlan::Range,
            Plan::InnerFirst(_) => TripleTermPlan::InnerFirst,
            Plan::OuterFirst(_) => TripleTermPlan::OuterFirst,
        }
    }

    /// The ordinary selection a [`TripleTermPlan::Range`] plan is.
    pub fn as_selection(&self) -> Option<&Selection<'a>> {
        match &self.plan {
            Plan::Range(selection) => Some(selection),
            _ => None,
        }
    }

    /// Candidates the whole enumeration examines: matching triple terms (inner
    /// first) or the outer subject's triple-term objects (outer first). A
    /// [`Range`](TripleTermPlan::Range) plan examines none.
    pub fn candidates(&self) -> u64 {
        match &self.plan {
            Plan::Range(_) => 0,
            Plan::InnerFirst(plan) => plan.matches,
            Plan::OuterFirst(plan) => plan.candidates,
        }
    }

    /// The exact row count when it is arithmetic: a range width, or the outer
    /// subject's triple-term objects when nothing constrains the inner
    /// pattern.
    pub fn exact_count(&self) -> Option<u64> {
        match &self.plan {
            Plan::Range(selection) => Some(selection.count().value),
            Plan::OuterFirst(plan) if plan.inner.is_none() => Some(plan.candidates),
            _ => None,
        }
    }

    /// Enumerate from `from`, examining at most `budget` candidates.
    ///
    /// Yields rows in the plan's order and, if the budget runs out first, one
    /// final [`TripleTermStep::Spent`]. A [`Range`](TripleTermPlan::Range)
    /// plan is not enumerated here; page its [`Selection`].
    ///
    /// Fails with [`Error::ResumePositionOutOfRange`] when `from` names no
    /// candidate of this enumeration, or no row of the candidate it names.
    pub fn steps(
        &self,
        from: TripleTermPosition,
        budget: u64,
    ) -> Result<Box<dyn Iterator<Item = TripleTermStep> + '_>> {
        match &self.plan {
            Plan::Range(_) => panic!("a range plan is paged as a selection"),
            Plan::InnerFirst(plan) => plan.steps(from, budget),
            Plan::OuterFirst(plan) => plan.steps(from, budget),
        }
    }

    /// Count rows from `from`, examining at most `budget` candidates.
    pub fn count(&self, from: TripleTermPosition, budget: u64) -> Result<TripleTermCount> {
        match &self.plan {
            Plan::Range(selection) => Ok(TripleTermCount {
                counted: selection.count().value,
                next: None,
            }),
            Plan::InnerFirst(plan) => plan.count(from, budget),
            Plan::OuterFirst(plan) => {
                let mut counted = 0;
                for step in plan.steps(from, budget)? {
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

impl<'a> InnerFirst<'a> {
    /// The matching triple term at enumeration index `index`: its source's
    /// position in the list and its `data.hdt` object identifier, or `None`
    /// when that marked position fails the object probe.
    fn term(&self, index: u64) -> (usize, Option<u64>) {
        let mut index = index;
        for (i, source) in self.sources.iter().enumerate() {
            if index < source.matches {
                let rank = source.layer.rank(source.range.start) + index;
                if let Some(object) = source.inner.probes_object() {
                    let position = source.layer.select(rank);
                    let spo = source.hdt.permutations().spo();
                    debug_assert_eq!(source.permutation, Permutation::Spo);
                    if spo.level3_at(position) != object {
                        return (i, None);
                    }
                }
                return (i, Some(source.layer.term(rank)));
            }
            index -= source.matches;
        }
        unreachable!("index within the matches")
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
            None => range_len(&z_span(self.ops, y)),
        }
    }

    fn check(&self, from: TripleTermPosition) -> Result<()> {
        if from == TripleTermPosition::default() {
            return Ok(());
        }
        if from.term >= self.matches {
            return Err(Error::ResumePositionOutOfRange {
                position: from.term,
                length: self.matches,
            });
        }
        let rows = match self.term(from.term).1 {
            Some(term) => self.row_count(term),
            None => 0,
        };
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
        // enumeration index, and the row number of the next one.
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
                if index >= self.matches {
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
                if let (_, Some(term)) = self.term(term_index) {
                    term_id = term;
                    pending = Box::new(skip_rows(self.rows(term), row));
                }
            }
        })))
    }

    fn count(&self, from: TripleTermPosition, budget: u64) -> Result<TripleTermCount> {
        self.check(from)?;
        let mut counted = 0;
        let mut skip = from.row;
        for (examined, index) in (from.term..self.matches).enumerate() {
            if examined as u64 >= budget {
                return Ok(TripleTermCount {
                    counted,
                    next: Some(TripleTermPosition {
                        term: index,
                        row: 0,
                    }),
                });
            }
            if let (_, Some(term)) = self.term(index) {
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
                length: self.candidates,
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
        let start = from.term;
        let mut examined = 0u64;
        let mut positions = self
            .groups
            .iter()
            .flat_map(|group| group.clone())
            .skip_while(move |&z| from != TripleTermPosition::default() && z < start);
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
            let first = lower_bound_level3(spo, z.clone(), run.start);
            let last = lower_bound_level3(spo, z, run.end);
            (first < last).then_some(first..last)
        })
        .collect()
}

/// The first position in a sorted level-2 range whose value is at least
/// `value`.
fn lower_bound_level2(triples: BitmapTriples<'_>, range: Range<u64>, value: u64) -> u64 {
    match value.checked_sub(1) {
        Some(below) => triples.level2_upper_bound(range, below),
        None => range.start,
    }
}

/// The first position in a sorted level-3 range whose value is at least
/// `value`.
fn lower_bound_level3(triples: BitmapTriples<'_>, range: Range<u64>, value: u64) -> u64 {
    let (mut low, mut high) = (range.start, range.end);
    while low < high {
        let middle = low + (high - low) / 2;
        if triples.level3_at(middle) < value {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn z_span(triples: BitmapTriples<'_>, y: Range<u64>) -> Range<u64> {
    if y.is_empty() {
        return 0..0;
    }
    triples.level3_range(y.start).start..triples.level3_range(y.end - 1).end
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

fn layer_specs(
    index: &Mapping,
    sections: &[TripleTermSection],
    components: [u32; 3],
) -> Result<[LayerSpec; 3]> {
    let spec = |component: u32| -> Result<LayerSpec> {
        let layer = section(index, sections, component, kind::LAYER)?;
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
        if let Some(selection) = selection.as_selection() {
            return selection.page(0, usize::MAX).collect();
        }
        selection
            .steps(TripleTermPosition::default(), u64::MAX)
            .unwrap()
            .map(|step| match step {
                TripleTermStep::Row { triple, .. } => triple,
                TripleTermStep::Spent(_) => panic!("an unlimited budget ran out"),
            })
            .collect()
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
                let count = selection
                    .count(TripleTermPosition::default(), u64::MAX)
                    .unwrap();
                assert_eq!(count.counted, expected.len() as u64);
                assert_eq!(count.next, None);
                if let Some(exact) = selection.exact_count() {
                    assert_eq!(exact, expected.len() as u64);
                }
            }
        }
        // Not vacuous: every plan was exercised.
        assert_eq!(plans.len(), 3, "{plans:?}");
    }

    /// Resuming at any row, or after any budget, continues the enumeration
    /// exactly.
    #[test]
    fn budgeted_enumeration_resumes_without_loss_or_duplication() {
        let fixture = Fixture::build(TRIPLE_TERMS_NT);
        let store = open(&fixture);
        let terms = store.triple_terms().unwrap();
        let knows = store
            .dict()
            .locate(Role::Predicate, b"http://example.org/knows")
            .unwrap()
            .unwrap()
            .0;
        let props_knows = terms
            .propositions_dict()
            .unwrap()
            .locate(Role::Predicate, b"http://example.org/knows")
            .unwrap()
            .map(|id| id.0);
        let pattern = TripleTermPattern {
            subject: None,
            predicate: None,
            data: Some(InnerPattern {
                predicate: Some(knows),
                ..InnerPattern::default()
            }),
            propositions: props_knows.map(|p| InnerPattern {
                predicate: Some(p),
                ..InnerPattern::default()
            }),
        };
        let selection = store.resolve_triple_terms(pattern).unwrap().unwrap();
        assert_eq!(selection.plan(), TripleTermPlan::InnerFirst);
        let full: Vec<(IdTriple, TripleTermPosition)> = selection
            .steps(TripleTermPosition::default(), u64::MAX)
            .unwrap()
            .map(|step| match step {
                TripleTermStep::Row { triple, at } => (triple, at),
                TripleTermStep::Spent(_) => unreachable!(),
            })
            .collect();
        assert!(full.len() >= 3, "{full:?}");
        for (i, (_, at)) in full.iter().enumerate() {
            let rest: Vec<IdTriple> = selection
                .steps(*at, u64::MAX)
                .unwrap()
                .filter_map(|step| match step {
                    TripleTermStep::Row { triple, .. } => Some(triple),
                    TripleTermStep::Spent(_) => None,
                })
                .collect();
            let expected: Vec<IdTriple> = full[i..].iter().map(|(t, _)| *t).collect();
            assert_eq!(rest, expected, "resumed at {at:?}");
        }
        for budget in 1..=4 {
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
            let expected: Vec<IdTriple> = full.iter().map(|(t, _)| *t).collect();
            assert_eq!(collected, expected, "budget {budget}");
            // Counting resumes the same way.
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
        // A position past the end names nothing.
        assert!(matches!(
            selection.steps(
                TripleTermPosition {
                    term: selection.candidates(),
                    row: 0
                },
                1
            ),
            Err(Error::ResumePositionOutOfRange { .. })
        ));
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
}
