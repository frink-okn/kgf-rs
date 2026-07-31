//! The HDT dictionary: id ↔ term, and sorted prefix scans.
//!
//! # No sidecar is needed here
//!
//! Standard HDT already supports everything doc 20 §20.5 asks of the
//! dictionary. Each Plain Front Coding section stores its terms in
//! lexicographic order in blocks of `block_size` (16 by default), preceded by a
//! `LogArray` of block start offsets with a sentinel. So:
//!
//! - [`locate`](Dictionary::locate) is a binary search over block *heads* —
//!   which are stored uncompressed at each block offset — followed by one
//!   in-block scan. `O(log D)` scattered reads, each one page fault at worst.
//! - [`extract`](Dictionary::extract) decodes at most one block: seek to
//!   `id / block_size`, then front-decode up to `block_size` terms.
//! - [`prefix_bounds`](Dictionary::prefix_bounds) falls out of the same search,
//!   because the section is sorted.
//!
//! This is why `data.hdt` stays untouched (invariant 3) and why the read layer
//! needs nothing new for `/terms`, `/describe`, or serialization.
//!
//! # Roles and the shared section
//!
//! `dictionaryFour` splits terms into shared, subjects, objects, and
//! predicates. Subject ids run over shared-then-subjects; object ids run over
//! shared-then-objects; a term in the shared section therefore has the *same*
//! id as a subject and as an object, which is exactly what makes the
//! permutations' `ArrayZ` payloads interchangeable. Callers should not
//! open-code that arithmetic — [`Dictionary`] owns it.

use crate::error::Result;
use crate::{Role, TermId};

/// Section sizes, as recorded in the HDT header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictCounts {
    /// Terms appearing as both subject and object.
    pub shared: u64,
    /// Terms appearing only as subjects.
    pub subjects: u64,
    /// Terms appearing only as objects.
    pub objects: u64,
    /// Predicates.
    pub predicates: u64,
}

impl DictCounts {
    /// Size of a role's id space; ids run `1..=len(role)`.
    pub fn len(&self, role: Role) -> u64 {
        match role {
            Role::Subject => self.shared + self.subjects,
            Role::Object => self.shared + self.objects,
            Role::Predicate => self.predicates,
        }
    }

    /// Whether a subject and an object id denote the same term.
    ///
    /// True exactly when both fall in the shared section, where the two id
    /// spaces coincide.
    pub fn same_term(&self, subject: TermId, object: TermId) -> bool {
        subject == object && subject.0 >= 1 && subject.0 <= self.shared
    }
}

/// A term as it appears in the dictionary: the raw N-Triples-style bytes.
///
/// Literal metadata — language tag, datatype — is parsed from these bytes at
/// the serialization edge. The dictionary imposes no order on suffixes, which
/// is why `o.lang` and `o.dt` filtering is a candidate-budgeted scan rather
/// than a range (doc 03 §3.5 prices it).
pub type TermBytes<'a> = &'a [u8];

/// The four PFC sections of one bundle's dictionary, mapped.
#[derive(Debug)]
pub struct Dictionary {
    _counts: DictCounts,
}

impl Dictionary {
    /// Section sizes.
    pub fn counts(&self) -> &DictCounts {
        &self._counts
    }

    /// Find a term's id in `role`'s space, if present. `O(log D)`.
    pub fn locate(&self, _role: Role, _term: TermBytes<'_>) -> Option<TermId> {
        todo!("binary search block heads, then scan within the block")
    }

    /// Write the term for `id` into `buf` and return the written slice.
    ///
    /// Decodes at most one PFC block. The caller supplies the buffer so that a
    /// page of results costs no allocations.
    pub fn extract<'b>(&self, _role: Role, _id: TermId, _buf: &'b mut Vec<u8>) -> Result<&'b [u8]> {
        todo!("seek to id / block_size and front-decode up to block_size terms")
    }

    /// The half-open id range of terms starting with `prefix`, for `/terms`.
    pub fn prefix_bounds(&self, _role: Role, _prefix: &[u8]) -> std::ops::Range<TermId> {
        todo!("two binary searches over the sorted section")
    }
}
