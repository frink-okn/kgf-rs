//! The SPO permutation, read out of `data.hdt` itself.
//!
//! HDT's `BitmapTriples` is a two-level adjacency encoding with an implicit
//! level 1: subject ids `1..=S` all occur, so there is no array for them.
//! `ArrayY`/`BitmapY` hold each subject's predicates, `ArrayZ`/`BitmapZ` hold
//! each (subject, predicate) group's objects, sorted within every group.
//!
//! POS and OPS in `data.hdt.perm` have exactly this shape (doc 20 §20.2), which
//! is why [`BitmapTriples`] serves all three and [`crate::perm`] only supplies
//! the differently-sourced views. What is *not* shared is the framing: sections
//! here carry preambles and sit at unaligned offsets, so they are located by
//! walking those preambles — [`hdtc::format::scan_hdt_sections`], the *scan*
//! forms, which report where each payload starts without reading a byte of it.
//! Nothing on this path may call hdtc's materializing readers, which expand
//! dictionary-sized arrays into `Vec`s ([`crate::map`] explains the element
//! access that results).
//!
//! The dictionary is located by the same walk, since it is sections of the same
//! file; [`crate::dict`] owns what its parts mean.
//!
//! The rank directories for this file's two bitmaps live in `data.hdt.perm`
//! (its component `0x03`), since standard HDT has nowhere to put them.

use std::io::Cursor;
use std::ops::Range;

use crate::Role;
use crate::dict::{DictionaryLayout, PfcLayout};
use crate::error::{Error, Result};
use crate::map::{BitmapSpec, Mapping, PackedArray, PackedSpec};
use crate::rank::RankedBitmap;

/// The four regions of one BitmapTriples permutation, validated at open.
///
/// One shape for all three permutations: SPO's regions are located by walking
/// `data.hdt`'s preambles ([`HdtLayout::parse`]), POS's and OPS's by reading the
/// sidecar's section directory ([`crate::perm`]). What differs is where the
/// offsets come from, not what they describe.
///
/// The rank directories are not here. All six live in `data.hdt.perm`, so
/// pairing a bitmap with its directory is [`crate::rank::RankedSpec`]'s job and
/// happens once the sidecar is open.
#[derive(Debug, Clone, Copy)]
pub struct TriplesLayout {
    array_y: PackedSpec,
    bitmap_y: BitmapSpec,
    array_z: PackedSpec,
    bitmap_z: BitmapSpec,
}

impl TriplesLayout {
    /// Bind four located regions, checking that each level's array and bitmap
    /// describe the same number of positions, and that level 2 and level 3 have
    /// a valid no-empty-groups cardinality relationship.
    ///
    /// The first check is what makes `rank1`/`select1` over one level meaningful
    /// for the other. The second holds because every (level-1, level-2) pair owns
    /// at least one triple, and no triple can exist without such a pair. It also
    /// catches the one mistake these four same-typed arguments allow: passing
    /// unequal levels in the wrong order. Four numbers already read from headers,
    /// so both sources of a layout pay for it rather than trusting their input.
    pub fn new(
        array_y: PackedSpec,
        bitmap_y: BitmapSpec,
        array_z: PackedSpec,
        bitmap_z: BitmapSpec,
    ) -> Result<Self> {
        check_level("level 2", array_y.len(), bitmap_y.len())?;
        check_level("level 3", array_z.len(), bitmap_z.len())?;
        check_coverage(
            "level-2 entries",
            array_y.len(),
            "level-3 entries",
            array_z.len(),
        )?;
        Ok(Self {
            array_y,
            bitmap_y,
            array_z,
            bitmap_z,
        })
    }

    /// Level-2 values: predicates in SPO, objects in POS, predicates in OPS.
    pub fn array_y(&self) -> &PackedSpec {
        &self.array_y
    }

    /// The bitmap over `ArrayY` positions, one bit per level-2 entry.
    pub fn bitmap_y(&self) -> &BitmapSpec {
        &self.bitmap_y
    }

    /// Level-3 values: objects in SPO, subjects in POS and OPS.
    pub fn array_z(&self) -> &PackedSpec {
        &self.array_z
    }

    /// The bitmap over `ArrayZ` positions, one bit per triple.
    pub fn bitmap_z(&self) -> &BitmapSpec {
        &self.bitmap_z
    }

    /// Distinct (level-1, level-2) pairs.
    pub fn pairs(&self) -> u64 {
        self.array_y.len()
    }

    /// Triples, one per `ArrayZ` entry.
    pub fn triples(&self) -> u64 {
        self.array_z.len()
    }
}

fn check_level(level: &str, entries: u64, bits: u64) -> Result<()> {
    if entries == bits {
        Ok(())
    } else {
        Err(Error::Region(format!(
            "{level} has {entries} array entries but {bits} bitmap bits"
        )))
    }
}

/// Check that every distinct item occurs at least once and that neither side is
/// populated without the other.
fn check_coverage(
    item_name: &str,
    items: u64,
    occurrence_name: &str,
    occurrences: u64,
) -> Result<()> {
    if (items == 0) != (occurrences == 0) {
        return Err(Error::Region(format!(
            "{item_name} has {items} entries but {occurrence_name} has {occurrences}; they must be empty together"
        )));
    }
    if items > occurrences {
        return Err(Error::Region(format!(
            "{items} {item_name} cannot fit in {occurrences} {occurrence_name}"
        )));
    }
    Ok(())
}

/// Where everything inside a mapped `data.hdt` is, validated at open.
///
/// Produced by walking the file's control info and section preambles with
/// `hdtc::format`, once. Holds specs rather than views, because a
/// [`Store`](crate::store::Store) owns the mapping these were validated against
/// (see [`crate::map`]).
#[derive(Debug, Clone)]
pub struct HdtLayout {
    dictionary: DictionaryLayout,
    spo: TriplesLayout,
}

impl HdtLayout {
    /// Walk a mapped HDT and record where everything is.
    ///
    /// Preambles only: **no payload byte is read**, which keeps this part of
    /// opening independent of the HDT's size (doc 20 §20.1). The walk itself is
    /// hdtc's — one implementation of "where is `BitmapY`" for the builder and
    /// every reader — and this adds what a mapped reader needs on top: each
    /// located region becomes a spec validated against `mapping`, so a file that
    /// disagrees with its own headers is refused here with a path, rather than
    /// panicking on some later request.
    pub fn parse(mapping: &Mapping) -> Result<Self> {
        let mut cursor = Cursor::new(mapping.as_bytes());
        let sections = hdtc::format::scan_hdt_sections(&mut cursor)
            .map_err(|e| malformed(mapping, format!("{e:#}")))?;

        // Trailing bytes mean this is not the file its own headers describe —
        // most likely a different artifact under the name, since a published
        // bundle version is immutable (doc 04 §4.6).
        let file_len = mapping.as_bytes().len() as u64;
        if sections.end() != file_len {
            return Err(malformed(
                mapping,
                format!(
                    "sections end at {} but the file is {file_len} bytes",
                    sections.end()
                ),
            ));
        }

        with_artifact(
            mapping,
            (|| {
                let dictionary = DictionaryLayout::new(
                    PfcLayout::locate(mapping, &sections.shared)?,
                    PfcLayout::locate(mapping, &sections.subjects)?,
                    PfcLayout::locate(mapping, &sections.predicates)?,
                    PfcLayout::locate(mapping, &sections.objects)?,
                )?;

                let spo = TriplesLayout::new(
                    PackedSpec::new(
                        mapping,
                        sections.array_y.data_start,
                        sections.array_y.num_entries,
                        sections.array_y.bits_per_entry,
                    )?,
                    BitmapSpec::new(
                        mapping,
                        sections.bitmap_y.data_start,
                        sections.bitmap_y.num_bits,
                    )?,
                    PackedSpec::new(
                        mapping,
                        sections.array_z.data_start,
                        sections.array_z.num_entries,
                        sections.array_z.bits_per_entry,
                    )?,
                    BitmapSpec::new(
                        mapping,
                        sections.bitmap_z.data_start,
                        sections.bitmap_z.num_bits,
                    )?,
                )?;

                // Every dictionary term occurs in the triples. These inequalities
                // and empty-together checks are the size-independent part of that fact:
                // subjects and predicates occur in pairs; objects occur in triples.
                // `TriplesLayout::new` already checked pairs against triples.
                let counts = dictionary.counts();
                check_coverage(
                    "subjects",
                    counts.len(Role::Subject),
                    "(subject, predicate) pairs",
                    spo.pairs(),
                )?;
                check_coverage(
                    "predicates",
                    counts.len(Role::Predicate),
                    "(subject, predicate) pairs",
                    spo.pairs(),
                )?;
                check_coverage(
                    "objects",
                    counts.len(Role::Object),
                    "triples",
                    spo.triples(),
                )?;

                Ok(Self { dictionary, spo })
            })(),
        )
    }

    /// The dictionary's four sections.
    pub fn dictionary(&self) -> &DictionaryLayout {
        &self.dictionary
    }

    /// The SPO permutation carried by `data.hdt` itself.
    pub fn spo(&self) -> &TriplesLayout {
        &self.spo
    }

    /// Total triples, one per `ArrayZ` entry.
    ///
    /// The structural count, not the header's `void:triples`. Both agree in a
    /// well-formed file — hdtc's builders check that they do — but the header is
    /// the part of an HDT a rewrite may change, and it is excluded from identity
    /// digests for exactly that reason.
    pub fn triples(&self) -> u64 {
        self.spo.triples()
    }
}

/// A structural failure in a file whose path we know.
fn malformed(mapping: &Mapping, detail: impl Into<String>) -> Error {
    Error::Malformed {
        artifact: mapping.path().to_path_buf(),
        detail: detail.into(),
    }
}

/// Add the artifact path to a structural error raised while binding its specs.
fn with_artifact<T>(mapping: &Mapping, result: Result<T>) -> Result<T> {
    result.map_err(|error| malformed(mapping, error.to_string()))
}

/// One BitmapTriples permutation with an implicit level 1.
///
/// Serves SPO from `data.hdt` and POS/OPS from `data.hdt.perm` — the same
/// traversal over differently sourced views.
#[derive(Debug, Clone, Copy)]
pub struct BitmapTriples<'a> {
    array_y: PackedArray<'a>,
    bitmap_y: RankedBitmap<'a>,
    array_z: PackedArray<'a>,
    bitmap_z: RankedBitmap<'a>,
}

impl<'a> BitmapTriples<'a> {
    /// Assemble a permutation from its four regions and their directories.
    pub(crate) fn new(
        array_y: PackedArray<'a>,
        bitmap_y: RankedBitmap<'a>,
        array_z: PackedArray<'a>,
        bitmap_z: RankedBitmap<'a>,
    ) -> Self {
        Self {
            array_y,
            bitmap_y,
            array_z,
            bitmap_z,
        }
    }

    /// Level-1 keys in this permutation; ids run `1..=level1_count()`.
    ///
    /// One `u64` load: level 1 is implicit, so the count is the number of groups
    /// `BitmapY` closes, which its rank directory already carries as the
    /// population count. [`crate::perm::Permutations::open`] checks it against
    /// the dictionary so that [`level2_range`](Self::level2_range) cannot be
    /// reached out of range by a valid request.
    pub fn level1_count(&self) -> u64 {
        self.bitmap_y.count()
    }

    /// The half-open `ArrayY` range holding level-1 key `first`'s level-2 values.
    ///
    /// Two select operations. `first` is 1-based, as HDT ids are.
    ///
    /// # Panics
    ///
    /// Panics if `first` is not a level-1 key in this permutation.
    pub fn level2_range(&self, first: u64) -> Range<u64> {
        let level1_count = self.level1_count();
        assert!(
            first != 0 && first <= level1_count,
            "level-1 key {first} out of range for {level1_count} keys"
        );
        group_range(&self.bitmap_y, first - 1)
    }

    /// The half-open `ArrayZ` range for the level-2 entry at `y_position`.
    ///
    /// # Panics
    ///
    /// Panics if `y_position` is not an `ArrayY` position.
    pub fn level3_range(&self, y_position: u64) -> Range<u64> {
        assert_position("ArrayY", y_position, self.array_y.len());
        group_range(&self.bitmap_z, y_position)
    }

    /// Binary search for `value` within a sorted `ArrayY` range.
    ///
    /// Sorted-within-group is normative in every permutation, which is what
    /// makes this legal.
    ///
    /// # Panics
    ///
    /// Panics if `range` is not within `ArrayY`.
    pub fn find_level2(&self, range: Range<u64>, value: u64) -> Option<u64> {
        find_in(self.array_y, range, value)
    }

    /// First `ArrayY` position whose value is greater than `value`.
    ///
    /// Used by the `s ? o` cursor, whose route-independent resume position is
    /// the last predicate id returned rather than a route-specific Y position.
    pub(crate) fn level2_upper_bound(&self, range: Range<u64>, value: u64) -> u64 {
        upper_bound_in(self.array_y, range, value)
    }

    /// Binary search for `value` within a sorted `ArrayZ` range.
    ///
    /// # Panics
    ///
    /// Panics if `range` is not within `ArrayZ`.
    pub fn find_level3(&self, range: Range<u64>, value: u64) -> Option<u64> {
        find_in(self.array_z, range, value)
    }

    /// The level-1 key owning `y_position`, by rank.
    ///
    /// # Panics
    ///
    /// Panics if `y_position` is not an `ArrayY` position.
    pub fn level1_of(&self, y_position: u64) -> u64 {
        assert_position("ArrayY", y_position, self.array_y.len());
        self.bitmap_y.rank1(y_position) + 1
    }

    /// The `ArrayY` position owning `z_position`, by rank.
    ///
    /// # Panics
    ///
    /// Panics if `z_position` is not an `ArrayZ` position.
    pub fn level2_of(&self, z_position: u64) -> u64 {
        assert_position("ArrayZ", z_position, self.array_z.len());
        self.bitmap_z.rank1(z_position)
    }

    /// Raw level-2 value at an `ArrayY` position.
    ///
    /// Together with [`level2_of`](Self::level2_of), this materializes the
    /// middle component of an otherwise unbound triple.
    ///
    /// # Panics
    ///
    /// Panics if `y_position` is not an `ArrayY` position.
    #[inline]
    pub fn level2_at(&self, y_position: u64) -> u64 {
        self.array_y.get(y_position)
    }

    /// Raw level-3 value at a position — the innermost read on every hot path.
    ///
    /// # Panics
    ///
    /// Panics if `z_position` is not an `ArrayZ` position.
    #[inline]
    pub fn level3_at(&self, z_position: u64) -> u64 {
        self.array_z.get(z_position)
    }

    /// Packed width of a level-3 value, for the bounded linear-probe heuristic.
    pub(crate) fn level3_width(&self) -> u8 {
        self.array_z.width()
    }
}

/// Positions belonging to zero-based `group`, where set bits close groups.
fn group_range(bitmap: &RankedBitmap<'_>, group: u64) -> Range<u64> {
    let end = bitmap.select1(group) + 1;
    let start = if group == 0 {
        0
    } else {
        bitmap.select1(group - 1) + 1
    };
    start..end
}

fn find_in(array: PackedArray<'_>, range: Range<u64>, value: u64) -> Option<u64> {
    assert_array_range(&array, &range);
    let (mut low, mut high) = (range.start, range.end);
    while low < high {
        let middle = low + (high - low) / 2;
        match array.get(middle).cmp(&value) {
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
            std::cmp::Ordering::Equal => return Some(middle),
        }
    }
    None
}

fn upper_bound_in(array: PackedArray<'_>, range: Range<u64>, value: u64) -> u64 {
    assert_array_range(&array, &range);
    let (mut low, mut high) = (range.start, range.end);
    while low < high {
        let middle = low + (high - low) / 2;
        if array.get(middle) <= value {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn assert_array_range(array: &PackedArray<'_>, range: &Range<u64>) {
    assert!(
        range.start <= range.end && range.end <= array.len(),
        "array range {}..{} out of range for {} entries",
        range.start,
        range.end,
        array.len()
    );
}

fn assert_position(array: &str, position: u64, len: u64) {
    assert!(
        position < len,
        "{array} position {position} out of range for {len} entries"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdTriple;
    use crate::dict::Section;
    use crate::perm::Permutations;
    use crate::testing::{Fixture, TINY_NT, tiny_id_triples};

    fn sorted_projection(
        triples: &[IdTriple],
        project: impl Fn(IdTriple) -> [u64; 3],
    ) -> Vec<[u64; 3]> {
        let mut projected: Vec<_> = triples.iter().copied().map(project).collect();
        projected.sort_unstable();
        projected
    }

    fn assert_projection(
        name: &str,
        triples: BitmapTriples<'_>,
        first_count: u64,
        expected: &[[u64; 3]],
    ) {
        let mut actual = Vec::new();
        let (mut next_y, mut next_z) = (0, 0);

        for first in 1..=first_count {
            let y_range = triples.level2_range(first);
            assert_eq!(y_range.start, next_y, "{name}: level-2 gap");
            assert!(!y_range.is_empty(), "{name}: implicit key {first} is empty");
            assert_eq!(triples.find_level2(y_range.clone(), 0), None, "{name}");
            assert_eq!(
                triples.find_level2(y_range.clone(), u64::MAX),
                None,
                "{name}"
            );

            for y_position in y_range.clone() {
                assert_eq!(triples.level1_of(y_position), first, "{name}");
                let second = triples.level2_at(y_position);
                assert_eq!(
                    triples.find_level2(y_range.clone(), second),
                    Some(y_position),
                    "{name}"
                );

                let z_range = triples.level3_range(y_position);
                assert_eq!(z_range.start, next_z, "{name}: level-3 gap");
                assert!(!z_range.is_empty(), "{name}: level-2 group is empty");
                assert_eq!(triples.find_level3(z_range.clone(), 0), None, "{name}");
                assert_eq!(
                    triples.find_level3(z_range.clone(), u64::MAX),
                    None,
                    "{name}"
                );

                for z_position in z_range.clone() {
                    assert_eq!(triples.level2_of(z_position), y_position, "{name}");
                    let third = triples.level3_at(z_position);
                    assert_eq!(
                        triples.find_level3(z_range.clone(), third),
                        Some(z_position),
                        "{name}"
                    );
                    actual.push([first, second, third]);
                }
                next_z = z_range.end;
            }
            next_y = y_range.end;
        }

        let expected_pairs = expected
            .iter()
            .map(|triple| [triple[0], triple[1]])
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u64;
        assert_eq!(next_y, expected_pairs, "{name}: level-2 coverage");
        assert_eq!(next_z, expected.len() as u64, "{name}: level-3 coverage");
        assert_eq!(
            triples.find_level2(next_y..next_y, 1),
            None,
            "{name}: empty level-2 search"
        );
        assert_eq!(
            triples.find_level3(next_z..next_z, 1),
            None,
            "{name}: empty level-3 search"
        );
        assert_eq!(actual, expected, "{name}: traversal order");
    }

    #[test]
    fn shared_traversal_reconstructs_every_projection_and_inverse() {
        let fixture = Fixture::build(TINY_NT);
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let dictionary = permutations
            .hdt_layout()
            .dictionary()
            .view(permutations.hdt_mapping());
        let counts = *dictionary.counts();
        let triples = tiny_id_triples(&dictionary);

        assert_projection(
            "SPO",
            permutations.spo(),
            counts.len(Role::Subject),
            &sorted_projection(&triples, |triple| {
                [triple.subject, triple.predicate, triple.object]
            }),
        );
        assert_projection(
            "POS",
            permutations.pos(),
            counts.len(Role::Predicate),
            &sorted_projection(&triples, |triple| {
                [triple.predicate, triple.object, triple.subject]
            }),
        );
        assert_projection(
            "OPS",
            permutations.ops(),
            counts.len(Role::Object),
            &sorted_projection(&triples, |triple| {
                [triple.object, triple.predicate, triple.subject]
            }),
        );
    }

    /// The walk must agree with what hdtc records about the same file from its
    /// own read of it — the differential check this unit is verified by.
    #[test]
    fn the_layout_agrees_with_the_sidecar_hdtc_built_from_the_same_file() {
        let fixture = Fixture::build(TINY_NT);
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse layout");

        let index = hdtc::format::PermutationIndex::open(&fixture.perm_path(), &fixture.hdt_path())
            .expect("open sidecar");
        let header = index.header();
        let counts = layout.dictionary().counts();

        assert_eq!(layout.triples(), header.triples);
        assert_eq!(counts.len(Role::Subject), header.subjects);
        assert_eq!(counts.len(Role::Object), header.objects);
        assert_eq!(counts.len(Role::Predicate), header.predicates);

        // The sidecar's SPO directories record the length of each HDT bitmap
        // they index, which is an independent statement of what the walk found.
        // Kinds 5–6 index BitmapY, 7–8 BitmapZ.
        let indexed_bits = |kind| {
            let section_type = hdtc::format::PermutationComponent::Spo.section_type(kind);
            index
                .sections()
                .iter()
                .find(|s| s.section_type == section_type)
                .unwrap_or_else(|| panic!("missing SPO directory section {section_type:#06x}"))
                .indexed_bits
        };
        use hdtc::format::PermutationSectionKind::{
            BitmapYSubrank, BitmapYSuperrank, BitmapZSubrank, BitmapZSuperrank,
        };
        assert_eq!(
            indexed_bits(BitmapYSuperrank),
            layout.spo().bitmap_y().len()
        );
        assert_eq!(indexed_bits(BitmapYSubrank), layout.spo().bitmap_y().len());
        assert_eq!(
            indexed_bits(BitmapZSuperrank),
            layout.spo().bitmap_z().len()
        );
        assert_eq!(indexed_bits(BitmapZSubrank), layout.spo().bitmap_z().len());

        // And against the source: the fixture is duplicate-free, so one line is
        // one triple, and a misidentified `ArrayZ` could not match.
        assert_eq!(layout.triples(), TINY_NT.lines().count() as u64);
    }

    /// Level 1 is implicit, so the bitmaps must say what the dictionary says:
    /// one set bit per subject in `BitmapY` (each closes that subject's
    /// predicates) and one per (subject, predicate) pair in `BitmapZ`.
    ///
    /// The strongest statement available before the dictionary can be read, and
    /// it holds only if all four regions were located relative to each other
    /// correctly.
    #[test]
    fn the_bitmaps_close_exactly_the_groups_the_dictionary_implies() {
        let fixture = Fixture::build(TINY_NT);
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse layout");

        let bitmap_y = layout.spo().bitmap_y().view(&hdt);
        let bitmap_z = layout.spo().bitmap_z().view(&hdt);

        assert_eq!(
            bitmap_y.count_ones_in(0..bitmap_y.len()),
            layout.dictionary().counts().len(Role::Subject),
            "one closed predicate group per subject"
        );
        assert_eq!(
            bitmap_z.count_ones_in(0..bitmap_z.len()),
            layout.spo().pairs(),
            "one closed object group per (subject, predicate) pair"
        );
    }

    /// Every located region must project onto the mapping it was validated
    /// against, and the values it yields must be in range for what they mean.
    #[test]
    fn every_located_region_projects_and_reads_in_range() {
        let fixture = Fixture::build(TINY_NT);
        let hdt = fixture.map_hdt();
        let layout = HdtLayout::parse(&hdt).expect("parse layout");
        let counts = *layout.dictionary().counts();

        let array_y = layout.spo().array_y().view(&hdt);
        for position in 0..array_y.len() {
            let predicate = array_y.get(position);
            assert!(
                predicate >= 1 && predicate <= counts.predicates,
                "ArrayY[{position}] = {predicate}, outside 1..={}",
                counts.predicates
            );
        }

        let array_z = layout.spo().array_z().view(&hdt);
        let objects = counts.len(Role::Object);
        for position in 0..array_z.len() {
            let object = array_z.get(position);
            assert!(
                object >= 1 && object <= objects,
                "ArrayZ[{position}] = {object}, outside 1..={objects}"
            );
        }

        // Each bitmap's last bit is set: the final group of every level closes.
        for bitmap in [layout.spo().bitmap_y(), layout.spo().bitmap_z()] {
            let view = bitmap.view(&hdt);
            assert!(view.get(view.len() - 1), "the last group must close");
        }

        // A dictionary section's block offsets ascend and end at the sentinel,
        // which is the buffer's length.
        for section in [
            Section::Shared,
            Section::Subjects,
            Section::Predicates,
            Section::Objects,
        ] {
            let pfc = layout.dictionary().section(section);
            let offsets = pfc.block_offsets().view(&hdt);
            let buffer = pfc.buffer().view(&hdt);

            assert_eq!(offsets.len(), pfc.blocks() + 1);
            assert_eq!(
                pfc.blocks(),
                pfc.terms().div_ceil(pfc.block_size().get()),
                "{section:?}"
            );
            assert_eq!(
                offsets.get(offsets.len() - 1),
                buffer.len() as u64,
                "{section:?}: the sentinel offset is the buffer length"
            );
            for position in 1..offsets.len() {
                assert!(
                    offsets.get(position - 1) <= offsets.get(position),
                    "{section:?}: block offsets must ascend"
                );
            }
        }
    }

    /// The pairing checks, exercised directly: unit 5 builds layouts from the
    /// sidecar's directory and gets the same guarantee.
    #[test]
    fn a_layout_whose_levels_disagree_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regions");
        std::fs::write(&path, [0u8; 256]).unwrap();
        let mapping = crate::testing::map_fixture(&path);

        let array = |entries| PackedSpec::new(&mapping, 0, entries, 8).unwrap();
        let bitmap = |bits| BitmapSpec::new(&mapping, 128, bits).unwrap();

        // Four pairs holding nine triples between them.
        assert!(TriplesLayout::new(array(4), bitmap(4), array(9), bitmap(9)).is_ok());
        // The canonical empty layout has neither pairs nor triples.
        assert!(TriplesLayout::new(array(0), bitmap(0), array(0), bitmap(0)).is_ok());
        // A bitmap that does not cover the array it indexes.
        assert!(TriplesLayout::new(array(4), bitmap(5), array(9), bitmap(9)).is_err());
        assert!(TriplesLayout::new(array(4), bitmap(4), array(9), bitmap(8)).is_err());
        // A triple cannot exist without a level-2 pair that owns it.
        assert!(TriplesLayout::new(array(0), bitmap(0), array(1), bitmap(1)).is_err());
        // The two levels passed the wrong way round.
        assert!(TriplesLayout::new(array(9), bitmap(9), array(4), bitmap(4)).is_err());
    }

    #[test]
    fn dictionary_counts_must_fit_the_structures_where_they_occur() {
        assert!(check_coverage("subjects", 3, "pairs", 4).is_ok());
        assert!(check_coverage("subjects", 0, "pairs", 0).is_ok());

        assert!(check_coverage("subjects", 0, "pairs", 1).is_err());
        assert!(check_coverage("predicates", 5, "pairs", 4).is_err());
        assert!(check_coverage("objects", 10, "triples", 9).is_err());
    }

    #[test]
    fn binding_errors_are_wrapped_with_the_artifact_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.hdt");
        std::fs::write(&path, [0u8]).unwrap();
        let mapping = crate::testing::map_fixture(&path);

        let error = with_artifact::<()>(
            &mapping,
            Err(Error::Region("impossible cardinalities".to_owned())),
        )
        .expect_err("must be refused");
        match error {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, path);
                assert!(detail.contains("impossible cardinalities"), "{detail}");
            }
            other => panic!("expected a malformed-artifact error, got {other}"),
        }
    }

    /// Opening something that is not an HDT must fail with the file's name, not
    /// panic somewhere deeper.
    #[test]
    fn a_file_that_is_not_an_hdt_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.hdt");
        std::fs::write(
            &path,
            b"this is not an HDT file, but it is not empty either",
        )
        .unwrap();
        let mapping = crate::testing::map_fixture(&path);

        let error = HdtLayout::parse(&mapping).expect_err("must be refused");
        let message = error.to_string();
        assert!(message.contains("data.hdt"), "{message}");
        assert!(message.contains("malformed"), "{message}");
    }

    /// A truncated HDT is refused too: the walk's section extents no longer fit
    /// the file, which is what the specs validate.
    #[test]
    fn a_truncated_hdt_is_refused() {
        let fixture = Fixture::build(TINY_NT);
        let whole = std::fs::read(fixture.hdt_path()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.hdt");
        std::fs::write(&path, &whole[..whole.len() - 8]).unwrap();
        let mapping = crate::testing::map_fixture(&path);

        assert!(HdtLayout::parse(&mapping).is_err());
    }
}
