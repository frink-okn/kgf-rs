//! The HDT dictionary: id ↔ term, and sorted prefix scans.
//!
//! # No sidecar is needed here
//!
//! Standard HDT already supports every dictionary operation needed here. Each
//! Plain Front Coding section stores its terms in
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

    /// Terms stored in one section.
    pub fn section_len(&self, section: Section) -> u64 {
        match section {
            Section::Shared => self.shared,
            Section::Subjects => self.subjects,
            Section::Predicates => self.predicates,
            Section::Objects => self.objects,
        }
    }

    /// Where a section starts in [`DictPosition`]'s space.
    fn section_base(&self, section: Section) -> Result<u64> {
        let mut base = 0u64;
        for earlier in Section::ALL {
            if earlier == section {
                return Ok(base);
            }
            base = base.checked_add(self.section_len(earlier)).ok_or_else(|| {
                Error::Region("dictionary section positions overflow u64".to_owned())
            })?;
        }
        unreachable!("every section occurs in Section::ALL")
    }

    /// The whole-dictionary position of a section-local id.
    ///
    /// `None` when the id is past the end of its section, which is how a
    /// caller learns that a position it was handed does not name a term.
    pub fn position_of(&self, id: SectionTermId) -> Option<DictPosition> {
        if id.local_id() > self.section_len(id.section()) {
            return None;
        }
        let base = self.section_base(id.section()).ok()?;
        base.checked_add(id.local_id() - 1).map(DictPosition)
    }

    /// The term a whole-dictionary position names, or `None` past the last one.
    ///
    /// The inverse of [`position_of`](Self::position_of).
    pub fn term_at(&self, position: DictPosition) -> Option<SectionTermId> {
        let mut remaining = position.0;
        for section in Section::ALL {
            let terms = self.section_len(section);
            if remaining < terms {
                return SectionTermId::new(section, remaining + 1);
            }
            remaining -= terms;
        }
        None
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

impl Section {
    /// The four sections in `dictionaryFour`'s own order.
    ///
    /// Load-bearing rather than cosmetic: it is the order a
    /// [`DictPosition`] counts in, and cursors into a merged prefix scan are
    /// positions in that space.
    pub const ALL: [Self; 4] = [
        Self::Shared,
        Self::Subjects,
        Self::Predicates,
        Self::Objects,
    ];

    /// The roles a term occupies by virtue of being stored in this section.
    ///
    /// The shared section is the only one that answers with two, and that is
    /// the whole reason it exists.
    pub fn roles(self) -> &'static [Role] {
        match self {
            Self::Shared => &[Role::Subject, Role::Object],
            Self::Subjects => &[Role::Subject],
            Self::Predicates => &[Role::Predicate],
            Self::Objects => &[Role::Object],
        }
    }

    /// Index into a four-slot array laid out in [`ALL`](Self::ALL)'s order.
    fn slot(self) -> usize {
        match self {
            Self::Shared => 0,
            Self::Subjects => 1,
            Self::Predicates => 2,
            Self::Objects => 3,
        }
    }
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

/// A term's zero-based position over all four sections, taken in
/// [`Section::ALL`]'s order.
///
/// One number that names exactly one stored term. A [`TermId`] cannot: it is
/// scoped to a role, so the same integer is one term as a subject and another as
/// a predicate — and a scan that merges sections needs a position independent of
/// which role asked for it. That makes this the position a cursor into such a
/// scan carries. Both directions are arithmetic on [`DictCounts`]' running sums,
/// so neither costs a read.
///
/// Opaque on purpose: the integer is meaningless without the counts of the
/// bundle it came from, and whether it names a term at all is
/// [`DictCounts::term_at`]'s answer rather than a property of the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DictPosition(u64);

impl DictPosition {
    /// Wrap an integer a caller round-tripped through a token of its own.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The integer, for a caller that has to encode it.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Where one PFC section's parts are, validated at open.
///
/// The block-offset array is a [`PackedSpec`] mapped in place rather than a
/// materialized `Vec`: on a large dictionary it runs to millions of entries, and
/// reading it at open would make startup proportional to dictionary size.
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

    /// The zero-based position of `key` in this section, if the section holds
    /// it.
    ///
    /// For a section read on its own rather than as one of the dictionary's
    /// four — the graph sidecar's dictionary is one such section. Positions
    /// rather than ids, because what an id means is the caller's: the graph
    /// dictionary numbers its terms from one, and offsets from there.
    pub(crate) fn position_of(&self, mapping: &Mapping, key: &[u8]) -> Result<Option<u64>> {
        let mut scratch = Vec::new();
        let found = self.view(mapping).search(key, &mut scratch)?;
        Ok(found.equal.then_some(found.position))
    }

    /// The term at a zero-based position of this section.
    ///
    /// # Panics
    ///
    /// Panics if `position >= terms()`.
    pub(crate) fn term_at<'b>(
        &self,
        mapping: &Mapping,
        position: u64,
        buf: &'b mut Vec<u8>,
    ) -> Result<&'b [u8]> {
        self.view(mapping).extract_position(position, buf)
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
/// than a range.
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

impl<'a> Dictionary<'a> {
    /// Every term starting with `prefix` in the sections `role` covers, in one
    /// lexicographic order.
    ///
    /// `prefix` is raw stored bytes, not request syntax: an IRI is spelled bare
    /// and a literal carries its quotes, because that is how a section sorts.
    /// An empty prefix scans everything the role covers.
    ///
    /// Two binary searches per covered section and no payload scan, so building
    /// a scan costs `O(log D)` whatever it goes on to return.
    pub fn terms(&self, role: ScanRole, prefix: &[u8]) -> Result<TermScan<'a>> {
        let mut scratch = Vec::new();
        let mut runs: [Option<Run<'a>>; 4] = [None, None, None, None];
        for section in Section::ALL {
            if !role.covers(section) {
                continue;
            }
            let view = self.section(section);
            let positions = view.prefix_positions(prefix, &mut scratch)?;
            if positions.start == positions.end {
                continue;
            }
            runs[section.slot()] = Some(Run { view, positions });
        }
        Ok(TermScan {
            counts: self.counts,
            role,
            runs,
        })
    }

    /// Every role's exact distinct term count under `prefix`.
    ///
    /// Two binary searches per section and no enumeration, so the whole
    /// breakdown costs `O(log D)` however many terms match — with the one
    /// addition [`TermScan::count`] documents, a probe per matching predicate to
    /// deduplicate [`ScanRole::Any`]. Measured at a tenth of a millisecond over
    /// the widest predicate set in the OKN corpus, which is why nothing prices
    /// this differently from a page.
    pub fn term_counts(&self, prefix: &[u8]) -> Result<RoleCounts> {
        // One `Any` scan brackets all four sections, so the per-role numbers are
        // sums of run lengths it already has and only the deduplicated total
        // needs its own work.
        let scan = self.terms(ScanRole::Any, prefix)?;
        let shared = scan.matches(Section::Shared);
        Ok(RoleCounts {
            subject: shared + scan.matches(Section::Subjects),
            predicate: scan.matches(Section::Predicates),
            object: shared + scan.matches(Section::Objects),
            any: scan.count()?,
        })
    }

    /// One section's view. Which section a role or an id belongs to is
    /// arithmetic this module owns, so nothing above it chooses between these.
    fn section(&self, section: Section) -> PfcView<'a> {
        match section {
            Section::Shared => self.shared,
            Section::Subjects => self.subjects,
            Section::Predicates => self.predicates,
            Section::Objects => self.objects,
        }
    }
}

// ---------------------------------------------------------------------------
// Prefix scans
// ---------------------------------------------------------------------------

/// Which dictionary sections one prefix scan reads.
///
/// Not a [`Role`]: "any role" is a fourth answer no role names, and the subject
/// and object roles each span two sections anyway. What a scan needs is the set
/// of sections to merge, and this is the closed set of choices worth offering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanRole {
    /// Shared and subject-only terms.
    Subject,
    /// Predicates.
    Predicate,
    /// Shared and object-only terms.
    Object,
    /// All four sections, so one distinct term is one row whatever positions it
    /// occupies.
    Any,
}

impl ScanRole {
    /// Whether a scan of this role reads `section`.
    pub fn covers(self, section: Section) -> bool {
        match self {
            Self::Any => true,
            Self::Subject => matches!(section, Section::Shared | Section::Subjects),
            Self::Object => matches!(section, Section::Shared | Section::Objects),
            Self::Predicate => section == Section::Predicates,
        }
    }
}

/// One section's contribution to a scan: the prefix's positions inside it.
///
/// Section-local rather than whole-dictionary, because the merge compares and
/// decodes within a section; [`DictCounts::position_of`] is the one place that
/// turns a local id into the position a resume point carries.
#[derive(Debug, Clone)]
struct Run<'a> {
    view: PfcView<'a>,
    /// Zero-based half-open positions matching the prefix, inside the section.
    positions: Range<u64>,
}

/// Exact distinct term counts under one prefix, per role and over all of them.
///
/// One value per [`ScanRole`], which is the answer to "does this dataset use this
/// namespace, and how" — the question a client fans across a federation. It is
/// four numbers rather than one because they come from the same four bracketing
/// searches: computing the second, third and fourth costs nothing the first has
/// not already paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleCounts {
    subject: u64,
    predicate: u64,
    object: u64,
    any: u64,
}

impl RoleCounts {
    /// Terms in the shared and subject-only sections.
    pub fn subject(&self) -> u64 {
        self.subject
    }

    /// Terms in the predicate section.
    pub fn predicate(&self) -> u64 {
        self.predicate
    }

    /// Terms in the shared and object-only sections.
    pub fn object(&self) -> u64 {
        self.object
    }

    /// Distinct terms over all four sections.
    ///
    /// Not the sum of the other three: a term stored in the shared section is
    /// both a subject and an object, and a predicate may repeat one of them.
    pub fn any(&self) -> u64 {
        self.any
    }

    /// The count for one role.
    pub fn of(&self, role: ScanRole) -> u64 {
        match role {
            ScanRole::Subject => self.subject,
            ScanRole::Predicate => self.predicate,
            ScanRole::Object => self.object,
            ScanRole::Any => self.any,
        }
    }
}

/// A resumable lexicographic scan of the terms under one byte prefix.
///
/// # Why this is a visitor rather than an iterator of ids
///
/// The subject and object roles each span two independently sorted sections and
/// [`ScanRole::Any`] spans four, so a lexicographic answer is a merge — and a
/// merge needs the strings it is ordering. Yielding ids would make the caller
/// decode every term a second time to put them back into the order it asked
/// for. So the scan decodes once and hands each term to a visitor, which also
/// lets the caller stop on a budget of its own.
///
/// Set semantics come free with the merge: a term stored in several sections is
/// one row, and the sections it occupied are reported with it.
///
/// # Cost
///
/// A page decodes at most one PFC block per row per covered section and holds
/// one buffer per covered section, so it is bounded by the page rather than by
/// the prefix or the dictionary. [`count`](Self::count) is arithmetic on the
/// bracketing searches, with the documented exception of a distinct count over
/// [`ScanRole::Any`].
#[derive(Debug, Clone)]
pub struct TermScan<'a> {
    counts: DictCounts,
    role: ScanRole,
    /// Runs in [`Section::ALL`]'s order; `None` where the section holds no term
    /// with the prefix.
    runs: [Option<Run<'a>>; 4],
}

/// A resume point a [`TermScan`] has accepted as one of its own.
///
/// Only [`TermScan::resume_at`] builds one, so a page cannot be started from a
/// position that names a term in another section, another bundle, or nothing at
/// all.
#[derive(Debug, Clone, Copy)]
pub struct ScanResume {
    id: SectionTermId,
}

/// One term a [`TermScan`] found.
#[derive(Debug, Clone, Copy)]
pub struct ScannedTerm<'a> {
    position: DictPosition,
    sections: TermSections,
    bytes: &'a [u8],
    counts: DictCounts,
}

impl<'a> ScannedTerm<'a> {
    /// The position naming this term, which a resuming caller records.
    ///
    /// The earliest section the term occurs in names it, so the value does not
    /// depend on which section a merge happened to read first.
    pub fn position(&self) -> DictPosition {
        self.position
    }

    /// The sections this term occurs in, among those the scan read.
    ///
    /// Complete for [`ScanRole::Any`]. For a single role it is limited to what
    /// the scan looked at: a subject scan reads the shared and subject-only
    /// sections, so it can say whether a subject is also an object, and says
    /// nothing about predicates.
    ///
    /// **An absent section is not evidence the term is missing from it.** A
    /// predicate scan reports every row as a predicate and nothing else, though
    /// most predicates are also subjects carrying their own label and
    /// definition. Presence is a fact; absence is only silence, and a caller
    /// that needs the other answer has to look — [`Dictionary::locate`] in the
    /// role it cares about.
    pub fn sections(&self) -> TermSections {
        self.sections
    }

    /// The stored bytes, borrowed from the scan's own buffer.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// This term's id in `role`'s space, when a section it occurs in belongs to
    /// that role.
    ///
    /// Free where the scan already read that section, which is what lets a
    /// caller follow a scanned subject into the permutations without a second
    /// dictionary search.
    pub fn id(&self, role: Role) -> Option<TermId> {
        self.sections
            .ids()
            .find_map(|id| self.counts.role_id(role, id))
    }
}

/// The sections one scanned term occurs in, with its local id in each.
///
/// Local ids differ between sections for the same string, so they are kept
/// rather than derived: without them a term found in the predicate section could
/// not also be named as an object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TermSections([Option<NonZeroU64>; 4]);

impl TermSections {
    /// Whether the term is stored in `section`.
    pub fn contains(&self, section: Section) -> bool {
        self.0[section.slot()].is_some()
    }

    /// The term's section-local ids, in [`Section::ALL`]'s order.
    pub fn ids(&self) -> impl Iterator<Item = SectionTermId> + '_ {
        Section::ALL.into_iter().filter_map(move |section| {
            self.0[section.slot()]
                .map(|local| SectionTermId::new(section, local.get()).expect("a nonzero local id"))
        })
    }

    /// The roles these sections put the term in, in subject-predicate-object
    /// order.
    pub fn roles(&self) -> impl Iterator<Item = Role> + '_ {
        [Role::Subject, Role::Predicate, Role::Object]
            .into_iter()
            .filter(move |role| self.ids().any(|id| id.section().roles().contains(role)))
    }

    fn insert(&mut self, section: Section, local_id: u64) {
        self.0[section.slot()] = NonZeroU64::new(local_id);
    }
}

/// What a visitor wants after being shown one term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFlow {
    /// Keep the term and continue.
    Continue,
    /// Keep the term and end the page after it.
    Stop,
    /// Drop the term and end the page before it.
    ///
    /// A visitor must not reject the first term it is shown: the page would
    /// resume exactly where it started, and a caller paging on that would never
    /// advance. [`ScanStop`] reports the situation faithfully rather than
    /// pretending otherwise.
    Reject,
}

/// Where a page of a [`TermScan`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanStop {
    /// The last term the visitor kept, if it kept any.
    pub last: Option<DictPosition>,
    /// Whether the scan has terms the page did not deliver.
    pub more: bool,
}

impl TermScan<'_> {
    /// The role this scan covers.
    pub fn role(&self) -> ScanRole {
        self.role
    }

    /// Whether no term in the covered sections starts with the prefix.
    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(Option::is_none)
    }

    /// Terms matching the prefix in one section this scan reads.
    ///
    /// Zero both when the section holds no matching term and when the scan does
    /// not read it, which is why this stays inside the module: only a caller that
    /// knows the role can tell those apart, and [`Dictionary::term_counts`] is
    /// the one that does.
    fn matches(&self, section: Section) -> u64 {
        self.runs[section.slot()]
            .as_ref()
            .map_or(0, |run| run.positions.end - run.positions.start)
    }

    /// Exact number of distinct terms with the prefix.
    ///
    /// For a single role this is arithmetic on searches already done, so the
    /// whole question costs `O(log D)` however many terms match.
    ///
    /// [`ScanRole::Any`] costs more, because it is the only role that can see
    /// one string twice. The shared, subject-only, and object-only sections
    /// partition their terms by construction, but a predicate may also be stored
    /// as a subject or an object, so an exact distinct count decodes each
    /// matching predicate and searches the other covered sections for it: one
    /// block decode and up to three binary searches per matching predicate.
    ///
    /// **Bounded, and the request cannot raise the bound.** The work is
    /// proportional to the predicates matching the prefix, so the maximum is the
    /// bundle's whole predicate count — a number its manifest publishes — and it
    /// is reached by the empty prefix. No prefix can ask for more; a narrower one
    /// asks for less. Measured: a tenth of a millisecond at 119 matching
    /// predicates, the widest vocabulary in the corpus this serves, and 33 ms at
    /// a synthetic 50 000, where one full-sized page of the same operation costs
    /// 38 ms. That is why the exact answer is kept rather than estimated, and why
    /// nothing prices this differently from a page.
    pub fn count(&self) -> Result<u64> {
        let mut total = 0u64;
        for section in Section::ALL {
            total = total.checked_add(self.matches(section)).ok_or_else(|| {
                Error::Region("a dictionary prefix count overflows u64".to_owned())
            })?;
        }
        if self.role != ScanRole::Any {
            return Ok(total);
        }
        let Some(predicates) = self.runs[Section::Predicates.slot()].as_ref() else {
            return Ok(total);
        };
        // A predicate can only repeat a term one of the other three sections
        // holds, so when none of them has a match under this prefix there is
        // nothing to deduplicate and no predicate needs looking at. Worth its
        // own check rather than falling out of the loop: the loop would decode
        // every matching predicate to search sections that cannot answer, which
        // on a wide predicate vocabulary is the whole cost of the request.
        if Section::ALL
            .into_iter()
            .filter(|section| *section != Section::Predicates)
            .all(|section| self.matches(section) == 0)
        {
            return Ok(total);
        }

        let mut scratch = Vec::new();
        let mut term = Vec::new();
        for position in predicates.positions.start..predicates.positions.end {
            let bytes = predicates.view.extract_position(position, &mut term)?;
            for (slot, run) in self.runs.iter().enumerate() {
                let Some(run) = run else { continue };
                if slot == Section::Predicates.slot() {
                    continue;
                }
                if run.view.search(bytes, &mut scratch)?.equal {
                    total -= 1;
                    break;
                }
            }
        }
        Ok(total)
    }

    /// Accept a position as a resume point for this scan, or refuse it.
    ///
    /// `None` when the position does not name a term this scan enumerates —
    /// which is how a stale, foreign, or edited resume point becomes a refusal
    /// rather than a page that silently starts somewhere else.
    pub fn resume_at(&self, position: DictPosition) -> Option<ScanResume> {
        let id = self.counts.term_at(position)?;
        let run = self.runs[id.section().slot()].as_ref()?;
        let local = id.local_id() - 1;
        (run.positions.start..run.positions.end)
            .contains(&local)
            .then_some(ScanResume { id })
    }

    /// Show the visitor up to `limit` terms in lexicographic order, resuming
    /// strictly after `after`.
    ///
    /// Strictly after is what keeps a term stored in several sections one row
    /// across a page boundary: every run skips past the resumed term itself
    /// rather than only the section it was named in.
    pub fn page<F>(&self, after: Option<ScanResume>, limit: usize, mut visit: F) -> Result<ScanStop>
    where
        F: FnMut(ScannedTerm<'_>) -> ScanFlow,
    {
        // One buffer per section rather than per row: a merge has to hold every
        // run's current term at once to order them, and each is decoded once.
        let mut heads: [Vec<u8>; 4] = Default::default();
        let mut next: [Option<u64>; 4] = [None; 4];
        let mut scratch = Vec::new();

        let boundary = match after {
            None => None,
            Some(resume) => {
                let run = self.runs[resume.id.section().slot()]
                    .as_ref()
                    .expect("a resume point this scan accepted names a run it reads");
                let mut bytes = Vec::new();
                run.view
                    .extract_position(resume.id.local_id() - 1, &mut bytes)?;
                Some(bytes)
            }
        };

        for section in Section::ALL {
            let slot = section.slot();
            let Some(run) = self.runs[slot].as_ref() else {
                continue;
            };
            let start = match &boundary {
                Some(term) => {
                    let found = run.view.search(term, &mut scratch)?;
                    (found.position + u64::from(found.equal)).max(run.positions.start)
                }
                None => run.positions.start,
            };
            if start < run.positions.end {
                run.view.extract_position(start, &mut heads[slot])?;
                next[slot] = Some(start);
            }
        }

        let mut last = None;
        let mut delivered = 0usize;
        loop {
            let mut smallest = None;
            for section in Section::ALL {
                let slot = section.slot();
                if next[slot].is_none() {
                    continue;
                }
                smallest = match smallest {
                    Some(best) if heads[best] <= heads[slot] => Some(best),
                    _ => Some(slot),
                };
            }
            let Some(slot) = smallest else {
                return Ok(ScanStop { last, more: false });
            };
            if delivered == limit {
                return Ok(ScanStop { last, more: true });
            }

            // Every run standing at the same string contributes its own local id
            // and is consumed with it, so one term is one row however many
            // sections hold it.
            let mut sections = TermSections::default();
            for section in Section::ALL {
                let other = section.slot();
                if let Some(position) = next[other]
                    && heads[other] == heads[slot]
                {
                    sections.insert(section, position + 1);
                }
            }
            let position = self
                .counts
                .position_of(
                    sections
                        .ids()
                        .next()
                        .expect("the smallest term occurs in at least its own section"),
                )
                .ok_or_else(|| {
                    Error::Region("a scanned term has no whole-dictionary position".to_owned())
                })?;

            let flow = visit(ScannedTerm {
                position,
                sections,
                bytes: &heads[slot],
                counts: self.counts,
            });
            if flow == ScanFlow::Reject {
                return Ok(ScanStop { last, more: true });
            }
            last = Some(position);
            delivered += 1;

            for section in Section::ALL {
                if !sections.contains(section) {
                    continue;
                }
                let advanced = section.slot();
                let run = self.runs[advanced]
                    .as_ref()
                    .expect("a matched section is a run");
                let position = next[advanced].expect("a matched section has a head") + 1;
                next[advanced] = if position < run.positions.end {
                    run.view.extract_position(position, &mut heads[advanced])?;
                    Some(position)
                } else {
                    None
                };
            }

            if flow == ScanFlow::Stop {
                return Ok(ScanStop {
                    last,
                    more: next.iter().any(Option::is_some),
                });
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
    use std::collections::BTreeMap;
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

    /// `p` is a predicate *and* a subject and an object, so it is stored in the
    /// shared section and in the predicate section — the one way a term can
    /// occupy two sections at once, and the case `ScanRole::Any` has to
    /// deduplicate. `q` is a predicate only, `c` an object only, `b` a subject
    /// only, and `_:b1` keeps a blank node in the sort.
    const SCAN_NT: &str = concat!(
        "<http://example.org/a> <http://example.org/p> <http://example.org/p> .\n",
        "<http://example.org/p> <http://example.org/q> \"shared value\" .\n",
        "<http://example.org/b> <http://example.org/q> <http://example.org/c> .\n",
        "_:b1 <http://example.org/q> \"blank subject\" .\n",
    );

    /// Wide enough that every section crosses several PFC blocks, so a merge is
    /// decoding blocks rather than reading one.
    fn scan_source() -> String {
        let mut source = SCAN_NT.to_owned();
        for index in 0..40 {
            source.push_str(&format!(
                "<http://example.org/s{index:02}> <http://example.org/many> \"value{index:02}\" .\n"
            ));
        }
        source
    }

    const SCAN_PREFIXES: [&[u8]; 7] = [
        b"".as_slice(),
        b"http://example.org/p",
        b"http://example.org/s1",
        b"\"value2",
        b"_:",
        b"http://example.org/zzz",
        &[u8::MAX],
    ];

    #[test]
    fn a_section_slot_is_its_place_in_the_all_order() {
        for (slot, section) in Section::ALL.into_iter().enumerate() {
            assert_eq!(section.slot(), slot);
            assert_eq!(section as usize, slot);
        }
    }

    #[test]
    fn a_prefix_scan_merges_its_sections_into_one_lexicographic_order() {
        let fixture = Fixture::build(&scan_source());
        let expected_sections = sequential_sections(&fixture);
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);

        for role in [
            ScanRole::Subject,
            ScanRole::Predicate,
            ScanRole::Object,
            ScanRole::Any,
        ] {
            for prefix in SCAN_PREFIXES {
                let expected = expected_scan(&expected_sections, role, prefix);
                let scan = dictionary.terms(role, prefix).unwrap();
                let found = collect(&scan, None, usize::MAX);

                let terms: Vec<_> = found.iter().map(|row| row.term.clone()).collect();
                let expected_terms: Vec<_> =
                    expected.iter().map(|(term, _)| term.clone()).collect();
                assert_eq!(terms, expected_terms, "{role:?} {prefix:?}");

                let roles: Vec<_> = found.iter().map(|row| row.roles.clone()).collect();
                let expected_roles: Vec<_> =
                    expected.iter().map(|(_, roles)| roles.clone()).collect();
                assert_eq!(roles, expected_roles, "{role:?} {prefix:?}");

                assert_eq!(scan.count().unwrap(), expected.len() as u64);
                assert_eq!(scan.is_empty(), expected.is_empty());

                // A scanned term's id is the one the dictionary's own lookup
                // gives, in every role the scan reported it in — and nothing in
                // a role it did not.
                for row in &found {
                    assert!(
                        scan.resume_at(row.position).is_some(),
                        "a scan accepts the position of a term it emitted"
                    );
                    for (role, id) in ROLES.into_iter().zip(row.ids) {
                        let located = dictionary.locate(role, &row.term).unwrap();
                        if row.roles.contains(&role) {
                            assert!(located.is_some(), "{:?} located as {role:?}", row.term);
                            assert_eq!(id, located);
                        } else {
                            assert_eq!(id, None, "{:?} has no {role:?} id here", row.term);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_role_breakdown_agrees_with_each_role_counted_alone() {
        let fixture = Fixture::build(&scan_source());
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);

        for prefix in SCAN_PREFIXES {
            let counts = dictionary.term_counts(prefix).unwrap();
            for role in [
                ScanRole::Subject,
                ScanRole::Predicate,
                ScanRole::Object,
                ScanRole::Any,
            ] {
                let alone = dictionary.terms(role, prefix).unwrap().count().unwrap();
                assert_eq!(counts.of(role), alone, "{role:?} {prefix:?}");
            }
            // `any` deduplicates rather than adding up: `p` is stored in the
            // shared section *and* the predicate section, and the sum of the
            // three roles double-counts every shared term besides.
            assert!(
                counts.any() <= counts.subject() + counts.predicate() + counts.object(),
                "{prefix:?}"
            );
        }

        // The one case that makes the deduplication observable.
        let counts = dictionary.term_counts(b"http://example.org/p").unwrap();
        assert_eq!(counts.predicate(), 1);
        assert_eq!(counts.subject(), 1);
        assert_eq!(counts.object(), 1);
        assert_eq!(counts.any(), 1, "one term, whatever positions it occupies");
    }

    #[test]
    fn exhaustive_paging_of_a_scan_yields_each_term_once_at_every_size() {
        let fixture = Fixture::build(&scan_source());
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);

        for role in [
            ScanRole::Subject,
            ScanRole::Predicate,
            ScanRole::Object,
            ScanRole::Any,
        ] {
            for prefix in SCAN_PREFIXES {
                let scan = dictionary.terms(role, prefix).unwrap();
                let whole = collect(&scan, None, usize::MAX);
                for limit in [1usize, 2, 3, 7, 13, whole.len().max(1)] {
                    let mut paged = Vec::new();
                    let mut after = None;
                    loop {
                        let mut page = Vec::new();
                        let stop = scan
                            .page(after, limit, |term| {
                                page.push(ScannedRow::of(&term));
                                ScanFlow::Continue
                            })
                            .unwrap();
                        assert!(page.len() <= limit);
                        paged.extend(page);
                        if !stop.more {
                            break;
                        }
                        let last = stop.last.expect("a page that leaves more kept a term");
                        after = Some(scan.resume_at(last).expect("its own resume point"));
                    }
                    assert_eq!(paged, whole, "{role:?} {prefix:?} at limit {limit}");
                }
            }
        }
    }

    #[test]
    fn a_rejected_term_ends_the_page_before_itself() {
        let fixture = Fixture::build(&scan_source());
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);
        let scan = dictionary.terms(ScanRole::Any, b"").unwrap();
        let whole = collect(&scan, None, usize::MAX);

        let mut kept = 0;
        let stop = scan
            .page(None, usize::MAX, |_| {
                if kept == 3 {
                    return ScanFlow::Reject;
                }
                kept += 1;
                ScanFlow::Continue
            })
            .unwrap();
        assert_eq!(kept, 3);
        assert!(stop.more);
        assert_eq!(stop.last, Some(whole[2].position));

        // Resuming from the last kept term delivers the rejected one first, so
        // nothing is lost by refusing it.
        let after = scan.resume_at(stop.last.unwrap()).unwrap();
        let rest = collect(&scan, Some(after), usize::MAX);
        assert_eq!(rest, whole[3..]);
    }

    #[test]
    fn a_page_that_stops_on_its_last_term_reports_nothing_more() {
        let fixture = Fixture::build(&scan_source());
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);
        let scan = dictionary.terms(ScanRole::Predicate, b"").unwrap();
        let whole = collect(&scan, None, usize::MAX);

        let mut seen = 0;
        let stop = scan
            .page(None, usize::MAX, |_| {
                seen += 1;
                if seen == whole.len() {
                    ScanFlow::Stop
                } else {
                    ScanFlow::Continue
                }
            })
            .unwrap();
        assert_eq!(seen, whole.len());
        assert!(!stop.more);
    }

    #[test]
    fn a_resume_point_this_scan_does_not_enumerate_is_refused() {
        let fixture = Fixture::build(&scan_source());
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse HDT");
        let dictionary = layout.dictionary().view(&hdt);
        let counts = *dictionary.counts();
        let total = counts.shared + counts.subjects + counts.predicates + counts.objects;

        // Past the last term of the dictionary.
        let scan = dictionary.terms(ScanRole::Any, b"").unwrap();
        assert!(scan.resume_at(DictPosition::new(total)).is_none());
        assert!(scan.resume_at(DictPosition::new(u64::MAX)).is_none());

        // A term the scan's own prefix excludes, and a term in a section this
        // role does not read.
        let narrowed = dictionary
            .terms(ScanRole::Any, b"http://example.org/s1")
            .unwrap();
        let outside = collect(&scan, None, usize::MAX)
            .into_iter()
            .find(|row| !row.term.starts_with(b"http://example.org/s1"))
            .expect("a term outside the narrowed prefix");
        assert!(narrowed.resume_at(outside.position).is_none());

        let predicates = dictionary.terms(ScanRole::Predicate, b"").unwrap();
        let subjects = dictionary.terms(ScanRole::Subject, b"").unwrap();
        let subject_only = collect(&subjects, None, usize::MAX)
            .into_iter()
            .find(|row| row.roles == [Role::Subject])
            .expect("a subject-only term");
        assert!(predicates.resume_at(subject_only.position).is_none());
    }

    const ROLES: [Role; 3] = [Role::Subject, Role::Predicate, Role::Object];

    /// Everything one visited term reported, kept so a page can be compared with
    /// a differently sized page term for term.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ScannedRow {
        term: Vec<u8>,
        roles: Vec<Role>,
        position: DictPosition,
        ids: [Option<TermId>; 3],
    }

    impl ScannedRow {
        fn of(term: &ScannedTerm<'_>) -> Self {
            Self {
                term: term.bytes().to_vec(),
                roles: term.sections().roles().collect(),
                position: term.position(),
                ids: ROLES.map(|role| term.id(role)),
            }
        }
    }

    fn collect(scan: &TermScan<'_>, after: Option<ScanResume>, limit: usize) -> Vec<ScannedRow> {
        let mut rows = Vec::new();
        scan.page(after, limit, |term| {
            rows.push(ScannedRow::of(&term));
            ScanFlow::Continue
        })
        .unwrap();
        rows
    }

    fn expected_scan(
        sections: &[Vec<Vec<u8>>; 4],
        role: ScanRole,
        prefix: &[u8],
    ) -> Vec<(Vec<u8>, Vec<Role>)> {
        let mut found: BTreeMap<Vec<u8>, [bool; 4]> = BTreeMap::new();
        for section in Section::ALL {
            if !role.covers(section) {
                continue;
            }
            for term in &sections[section as usize] {
                if term.starts_with(prefix) {
                    found.entry(term.clone()).or_default()[section as usize] = true;
                }
            }
        }
        found
            .into_iter()
            .map(|(term, members)| {
                let roles = [Role::Subject, Role::Predicate, Role::Object]
                    .into_iter()
                    .filter(|role| {
                        Section::ALL.into_iter().any(|section| {
                            members[section as usize] && section.roles().contains(role)
                        })
                    })
                    .collect();
                (term, roles)
            })
            .collect()
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
