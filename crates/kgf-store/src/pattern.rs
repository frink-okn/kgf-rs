//! Pattern resolution: the doc 20 §20.2 table, implemented.
//!
//! | pattern | permutation | resolution | count |
//! |---|---|---|---|
//! | `? ? ?` | SPO | full Z scan, paged | N (exact, free) |
//! | `s ? ?` | SPO | level-1 seek | range width (exact) |
//! | `s p ?` | SPO | binary search p in s's Y-group | range width (exact) |
//! | `s p o` | SPO | + binary search o in Z-range | 0/1 (exact) |
//! | `s ? o` | SPO **or** OPS | probe the smaller side | exact; costs what enumeration costs |
//! | `? p ?` | POS | level-1 seek | range width (exact) |
//! | `? p o` | POS | binary search o in p's Y-group | range width (exact) |
//! | `? ? o` | OPS | level-1 seek | range width (exact) |
//!
//! **The order of this table is the enumeration order**, and cursors are
//! positions in it (doc 20 §20.7). Changing which permutation serves a pattern
//! is a breaking change to every outstanding cursor, not an optimization.
//!
//! Every count but `s ? o` is a range width after bounded rank/select descent.
//! That is what backs `cardinality.exact = true` on plain patterns, per-binding
//! `/count` at `O(log N)`, and doc 04's invariant that top-level VoID numbers
//! equal `/count` results.

use std::ops::Range;

use crate::dict::DictCounts;
use crate::error::{Error, Result};
use crate::hdt::BitmapTriples;
use crate::perm::Permutations;
use crate::{IdTriple, Role};

/// A term identifier in a triple pattern, or `None` for a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdPattern {
    /// Subject id, or `None` for a variable.
    pub subject: Option<u64>,
    /// Predicate id, or `None` for a variable.
    pub predicate: Option<u64>,
    /// Object id, or `None` for a variable.
    pub object: Option<u64>,
}

/// Which permutation a selection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permutation {
    /// Subject-rooted, from `data.hdt`.
    Spo,
    /// Predicate-rooted, from the sidecar.
    Pos,
    /// Object-rooted, from the sidecar.
    Ops,
}

/// An exact cardinality, and how it was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    /// The number of matching triples.
    pub value: u64,
    /// Whether it came from range arithmetic rather than enumeration.
    ///
    /// Both are exact. This distinguishes the `s ? o` case, which is exact but
    /// costs what enumeration costs — acceptable only because the answer is
    /// bounded by the dataset's distinct predicate count (doc 20 §20.2.1).
    pub arithmetic: bool,
}

/// A resolved pattern: a contiguous range in one permutation, or the bounded
/// group plan `s ? o` requires.
///
/// Resolution is `O(log N)` and enumerates nothing. The selection holds mapped
/// views borrowing the bundle it was resolved against, so it cannot outlive or
/// accidentally execute against a different [`Permutations`] value.
#[derive(Debug)]
pub struct Selection<'a> {
    plan: SelectionPlan<'a>,
}

#[derive(Debug, Clone)]
enum SelectionPlan<'a> {
    Contiguous(ContiguousPlan<'a>),
    SubjectObject(SubjectObjectPlan<'a>),
}

#[derive(Debug, Clone)]
struct ContiguousPlan<'a> {
    permutation: Permutation,
    triples: BitmapTriples<'a>,
    z_range: Range<u64>,
}

#[derive(Debug, Clone)]
struct SubjectObjectPlan<'a> {
    route: SubjectObjectRoute,
    probe: SubjectObjectProbe,
    triples: BitmapTriples<'a>,
    y_range: Range<u64>,
    subject: u64,
    object: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectObjectProbe {
    Linear,
    Binary,
}

impl<'a> Selection<'a> {
    /// Which permutation this reads.
    pub fn permutation(&self) -> Permutation {
        match &self.plan {
            SelectionPlan::Contiguous(plan) => plan.permutation,
            SelectionPlan::SubjectObject(plan) => match plan.route {
                SubjectObjectRoute::ViaSubject => Permutation::Spo,
                SubjectObjectRoute::ViaObject => Permutation::Ops,
            },
        }
    }

    /// The planner route for `s ? o`, or `None` for a contiguous selection.
    pub fn subject_object_route(&self) -> Option<SubjectObjectRoute> {
        match &self.plan {
            SelectionPlan::Contiguous(_) => None,
            SelectionPlan::SubjectObject(plan) => Some(plan.route),
        }
    }

    /// Exact cardinality.
    pub fn count(&self) -> Count {
        match &self.plan {
            SelectionPlan::Contiguous(plan) => Count {
                value: plan.z_range.end - plan.z_range.start,
                arithmetic: true,
            },
            SelectionPlan::SubjectObject(plan) => Count {
                value: subject_object_count(plan),
                arithmetic: false,
            },
        }
    }

    /// Enumerate at most `limit` triples in the table's stable order.
    ///
    /// For contiguous selections, `from` is the zero-based result offset. For
    /// `s ? o`, `from` is the last predicate id returned, with zero denoting the
    /// beginning. The latter is deliberately route-independent, so a later page
    /// may choose either endpoint without changing cursor semantics (doc 20
    /// §20.2.1).
    pub fn page(&self, from: u64, limit: usize) -> impl Iterator<Item = IdTriple> + '_ {
        SelectionPage::new(&self.plan, from, limit)
    }

    /// The `i`-th triple, for `/sample`.
    ///
    /// This is constant-rank work for contiguous patterns. `s ? o` has no
    /// contiguous result range and therefore uses its bounded predicate-group
    /// probe, the same explicit exception as its exact count.
    ///
    /// # Panics
    ///
    /// Panics if `i >= count().value`.
    pub fn at(&self, i: u64) -> IdTriple {
        match &self.plan {
            SelectionPlan::Contiguous(plan) => {
                let len = plan.z_range.end - plan.z_range.start;
                assert!(
                    i < len,
                    "selection index {i} out of range for {len} triples"
                );
                materialize(plan.permutation, plan.triples, plan.z_range.start + i)
            }
            SelectionPlan::SubjectObject(plan) => {
                let mut seen = 0;
                for y_position in plan.y_range.clone() {
                    if let Some(triple) = subject_object_hit(plan, y_position) {
                        if seen == i {
                            return triple;
                        }
                        seen += 1;
                    }
                }
                panic!("selection index {i} out of range for {seen} triples")
            }
        }
    }
}

/// How `s ? o` will be answered.
///
/// Not a fallback: one algorithm choosing the cheaper of two routes to the same
/// answer, in the same order (doc 20 §20.8). Both routes enumerate in ascending
/// predicate id, because level 2 is predicate-sorted in SPO and OPS alike, so
/// the cursor is identical and a planner may switch routes between pages
/// without violating no-loss/no-duplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectObjectRoute {
    /// Walk `s`'s predicate groups in SPO, probing each for `o`.
    ///
    /// Chosen when `deg(s) <= deg(o)` — the rare-subject, hub-object case.
    ViaSubject,
    /// Walk `o`'s predicate groups in OPS, probing each for `s`.
    ///
    /// Chosen when `deg(o) < deg(s)` — the hub-subject, rare-object case.
    ViaObject,
}

/// Resolve a pattern against a bundle's permutations.
///
/// Borrows the permutations it resolved against, which is the whole point of
/// the lifetime: a `Selection` that outlived its store would be reading a
/// mapping that had been unmapped. [`Store::resolve`](crate::store::Store::resolve)
/// is the entry point callers use; this is where the §20.2 dispatch lives.
pub fn resolve(perms: &Permutations, pattern: IdPattern) -> Result<Selection<'_>> {
    validate_pattern(perms.hdt_layout().dictionary().counts(), pattern)?;

    Ok(match (pattern.subject, pattern.predicate, pattern.object) {
        (None, None, None) => contiguous(Permutation::Spo, perms.spo(), 0..perms.triples()),
        (Some(subject), None, None) => {
            let triples = perms.spo();
            let range = root_range(triples, subject);
            contiguous(Permutation::Spo, triples, range)
        }
        (Some(subject), Some(predicate), None) => {
            let triples = perms.spo();
            let range = second_range(triples, subject, predicate);
            contiguous(Permutation::Spo, triples, range)
        }
        (Some(subject), Some(predicate), Some(object)) => {
            let triples = perms.spo();
            let range = third_range(triples, subject, predicate, object);
            contiguous(Permutation::Spo, triples, range)
        }
        (Some(subject), None, Some(object)) => subject_object(perms, subject, object),
        (None, Some(predicate), None) => {
            let triples = perms.pos();
            let range = root_range(triples, predicate);
            contiguous(Permutation::Pos, triples, range)
        }
        (None, Some(predicate), Some(object)) => {
            let triples = perms.pos();
            let range = second_range(triples, predicate, object);
            contiguous(Permutation::Pos, triples, range)
        }
        (None, None, Some(object)) => {
            let triples = perms.ops();
            let range = root_range(triples, object);
            contiguous(Permutation::Ops, triples, range)
        }
    })
}

fn validate_pattern(counts: &DictCounts, pattern: IdPattern) -> Result<()> {
    validate_id(counts, Role::Subject, pattern.subject)?;
    validate_id(counts, Role::Predicate, pattern.predicate)?;
    validate_id(counts, Role::Object, pattern.object)
}

fn validate_id(counts: &DictCounts, role: Role, id: Option<u64>) -> Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    let maximum = counts.len(role);
    if id == 0 || id > maximum {
        Err(Error::TermIdOutOfRange { role, id, maximum })
    } else {
        Ok(())
    }
}

fn contiguous<'a>(
    permutation: Permutation,
    triples: BitmapTriples<'a>,
    z_range: Range<u64>,
) -> Selection<'a> {
    Selection {
        plan: SelectionPlan::Contiguous(ContiguousPlan {
            permutation,
            triples,
            z_range,
        }),
    }
}

fn root_range(triples: BitmapTriples<'_>, first: u64) -> Range<u64> {
    z_span(triples, triples.level2_range(first))
}

fn second_range(triples: BitmapTriples<'_>, first: u64, second: u64) -> Range<u64> {
    let y_range = triples.level2_range(first);
    triples
        .find_level2(y_range, second)
        .map(|y_position| triples.level3_range(y_position))
        .unwrap_or(0..0)
}

fn third_range(triples: BitmapTriples<'_>, first: u64, second: u64, third: u64) -> Range<u64> {
    let z_range = second_range(triples, first, second);
    triples
        .find_level3(z_range, third)
        .map(|z_position| z_position..z_position + 1)
        .unwrap_or(0..0)
}

fn subject_object(perms: &Permutations, subject: u64, object: u64) -> Selection<'_> {
    let spo = perms.spo();
    let subject_y = spo.level2_range(subject);
    let subject_degree = range_len(&z_span(spo, subject_y.clone()));

    let ops = perms.ops();
    let object_y = ops.level2_range(object);
    let object_degree = range_len(&z_span(ops, object_y.clone()));

    let (route, triples, y_range, endpoint_degree) = if subject_degree <= object_degree {
        (
            SubjectObjectRoute::ViaSubject,
            spo,
            subject_y,
            subject_degree,
        )
    } else {
        (SubjectObjectRoute::ViaObject, ops, object_y, object_degree)
    };
    let plan = SubjectObjectPlan {
        route,
        probe: subject_object_probe(endpoint_degree, triples.level3_width()),
        triples,
        y_range,
        subject,
        object,
    };
    Selection {
        plan: SelectionPlan::SubjectObject(plan),
    }
}

fn z_span(triples: BitmapTriples<'_>, y_range: Range<u64>) -> Range<u64> {
    debug_assert!(!y_range.is_empty(), "implicit level-1 keys are non-empty");
    let start = triples.level3_range(y_range.start).start;
    let end = triples.level3_range(y_range.end - 1).end;
    start..end
}

fn range_len(range: &Range<u64>) -> u64 {
    range.end - range.start
}

fn materialize(permutation: Permutation, triples: BitmapTriples<'_>, z_position: u64) -> IdTriple {
    let y_position = triples.level2_of(z_position);
    let first = triples.level1_of(y_position);
    let second = triples.level2_at(y_position);
    let third = triples.level3_at(z_position);
    match permutation {
        Permutation::Spo => IdTriple {
            subject: first,
            predicate: second,
            object: third,
        },
        Permutation::Pos => IdTriple {
            subject: third,
            predicate: first,
            object: second,
        },
        Permutation::Ops => IdTriple {
            subject: third,
            predicate: second,
            object: first,
        },
    }
}

fn subject_object_count(plan: &SubjectObjectPlan<'_>) -> u64 {
    plan.y_range.clone().fold(0, |count, y_position| {
        count + u64::from(subject_object_hit(plan, y_position).is_some())
    })
}

fn subject_object_hit(plan: &SubjectObjectPlan<'_>, y_position: u64) -> Option<IdTriple> {
    let predicate = plan.triples.level2_at(y_position);
    let target = match plan.route {
        SubjectObjectRoute::ViaSubject => plan.object,
        SubjectObjectRoute::ViaObject => plan.subject,
    };
    let z_range = plan.triples.level3_range(y_position);
    contains_level3(plan.triples, z_range, target, plan.probe).then_some(IdTriple {
        subject: plan.subject,
        predicate,
        object: plan.object,
    })
}

// Two conventional 4 KiB pages. This is a performance choice inside the same
// bounded probe: a larger endpoint binary-searches every predicate group.
const LINEAR_PROBE_BITS: u128 = 2 * 4096 * 8;

fn subject_object_probe(endpoint_degree: u64, width: u8) -> SubjectObjectProbe {
    let packed_bits = u128::from(endpoint_degree) * u128::from(width);
    if packed_bits <= LINEAR_PROBE_BITS {
        SubjectObjectProbe::Linear
    } else {
        SubjectObjectProbe::Binary
    }
}

fn contains_level3(
    triples: BitmapTriples<'_>,
    mut range: Range<u64>,
    target: u64,
    probe: SubjectObjectProbe,
) -> bool {
    match probe {
        SubjectObjectProbe::Linear => range.any(|position| triples.level3_at(position) == target),
        SubjectObjectProbe::Binary => triples.find_level3(range, target).is_some(),
    }
}

struct SelectionPage<'a> {
    state: PageState<'a>,
    remaining: usize,
}

enum PageState<'a> {
    Contiguous {
        plan: ContiguousPlan<'a>,
        next_z: u64,
    },
    SubjectObject {
        plan: SubjectObjectPlan<'a>,
        next_y: u64,
    },
}

impl<'a> SelectionPage<'a> {
    fn new(plan: &SelectionPlan<'a>, from: u64, limit: usize) -> Self {
        let state = match plan {
            SelectionPlan::Contiguous(plan) => {
                let len = range_len(&plan.z_range);
                let next_z = if from < len {
                    plan.z_range.start + from
                } else {
                    plan.z_range.end
                };
                PageState::Contiguous {
                    plan: plan.clone(),
                    next_z,
                }
            }
            SelectionPlan::SubjectObject(plan) => {
                let next_y = if from == 0 {
                    plan.y_range.start
                } else {
                    plan.triples.level2_upper_bound(plan.y_range.clone(), from)
                };
                PageState::SubjectObject {
                    plan: plan.clone(),
                    next_y,
                }
            }
        };
        Self {
            state,
            remaining: limit,
        }
    }
}

impl Iterator for SelectionPage<'_> {
    type Item = IdTriple;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        match &mut self.state {
            PageState::Contiguous { plan, next_z } => {
                if *next_z == plan.z_range.end {
                    return None;
                }
                let triple = materialize(plan.permutation, plan.triples, *next_z);
                *next_z += 1;
                self.remaining -= 1;
                Some(triple)
            }
            PageState::SubjectObject { plan, next_y } => {
                while *next_y < plan.y_range.end {
                    let y_position = *next_y;
                    *next_y += 1;
                    if let Some(triple) = subject_object_hit(plan, y_position) {
                        self.remaining -= 1;
                        return Some(triple);
                    }
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TermId;
    use crate::dict::Dictionary;
    use crate::testing::{Fixture, TINY_NT, tiny_id_triples};

    #[test]
    fn every_pattern_has_exact_counts_stable_order_and_exhaustive_resume() {
        let fixture = Fixture::build(TINY_NT);
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let dictionary = permutations
            .hdt_layout()
            .dictionary()
            .view(permutations.hdt_mapping());
        let counts = *dictionary.counts();
        let source = tiny_id_triples(&dictionary);

        let subjects = options(counts.len(Role::Subject));
        let predicates = options(counts.len(Role::Predicate));
        let objects = options(counts.len(Role::Object));
        for &subject in &subjects {
            for &predicate in &predicates {
                for &object in &objects {
                    let pattern = IdPattern {
                        subject,
                        predicate,
                        object,
                    };
                    let expected = expected_rows(&source, pattern);
                    let selection = resolve(&permutations, pattern).expect("resolve valid ids");

                    let expected_route = route_for(&source, pattern);
                    assert_eq!(
                        selection.subject_object_route(),
                        expected_route,
                        "{pattern:?}"
                    );
                    assert_eq!(
                        selection.permutation(),
                        permutation_for(pattern, expected_route),
                        "{pattern:?}"
                    );
                    assert_eq!(
                        selection.count(),
                        Count {
                            value: expected.len() as u64,
                            arithmetic: expected_route.is_none(),
                        },
                        "{pattern:?}"
                    );

                    let random_access: Vec<_> = (0..expected.len())
                        .map(|index| selection.at(index as u64))
                        .collect();
                    assert_eq!(random_access, expected, "random access: {pattern:?}");
                    assert_eq!(selection.page(0, 0).count(), 0, "{pattern:?}");

                    for page_size in [1, 2, 3, 7, 100] {
                        assert_eq!(
                            collect_pages(&selection, page_size),
                            expected,
                            "page size {page_size}: {pattern:?}"
                        );
                    }

                    for suffix in 0..=expected.len() {
                        let from = match expected_route {
                            Some(_) if suffix != 0 => expected[suffix - 1].predicate,
                            Some(_) => 0,
                            None => suffix as u64,
                        };
                        assert_eq!(
                            selection.page(from, usize::MAX).collect::<Vec<_>>(),
                            expected[suffix..],
                            "resume suffix {suffix}: {pattern:?}"
                        );
                    }
                    assert_eq!(selection.page(u64::MAX, 1).count(), 0, "{pattern:?}");
                }
            }
        }
    }

    #[test]
    fn all_eight_shapes_agree_with_hdtc_search() {
        let fixture = Fixture::build(TINY_NT);
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let dictionary = permutations
            .hdt_layout()
            .dictionary()
            .view(permutations.hdt_mapping());

        let alice_s = id(&dictionary, Role::Subject, b"http://example.org/alice");
        let alice_o = id(&dictionary, Role::Object, b"http://example.org/alice");
        let bob_o = id(&dictionary, Role::Object, b"http://example.org/bob");
        let knows = id(&dictionary, Role::Predicate, b"http://example.org/knows");
        let label = id(&dictionary, Role::Predicate, b"http://example.org/label");
        let alice_en = id(&dictionary, Role::Object, b"\"Alice\"@en");

        let cases = [
            (
                "? ? ?",
                IdPattern {
                    subject: None,
                    predicate: None,
                    object: None,
                },
            ),
            (
                "<http://example.org/alice> ? ?",
                IdPattern {
                    subject: Some(alice_s),
                    predicate: None,
                    object: None,
                },
            ),
            (
                "<http://example.org/alice> <http://example.org/label> ?",
                IdPattern {
                    subject: Some(alice_s),
                    predicate: Some(label),
                    object: None,
                },
            ),
            (
                "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob>",
                IdPattern {
                    subject: Some(alice_s),
                    predicate: Some(knows),
                    object: Some(bob_o),
                },
            ),
            (
                "<http://example.org/alice> ? <http://example.org/bob>",
                IdPattern {
                    subject: Some(alice_s),
                    predicate: None,
                    object: Some(bob_o),
                },
            ),
            (
                "? <http://example.org/knows> ?",
                IdPattern {
                    subject: None,
                    predicate: Some(knows),
                    object: None,
                },
            ),
            (
                "? <http://example.org/label> \"Alice\"@en",
                IdPattern {
                    subject: None,
                    predicate: Some(label),
                    object: Some(alice_en),
                },
            ),
            (
                "? ? <http://example.org/alice>",
                IdPattern {
                    subject: None,
                    predicate: None,
                    object: Some(alice_o),
                },
            ),
        ];

        for (query, pattern) in cases {
            let mut expected: Vec<_> = resolve(&permutations, pattern)
                .unwrap()
                .page(0, usize::MAX)
                .map(|triple| render_hdtc_row(&dictionary, triple))
                .collect();
            let mut actual = fixture.search(query);
            expected.sort_unstable();
            actual.sort_unstable();
            assert_eq!(actual, expected, "{query}");
        }
    }

    #[test]
    fn permutation_counts_close_over_predicates_and_objects() {
        let fixture = Fixture::build(TINY_NT);
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let counts = permutations.hdt_layout().dictionary().counts();
        let total = permutations.triples();

        let by_predicate: u64 = (1..=counts.len(Role::Predicate))
            .map(|predicate| {
                resolve(
                    &permutations,
                    IdPattern {
                        subject: None,
                        predicate: Some(predicate),
                        object: None,
                    },
                )
                .unwrap()
                .count()
                .value
            })
            .sum();
        let by_object: u64 = (1..=counts.len(Role::Object))
            .map(|object| {
                resolve(
                    &permutations,
                    IdPattern {
                        subject: None,
                        predicate: None,
                        object: Some(object),
                    },
                )
                .unwrap()
                .count()
                .value
            })
            .sum();
        assert_eq!(by_predicate, total);
        assert_eq!(by_object, total);

        let ops = permutations.ops();
        for predicate in 1..=counts.len(Role::Predicate) {
            for object in 1..=counts.len(Role::Object) {
                let pos_count = resolve(
                    &permutations,
                    IdPattern {
                        subject: None,
                        predicate: Some(predicate),
                        object: Some(object),
                    },
                )
                .unwrap()
                .count()
                .value;
                let y_range = ops.level2_range(object);
                let ops_count = ops
                    .find_level2(y_range, predicate)
                    .map(|y_position| range_len(&ops.level3_range(y_position)))
                    .unwrap_or(0);
                assert_eq!(
                    pos_count, ops_count,
                    "predicate {predicate}, object {object}"
                );
            }
        }
    }

    #[test]
    fn role_scoped_ids_are_rejected_before_descent() {
        let fixture = Fixture::build(TINY_NT);
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let counts = permutations.hdt_layout().dictionary().counts();

        for (role, maximum) in [
            (Role::Subject, counts.len(Role::Subject)),
            (Role::Predicate, counts.len(Role::Predicate)),
            (Role::Object, counts.len(Role::Object)),
        ] {
            for id in [0, maximum + 1] {
                let mut pattern = IdPattern {
                    subject: None,
                    predicate: None,
                    object: None,
                };
                match role {
                    Role::Subject => pattern.subject = Some(id),
                    Role::Predicate => pattern.predicate = Some(id),
                    Role::Object => pattern.object = Some(id),
                }
                match resolve(&permutations, pattern).expect_err("invalid id must fail") {
                    Error::TermIdOutOfRange {
                        role: actual_role,
                        id: actual_id,
                        maximum: actual_maximum,
                    } => {
                        assert_eq!(actual_role, role);
                        assert_eq!(actual_id, id);
                        assert_eq!(actual_maximum, maximum);
                    }
                    other => panic!("unexpected error: {other}"),
                }
            }
        }
    }

    #[test]
    fn a_large_endpoint_binary_searches_even_when_each_predicate_group_is_small() {
        use std::fmt::Write;

        const DEGREE: usize = 5100;
        const GROUP_SIZE: usize = 51;

        let mut source = String::new();
        for index in 0..DEGREE {
            let object = if index == 0 {
                "http://example.org/hub".to_owned()
            } else {
                format!("http://example.org/o/{index}")
            };
            writeln!(
                source,
                "<http://example.org/s> <http://example.org/p/{}> <{object}> .",
                index / GROUP_SIZE
            )
            .unwrap();
        }
        for index in 0..DEGREE - 1 {
            writeln!(
                source,
                "<http://example.org/t/{index}> <http://example.org/incoming> <http://example.org/hub> ."
            )
            .unwrap();
        }

        let fixture = Fixture::build(&source);
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let dictionary = permutations
            .hdt_layout()
            .dictionary()
            .view(permutations.hdt_mapping());
        let subject = id(&dictionary, Role::Subject, b"http://example.org/s");
        let object = id(&dictionary, Role::Object, b"http://example.org/hub");
        let selection = resolve(
            &permutations,
            IdPattern {
                subject: Some(subject),
                predicate: None,
                object: Some(object),
            },
        )
        .unwrap();

        let SelectionPlan::SubjectObject(plan) = &selection.plan else {
            panic!("s ? o must resolve to its bounded group plan");
        };
        assert_eq!(plan.route, SubjectObjectRoute::ViaSubject);
        assert_eq!(plan.probe, SubjectObjectProbe::Binary);
        assert!(
            plan.y_range.clone().all(|y_position| {
                let group = plan.triples.level3_range(y_position);
                u128::from(range_len(&group)) * u128::from(plan.triples.level3_width())
                    <= LINEAR_PROBE_BITS
            }),
            "the regression requires every individual group to fit the linear threshold"
        );
        assert_eq!(selection.count().value, 1);
        assert_eq!(selection.page(0, usize::MAX).count(), 1);
    }

    fn options(maximum: u64) -> Vec<Option<u64>> {
        std::iter::once(None)
            .chain((1..=maximum).map(Some))
            .collect()
    }

    fn expected_rows(source: &[IdTriple], pattern: IdPattern) -> Vec<IdTriple> {
        let mut rows: Vec<_> = source
            .iter()
            .copied()
            .filter(|triple| {
                pattern.subject.is_none_or(|id| id == triple.subject)
                    && pattern.predicate.is_none_or(|id| id == triple.predicate)
                    && pattern.object.is_none_or(|id| id == triple.object)
            })
            .collect();
        match permutation_for(pattern, route_for(source, pattern)) {
            Permutation::Spo => rows
                .sort_unstable_by_key(|triple| (triple.subject, triple.predicate, triple.object)),
            Permutation::Pos => rows
                .sort_unstable_by_key(|triple| (triple.predicate, triple.object, triple.subject)),
            Permutation::Ops => rows
                .sort_unstable_by_key(|triple| (triple.object, triple.predicate, triple.subject)),
        }
        rows
    }

    fn route_for(source: &[IdTriple], pattern: IdPattern) -> Option<SubjectObjectRoute> {
        let (Some(subject), None, Some(object)) =
            (pattern.subject, pattern.predicate, pattern.object)
        else {
            return None;
        };
        let subject_degree = source
            .iter()
            .filter(|triple| triple.subject == subject)
            .count();
        let object_degree = source
            .iter()
            .filter(|triple| triple.object == object)
            .count();
        Some(if subject_degree <= object_degree {
            SubjectObjectRoute::ViaSubject
        } else {
            SubjectObjectRoute::ViaObject
        })
    }

    fn permutation_for(pattern: IdPattern, route: Option<SubjectObjectRoute>) -> Permutation {
        match route {
            Some(SubjectObjectRoute::ViaSubject) => Permutation::Spo,
            Some(SubjectObjectRoute::ViaObject) => Permutation::Ops,
            None if pattern.subject.is_some() => Permutation::Spo,
            None if pattern.predicate.is_some() => Permutation::Pos,
            None if pattern.object.is_some() => Permutation::Ops,
            None => Permutation::Spo,
        }
    }

    fn collect_pages(selection: &Selection<'_>, page_size: usize) -> Vec<IdTriple> {
        let mut rows = Vec::new();
        let mut from = 0;
        loop {
            let page: Vec<_> = selection.page(from, page_size).collect();
            if page.is_empty() {
                return rows;
            }
            from = if selection.subject_object_route().is_some() {
                page.last().unwrap().predicate
            } else {
                from + page.len() as u64
            };
            rows.extend(page);
        }
    }

    fn id(dictionary: &Dictionary<'_>, role: Role, term: &[u8]) -> u64 {
        dictionary
            .locate(role, term)
            .unwrap()
            .unwrap_or_else(|| panic!("missing fixture term: {}", String::from_utf8_lossy(term)))
            .0
    }

    fn render_hdtc_row(dictionary: &Dictionary<'_>, triple: IdTriple) -> Vec<u8> {
        let mut scratch = Vec::new();
        let subject = dictionary
            .extract(Role::Subject, TermId(triple.subject), &mut scratch)
            .unwrap()
            .to_vec();
        let predicate = dictionary
            .extract(Role::Predicate, TermId(triple.predicate), &mut scratch)
            .unwrap()
            .to_vec();
        let object = dictionary
            .extract(Role::Object, TermId(triple.object), &mut scratch)
            .unwrap()
            .to_vec();

        let mut row = Vec::new();
        push_iri_or_blank(&mut row, &subject);
        row.push(b'\t');
        row.push(b'<');
        row.extend_from_slice(&predicate);
        row.extend_from_slice(b">\t");
        if object.starts_with(b"\"") {
            row.extend_from_slice(&object);
        } else {
            push_iri_or_blank(&mut row, &object);
        }
        row.extend_from_slice(b"\t.");
        row
    }

    fn push_iri_or_blank(output: &mut Vec<u8>, term: &[u8]) {
        if term.starts_with(b"_:") {
            output.extend_from_slice(term);
        } else {
            output.push(b'<');
            output.extend_from_slice(term);
            output.push(b'>');
        }
    }
}
