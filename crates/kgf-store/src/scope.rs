//! Graph-scoped and quad-view enumeration over a resolved pattern.
//!
//! A [`Selection`] answers a pattern over the union. This module restricts
//! one to a single graph's layer, or expands it to one row per membership,
//! without leaving the pattern's own position space: the sidecar keys the
//! unnamed and named graphs to SPO positions and the index re-keys them to
//! POS and OPS, so whichever permutation the pattern is contiguous in, a
//! layer keyed to that permutation counts and enumerates it directly.
//!
//! # Scoped
//!
//! For a contiguous selection with range `[a, b)` in its permutation, the
//! scoped count is `rank(b) - rank(a)` over the graph's layer, and the scoped
//! page from offset `k` is the run of members starting at ordinal
//! `rank(a) + k`, one `select` each, while they stay below `b`. `s ? o` has no
//! contiguous range and already probes each predicate group; scoping filters
//! each hit by `access` at the position the hit was found, in the route's own
//! space. Cursors are unchanged: an offset into the scoped result, or the last
//! predicate id.
//!
//! # Quad view
//!
//! One row per `(triple, graph)` membership, in position order and then
//! ascending graph id. The count is the memberships of the selection's range
//! — a rank difference per layer, or two selects on the transpose — and for
//! `s ? o` the sum over its hits. A page resumes at a triple *and* a number of
//! that triple's memberships already delivered, so a page may end inside one
//! triple's graphs in any space; the resume position for the triple is the
//! same offset or predicate id an unscoped page uses. For `s ? o` that is the
//! predicate *before* the triple, so a page that ends inside a triple's run
//! carries the predicate that started the triple — the `from` it was itself
//! resumed with, when the whole page lies in one run — and not the triple's
//! own predicate, which would skip the rest of the run.
//!
//! # Cost
//!
//! Every scoped operation is bounded by the page: a count is two ranks, a row
//! is one select, and an `s ? o` probe adds one `access` per predicate group.
//! A quad-view row adds one array read with the transpose's ids, or one probe
//! per layer without — bounded by the graph count, a per-bundle constant.

use std::ops::Range;

use crate::IdTriple;
use crate::error::Result;
use crate::graphs::{GraphId, Graphs, Layer, Memberships};
use crate::pattern::{Permutation, Positioned, Selection, SubjectObjectRoute};

/// A selection restricted to the triples one graph contains.
pub struct ScopedSelection<'a> {
    selection: Selection<'a>,
    layer: Layer<'a>,
}

/// A selection expanded to one row per membership.
pub struct QuadSelection<'a> {
    selection: Selection<'a>,
    memberships: Memberships<'a>,
}

/// One membership row of the quad view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuadRow {
    /// The triple.
    pub triple: IdTriple,
    /// One of the graphs containing it.
    pub graph: GraphId,
    /// How many of this triple's memberships precede this one in its run —
    /// the index of `graph` among the triple's graphs, ascending — which is
    /// what a cursor records to resume inside the run.
    pub delivered: u64,
}

impl<'a> Selection<'a> {
    /// Restrict this selection to the triples `graph` contains.
    ///
    /// Opens the graph's layer in this selection's own position space, which
    /// decodes one directory entry and, for an Elias–Fano layer, one header.
    pub fn in_graph(self, graphs: &'a Graphs, graph: GraphId) -> Result<ScopedSelection<'a>> {
        let layer = graphs.layer(self.permutation(), graph)?;
        Ok(ScopedSelection {
            selection: self,
            layer,
        })
    }

    /// Expand this selection to one row per membership.
    ///
    /// Prepares the membership questions once for the page: every layer of
    /// this selection's space, unless the transpose answers for it.
    pub fn memberships(self, graphs: &'a Graphs) -> Result<QuadSelection<'a>> {
        let memberships = graphs.memberships(self.permutation())?;
        Ok(QuadSelection {
            selection: self,
            memberships,
        })
    }
}

impl<'a> ScopedSelection<'a> {
    /// Which permutation this reads; the layer is keyed to it.
    pub fn permutation(&self) -> Permutation {
        self.selection.permutation()
    }

    /// The planner route for `s ? o`, or `None` for a contiguous selection.
    pub fn subject_object_route(&self) -> Option<SubjectObjectRoute> {
        self.selection.subject_object_route()
    }

    /// The graph this is scoped to.
    pub fn graph(&self) -> GraphId {
        self.layer.id()
    }

    /// Exact cardinality: two ranks, or for `s ? o` one `access` per hit.
    pub fn count(&self) -> Result<u64> {
        match self.selection.contiguous_range() {
            Some(range) => Ok(self.layer.rank(range.end)? - self.layer.rank(range.start)?),
            None => {
                let mut count = 0;
                for hit in self.selection.positions(0, usize::MAX) {
                    count += u64::from(self.layer.access(hit.position)?);
                }
                Ok(count)
            }
        }
    }

    /// At most `limit` triples in the selection's order, from `from`: the
    /// zero-based offset into the scoped result, or for `s ? o` the last
    /// predicate id returned, exactly as [`Selection::page`] reads it.
    pub fn page(&self, from: u64, limit: usize) -> impl Iterator<Item = Result<IdTriple>> + '_ {
        let contiguous = self.selection.contiguous_range();
        ScopedPage {
            scoped: self,
            state: match contiguous {
                Some(range) => ScopedState::Contiguous {
                    range,
                    next: None,
                    from,
                },
                None => ScopedState::Probe(Box::new(self.selection.positions(from, usize::MAX))),
            },
            remaining: limit,
        }
    }
}

struct ScopedPage<'s, 'a> {
    scoped: &'s ScopedSelection<'a>,
    state: ScopedState<'s>,
    remaining: usize,
}

enum ScopedState<'s> {
    Contiguous {
        range: Range<u64>,
        /// The next member ordinal, once the first is known.
        next: Option<u64>,
        from: u64,
    },
    Probe(Box<dyn Iterator<Item = Positioned> + 's>),
}

impl Iterator for ScopedPage<'_, '_> {
    type Item = Result<IdTriple>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let layer = &self.scoped.layer;
        match &mut self.state {
            ScopedState::Contiguous { range, next, from } => {
                let ordinal = match *next {
                    Some(ordinal) => ordinal,
                    None => match layer.rank(range.start) {
                        Ok(first) => first.saturating_add(*from),
                        Err(error) => {
                            self.remaining = 0;
                            return Some(Err(error));
                        }
                    },
                };
                if ordinal >= layer.count() {
                    return None;
                }
                let position = match layer.select(ordinal) {
                    Ok(position) => position,
                    Err(error) => {
                        self.remaining = 0;
                        return Some(Err(error));
                    }
                };
                if position >= range.end {
                    return None;
                }
                if position < range.start {
                    // Members are strictly increasing from the one rank
                    // located, so a position before the range is a layer
                    // whose rank and select disagree — reported, as every
                    // lazily decoded structure is, rather than trusted.
                    self.remaining = 0;
                    return Some(Err(self.scoped.layer.inconsistent(
                        "select returned a member before the position its rank located",
                    )));
                }
                *next = Some(ordinal + 1);
                self.remaining -= 1;
                Some(Ok(self.scoped.selection.at_position(position)))
            }
            ScopedState::Probe(hits) => {
                for hit in hits.by_ref() {
                    match layer.access(hit.position) {
                        Ok(true) => {
                            self.remaining -= 1;
                            return Some(Ok(hit.triple));
                        }
                        Ok(false) => {}
                        Err(error) => {
                            self.remaining = 0;
                            return Some(Err(error));
                        }
                    }
                }
                None
            }
        }
    }
}

impl<'a> QuadSelection<'a> {
    /// Which permutation this reads.
    pub fn permutation(&self) -> Permutation {
        self.selection.permutation()
    }

    /// The planner route for `s ? o`, or `None` for a contiguous selection.
    pub fn subject_object_route(&self) -> Option<SubjectObjectRoute> {
        self.selection.subject_object_route()
    }

    /// Exact number of memberships: the selection's range summed over the
    /// layers or read from the transpose, or for `s ? o` summed over its hits.
    pub fn count(&self) -> Result<u64> {
        match self.selection.contiguous_range() {
            Some(range) => self.memberships.in_range(range),
            None => {
                let mut graphs = Vec::new();
                let mut count = 0u64;
                for hit in self.selection.positions(0, usize::MAX) {
                    graphs.clear();
                    self.memberships.graphs_of(hit.position, &mut graphs)?;
                    count += graphs.len() as u64;
                }
                Ok(count)
            }
        }
    }

    /// The triples this expands, for a caller that wants the triple count
    /// beside the membership count.
    pub fn triples(&self) -> u64 {
        self.selection.count().value
    }

    /// At most `limit` rows from the triple at `from` — an offset or the last
    /// predicate id, as [`Selection::page`] reads it — skipping the first
    /// `skip` memberships of that triple.
    pub fn page(
        &self,
        from: u64,
        skip: u64,
        limit: usize,
    ) -> impl Iterator<Item = Result<QuadRow>> + '_ {
        QuadPage {
            quads: self,
            positions: self.selection.positions(from, usize::MAX),
            current: None,
            graphs: Vec::new(),
            skip,
            remaining: limit,
        }
    }
}

struct QuadPage<'s, 'a, I> {
    quads: &'s QuadSelection<'a>,
    positions: I,
    /// The triple whose graphs are being delivered, and the next index into
    /// `graphs`.
    current: Option<(IdTriple, usize)>,
    graphs: Vec<GraphId>,
    /// Memberships of the first triple to pass over; zero after it.
    skip: u64,
    remaining: usize,
}

impl<I: Iterator<Item = Positioned>> Iterator for QuadPage<'_, '_, I> {
    type Item = Result<QuadRow>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.remaining == 0 {
                return None;
            }
            if let Some((triple, index)) = self.current
                && index < self.graphs.len()
            {
                self.current = Some((triple, index + 1));
                self.remaining -= 1;
                return Some(Ok(QuadRow {
                    triple,
                    graph: self.graphs[index],
                    delivered: index as u64,
                }));
            }
            let hit = self.positions.next()?;
            self.graphs.clear();
            if let Err(error) = self
                .quads
                .memberships
                .graphs_of(hit.position, &mut self.graphs)
            {
                self.remaining = 0;
                return Some(Err(error));
            }
            // Every position has at least one graph, so an empty run is a
            // sidecar that is not exhaustive — refused rather than skipped.
            if self.graphs.is_empty() {
                self.remaining = 0;
                return Some(Err(self.quads.memberships.inconsistent(&format!(
                    "position {} belongs to no graph",
                    hit.position
                ))));
            }
            let skip = std::mem::take(&mut self.skip);
            let start = usize::try_from(skip)
                .unwrap_or(usize::MAX)
                .min(self.graphs.len());
            self.current = Some((hit.triple, start));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Role;
    use crate::pattern::{IdPattern, resolve};
    use crate::perm::Permutations;
    use crate::testing::{
        Fixture, TINY_NQ, Transpose, WORKED_EXAMPLE_NQ, parse_hdtc_row, synthetic_quads,
    };
    use std::collections::BTreeMap;

    /// Every graph of every triple, from hdtc's own four-position search: the
    /// oracle for membership questions, sharing nothing with the mapped reader.
    fn hdtc_memberships(
        fixture: &Fixture,
        dictionary: &crate::dict::Dictionary<'_>,
        graphs: &Graphs,
    ) -> std::collections::BTreeMap<IdTriple, Vec<GraphId>> {
        let mut memberships: std::collections::BTreeMap<IdTriple, Vec<GraphId>> =
            std::collections::BTreeMap::new();
        let mut buf = Vec::new();
        for graph in 0..=graphs.facts().named_graphs {
            let query = if graph == 0 {
                "? ? ? default".to_owned()
            } else {
                let name = graphs
                    .name(GraphId(graph), &mut buf)
                    .expect("read a graph name");
                format!("? ? ? <{}>", String::from_utf8_lossy(name))
            };
            for row in fixture.search(&query) {
                memberships
                    .entry(parse_hdtc_row(dictionary, &row))
                    .or_default()
                    .push(GraphId(graph));
            }
        }
        for graphs in memberships.values_mut() {
            graphs.sort_unstable();
        }
        memberships
    }

    struct Bundle {
        perms: Permutations,
        graphs: Graphs,
        memberships: BTreeMap<IdTriple, Vec<GraphId>>,
    }

    fn open(source: &str, transpose: Transpose) -> Bundle {
        let fixture = Fixture::build_quads_with(source, transpose);
        let perms = Permutations::open(fixture.map_hdt(), fixture.map_perm()).unwrap();
        let graphs = Graphs::open(
            &fixture.hdt_path(),
            fixture.map_graphs(),
            fixture.map_graph_index(),
        )
        .unwrap();
        let memberships = hdtc_memberships(&fixture, &perms.dict(), &graphs);
        Bundle {
            perms,
            graphs,
            memberships,
        }
    }

    /// Every bound/unbound combination over every id for the small fixtures.
    fn every_pattern(perms: &Permutations) -> Vec<IdPattern> {
        let counts = perms.dict_counts();
        let options = |role| {
            std::iter::once(None)
                .chain((1..=counts.len(role)).map(Some))
                .collect::<Vec<_>>()
        };
        let mut patterns = Vec::new();
        for subject in options(Role::Subject) {
            for predicate in options(Role::Predicate) {
                for object in options(Role::Object) {
                    patterns.push(IdPattern {
                        subject,
                        predicate,
                        object,
                    });
                }
            }
        }
        patterns
    }

    /// A sample of shapes over the wide fixture: every predicate-rooted
    /// pattern, and a handful of the others.
    fn sampled_patterns(perms: &Permutations) -> Vec<IdPattern> {
        let counts = perms.dict_counts();
        let mut patterns = vec![IdPattern {
            subject: None,
            predicate: None,
            object: None,
        }];
        for predicate in 1..=counts.len(Role::Predicate) {
            patterns.push(IdPattern {
                subject: None,
                predicate: Some(predicate),
                object: None,
            });
        }
        for id in (1..=counts.len(Role::Subject)).step_by(9973) {
            patterns.push(IdPattern {
                subject: Some(id),
                predicate: None,
                object: None,
            });
            patterns.push(IdPattern {
                subject: Some(id),
                predicate: Some(1),
                object: None,
            });
        }
        for id in (1..=counts.len(Role::Object)).step_by(9973) {
            patterns.push(IdPattern {
                subject: None,
                predicate: None,
                object: Some(id),
            });
            patterns.push(IdPattern {
                subject: None,
                predicate: Some(1),
                object: Some(id),
            });
        }
        // `s ? o` over real edges, so the probe has hits — and edges the thin
        // graphs contain, so a probe testing the wrong space's position
        // against a layer shows up as a miss. Every subject here has four
        // triples and every object one, so the probe runs through OPS, and
        // the OPS layers are what the hits must be tested against.
        let all = resolve(perms, patterns[0]).unwrap();
        for triple in all.page(0, usize::MAX).step_by(17_011) {
            patterns.push(IdPattern {
                subject: Some(triple.subject),
                predicate: None,
                object: Some(triple.object),
            });
            patterns.push(IdPattern {
                subject: Some(triple.subject),
                predicate: Some(triple.predicate),
                object: Some(triple.object),
            });
        }
        patterns
    }

    /// `s ? o` for every triple of a thin graph.
    fn subject_object_members(bundle: &Bundle, graph: GraphId) -> Vec<IdPattern> {
        bundle
            .memberships
            .iter()
            .filter(|(_, graphs)| graphs.contains(&graph))
            .map(|(triple, _)| IdPattern {
                subject: Some(triple.subject),
                predicate: None,
                object: Some(triple.object),
            })
            .collect()
    }

    fn check(bundle: &Bundle, patterns: &[IdPattern], exhaustive_resume: bool) {
        let Bundle {
            perms,
            graphs,
            memberships,
        } = bundle;
        let named = graphs.facts().named_graphs;
        for pattern in patterns {
            let base: Vec<IdTriple> = resolve(perms, *pattern)
                .unwrap()
                .page(0, usize::MAX)
                .collect();
            let route = resolve(perms, *pattern).unwrap().subject_object_route();

            // Scoped to each graph: the union's rows filtered by membership.
            for graph in (0..=named).map(GraphId) {
                let expected: Vec<IdTriple> = base
                    .iter()
                    .copied()
                    .filter(|triple| memberships[triple].contains(&graph))
                    .collect();
                let scoped = resolve(perms, *pattern)
                    .unwrap()
                    .in_graph(graphs, graph)
                    .unwrap();
                assert_eq!(scoped.graph(), graph);
                assert_eq!(
                    scoped.count().unwrap(),
                    expected.len() as u64,
                    "{pattern:?} in graph {graph:?}"
                );
                for page_size in [1, 2, 3, 7, 100] {
                    assert_eq!(
                        collect_scoped(&scoped, page_size),
                        expected,
                        "{pattern:?} in graph {graph:?} at page size {page_size}"
                    );
                }
                let suffixes: Vec<usize> = if exhaustive_resume {
                    (0..=expected.len()).collect()
                } else {
                    (0..=expected.len())
                        .step_by(expected.len() / 7 + 1)
                        .collect()
                };
                for suffix in suffixes {
                    let from = match route {
                        Some(_) if suffix != 0 => expected[suffix - 1].predicate,
                        Some(_) => 0,
                        None => suffix as u64,
                    };
                    let rows: Vec<IdTriple> =
                        scoped.page(from, usize::MAX).map(Result::unwrap).collect();
                    assert_eq!(
                        rows,
                        expected[suffix..],
                        "{pattern:?} in graph {graph:?} resumed at {suffix}"
                    );
                }
            }

            // The quad view: each row's graphs, ascending, in the union's order.
            let expected: Vec<QuadRow> = base
                .iter()
                .flat_map(|triple| {
                    memberships[triple]
                        .iter()
                        .enumerate()
                        .map(|(delivered, graph)| QuadRow {
                            triple: *triple,
                            graph: *graph,
                            delivered: delivered as u64,
                        })
                })
                .collect();
            let quads = resolve(perms, *pattern)
                .unwrap()
                .memberships(graphs)
                .unwrap();
            assert_eq!(
                quads.count().unwrap(),
                expected.len() as u64,
                "{pattern:?} quad view"
            );
            assert_eq!(quads.triples(), base.len() as u64);
            for page_size in [1, 2, 3, 7, 100] {
                assert_eq!(
                    collect_quads(&quads, page_size),
                    expected,
                    "{pattern:?} quad view at page size {page_size}"
                );
            }
            let suffixes: Vec<usize> = if exhaustive_resume {
                (0..=expected.len()).collect()
            } else {
                (0..=expected.len())
                    .step_by(expected.len() / 7 + 1)
                    .collect()
            };
            for suffix in suffixes {
                let (from, skip) = quad_resume(&expected, route, suffix);
                let rows: Vec<QuadRow> = quads
                    .page(from, skip, usize::MAX)
                    .map(Result::unwrap)
                    .collect();
                assert_eq!(
                    rows,
                    expected[suffix..],
                    "{pattern:?} quad view resumed at {suffix}"
                );
            }
        }
    }

    /// The cursor that resumes the quad view at row `suffix`: the triple's
    /// own resume position, and the memberships of it already delivered.
    fn quad_resume(
        expected: &[QuadRow],
        route: Option<SubjectObjectRoute>,
        suffix: usize,
    ) -> (u64, u64) {
        let Some(row) = expected.get(suffix) else {
            // Past the end: the position after the last triple, whole.
            return match (route, expected.last()) {
                (Some(_), Some(last)) => (last.triple.predicate, 0),
                (Some(_), None) => (0, 0),
                (None, _) => (triples_before(expected, suffix), 0),
            };
        };
        let from = match route {
            Some(_) => expected[..suffix]
                .iter()
                .rev()
                .find(|earlier| earlier.triple != row.triple)
                .map_or(0, |earlier| earlier.triple.predicate),
            None => triples_before(expected, suffix) - u64::from(row.delivered > 0),
        };
        (from, row.delivered)
    }

    /// Distinct triples among the first `suffix` rows.
    fn triples_before(expected: &[QuadRow], suffix: usize) -> u64 {
        expected[..suffix]
            .iter()
            .filter(|row| row.delivered == 0)
            .count() as u64
    }

    fn collect_scoped(scoped: &ScopedSelection<'_>, page_size: usize) -> Vec<IdTriple> {
        let mut rows = Vec::new();
        let mut from = 0;
        loop {
            let page: Vec<IdTriple> = scoped.page(from, page_size).map(Result::unwrap).collect();
            if page.is_empty() {
                return rows;
            }
            from = if scoped.subject_object_route().is_some() {
                page.last().unwrap().predicate
            } else {
                from + page.len() as u64
            };
            rows.extend(page);
        }
    }

    fn collect_quads(quads: &QuadSelection<'_>, page_size: usize) -> Vec<QuadRow> {
        let mut rows: Vec<QuadRow> = Vec::new();
        let (mut from, mut skip) = (0, 0);
        // Running state a real pager keeps: how many triples have started, and
        // the predicate of the triple before the current one.
        let mut triples_so_far = 0u64;
        let mut current: Option<IdTriple> = None;
        let mut previous_predicate = 0u64;
        loop {
            let page: Vec<QuadRow> = quads
                .page(from, skip, page_size)
                .map(Result::unwrap)
                .collect();
            if page.is_empty() {
                return rows;
            }
            for row in &page {
                if current != Some(row.triple) {
                    if let Some(finished) = current {
                        previous_predicate = finished.predicate;
                    }
                    current = Some(row.triple);
                    triples_so_far += 1;
                }
            }
            let last = *page.last().unwrap();
            // Resume at the last row's triple with its delivered count, which
            // is what a cursor records when a page ends inside a run.
            (from, skip) = if quads.subject_object_route().is_some() {
                (previous_predicate, last.delivered + 1)
            } else {
                (triples_so_far - 1, last.delivered + 1)
            };
            rows.extend(page);
        }
    }

    #[test]
    fn scoped_and_quad_views_agree_with_hdtc_for_every_pattern_of_the_small_fixtures() {
        for (source, transpose) in [
            (WORKED_EXAMPLE_NQ, Transpose::None),
            (WORKED_EXAMPLE_NQ, Transpose::Ranks),
            (WORKED_EXAMPLE_NQ, Transpose::Ids),
            (TINY_NQ, Transpose::None),
        ] {
            let bundle = open(source, transpose);
            let patterns = every_pattern(&bundle.perms);
            check(&bundle, &patterns, true);
        }
    }

    #[test]
    fn scoped_and_quad_views_agree_with_hdtc_across_every_encoding() {
        for transpose in [Transpose::None, Transpose::Ids] {
            let bundle = open(&synthetic_quads(), transpose);
            let mut patterns = sampled_patterns(&bundle.perms);
            // Graph 2 is `run` (40 triples) and graph 3 is `sparse` (467).
            for graph in [GraphId(2), GraphId(3)] {
                let thin = subject_object_members(&bundle, graph);
                assert!(!thin.is_empty());
                patterns.extend(thin.into_iter().step_by(7));
            }
            check(&bundle, &patterns, false);
        }
    }

    #[test]
    fn the_worked_example_counts_every_form_of_scope() {
        let bundle = open(WORKED_EXAMPLE_NQ, Transpose::None);
        let all = IdPattern {
            subject: None,
            predicate: None,
            object: None,
        };
        let count = |graph| {
            resolve(&bundle.perms, all)
                .unwrap()
                .in_graph(&bundle.graphs, graph)
                .unwrap()
                .count()
                .unwrap()
        };
        assert_eq!(resolve(&bundle.perms, all).unwrap().count().value, 3);
        assert_eq!(count(GraphId(1)), 2);
        assert_eq!(count(GraphId(2)), 1);
        assert_eq!(count(GraphId::UNNAMED), 2);
        assert_eq!(
            resolve(&bundle.perms, all)
                .unwrap()
                .memberships(&bundle.graphs)
                .unwrap()
                .count()
                .unwrap(),
            5
        );
    }
}
