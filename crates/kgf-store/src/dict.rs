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

use std::num::NonZeroU64;

use crate::error::{Error, Result};
use crate::map::{BytesSpec, Mapping, PackedSpec};
use crate::{Role, TermId};

/// Section sizes, taken from the four PFC sections' own preambles.
///
/// Not from the HDT header: the header is the one part of an HDT that a rewrite
/// may change (which is why identity digests start past it), while each section
/// declares its own term count as a structural fact.
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

    /// Establish the invariant that makes [`len`](Self::len)'s additions total.
    fn validate_role_lengths(&self) -> Result<()> {
        self.shared.checked_add(self.subjects).ok_or_else(|| {
            Error::Region(format!(
                "subject count overflows u64: {} shared + {} subject-only terms",
                self.shared, self.subjects
            ))
        })?;
        self.shared.checked_add(self.objects).ok_or_else(|| {
            Error::Region(format!(
                "object count overflows u64: {} shared + {} object-only terms",
                self.shared, self.objects
            ))
        })?;
        Ok(())
    }
}

/// One of `dictionaryFour`'s four PFC sections.
///
/// A section, not a [`Role`]: the subject and object id spaces each span *two*
/// sections, and which one an id falls in is the arithmetic [`Dictionary`] owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Terms occurring as both subject and object. Ids `1..=shared` in both
    /// spaces, which is what makes the permutations' `ArrayZ` payloads
    /// interchangeable.
    Shared,
    /// Terms occurring only as subjects.
    Subjects,
    /// Predicates.
    Predicates,
    /// Terms occurring only as objects.
    Objects,
}

/// Where one PFC section's parts are, validated at open.
///
/// The block-offset array is a [`PackedSpec`] mapped in place rather than a
/// materialized `Vec`: on a large dictionary it runs to millions of entries, and
/// reading it at open is the cost doc 20 §20.4 forbids.
#[derive(Debug, Clone, Copy)]
pub struct PfcLayout {
    terms: u64,
    block_size: NonZeroU64,
    block_offsets: PackedSpec,
    buffer: BytesSpec,
}

impl PfcLayout {
    /// Validate a scanned PFC section against the mapping it was scanned from.
    ///
    /// The scan is hdtc's ([`hdtc::format::scan_pfc_section`], reached through
    /// [`crate::hdt::HdtLayout::parse`]); this turns its offsets into specs, so
    /// a section that does not fit its file is refused here rather than at the
    /// ten-thousandth request.
    pub fn locate(mapping: &Mapping, section: &hdtc::format::PfcSection) -> Result<Self> {
        let block_offsets = PackedSpec::new(
            mapping,
            section.offsets.data_start,
            section.offsets.num_entries,
            section.offsets.bits_per_entry,
        )?;
        let buffer = BytesSpec::new(mapping, section.buffer_start, section.buffer_length)?;

        // Every id lookup divides by the block size, so it is held as a type
        // that cannot be zero rather than checked at each division. hdtc's scan
        // rejects zero already; this is where that becomes a static fact.
        let block_size = NonZeroU64::new(section.block_size)
            .ok_or_else(|| Error::Region("a PFC section declares block size 0".to_owned()))?;

        Ok(Self {
            terms: section.string_count,
            block_size,
            block_offsets,
            buffer,
        })
    }

    /// Terms in the section; ids within it run `1..=terms()`.
    pub fn terms(&self) -> u64 {
        self.terms
    }

    /// Terms per block. Only a block's first term is stored uncompressed.
    pub fn block_size(&self) -> NonZeroU64 {
        self.block_size
    }

    /// Blocks in the section. The offset array holds one entry per block plus a
    /// sentinel, so this is one less than its length.
    pub fn blocks(&self) -> u64 {
        self.block_offsets.len().saturating_sub(1)
    }

    /// Block start offsets into [`buffer`](Self::buffer), with a sentinel entry
    /// holding the buffer's length.
    pub fn block_offsets(&self) -> &PackedSpec {
        &self.block_offsets
    }

    /// The front-coded string buffer.
    pub fn buffer(&self) -> &BytesSpec {
        &self.buffer
    }
}

/// The four PFC sections of a mapped `data.hdt`, located at open.
#[derive(Debug, Clone)]
pub struct DictionaryLayout {
    counts: DictCounts,
    shared: PfcLayout,
    subjects: PfcLayout,
    predicates: PfcLayout,
    objects: PfcLayout,
}

impl DictionaryLayout {
    /// Assemble the four located sections, deriving and validating their counts.
    pub fn new(
        shared: PfcLayout,
        subjects: PfcLayout,
        predicates: PfcLayout,
        objects: PfcLayout,
    ) -> Result<Self> {
        let counts = DictCounts {
            shared: shared.terms(),
            subjects: subjects.terms(),
            objects: objects.terms(),
            predicates: predicates.terms(),
        };
        counts.validate_role_lengths()?;

        Ok(Self {
            counts,
            shared,
            subjects,
            predicates,
            objects,
        })
    }

    /// Section sizes.
    pub fn counts(&self) -> &DictCounts {
        &self.counts
    }

    /// One section's layout.
    pub fn section(&self, section: Section) -> &PfcLayout {
        match section {
            Section::Shared => &self.shared,
            Section::Subjects => &self.subjects,
            Section::Predicates => &self.predicates,
            Section::Objects => &self.objects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflowing_role_lengths_are_rejected() {
        let subject_overflow = DictCounts {
            shared: u64::MAX,
            subjects: 1,
            objects: 0,
            predicates: 0,
        };
        assert!(subject_overflow.validate_role_lengths().is_err());

        let object_overflow = DictCounts {
            shared: u64::MAX,
            subjects: 0,
            objects: 1,
            predicates: 0,
        };
        assert!(object_overflow.validate_role_lengths().is_err());
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
