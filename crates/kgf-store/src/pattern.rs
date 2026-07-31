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
//! Every count but `s ? o` is two rank operations and a subtraction. That is
//! what backs `cardinality.exact = true` on plain patterns, per-binding
//! `/count` at `O(log N)`, and doc 04's invariant that top-level VoID numbers
//! equal `/count` results.

use crate::{IdTriple, error::Result, perm::Permutations};

/// A triple pattern in id space; `None` is a variable.
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
    /// Whether it came from rank arithmetic rather than enumeration.
    ///
    /// Both are exact. This distinguishes the `s ? o` case, which is exact but
    /// costs what enumeration costs — acceptable only because the answer is
    /// bounded by the dataset's distinct predicate count (doc 20 §20.2.1).
    pub arithmetic: bool,
}

/// A resolved pattern: a contiguous range in one permutation, or the bounded
/// group plan `s ? o` requires.
///
/// Resolution is `O(log N)` and enumerates nothing. Borrows the store it was
/// resolved against, so the views it pages through stay valid without being
/// re-projected per row.
#[derive(Debug)]
pub struct Selection<'a> {
    _permutation: Permutation,
    _perms: std::marker::PhantomData<&'a Permutations>,
}

impl Selection<'_> {
    /// Which permutation this reads.
    pub fn permutation(&self) -> Permutation {
        self._permutation
    }

    /// Exact cardinality.
    pub fn count(&self) -> Count {
        todo!("rank difference, or the bounded predicate-group probe for s ? o")
    }

    /// Enumerate `limit` triples from `from`, in the table's order.
    pub fn page(&self, _from: u64, _limit: usize) -> impl Iterator<Item = IdTriple> + '_ {
        std::iter::from_fn(|| todo!("walk the z-range, re-deriving group context by rank"))
    }

    /// The `i`-th triple, for `/sample`. `O(log N)`.
    pub fn at(&self, _i: u64) -> IdTriple {
        todo!("rank/select into the range")
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
pub fn resolve(perms: &Permutations, _pattern: IdPattern) -> Result<Selection<'_>> {
    let _ = perms;
    todo!("dispatch on the bound positions per the §20.2 table")
}
