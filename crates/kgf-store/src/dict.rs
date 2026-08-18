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
//!   because each section is sorted. Subject and object roles may produce one
//!   range in each of their two sections.
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

use std::cmp::Ordering;
use std::num::NonZeroU64;
use std::ops::Range;

use crate::error::{Error, Result};
use crate::map::{BytesSpec, Mapping, PackedArray, PackedSpec};
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

    /// Convert a role-scoped HDT id to its section and one-based local id.
    ///
    /// The inverse is [`role_id`](Self::role_id). Keeping both conversions
    /// here prevents HTTP and serialization layers from duplicating the
    /// shared-section arithmetic owned by the dictionary.
    pub fn section_id(&self, role: Role, id: TermId) -> Result<SectionTermId> {
        let maximum = self.len(role);
        if id.0 == 0 || id.0 > maximum {
            return Err(Error::TermIdOutOfRange {
                role,
                id: id.0,
                maximum,
            });
        }
        let (section, local_id) = match role {
            Role::Predicate => (Section::Predicates, id.0),
            Role::Subject if id.0 <= self.shared => (Section::Shared, id.0),
            Role::Subject => (Section::Subjects, id.0 - self.shared),
            Role::Object if id.0 <= self.shared => (Section::Shared, id.0),
            Role::Object => (Section::Objects, id.0 - self.shared),
        };
        Ok(SectionTermId::new(section, local_id)
            .expect("a validated role id maps to a nonzero section-local id"))
    }

    /// Convert a section-local id into `role`'s id space when that section is
    /// part of the role and the local id exists.
    pub fn role_id(&self, role: Role, id: SectionTermId) -> Option<TermId> {
        let local_id = id.local_id();
        let in_section = match id.section() {
            Section::Shared => local_id <= self.shared,
            Section::Subjects => local_id <= self.subjects,
            Section::Predicates => local_id <= self.predicates,
            Section::Objects => local_id <= self.objects,
        };
        if !in_section {
            return None;
        }
        match (role, id.section()) {
            (Role::Subject | Role::Object, Section::Shared)
            | (Role::Predicate, Section::Predicates) => Some(TermId(local_id)),
            (Role::Subject, Section::Subjects) | (Role::Object, Section::Objects) => {
                self.shared.checked_add(local_id).map(TermId)
            }
            _ => None,
        }
    }

    /// Establish the invariant that makes [`len`](Self::len)'s additions total.
    fn validate_role_lengths(&self) -> Result<()> {
        let subjects = self.shared.checked_add(self.subjects).ok_or_else(|| {
            Error::Region(format!(
                "subject count overflows u64: {} shared + {} subject-only terms",
                self.shared, self.subjects
            ))
        })?;
        let objects = self.shared.checked_add(self.objects).ok_or_else(|| {
            Error::Region(format!(
                "object count overflows u64: {} shared + {} object-only terms",
                self.shared, self.objects
            ))
        })?;
        for (role, terms) in [
            ("subject", subjects),
            ("predicate", self.predicates),
            ("object", objects),
        ] {
            if terms == u64::MAX {
                return Err(Error::Region(format!(
                    "{role} count {terms} leaves no representable one-past id"
                )));
            }
        }
        Ok(())
    }
}

/// One of `dictionaryFour`'s four PFC sections.
///
/// A section, not a [`Role`]: the subject and object id spaces each span *two*
/// sections, and which one an id falls in is the arithmetic [`Dictionary`] owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// A one-based term id scoped to one `dictionaryFour` section.
///
/// Unlike [`TermId`], this identifier is independent of a subject or object
/// role. That makes a shared term one identifier even when it occurs in both
/// positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SectionTermId {
    section: Section,
    local_id: NonZeroU64,
}

impl SectionTermId {
    /// Construct a section-local id, rejecting zero because HDT ids are
    /// one-based.
    pub fn new(section: Section, local_id: u64) -> Option<Self> {
        Some(Self {
            section,
            local_id: NonZeroU64::new(local_id)?,
        })
    }

    /// The dictionary section that scopes this integer.
    pub fn section(self) -> Section {
        self.section
    }

    /// The one-based integer within [`section`](Self::section).
    pub fn local_id(self) -> u64 {
        self.local_id.get()
    }
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

    /// Project this validated layout onto its HDT mapping.
    fn view<'a>(&self, mapping: &'a Mapping) -> PfcView<'a> {
        PfcView {
            terms: self.terms,
            block_size: self.block_size,
            block_offsets: self.block_offsets.view(mapping),
            buffer: self.buffer.view(mapping),
        }
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

    /// Project the four validated layouts onto their HDT mapping.
    pub fn view<'a>(&self, mapping: &'a Mapping) -> Dictionary<'a> {
        Dictionary {
            counts: self.counts,
            shared: self.shared.view(mapping),
            subjects: self.subjects.view(mapping),
            predicates: self.predicates.view(mapping),
            objects: self.objects.view(mapping),
        }
    }
}

/// A term as it appears in the HDT dictionary: its raw lexical bytes.
///
/// Literal metadata — language tag, datatype — is parsed from these bytes at
/// the serialization edge. The dictionary imposes no order on suffixes, which
/// is why `o.lang` and `o.dt` filtering is a candidate-budgeted scan rather
/// than a range (doc 03 §3.5 prices it).
pub type TermBytes<'a> = &'a [u8];

/// A projected random-access view of one PFC section.
///
/// Block offsets remain packed in the mapping. A lookup reads only the
/// `O(log blocks)` heads it probes and decodes at most one block.
#[derive(Debug, Clone, Copy)]
struct PfcView<'a> {
    terms: u64,
    block_size: NonZeroU64,
    block_offsets: PackedArray<'a>,
    buffer: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
struct Search {
    position: u64,
    equal: bool,
}

impl PfcView<'_> {
    fn blocks(&self) -> u64 {
        self.block_offsets.len() - 1
    }

    fn block(&self, block: u64) -> Result<&[u8]> {
        if block >= self.blocks() {
            return Err(Error::Region(format!(
                "PFC block {block} is out of range for {} blocks",
                self.blocks()
            )));
        }
        let start = self.block_offsets.get(block);
        let end = self.block_offsets.get(block + 1);
        if start > end || end > self.buffer.len() as u64 {
            return Err(Error::Region(format!(
                "PFC block {block} has invalid buffer range [{start}, {end}) for {} bytes",
                self.buffer.len()
            )));
        }
        Ok(&self.buffer[start as usize..end as usize])
    }

    fn compare_block_head(&self, block: u64, key: &[u8]) -> Result<Ordering> {
        compare_terminated_head(self.block(block)?, key, block)
    }

    /// Find the first zero-based position whose term is not less than `key`.
    fn search(&self, key: &[u8], scratch: &mut Vec<u8>) -> Result<Search> {
        if self.terms == 0 {
            return Ok(Search {
                position: 0,
                equal: false,
            });
        }

        // Upper-bound the block heads, then search the block immediately to
        // the left. Its head is the last one <= key; if no such head exists,
        // the first dictionary term is already the lower bound.
        let mut low = 0;
        let mut high = self.blocks();
        while low < high {
            let middle = low + (high - low) / 2;
            if self.compare_block_head(middle, key)? != Ordering::Greater {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low == 0 {
            return Ok(Search {
                position: 0,
                equal: false,
            });
        }

        let block_index = low - 1;
        let base = block_index * self.block_size.get();
        let entries = (self.terms - base).min(self.block_size.get());
        let block = self.block(block_index)?;
        let mut cursor = 0;
        scratch.clear();
        for entry in 0..entries {
            decode_next(block, &mut cursor, scratch, entry == 0, block_index)?;
            match scratch.as_slice().cmp(key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Ok(Search {
                        position: base + entry,
                        equal: true,
                    });
                }
                Ordering::Greater => {
                    return Ok(Search {
                        position: base + entry,
                        equal: false,
                    });
                }
            }
        }
        Ok(Search {
            position: base + entries,
            equal: false,
        })
    }

    fn extract_position<'b>(&self, position: u64, buf: &'b mut Vec<u8>) -> Result<&'b [u8]> {
        assert!(position < self.terms, "validated local dictionary position");
        let block_index = position / self.block_size.get();
        let entry_in_block = position % self.block_size.get();
        let block = self.block(block_index)?;
        let mut cursor = 0;
        buf.clear();
        for entry in 0..=entry_in_block {
            decode_next(block, &mut cursor, buf, entry == 0, block_index)?;
        }
        Ok(buf)
    }

    fn prefix_positions(&self, prefix: &[u8], scratch: &mut Vec<u8>) -> Result<Range<u64>> {
        let start = self.search(prefix, scratch)?.position;
        let end = match prefix_successor(prefix) {
            Some(successor) => self.search(&successor, scratch)?.position,
            None => self.terms,
        };
        Ok(start..end)
    }
}

fn terminator(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&byte| byte == 0)
}

/// Compare a block's verbatim, null-terminated head with `key` without first
/// scanning the whole head. Request terms are capped, while a legal stored
/// literal can be megabytes, so a binary-search probe must stop as soon as the
/// ordering is known.
fn compare_terminated_head(block: &[u8], key: &[u8], block_index: u64) -> Result<Ordering> {
    for (index, &key_byte) in key.iter().enumerate() {
        let head_byte = *block.get(index).ok_or_else(|| {
            Error::Region(format!(
                "PFC block {block_index} head ends without a null terminator"
            ))
        })?;
        if head_byte == 0 {
            return Ok(Ordering::Less);
        }
        match head_byte.cmp(&key_byte) {
            Ordering::Equal => {}
            ordering => return Ok(ordering),
        }
    }

    match block.get(key.len()) {
        Some(0) => Ok(Ordering::Equal),
        Some(_) => Ok(Ordering::Greater),
        None => Err(Error::Region(format!(
            "PFC block {block_index} head ends without a null terminator"
        ))),
    }
}

fn decode_next(
    block: &[u8],
    cursor: &mut usize,
    value: &mut Vec<u8>,
    first: bool,
    block_index: u64,
) -> Result<()> {
    if *cursor >= block.len() {
        return Err(Error::Region(format!(
            "PFC block {block_index} ends before all declared terms"
        )));
    }

    let shared = if first {
        value.clear();
        0
    } else {
        let (shared, consumed) =
            hdtc::format::decode_vbyte(&block[*cursor..]).map_err(|error| {
                Error::Region(format!(
                    "PFC block {block_index} has an invalid shared-prefix VByte: {error}"
                ))
            })?;
        *cursor += consumed;
        usize::try_from(shared).map_err(|_| {
            Error::Region(format!(
                "PFC block {block_index} shared-prefix length does not fit usize"
            ))
        })?
    };

    if shared > value.len() {
        return Err(Error::Region(format!(
            "PFC block {block_index} shares {shared} bytes with a {}-byte predecessor",
            value.len()
        )));
    }
    let suffix_end = terminator(&block[*cursor..]).ok_or_else(|| {
        Error::Region(format!(
            "PFC block {block_index} term has no null terminator"
        ))
    })?;
    value.truncate(shared);
    value.extend_from_slice(&block[*cursor..*cursor + suffix_end]);
    *cursor += suffix_end + 1;
    Ok(())
}

/// The smallest byte string strictly above every string starting with
/// `prefix`, or `None` when no finite upper bound exists.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut successor = prefix.to_vec();
    let last = successor.iter().rposition(|&byte| byte != u8::MAX)?;
    successor[last] += 1;
    successor.truncate(last + 1);
    Some(successor)
}

/// The one or two id ranges in a role that start with a prefix.
///
/// Subject and object ids concatenate the shared and role-only PFC sections,
/// but that concatenation is not globally lexicographic. A prefix can therefore
/// occupy one range in each section. Predicate ids have at most one range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixBounds {
    first: Option<Range<TermId>>,
    second: Option<Range<TermId>>,
}

impl PrefixBounds {
    fn new(first: Range<TermId>, second: Option<Range<TermId>>) -> Self {
        let mut ranges = [Some(first), second]
            .into_iter()
            .flatten()
            .filter(|range| range.start != range.end);
        Self {
            first: ranges.next(),
            second: ranges.next(),
        }
    }

    /// The non-empty half-open ranges, in HDT id-space order.
    pub fn ranges(&self) -> impl Iterator<Item = &Range<TermId>> {
        self.first.iter().chain(self.second.iter())
    }

    /// Exact number of matching ids, without enumerating them.
    pub fn count(&self) -> u64 {
        self.ranges().map(|range| range.end.0 - range.start.0).sum()
    }

    /// Whether no term in the role starts with the prefix.
    pub fn is_empty(&self) -> bool {
        self.first.is_none()
    }
}

/// The four PFC sections of one bundle's dictionary, projected from its
/// validated layout for the duration of a request.
#[derive(Debug, Clone, Copy)]
pub struct Dictionary<'a> {
    counts: DictCounts,
    shared: PfcView<'a>,
    subjects: PfcView<'a>,
    predicates: PfcView<'a>,
    objects: PfcView<'a>,
}

impl Dictionary<'_> {
    /// Section sizes.
    pub fn counts(&self) -> &DictCounts {
        &self.counts
    }

    /// Find a term's id in `role`'s space, if present. `O(log D)`.
    pub fn locate(&self, role: Role, term: TermBytes<'_>) -> Result<Option<TermId>> {
        let mut scratch = Vec::new();
        match role {
            Role::Predicate => locate_in(self.predicates, term, 0, &mut scratch),
            Role::Subject | Role::Object => {
                if let Some(id) = locate_in(self.shared, term, 0, &mut scratch)? {
                    return Ok(Some(id));
                }
                let section = match role {
                    Role::Subject => self.subjects,
                    Role::Object => self.objects,
                    Role::Predicate => unreachable!(),
                };
                locate_in(section, term, self.counts.shared, &mut scratch)
            }
        }
    }

    /// Write the term for `id` into `buf` and return the written slice.
    ///
    /// Decodes at most one PFC block. The caller supplies the buffer so that a
    /// page of results costs no allocations.
    pub fn extract<'b>(&self, role: Role, id: TermId, buf: &'b mut Vec<u8>) -> Result<&'b [u8]> {
        let maximum = self.counts.len(role);
        if id.0 == 0 || id.0 > maximum {
            return Err(Error::TermIdOutOfRange {
                role,
                id: id.0,
                maximum,
            });
        }

        let (section, local_id) = match role {
            Role::Predicate => (self.predicates, id.0),
            Role::Subject if id.0 <= self.counts.shared => (self.shared, id.0),
            Role::Subject => (self.subjects, id.0 - self.counts.shared),
            Role::Object if id.0 <= self.counts.shared => (self.shared, id.0),
            Role::Object => (self.objects, id.0 - self.counts.shared),
        };
        section.extract_position(local_id - 1, buf)
    }

    /// The one or two half-open id ranges of terms starting with `prefix`.
    ///
    /// Each PFC section takes two `O(log D)` searches. Subject and object roles
    /// can yield two ranges because their shared and role-only sections are
    /// sorted independently.
    pub fn prefix_bounds(&self, role: Role, prefix: &[u8]) -> Result<PrefixBounds> {
        let mut scratch = Vec::new();
        match role {
            Role::Predicate => {
                let range = self.predicates.prefix_positions(prefix, &mut scratch)?;
                Ok(PrefixBounds::new(global_range(0, range)?, None))
            }
            Role::Subject | Role::Object => {
                let shared = self.shared.prefix_positions(prefix, &mut scratch)?;
                let section = match role {
                    Role::Subject => self.subjects,
                    Role::Object => self.objects,
                    Role::Predicate => unreachable!(),
                };
                let role_only = section.prefix_positions(prefix, &mut scratch)?;
                Ok(PrefixBounds::new(
                    global_range(0, shared)?,
                    Some(global_range(self.counts.shared, role_only)?),
                ))
            }
        }
    }
}

fn locate_in(
    section: PfcView<'_>,
    term: &[u8],
    id_offset: u64,
    scratch: &mut Vec<u8>,
) -> Result<Option<TermId>> {
    let search = section.search(term, scratch)?;
    if !search.equal {
        return Ok(None);
    }
    let id = id_offset
        .checked_add(search.position)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::Region("dictionary id overflows u64".to_owned()))?;
    Ok(Some(TermId(id)))
}

fn global_range(id_offset: u64, positions: Range<u64>) -> Result<Range<TermId>> {
    let to_id = |position: u64| {
        id_offset
            .checked_add(position)
            .and_then(|value| value.checked_add(1))
            .map(TermId)
            .ok_or_else(|| Error::Region("dictionary range endpoint overflows u64".to_owned()))
    };
    Ok(to_id(positions.start)?..to_id(positions.end)?)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Seek, SeekFrom};

    use super::*;
    use crate::hdt::HdtLayout;
    use crate::testing::{Fixture, TINY_NT};

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

        let no_one_past_predicate = DictCounts {
            shared: 0,
            subjects: 0,
            objects: 0,
            predicates: u64::MAX,
        };
        assert!(no_one_past_predicate.validate_role_lengths().is_err());
    }

    #[test]
    fn section_local_ids_invert_role_ids_without_conflating_sections() {
        let counts = DictCounts {
            shared: 3,
            subjects: 4,
            objects: 5,
            predicates: 2,
        };

        let shared = counts.section_id(Role::Subject, TermId(2)).unwrap();
        assert_eq!(shared, SectionTermId::new(Section::Shared, 2).unwrap());
        assert_eq!(counts.role_id(Role::Subject, shared), Some(TermId(2)));
        assert_eq!(counts.role_id(Role::Object, shared), Some(TermId(2)));

        let subject = counts.section_id(Role::Subject, TermId(5)).unwrap();
        assert_eq!(subject, SectionTermId::new(Section::Subjects, 2).unwrap());
        assert_eq!(counts.role_id(Role::Subject, subject), Some(TermId(5)));
        assert_eq!(counts.role_id(Role::Object, subject), None);

        let object = counts.section_id(Role::Object, TermId(6)).unwrap();
        assert_eq!(object, SectionTermId::new(Section::Objects, 3).unwrap());
        assert_eq!(counts.role_id(Role::Object, object), Some(TermId(6)));
        assert_eq!(counts.role_id(Role::Subject, object), None);

        assert_eq!(
            counts.role_id(
                Role::Subject,
                SectionTermId::new(Section::Subjects, 5).unwrap()
            ),
            None
        );
    }

    #[test]
    fn block_head_comparison_stops_when_order_is_known() {
        // The missing terminator is a canary: payload verification is off the
        // query path, which must not inspect bytes after the decisive first
        // byte. A valid head may have an equally large tail before its terminator.
        let long_tail = vec![b'z'; 1024 * 1024];
        assert_eq!(
            compare_terminated_head(&long_tail, b"a", 0).unwrap(),
            Ordering::Greater
        );

        assert_eq!(
            compare_terminated_head(b"alpha\0suffix", b"alpha", 0).unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_terminated_head(b"alpha\0suffix", b"alphabet", 0).unwrap(),
            Ordering::Less
        );
    }

    #[test]
    fn every_id_and_term_matches_hdtcs_sequential_dictionary_reader() {
        let mut source = TINY_NT.to_owned();
        for index in 0..40 {
            source.push_str(&format!(
                "<http://example.org/s{index:02}> <http://example.org/many> \"value{index:02}\" .\n"
            ));
        }
        let fixture = Fixture::build(&source);
        let expected = sequential_sections(&fixture);
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);
        let shared = expected[Section::Shared as usize].len() as u64;

        assert_terms(
            &dictionary,
            Role::Subject,
            0,
            &expected[Section::Shared as usize],
        );
        assert_terms(
            &dictionary,
            Role::Object,
            0,
            &expected[Section::Shared as usize],
        );
        assert_terms(
            &dictionary,
            Role::Subject,
            shared,
            &expected[Section::Subjects as usize],
        );
        assert_terms(
            &dictionary,
            Role::Predicate,
            0,
            &expected[Section::Predicates as usize],
        );
        assert_terms(
            &dictionary,
            Role::Object,
            shared,
            &expected[Section::Objects as usize],
        );

        for role in [Role::Subject, Role::Predicate, Role::Object] {
            assert_eq!(
                dictionary
                    .locate(role, b"<http://example.org/not-present>")
                    .unwrap(),
                None
            );
            assert!(
                dictionary
                    .extract(role, TermId(0), &mut Vec::new())
                    .is_err()
            );
            assert!(
                dictionary
                    .extract(
                        role,
                        TermId(dictionary.counts().len(role) + 1),
                        &mut Vec::new()
                    )
                    .is_err()
            );
        }

        let subject_terms = role_terms(&expected, Role::Subject);
        let predicate_terms = role_terms(&expected, Role::Predicate);
        let object_terms = role_terms(&expected, Role::Object);
        for prefix in [
            b"".as_slice(),
            b"http://example.org/s1",
            b"http://example.org/no",
            b"\"value2",
            b"_:",
            &[u8::MAX],
        ] {
            assert_prefix(&dictionary, Role::Subject, prefix, &subject_terms);
            assert_prefix(&dictionary, Role::Predicate, prefix, &predicate_terms);
            assert_prefix(&dictionary, Role::Object, prefix, &object_terms);
        }
    }

    #[test]
    fn a_role_prefix_can_require_two_disjoint_id_ranges() {
        let source = concat!(
            "<http://example.org/a-shared> <http://example.org/p> <http://example.org/a-shared> .\n",
            "<http://example.org/b-shared> <http://example.org/p> <http://example.org/b-shared> .\n",
            "<http://example.org/a-only> <http://example.org/p> \"a\" .\n",
            "<http://example.org/c-only> <http://example.org/p> \"c\" .\n",
        );
        let fixture = Fixture::build(source);
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);
        let bounds = dictionary
            .prefix_bounds(Role::Subject, b"http://example.org/a")
            .unwrap();
        let ranges: Vec<_> = bounds.ranges().cloned().collect();

        assert_eq!(ranges, vec![TermId(1)..TermId(2), TermId(3)..TermId(4)]);
        assert_eq!(bounds.count(), 2);
    }

    fn assert_terms(dictionary: &Dictionary<'_>, role: Role, id_offset: u64, expected: &[Vec<u8>]) {
        let mut buffer = Vec::new();
        for (position, term) in expected.iter().enumerate() {
            let id = TermId(id_offset + position as u64 + 1);
            assert_eq!(dictionary.extract(role, id, &mut buffer).unwrap(), term);
            assert_eq!(dictionary.locate(role, term).unwrap(), Some(id));
        }
    }

    fn assert_prefix(dictionary: &Dictionary<'_>, role: Role, prefix: &[u8], terms: &[Vec<u8>]) {
        let expected: Vec<_> = terms
            .iter()
            .enumerate()
            .filter(|(_, term)| term.starts_with(prefix))
            .map(|(position, _)| position as u64 + 1)
            .collect();
        let bounds = dictionary.prefix_bounds(role, prefix).unwrap();
        let actual: Vec<_> = bounds
            .ranges()
            .flat_map(|range| range.start.0..range.end.0)
            .collect();
        assert_eq!(actual, expected, "role {role:?}, prefix {prefix:?}");
        assert_eq!(bounds.count(), expected.len() as u64);
        assert_eq!(bounds.is_empty(), expected.is_empty());
    }

    fn role_terms(sections: &[Vec<Vec<u8>>; 4], role: Role) -> Vec<Vec<u8>> {
        let mut terms = sections[Section::Shared as usize].clone();
        match role {
            Role::Subject => terms.extend_from_slice(&sections[Section::Subjects as usize]),
            Role::Predicate => return sections[Section::Predicates as usize].clone(),
            Role::Object => terms.extend_from_slice(&sections[Section::Objects as usize]),
        }
        terms
    }

    fn sequential_sections(fixture: &Fixture) -> [Vec<Vec<u8>>; 4] {
        let path = fixture.hdt_path();
        let mut file = File::open(&path).unwrap();
        let sections = hdtc::format::scan_hdt_sections(&mut file).unwrap();
        [
            sequential_section(&path, &sections.shared, "shared"),
            sequential_section(&path, &sections.subjects, "subjects"),
            sequential_section(&path, &sections.predicates, "predicates"),
            sequential_section(&path, &sections.objects, "objects"),
        ]
    }

    fn sequential_section(
        path: &std::path::Path,
        section: &hdtc::format::PfcSection,
        name: &str,
    ) -> Vec<Vec<u8>> {
        let mut file = File::open(path).unwrap();
        file.seek(SeekFrom::Start(section.section_start)).unwrap();
        let header = hdtc::format::PfcSectionHeader::read_from(&mut file, name).unwrap();
        hdtc::format::PfcSectionIterator::new(file, header, name)
            .map(|term| term.unwrap())
            .collect()
    }
}
