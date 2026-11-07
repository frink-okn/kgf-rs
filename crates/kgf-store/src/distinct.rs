//! Distinct values of one pattern position, with counts, by walking an index.
//!
//! A pattern's matching triples, grouped by the term at one position and
//! counted. Every one of the twelve (bound positions, grouped position) pairs is
//! answered, by one of four walks, each emitting groups in ascending id of the
//! grouped role so that a cursor naming an id resumes any of them:
//!
//! | pattern | grouped | walk |
//! |---|---|---|
//! | `? ? ?` | s, p, o | level 1 of SPO, POS, OPS: a *run* |
//! | `s ? ?` | p | `s`'s level-2 entries in SPO: a run |
//! | `? p ?` | o | `p`'s level-2 entries in POS: a run |
//! | `? ? o` | p | `o`'s level-2 entries in OPS: a run |
//! | `s p ?`, `? p o` | o, s | a contiguous level-3 range: a run of single triples |
//! | `s ? o` | p | the bounded predicate-group probe |
//! | `s ? ?` | o | *merge* `s`'s level-3 runs in SPO, one per predicate of `s` |
//! | `? ? o` | s | merge `o`'s level-3 runs in OPS, one per predicate of `o` |
//! | `? p ?` | s | merge `p`'s level-3 runs in POS, one per object of `p`, when they are few; else *scan* subjects in SPO for `p` |
//!
//! A run costs one descent and constant work per group, whatever stands behind
//! the groups. A merge of `k` sorted runs costs `k` resume searches and
//! `O(log k)` per entry, and each group takes at most one entry from each run;
//! for the first two merges `k` is a term's predicate degree, bounded by the
//! dataset's predicate count. `? p ?` by subject is the one pair where no
//! permutation of three keeps either the answer contiguous or the fan-in small,
//! so it has two routes that emit the same order: the merge while `p` has at
//! most the caller's fan-in limit of distinct objects, and otherwise a scan of
//! the subjects in id order that probes each for `p` and stops at the caller's
//! candidate budget, reporting how far it got.
//!
//! A pattern whose grouped position is bound is one group, the bound term,
//! which is what a bindings row that binds the grouped variable contributes.
//! [`DistinctPlan`] merges one walk per distinct input row into one ascending
//! sequence, summing counts and counting the rows that reached each group.
//! Identical rows are walked once, weighted by how many rows they stand for,
//! and rows answered by scanning share one scan, so neither duplicates nor
//! scans multiply a page's work.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt;
use std::ops::Range;

use crate::Role;
use crate::error::Result;
use crate::hdt::BitmapTriples;
use crate::pattern::{IdPattern, Selection, resolve, validate_pattern};
use crate::perm::Permutations;

/// The caller's bounds on the two walks whose cost is not set by the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistinctLimits {
    /// Most object runs the request's `? p ?`-by-subject rows merge, summed
    /// over those rows, before the rest scan subjects instead; zero always
    /// scans. Shared rather than per row, so that many rows cannot multiply
    /// the merge's per-page setup.
    pub merge_fan_in: u64,
    /// Subjects one scan examines before it stops and reports how far it got.
    /// At least one, so that every page advances.
    pub candidates: u64,
}

/// One distinct value: a role-scoped term id, the triples holding it, and how
/// many input rows reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DistinctValue {
    /// The grouped term, in the grouped position's id space.
    pub id: u64,
    /// Matching triples holding it at the grouped position. Never zero.
    pub count: u64,
    /// Input rows with at least one such triple.
    pub rows: u32,
}

/// What a page of a plan yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistinctItem {
    /// The next value in ascending id order.
    Value(DistinctValue),
    /// The scan spent its candidate budget. Every value with an id at most
    /// `through` has been yielded, so resuming after `through` loses and
    /// repeats nothing; nothing more comes from this page.
    Exhausted {
        /// The last id the page has fully accounted for.
        through: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    Spo,
    Pos,
    Ops,
}

/// One input row's walk.
#[derive(Debug)]
enum Walk<'a> {
    /// Every level-1 key of one permutation is a group.
    Roots(BitmapTriples<'a>),
    /// The level-2 entries of one range are the groups.
    Second {
        triples: BitmapTriples<'a>,
        y_range: Range<u64>,
    },
    /// The level-3 values of one contiguous range, one triple each.
    Third {
        triples: BitmapTriples<'a>,
        z_range: Range<u64>,
    },
    /// `s ? o`: each predicate the probe finds is a group of one triple.
    Probe(Selection<'a>),
    /// The level-3 runs under one range of level-2 entries, merged.
    Merge {
        triples: BitmapTriples<'a>,
        y_range: Range<u64>,
    },
    /// The grouped position is bound: at most one group.
    Bound(Option<(u64, u64)>),
}

/// The rows of a request answered by scanning subjects for their predicates.
#[derive(Debug)]
struct Scan<'a> {
    spo: BitmapTriples<'a>,
    /// Predicate ids, ascending and distinct, each with the rows binding it.
    targets: Vec<(u64, u32)>,
    candidates: u64,
}

/// A resolved request: one walk per input row, or the shared scan, ready to
/// enumerate from any resume point.
///
/// Resolution costs one descent per row and enumerates nothing. Like a
/// [`Selection`], the plan borrows the permutations it was resolved against.
#[derive(Debug)]
pub struct DistinctPlan<'a> {
    /// One walk per distinct row pattern, with the rows it stands for.
    walks: Vec<(Walk<'a>, u32)>,
    scan: Option<Scan<'a>>,
    triples: u64,
    groups: Option<u64>,
}

impl<'a> DistinctPlan<'a> {
    /// Plan `patterns`, one per input row, each grouped by `group`.
    ///
    /// Ids are validated against their roles first, as [`resolve`] does.
    /// Identical patterns collapse into one walk weighted by their number, in
    /// a fixed order, so the fan-in budget is spent the same way on every page
    /// of one request and a cursor resumes the same plan.
    pub fn new(
        perms: &'a Permutations,
        patterns: impl IntoIterator<Item = IdPattern>,
        group: Role,
        limits: DistinctLimits,
    ) -> Result<Self> {
        let mut collapsed: BTreeMap<PatternKey, u32> = BTreeMap::new();
        for pattern in patterns {
            validate_pattern(perms.dict_counts(), pattern)?;
            let rows = collapsed.entry(PatternKey::of(pattern)).or_default();
            *rows = rows.saturating_add(1);
        }
        let mut walks = Vec::new();
        let mut scanned: Vec<(u64, u32)> = Vec::new();
        let mut triples = 0u64;
        let mut single_run = None;
        let mut fan_in_left = limits.merge_fan_in;
        for (key, rows) in &collapsed {
            let weight = u64::from(*rows);
            match plan_row(perms, key.pattern(), group, &mut fan_in_left)? {
                Planned::Walk(walk, count, exact) => {
                    triples = triples.saturating_add(count.saturating_mul(weight));
                    single_run = exact;
                    walks.push((*walk, *rows));
                }
                Planned::Scan(predicate, count) => {
                    triples = triples.saturating_add(count.saturating_mul(weight));
                    single_run = None;
                    scanned.push((predicate, *rows));
                }
            }
        }
        scanned.sort_unstable();
        let mut targets: Vec<(u64, u32)> = Vec::with_capacity(scanned.len());
        for (predicate, rows) in scanned {
            match targets.last_mut() {
                Some((last, seen)) if *last == predicate => *seen += rows,
                _ => targets.push((predicate, rows)),
            }
        }
        let scan = (!targets.is_empty()).then(|| Scan {
            spo: perms.spo(),
            targets,
            candidates: limits.candidates.max(1),
        });
        // One distinct pattern has the same values however many rows repeat
        // it; several patterns' values are learned only by merging them.
        let groups = match collapsed.len() {
            0 => Some(0),
            1 => single_run,
            _ => None,
        };
        Ok(Self {
            walks,
            scan,
            triples,
            groups,
        })
    }

    /// The exact number of matching triples, summed over every row.
    pub fn triples(&self) -> u64 {
        self.triples
    }

    /// The exact number of distinct values, when it is known without
    /// enumerating: for one row walked as a run, a run width. `None` for a
    /// merge, a scan, or several rows, whose values are learned only by walking
    /// to the end.
    pub fn groups(&self) -> Option<u64> {
        self.groups
    }

    /// Whether a page may end on the candidate budget.
    pub fn scans(&self) -> bool {
        self.scan.is_some()
    }

    /// The values whose id is greater than `after`, in ascending id order; all
    /// of them for `None`.
    pub fn values_after(&self, after: Option<u64>) -> DistinctValues<'_> {
        let mut streams: Vec<(Stream<'_>, u32)> = self
            .walks
            .iter()
            .map(|(walk, rows)| (walk.stream(after), *rows))
            .collect();
        if let Some(scan) = &self.scan {
            // The scan weights its own matches by the rows behind each target.
            streams.push((
                Stream::Scan {
                    scan,
                    next: after.map_or(1, |id| id.saturating_add(1)),
                    remaining: scan.candidates,
                },
                1,
            ));
        }
        DistinctValues::new(streams)
    }
}

enum Planned<'a> {
    /// A walk, the triples it covers, and its group count when that is known
    /// without enumerating.
    Walk(Box<Walk<'a>>, u64, Option<u64>),
    /// `? p ?` grouped by subject, past the fan-in limit: the predicate, and
    /// its triples.
    Scan(u64, u64),
}

/// An [`IdPattern`] as an ordered key, for collapsing identical rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PatternKey(Option<u64>, Option<u64>, Option<u64>);

impl PatternKey {
    fn of(pattern: IdPattern) -> Self {
        Self(pattern.subject, pattern.predicate, pattern.object)
    }

    fn pattern(self) -> IdPattern {
        IdPattern {
            subject: self.0,
            predicate: self.1,
            object: self.2,
        }
    }
}

/// Plan one distinct row. A `? p ?`-by-subject row merges when its object
/// runs fit what is left of the request's fan-in budget, which it then spends,
/// and otherwise joins the shared scan.
fn plan_row<'a>(
    perms: &'a Permutations,
    pattern: IdPattern,
    group: Role,
    fan_in_left: &mut u64,
) -> Result<Planned<'a>> {
    use Role::{Object, Predicate, Subject};
    let bound = |role: Role| match role {
        Subject => pattern.subject,
        Predicate => pattern.predicate,
        Object => pattern.object,
    };
    if let Some(id) = bound(group) {
        let count = resolve(perms, pattern)?.count().value;
        let walk = Walk::Bound((count > 0).then_some((id, count)));
        return Ok(Planned::Walk(
            Box::new(walk),
            count,
            Some(u64::from(count > 0)),
        ));
    }
    let shape = (
        pattern.subject.is_some(),
        pattern.predicate.is_some(),
        pattern.object.is_some(),
        group,
    );
    let key = |root: Root| {
        match root {
            Root::Spo => pattern.subject,
            Root::Pos => pattern.predicate,
            Root::Ops => pattern.object,
        }
        .expect("a rooted walk has its level-1 key bound")
    };
    let second = |root: Root| {
        let triples = permutation(perms, root);
        let y_range = triples.level2_range(key(root));
        let span = triples.z_span(y_range.clone());
        (triples, y_range, span.end - span.start)
    };
    Ok(match shape {
        (false, false, false, _) => {
            let root = match group {
                Subject => Root::Spo,
                Predicate => Root::Pos,
                Object => Root::Ops,
            };
            let triples = permutation(perms, root);
            let groups = triples.level1_count();
            Planned::Walk(
                Box::new(Walk::Roots(triples)),
                perms.triples(),
                Some(groups),
            )
        }
        (true, false, false, Predicate)
        | (false, true, false, Object)
        | (false, false, true, Predicate) => {
            let root = match group {
                Predicate if pattern.subject.is_some() => Root::Spo,
                Predicate => Root::Ops,
                _ => Root::Pos,
            };
            let (triples, y_range, count) = second(root);
            let groups = y_range.end - y_range.start;
            Planned::Walk(
                Box::new(Walk::Second { triples, y_range }),
                count,
                Some(groups),
            )
        }
        (true, true, false, Object) | (false, true, true, Subject) => {
            // `s p ?` reads SPO and `? p o` reads POS, as the pattern table
            // does, so the range is the selection's own.
            let z_range = resolve(perms, pattern)?
                .contiguous_range()
                .expect("a two-bound pattern other than `s ? o` is contiguous");
            let triples = if group == Object {
                perms.spo()
            } else {
                perms.pos()
            };
            let count = z_range.end - z_range.start;
            Planned::Walk(
                Box::new(Walk::Third { triples, z_range }),
                count,
                Some(count),
            )
        }
        (true, false, true, Predicate) => {
            let selection = resolve(perms, pattern)?;
            let count = selection.count().value;
            Planned::Walk(Box::new(Walk::Probe(selection)), count, Some(count))
        }
        (true, false, false, Object) | (false, false, true, Subject) => {
            let root = if group == Object {
                Root::Spo
            } else {
                Root::Ops
            };
            let (triples, y_range, count) = second(root);
            Planned::Walk(Box::new(Walk::Merge { triples, y_range }), count, None)
        }
        (false, true, false, Subject) => {
            let (triples, y_range, count) = second(Root::Pos);
            let fan_in = y_range.end - y_range.start;
            if fan_in <= *fan_in_left {
                *fan_in_left -= fan_in;
                Planned::Walk(Box::new(Walk::Merge { triples, y_range }), count, None)
            } else {
                Planned::Scan(key(Root::Pos), count)
            }
        }
        _ => unreachable!("a bound grouped position is one group, handled above"),
    })
}

fn permutation(perms: &Permutations, root: Root) -> BitmapTriples<'_> {
    match root {
        Root::Spo => perms.spo(),
        Root::Pos => perms.pos(),
        Root::Ops => perms.ops(),
    }
}

impl<'a> Walk<'a> {
    fn stream(&self, after: Option<u64>) -> Stream<'_> {
        match self {
            Walk::Roots(triples) => Stream::Roots {
                triples: *triples,
                next: after.map_or(1, |id| id.saturating_add(1)),
            },
            Walk::Second { triples, y_range } => Stream::Second {
                triples: *triples,
                next: match after {
                    None => y_range.start,
                    Some(id) => triples.level2_upper_bound(y_range.clone(), id),
                },
                end: y_range.end,
            },
            Walk::Third { triples, z_range } => Stream::Third {
                triples: *triples,
                next: after_in_level3(*triples, z_range.clone(), after),
                end: z_range.end,
            },
            // The probe's own resume point is the last predicate returned,
            // with zero for the beginning: predicate ids start at one.
            Walk::Probe(selection) => {
                Stream::Probe(Box::new(selection.page(after.unwrap_or(0), usize::MAX)))
            }
            Walk::Merge { triples, y_range } => {
                let mut heap = BinaryHeap::with_capacity(
                    usize::try_from(y_range.end - y_range.start).unwrap_or(0),
                );
                for y_position in y_range.clone() {
                    let run = triples.level3_range(y_position);
                    let start = after_in_level3(*triples, run.clone(), after);
                    if start < run.end {
                        heap.push(Reverse((triples.level3_at(start), start, run.end)));
                    }
                }
                Stream::Merge {
                    triples: *triples,
                    heap,
                }
            }
            Walk::Bound(group) => {
                Stream::Bound(group.filter(|(id, _)| after.is_none_or(|after| *id > after)))
            }
        }
    }
}

/// The first level-3 position of a sorted run holding a value above `after`.
fn after_in_level3(triples: BitmapTriples<'_>, run: Range<u64>, after: Option<u64>) -> u64 {
    match after {
        None => run.start,
        Some(u64::MAX) => run.end,
        Some(id) => triples.level3_lower_bound(run, id + 1),
    }
}

/// One walk positioned at its resume point.
enum Stream<'p> {
    Roots {
        triples: BitmapTriples<'p>,
        next: u64,
    },
    Second {
        triples: BitmapTriples<'p>,
        next: u64,
        end: u64,
    },
    Third {
        triples: BitmapTriples<'p>,
        next: u64,
        end: u64,
    },
    Probe(Box<dyn Iterator<Item = crate::IdTriple> + 'p>),
    Merge {
        triples: BitmapTriples<'p>,
        /// Each run's next value, its position, and the run's end.
        heap: BinaryHeap<Reverse<(u64, u64, u64)>>,
    },
    Scan {
        scan: &'p Scan<'p>,
        next: u64,
        remaining: u64,
    },
    Bound(Option<(u64, u64)>),
}

impl Stream<'_> {
    fn next(&mut self) -> Option<DistinctItem> {
        let value = |id, count, rows| Some(DistinctItem::Value(DistinctValue { id, count, rows }));
        match self {
            Stream::Roots { triples, next } => {
                if *next == 0 || *next > triples.level1_count() {
                    return None;
                }
                let id = *next;
                *next += 1;
                let span = triples.z_span(triples.level2_range(id));
                value(id, span.end - span.start, 1)
            }
            Stream::Second { triples, next, end } => {
                if *next >= *end {
                    return None;
                }
                let y_position = *next;
                *next += 1;
                let z_range = triples.level3_range(y_position);
                value(
                    triples.level2_at(y_position),
                    z_range.end - z_range.start,
                    1,
                )
            }
            Stream::Third { triples, next, end } => {
                if *next >= *end {
                    return None;
                }
                let z_position = *next;
                *next += 1;
                value(triples.level3_at(z_position), 1, 1)
            }
            Stream::Probe(triples) => triples
                .next()
                .and_then(|triple| value(triple.predicate, 1, 1)),
            Stream::Merge { triples, heap } => {
                let Reverse((id, _, _)) = *heap.peek()?;
                let mut count = 0;
                while let Some(&Reverse((next_id, position, end))) = heap.peek() {
                    if next_id != id {
                        break;
                    }
                    heap.pop();
                    count += 1;
                    if position + 1 < end {
                        heap.push(Reverse((
                            triples.level3_at(position + 1),
                            position + 1,
                            end,
                        )));
                    }
                }
                value(id, count, 1)
            }
            Stream::Scan {
                scan,
                next,
                remaining,
            } => {
                let subjects = scan.spo.level1_count();
                while *next >= 1 && *next <= subjects {
                    if *remaining == 0 {
                        return Some(DistinctItem::Exhausted { through: *next - 1 });
                    }
                    *remaining -= 1;
                    let subject = *next;
                    *next += 1;
                    if let Some((count, rows)) = scan.matches(subject) {
                        return value(subject, count, rows);
                    }
                }
                None
            }
            Stream::Bound(group) => group.take().and_then(|(id, count)| value(id, count, 1)),
        }
    }
}

impl Scan<'_> {
    /// The triples `subject` has under the target predicates, and the rows
    /// those predicates stand for; `None` when it has none.
    ///
    /// Searches whichever list is shorter for the other's entries: the
    /// subject's predicates are sorted in SPO, the targets by construction.
    fn matches(&self, subject: u64) -> Option<(u64, u32)> {
        let y_range = self.spo.level2_range(subject);
        let mut count = 0u64;
        let mut rows = 0u32;
        let mut add = |y_position: u64, multiplicity: u32| {
            let z_range = self.spo.level3_range(y_position);
            count += (z_range.end - z_range.start) * u64::from(multiplicity);
            rows += multiplicity;
        };
        if (self.targets.len() as u64) <= y_range.end - y_range.start {
            for &(predicate, multiplicity) in &self.targets {
                if let Some(y_position) = self.spo.find_level2(y_range.clone(), predicate) {
                    add(y_position, multiplicity);
                }
            }
        } else {
            for y_position in y_range {
                let predicate = self.spo.level2_at(y_position);
                if let Ok(index) = self
                    .targets
                    .binary_search_by_key(&predicate, |&(target, _)| target)
                {
                    add(y_position, self.targets[index].1);
                }
            }
        }
        (count > 0).then_some((count, rows))
    }
}

/// A plan's values from a resume point: every row's stream merged by id.
///
/// Each stream ascends strictly, so a heap keyed on each stream's next id
/// yields the union in order, and every value takes at most one entry from
/// each stream. A stream that spends its budget bounds how far the page may
/// go: values up to that point are still yielded, then the page ends.
pub struct DistinctValues<'p> {
    streams: Vec<Stream<'p>>,
    /// The rows each stream stands for, which its counts are multiplied by.
    weights: Vec<u32>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
    pending: Vec<(u64, u32)>,
    through: Option<u64>,
    done: bool,
}

impl fmt::Debug for DistinctValues<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DistinctValues")
            .field("streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}

impl<'p> DistinctValues<'p> {
    fn new(weighted: Vec<(Stream<'p>, u32)>) -> Self {
        let (streams, weights): (Vec<_>, Vec<_>) = weighted.into_iter().unzip();
        let mut values = Self {
            pending: vec![(0, 0); streams.len()],
            heap: BinaryHeap::with_capacity(streams.len()),
            streams,
            weights,
            through: None,
            done: false,
        };
        for index in 0..values.streams.len() {
            values.pull(index);
        }
        values
    }

    fn pull(&mut self, index: usize) {
        match self.streams[index].next() {
            Some(DistinctItem::Value(value)) => {
                let weight = self.weights[index];
                self.pending[index] = (
                    value.count.saturating_mul(u64::from(weight)),
                    value.rows.saturating_mul(weight),
                );
                self.heap.push(Reverse((value.id, index)));
            }
            Some(DistinctItem::Exhausted { through }) => {
                self.through = Some(self.through.map_or(through, |seen| seen.min(through)));
            }
            None => {}
        }
    }
}

impl Iterator for DistinctValues<'_> {
    type Item = DistinctItem;

    fn next(&mut self) -> Option<DistinctItem> {
        if self.done {
            return None;
        }
        let next = self.heap.peek().map(|&Reverse((id, _))| id);
        match (next, self.through) {
            (Some(id), Some(through)) if id > through => {
                self.done = true;
                return Some(DistinctItem::Exhausted { through });
            }
            (None, Some(through)) => {
                self.done = true;
                return Some(DistinctItem::Exhausted { through });
            }
            (None, None) => {
                self.done = true;
                return None;
            }
            _ => {}
        }
        let id = next.expect("a value is pending");
        let mut merged = DistinctValue {
            id,
            count: 0,
            rows: 0,
        };
        while let Some(&Reverse((next_id, index))) = self.heap.peek() {
            if next_id != id {
                break;
            }
            self.heap.pop();
            let (count, rows) = self.pending[index];
            merged.count += count;
            merged.rows += rows;
            self.pull(index);
        }
        Some(DistinctItem::Value(merged))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    use super::*;
    use crate::IdTriple;
    use crate::testing::{Fixture, TINY_NT};

    const ROLES: [Role; 3] = [Role::Subject, Role::Predicate, Role::Object];

    /// Room for every walk, so a fixture's answers come back in one go.
    const AMPLE: DistinctLimits = DistinctLimits {
        merge_fan_in: u64::MAX,
        candidates: u64::MAX,
    };

    fn all_triples(perms: &Permutations) -> Vec<IdTriple> {
        resolve(perms, unbound())
            .unwrap()
            .page(0, usize::MAX)
            .collect()
    }

    fn unbound() -> IdPattern {
        IdPattern {
            subject: None,
            predicate: None,
            object: None,
        }
    }

    fn at(triple: IdTriple, role: Role) -> u64 {
        match role {
            Role::Subject => triple.subject,
            Role::Predicate => triple.predicate,
            Role::Object => triple.object,
        }
    }

    fn matches(pattern: IdPattern, triple: IdTriple) -> bool {
        pattern.subject.is_none_or(|id| id == triple.subject)
            && pattern.predicate.is_none_or(|id| id == triple.predicate)
            && pattern.object.is_none_or(|id| id == triple.object)
    }

    /// The answer computed the slow way: filter every triple for every row,
    /// then group, counting each row once per value it reaches.
    fn expected(source: &[IdTriple], patterns: &[IdPattern], group: Role) -> Vec<DistinctValue> {
        let mut values: BTreeMap<u64, (u64, u32)> = BTreeMap::new();
        for pattern in patterns {
            let mut reached = std::collections::BTreeSet::new();
            for triple in source.iter().filter(|triple| matches(*pattern, **triple)) {
                let id = at(*triple, group);
                values.entry(id).or_default().0 += 1;
                reached.insert(id);
            }
            for id in reached {
                values.entry(id).or_default().1 += 1;
            }
        }
        values
            .into_iter()
            .map(|(id, (count, rows))| DistinctValue { id, count, rows })
            .collect()
    }

    /// Every value of a plan from `after`, following each budget stop to its
    /// end, and how many stops it took.
    fn walk_all(plan: &DistinctPlan<'_>, mut after: Option<u64>) -> (Vec<DistinctValue>, usize) {
        let mut values = Vec::new();
        let mut stops = 0;
        loop {
            let mut stopped = None;
            for item in plan.values_after(after) {
                match item {
                    DistinctItem::Value(value) => values.push(value),
                    DistinctItem::Exhausted { through } => stopped = Some(through),
                }
            }
            match stopped {
                None => return (values, stops),
                Some(through) => {
                    assert!(after.is_none_or(|after| through > after), "a stop advances");
                    stops += 1;
                    after = Some(through);
                }
            }
        }
    }

    fn options(maximum: u64) -> Vec<Option<u64>> {
        std::iter::once(None)
            .chain((1..=maximum).map(Some))
            .collect()
    }

    fn every_pattern(perms: &Permutations) -> Vec<IdPattern> {
        let counts = perms.dict_counts();
        let mut patterns = Vec::new();
        for subject in options(counts.len(Role::Subject)) {
            for predicate in options(counts.len(Role::Predicate)) {
                for object in options(counts.len(Role::Object)) {
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

    /// Every pattern, every grouped position, and three settings of the limits:
    /// merging always, scanning whenever it can with a budget of one and of
    /// three subjects a page. Each must give the slow answer, resume after
    /// every value and between values, and report exact counts where it says
    /// it knows them.
    fn check_every_pattern(source_nt: &str) {
        let fixture = Fixture::build(source_nt);
        let perms =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let source = all_triples(&perms);
        let settings = [
            AMPLE,
            DistinctLimits {
                merge_fan_in: 0,
                candidates: 1,
            },
            DistinctLimits {
                merge_fan_in: 0,
                candidates: 3,
            },
        ];
        let mut scanned = 0;
        for pattern in every_pattern(&perms) {
            for group in ROLES {
                let want = expected(&source, &[pattern], group);
                for limits in settings {
                    let plan = DistinctPlan::new(&perms, [pattern], group, limits).unwrap();
                    scanned += usize::from(plan.scans());
                    assert_eq!(
                        plan.triples(),
                        want.iter().map(|value| value.count).sum::<u64>(),
                        "{pattern:?} by {group:?}"
                    );
                    if let Some(groups) = plan.groups() {
                        assert_eq!(groups, want.len() as u64, "{pattern:?} by {group:?}");
                    }
                    let (all, _) = walk_all(&plan, None);
                    assert_eq!(all, want, "{pattern:?} by {group:?}, {limits:?}");
                    for (index, value) in want.iter().enumerate() {
                        let (rest, _) = walk_all(&plan, Some(value.id));
                        assert_eq!(rest, want[index + 1..], "{pattern:?} after {value:?}");
                        if value.id > 1 {
                            let (between, _) = walk_all(&plan, Some(value.id - 1));
                            assert_eq!(between, want[index..], "{pattern:?} before {value:?}");
                        }
                    }
                    assert_eq!(walk_all(&plan, Some(u64::MAX)).0, []);
                }
            }
        }
        assert!(scanned > 0, "the scan route was exercised");
    }

    #[test]
    fn every_pattern_and_position_agrees_with_grouping_the_triples() {
        check_every_pattern(TINY_NT);
    }

    #[test]
    fn hubs_shared_terms_and_multi_valued_predicates_group_like_their_triples() {
        // A predicate with several objects of uneven multiplicity, a hub
        // subject, a subject reaching one object through two predicates, and a
        // literal reached through two predicates.
        let mut source = String::new();
        for index in 0..12 {
            writeln!(
                source,
                "<http://example.org/s/{index}> <http://example.org/status> \"{}\" .",
                ["a", "b", "c"][index % 3 * (index % 2)]
            )
            .unwrap();
            writeln!(
                source,
                "<http://example.org/hub> <http://example.org/links> <http://example.org/s/{index}> ."
            )
            .unwrap();
        }
        source.push_str(
            "<http://example.org/hub> <http://example.org/likes> <http://example.org/s/3> .\n",
        );
        source.push_str("<http://example.org/s/1> <http://example.org/note> \"a\" .\n");
        check_every_pattern(&source);
    }

    #[test]
    fn rows_merge_sum_counts_and_count_the_rows_that_reach_each_value() {
        let fixture = Fixture::build(TINY_NT);
        let perms =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let source = all_triples(&perms);
        let counts = perms.dict_counts();
        let subjects: Vec<IdPattern> = (1..=counts.len(Role::Subject))
            .map(|subject| IdPattern {
                subject: Some(subject),
                ..unbound()
            })
            .collect();
        let predicates: Vec<IdPattern> = (1..=counts.len(Role::Predicate))
            .map(|predicate| IdPattern {
                predicate: Some(predicate),
                ..unbound()
            })
            .collect();
        // Subjects grouped by predicate (runs) and by object (merges);
        // predicates grouped by subject, merged and scanned; each with every
        // row twice, as a duplicated VALUES row would be.
        for (rows, group) in [
            (&subjects, Role::Predicate),
            (&subjects, Role::Object),
            (&predicates, Role::Subject),
        ] {
            for duplicated in [false, true] {
                let mut rows = rows.clone();
                if duplicated {
                    rows.extend(rows.clone());
                }
                let want = expected(&source, &rows, group);
                for limits in [
                    AMPLE,
                    DistinctLimits {
                        merge_fan_in: 0,
                        candidates: 2,
                    },
                    // Enough for some rows' object runs and not all of them, so
                    // merges and the scan meet in one request.
                    DistinctLimits {
                        merge_fan_in: 3,
                        candidates: 2,
                    },
                ] {
                    let plan =
                        DistinctPlan::new(&perms, rows.iter().copied(), group, limits).unwrap();
                    assert_eq!(plan.groups(), None);
                    assert_eq!(
                        plan.triples(),
                        want.iter().map(|value| value.count).sum::<u64>()
                    );
                    let (all, _) = walk_all(&plan, None);
                    assert_eq!(all, want, "{group:?}, duplicated {duplicated}, {limits:?}");
                    for (index, value) in want.iter().enumerate() {
                        assert_eq!(walk_all(&plan, Some(value.id)).0, want[index + 1..]);
                    }
                }
            }
        }
    }

    #[test]
    fn identical_rows_walk_once_and_rows_share_one_fan_in_budget() {
        let fixture = Fixture::build(TINY_NT);
        let perms =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let source = all_triples(&perms);
        let predicates = perms.dict_counts().len(Role::Predicate);
        let row = |predicate| IdPattern {
            predicate: Some(predicate),
            ..unbound()
        };

        // A thousand copies of one row are one walk, weighted a thousand times.
        let copies = vec![row(1); 1000];
        let plan = DistinctPlan::new(&perms, copies.iter().copied(), Role::Subject, AMPLE).unwrap();
        assert_eq!(plan.walks.len(), 1);
        assert_eq!(plan.walks[0].1, 1000);
        assert_eq!(
            walk_all(&plan, None).0,
            expected(&source, &copies, Role::Subject)
        );

        // Every predicate's subjects: each row's object runs fit a budget of
        // three, but not all of them together, so some rows scan.
        let every: Vec<_> = (1..=predicates).map(row).collect();
        let budget = DistinctLimits {
            merge_fan_in: 3,
            candidates: u64::MAX,
        };
        let plan = DistinctPlan::new(&perms, every.iter().copied(), Role::Subject, budget).unwrap();
        let merged_runs: u64 = plan
            .walks
            .iter()
            .map(|(walk, _)| match walk {
                Walk::Merge { y_range, .. } => y_range.end - y_range.start,
                _ => 0,
            })
            .sum();
        assert!(merged_runs <= 3, "{merged_runs} runs merged");
        assert!(plan.scans());
        assert_eq!(
            walk_all(&plan, None).0,
            expected(&source, &every, Role::Subject)
        );
    }

    #[test]
    fn a_scan_stops_at_its_budget_and_says_how_far_it_got() {
        let fixture = Fixture::build(TINY_NT);
        let perms =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let subjects = perms.dict_counts().len(Role::Subject);
        let label = 1;
        let plan = DistinctPlan::new(
            &perms,
            [IdPattern {
                predicate: Some(label),
                ..unbound()
            }],
            Role::Subject,
            DistinctLimits {
                merge_fan_in: 0,
                candidates: 1,
            },
        )
        .unwrap();
        assert!(plan.scans());
        // One subject a page: every page but the last ends at its budget,
        // having accounted for exactly one more subject.
        let (_, stops) = walk_all(&plan, None);
        assert_eq!(stops as u64, subjects - 1);
    }

    #[test]
    fn ids_outside_their_role_are_refused_before_any_walk() {
        let fixture = Fixture::build(TINY_NT);
        let perms =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let maximum = perms.dict_counts().len(Role::Predicate);
        let pattern = IdPattern {
            predicate: Some(maximum + 1),
            ..unbound()
        };
        assert!(DistinctPlan::new(&perms, [pattern], Role::Object, AMPLE).is_err());
    }
}
