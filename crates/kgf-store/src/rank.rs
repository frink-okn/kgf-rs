//! Rank and select over persisted two-level directories.
//!
//! Every bitmap the read layer touches has a directory stored in
//! `data.hdt.perm` — the sidecar's own four, and the host HDT's SPO `BitmapY`
//! and `BitmapZ`, whose directories ride in the sidecar because `data.hdt`
//! cannot grow a section without ceasing to be standard HDT (invariant 3).
//!
//! Building these at open is the one thing lazy open exists to avoid: it is a
//! full read of every bitmap byte, relocated onto some unlucky first request.
//! So this module only ever *reads* directories. Nothing here constructs one.
//!
//! # Layout
//!
//! With superblock width `B` and subblock width `b`
//! (`permutation-index-format.md` §7.2 — read from the header, not assumed;
//! version 1 writes 4096 and 512):
//!
//! - `superrank[k]`, a `u64`: set bits before bit `min(k * B, L)`. There are
//!   `ceil(L / B) + 1` entries; the final entry is the total population count,
//!   which is what answers `rank1(L)` in a single load.
//! - `subrank[j]`, a `u16`: set bits from the start of the containing superblock
//!   to bit `min(j * b, L)`. There are `ceil(L / b)` entries, and every `B / b`-th
//!   entry — the first of each superblock — is zero. Values are bounded by
//!   `B - b`, which is why `u16` suffices.
//!
//! # Cost
//!
//! [`rank1`](RankedBitmap::rank1) is constant time: one `u64` load, one `u16`
//! load, and a popcount over less than one subblock.
//! [`select1`](RankedBitmap::select1) is `O(log(L / B))` for a binary search over
//! superblocks, then a bounded search within one superblock, then a scan of at
//! most one subblock. Version 1 stores no select samples; if select ever
//! profiles hot, the format reserves the extension point (doc 20 §20.10).
//!
//! # Malformed files
//!
//! Shapes are checked in [`RankedBitmap::new`]. Past that, these methods trust
//! that the directory and the bitmap describe the same data, and will panic
//! rather than return a wrong answer if they do not — a directory that
//! disagrees with its bitmap is a corrupt artifact, not a query-time condition.
//! Detecting that is `kgf verify`'s job, off the read path (doc 20 §20.6).

use crate::error::{Error, Result};
use crate::map::{BitmapSpec, BitmapView, Mapping, PackedArray, PackedSpec};

/// Where a bitmap and its directory live, and the geometry relating them,
/// validated once.
///
/// The bitmap and the directory need not share a mapping: the SPO directories
/// sit in `data.hdt.perm` and index bitmaps inside `data.hdt`, which is why
/// [`view`](RankedSpec::view) takes both files rather than one.
///
/// Building a spec reads no payload — only the sizes the headers already
/// declared — so binding every directory in a bundle stays part of a
/// header-only open.
#[derive(Debug, Clone, Copy)]
pub struct RankedSpec {
    bitmap: BitmapSpec,
    superrank: PackedSpec,
    subrank: PackedSpec,
    superblock_bits: u64,
    subblock_bits: u64,
    sentinel: u64,
    subs_per_super: u64,
}

impl RankedSpec {
    /// Bind a bitmap to its directory.
    ///
    /// Fails if the directory is not sized for the bitmap. A mismatch means the
    /// sidecar does not describe this HDT, which is a binding failure rather
    /// than something to work around.
    pub fn new(
        bitmap: BitmapSpec,
        superrank: PackedSpec,
        subrank: PackedSpec,
        superblock_bits: u32,
        subblock_bits: u32,
    ) -> Result<Self> {
        let superblock_bits = u64::from(superblock_bits);
        let subblock_bits = u64::from(subblock_bits);
        let bits = bitmap.len();
        let (want_super, want_sub) = directory_shape(bits, superblock_bits, subblock_bits)?;

        check_directory(superrank.len(), want_super, "superrank", bits)?;
        check_directory(subrank.len(), want_sub, "subrank", bits)?;
        check_width(&superrank, SUPERRANK_WIDTH, "superrank")?;
        check_width(&subrank, SUBRANK_WIDTH, "subrank")?;

        Ok(Self {
            bitmap,
            superrank,
            subrank,
            superblock_bits,
            subblock_bits,
            sentinel: want_super.saturating_sub(1),
            subs_per_super: superblock_bits / subblock_bits,
        })
    }

    /// Project onto the mapping holding the bitmap and the one holding the
    /// directory. They are frequently the same file; pass it twice.
    pub fn view<'a>(&self, bitmap: &'a Mapping, directory: &'a Mapping) -> RankedBitmap<'a> {
        RankedBitmap {
            bitmap: self.bitmap.view(bitmap),
            superrank: self.superrank.view(directory),
            subrank: self.subrank.view(directory),
            superblock_bits: self.superblock_bits,
            subblock_bits: self.subblock_bits,
            sentinel: self.sentinel,
            subs_per_super: self.subs_per_super,
        }
    }
}

/// Natural widths of the two directory arrays (`permutation-index-format.md`
/// §7.2). A directory at any other width is a different structure being read as
/// this one, which the entry counts alone would not catch — and since open
/// deliberately skips CRC verification (doc 20 §20.6), cheap structural checks
/// are what stand between a misparsed sidecar and wrong answers.
const SUPERRANK_WIDTH: u8 = 64;
const SUBRANK_WIDTH: u8 = 16;

/// Reject a directory array that is not at its natural width.
///
/// Empty arrays are exempt: an empty dataset stores no directory at all
/// (`permutation-index-format.md` §6.2), and an absent section declares no
/// width.
fn check_width(array: &PackedSpec, want: u8, name: &str) -> Result<()> {
    if array.is_empty() || array.width() == want {
        Ok(())
    } else {
        Err(Error::Region(format!(
            "{name} is {} bits per entry, expected {want}",
            array.width()
        )))
    }
}

/// The same check over an already-projected view.
fn check_view_width(array: &PackedArray<'_>, want: u8, name: &str) -> Result<()> {
    if array.is_empty() || array.width() == want {
        Ok(())
    } else {
        Err(Error::Region(format!(
            "{name} is {} bits per entry, expected {want}",
            array.width()
        )))
    }
}

/// Directory entry counts for a bitmap of `bits` bits, or an error if the block
/// widths do not nest.
fn directory_shape(bits: u64, superblock_bits: u64, subblock_bits: u64) -> Result<(u64, u64)> {
    if subblock_bits == 0 || superblock_bits == 0 {
        return Err(Error::Region(
            "rank directory block widths must be non-zero".to_owned(),
        ));
    }
    if !superblock_bits.is_multiple_of(subblock_bits) {
        return Err(Error::Region(format!(
            "superblock width {superblock_bits} is not a multiple of subblock width {subblock_bits}"
        )));
    }
    Ok(if bits == 0 {
        (0, 0)
    } else {
        (
            bits.div_ceil(superblock_bits) + 1,
            bits.div_ceil(subblock_bits),
        )
    })
}

fn check_directory(got: u64, want: u64, name: &str, bits: u64) -> Result<()> {
    if got == want {
        Ok(())
    } else {
        Err(Error::Region(format!(
            "{name} has {got} entries, expected {want} for {bits} bits"
        )))
    }
}

/// A bitmap together with the directory that indexes it.
///
/// The two need not live in the same file: the SPO directories in
/// `data.hdt.perm` index bitmaps inside `data.hdt`.
#[derive(Debug, Clone, Copy)]
pub struct RankedBitmap<'a> {
    bitmap: BitmapView<'a>,
    superrank: PackedArray<'a>,
    subrank: PackedArray<'a>,
    superblock_bits: u64,
    subblock_bits: u64,
    /// Index of the sentinel superrank entry, i.e. `superrank.len() - 1`.
    /// Pure arithmetic fixed at bind time; reading its *value* is deliberately
    /// left to `count()` so that binding touches no payload page.
    sentinel: u64,
    /// Subblocks per superblock.
    subs_per_super: u64,
}

impl<'a> RankedBitmap<'a> {
    /// Bind already-projected views.
    ///
    /// The form for callers that already hold views; anything reached from a
    /// `Store` should bind a [`RankedSpec`] at open instead.
    pub fn new(
        bitmap: BitmapView<'a>,
        superrank: PackedArray<'a>,
        subrank: PackedArray<'a>,
        superblock_bits: u32,
        subblock_bits: u32,
    ) -> Result<Self> {
        let superblock_bits = u64::from(superblock_bits);
        let subblock_bits = u64::from(subblock_bits);
        let bits = bitmap.len();
        let (want_super, want_sub) = directory_shape(bits, superblock_bits, subblock_bits)?;

        check_directory(superrank.len(), want_super, "superrank", bits)?;
        check_directory(subrank.len(), want_sub, "subrank", bits)?;
        check_view_width(&superrank, SUPERRANK_WIDTH, "superrank")?;
        check_view_width(&subrank, SUBRANK_WIDTH, "subrank")?;

        Ok(Self {
            bitmap,
            superrank,
            subrank,
            superblock_bits,
            subblock_bits,
            sentinel: want_super.saturating_sub(1),
            subs_per_super: superblock_bits / subblock_bits,
        })
    }

    /// The indexed bitmap.
    pub fn bitmap(&self) -> &BitmapView<'a> {
        &self.bitmap
    }

    /// Number of bits indexed.
    pub fn len(&self) -> u64 {
        self.bitmap.len()
    }

    /// Whether the bitmap holds no bits.
    pub fn is_empty(&self) -> bool {
        self.bitmap.is_empty()
    }

    /// Total set bits, from the directory's sentinel.
    pub fn count(&self) -> u64 {
        if self.bitmap.is_empty() {
            return 0;
        }
        self.superrank.get(self.sentinel)
    }

    /// Set bits strictly before `position`.
    ///
    /// The domain includes `position == len()`, which is how a half-open range
    /// ending at the bitmap's end is counted. That case reads the sentinel:
    /// neither sample array carries an entry past its last block, so the general
    /// formula would index one past the end of `subrank` whenever `len()` is a
    /// multiple of the subblock width.
    ///
    /// Panics if `position > len()`.
    pub fn rank1(&self, position: u64) -> u64 {
        let bits = self.bitmap.len();
        assert!(
            position <= bits,
            "rank1({position}) out of range for {bits} bits"
        );
        // Also covers the empty bitmap, where `position` can only be zero and
        // `count()` is zero.
        if position == bits {
            return self.count();
        }

        let superblock = position / self.superblock_bits;
        let subblock = position / self.subblock_bits;
        self.superrank.get(superblock)
            + self.subrank.get(subblock)
            + self
                .bitmap
                .count_ones_in(subblock * self.subblock_bits..position)
    }

    /// Position of the `i`-th set bit, zero-based.
    ///
    /// Panics if `i >= count()`.
    pub fn select1(&self, i: u64) -> u64 {
        let total = self.count();
        assert!(i < total, "select1({i}) out of range for {total} set bits");

        // The last superblock whose prefix count has not yet passed `i`. The
        // sentinel entry holds `total`, so it is never selected.
        let superblock = last_index_not_above(&self.superrank, 0, self.sentinel, i);
        let base = self.superrank.get(superblock);

        // Within that superblock, the last subblock whose prefix count has not
        // passed `i`. The first subblock of a superblock always holds zero, so
        // the search always has a valid answer.
        let first_sub = superblock * self.subs_per_super;
        let last_sub = ((superblock + 1) * self.subs_per_super).min(self.subrank.len()) - 1;
        let subblock = last_index_not_above(&self.subrank, first_sub, last_sub, i - base);

        // Everything above located a bounded starting point; finding the bit
        // within it belongs to the bitmap, which owns bit order.
        let seen = base + self.subrank.get(subblock);
        self.bitmap
            .select_from(subblock * self.subblock_bits, i - seen)
            .unwrap_or_else(|| panic!("directory claims {total} set bits but the bitmap has fewer"))
    }
}

/// The largest index in `lo..=hi` whose value does not exceed `target`.
///
/// Assumes `array[lo] <= target` and that the range is non-decreasing, both of
/// which hold for a cumulative-count directory queried below its total.
fn last_index_not_above(array: &PackedArray<'_>, lo: u64, hi: u64, target: u64) -> u64 {
    let (mut low, mut high) = (lo, hi);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if array.get(mid) <= target {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Rng, bit, map_fixture};

    const SUPER: u32 = 4096;
    const SUB: u32 = 512;

    /// A directory built the slow, obvious way, for tests only.
    ///
    /// Production code never constructs one — building a directory is a full
    /// pass over every bitmap byte, which is exactly the cost persisted
    /// directories exist to avoid.
    struct Directory {
        superrank: Vec<u8>,
        subrank: Vec<u8>,
    }

    fn build_directory(bytes: &[u8], bits: u64) -> Directory {
        let (superblock, subblock) = (u64::from(SUPER), u64::from(SUB));
        let ones_before = |end: u64| -> u64 {
            (0..end.min(bits))
                .filter(|index| bit(bytes, *index))
                .count() as u64
        };

        if bits == 0 {
            return Directory {
                superrank: Vec::new(),
                subrank: Vec::new(),
            };
        }

        let mut superrank = Vec::new();
        for k in 0..=bits.div_ceil(superblock) {
            superrank.extend_from_slice(&ones_before(k * superblock).to_le_bytes());
        }

        let mut subrank = Vec::new();
        for j in 0..bits.div_ceil(subblock) {
            let superblock_start = (j * subblock / superblock) * superblock;
            let within = ones_before((j * subblock).min(bits)) - ones_before(superblock_start);
            subrank.extend_from_slice(&u16::try_from(within).expect("fits u16").to_le_bytes());
        }

        Directory { superrank, subrank }
    }

    fn ranked<'a>(bytes: &'a [u8], bits: u64, directory: &'a Directory) -> RankedBitmap<'a> {
        let bitmap = BitmapView::new(bytes, bits).unwrap();
        // Derived from the bytes actually emitted, so the sizing rule lives in
        // one place and a wrong shared change cannot pass green.
        let super_len = directory.superrank.len() as u64 / 8;
        let sub_len = directory.subrank.len() as u64 / 2;
        RankedBitmap::new(
            bitmap,
            PackedArray::new(&directory.superrank, super_len, 64).unwrap(),
            PackedArray::new(&directory.subrank, sub_len, 16).unwrap(),
            SUPER,
            SUB,
        )
        .unwrap()
    }

    /// The answers, worked out by one linear walk over the bits.
    ///
    /// This is the oracle, so it shares no code with the implementation it
    /// checks: one pass, `ranks[p]` set bits before `p`, `ones` holding the
    /// positions in order. Memoized only so the tests stay quick — the
    /// derivation is still the obvious one.
    struct Oracle {
        ranks: Vec<u64>,
        ones: Vec<u64>,
    }

    impl Oracle {
        fn new(bytes: &[u8], bits: u64) -> Self {
            let mut ranks = Vec::with_capacity(bits as usize + 1);
            let mut ones = Vec::new();
            let mut seen = 0;
            for position in 0..bits {
                ranks.push(seen);
                if bit(bytes, position) {
                    ones.push(position);
                    seen += 1;
                }
            }
            ranks.push(seen);
            Self { ranks, ones }
        }

        fn rank(&self, position: u64) -> u64 {
            self.ranks[position as usize]
        }

        fn select(&self, i: u64) -> u64 {
            self.ones[i as usize]
        }

        fn count(&self) -> u64 {
            self.ones.len() as u64
        }
    }

    /// Fill `bits` bits with roughly `ones_in_64` set bits per 64, tail zeroed.
    fn make_bitmap(bits: u64, ones_in_64: u32, seed: u64) -> Vec<u8> {
        let mut rng = Rng::new(seed);
        let mut bytes = vec![0u8; bits.div_ceil(8) as usize];
        for bit in 0..bits {
            let keep = match ones_in_64 {
                0 => false,
                64 => true,
                n => rng.next() % 64 < u64::from(n),
            };
            if keep {
                bytes[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        }
        bytes
    }

    /// Lengths that land on, just below, and just above every block boundary
    /// that matters, plus a few sizes spanning several superblocks.
    const LENGTHS: &[u64] = &[
        0, 1, 2, 7, 8, 9, 63, 64, 65, 511, 512, 513, 1023, 1024, 4095, 4096, 4097, 8191, 8192,
        8193, 12_288, 20_000,
    ];

    #[test]
    fn rank_matches_a_naive_walk_at_every_position() {
        for &bits in LENGTHS {
            for density in [0u32, 1, 32, 63, 64] {
                let bytes = make_bitmap(bits, density, bits * 31 + u64::from(density));
                let directory = build_directory(&bytes, bits);
                let ranked = ranked(&bytes, bits, &directory);
                let oracle = Oracle::new(&bytes, bits);

                for position in 0..=bits {
                    assert_eq!(
                        ranked.rank1(position),
                        oracle.rank(position),
                        "{bits} bits at density {density}, rank1({position})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_sentinel_answers_rank_at_the_end() {
        // Lengths that are exact multiples of the subblock width are the case
        // the general formula would index one past the end of `subrank` for.
        for &bits in &[512u64, 1024, 4096, 8192] {
            let bytes = make_bitmap(bits, 32, bits);
            let directory = build_directory(&bytes, bits);
            let ranked = ranked(&bytes, bits, &directory);

            let total = Oracle::new(&bytes, bits).count();
            assert_eq!(ranked.rank1(bits), total, "{bits} bits");
            assert_eq!(ranked.count(), total, "{bits} bits");
        }
    }

    #[test]
    fn select_matches_a_naive_scan_for_every_set_bit() {
        for &bits in LENGTHS {
            for density in [1u32, 32, 63, 64] {
                let bytes = make_bitmap(bits, density, bits * 17 + u64::from(density));
                let directory = build_directory(&bytes, bits);
                let ranked = ranked(&bytes, bits, &directory);
                let oracle = Oracle::new(&bytes, bits);
                assert_eq!(ranked.count(), oracle.count());

                for i in 0..ranked.count() {
                    assert_eq!(
                        ranked.select1(i),
                        oracle.select(i),
                        "{bits} bits at density {density}, select1({i})"
                    );
                }
            }
        }
    }

    #[test]
    fn rank_and_select_invert_each_other() {
        let bits = 20_000u64;
        let bytes = make_bitmap(bits, 7, 0xDEFACED);
        let directory = build_directory(&bytes, bits);
        let ranked = ranked(&bytes, bits, &directory);

        for i in 0..ranked.count() {
            let position = ranked.select1(i);
            assert!(ranked.bitmap().get(position));
            assert_eq!(ranked.rank1(position), i);
            assert_eq!(ranked.rank1(position + 1), i + 1);
        }
    }

    #[test]
    fn a_single_set_bit_is_found_wherever_it_sits() {
        // One bit at each interesting offset, including the first of a
        // superblock, the last of one, and the very last bit of the bitmap.
        let bits = 8192u64;
        for position in [0u64, 1, 511, 512, 513, 4095, 4096, 4097, 8190, 8191] {
            let mut bytes = vec![0u8; (bits / 8) as usize];
            bytes[(position / 8) as usize] |= 1 << (position % 8);
            let directory = build_directory(&bytes, bits);
            let ranked = ranked(&bytes, bits, &directory);

            assert_eq!(ranked.count(), 1, "bit at {position}");
            assert_eq!(ranked.select1(0), position);
            assert_eq!(ranked.rank1(position), 0);
            assert_eq!(ranked.rank1(position + 1), 1);
        }
    }

    #[test]
    fn an_empty_bitmap_ranks_and_counts_zero() {
        let directory = build_directory(&[], 0);
        let ranked = ranked(&[], 0, &directory);
        assert!(ranked.is_empty());
        assert_eq!(ranked.len(), 0);
        assert_eq!(ranked.count(), 0);
        assert_eq!(ranked.rank1(0), 0);
    }

    #[test]
    fn a_spec_bound_at_open_projects_to_the_same_answers() {
        // The shape a Store actually uses: specs validated once against the
        // mappings, then projected per query. Also the cross-file case — SPO's
        // bitmap lives in data.hdt while its directory rides in data.hdt.perm —
        // so the two mappings are deliberately different files.
        let bits = 20_000u64;
        let bytes = make_bitmap(bits, 9, 0x5EC0);
        let directory = build_directory(&bytes, bits);

        let dir = tempfile::tempdir().unwrap();
        let bitmap_path = dir.path().join("data.hdt");
        let directory_path = dir.path().join("data.hdt.perm");
        std::fs::write(&bitmap_path, &bytes).unwrap();
        let mut packed = directory.superrank.clone();
        let subrank_at = packed.len() as u64;
        packed.extend_from_slice(&directory.subrank);
        std::fs::write(&directory_path, &packed).unwrap();

        let bitmap_map = map_fixture(&bitmap_path);
        let directory_map = map_fixture(&directory_path);

        let spec = RankedSpec::new(
            BitmapSpec::new(&bitmap_map, 0, bits).unwrap(),
            PackedSpec::new(&directory_map, 0, directory.superrank.len() as u64 / 8, 64).unwrap(),
            PackedSpec::new(
                &directory_map,
                subrank_at,
                directory.subrank.len() as u64 / 2,
                16,
            )
            .unwrap(),
            SUPER,
            SUB,
        )
        .unwrap();

        let ranked = spec.view(&bitmap_map, &directory_map);
        let oracle = Oracle::new(&bytes, bits);
        assert_eq!(ranked.count(), oracle.count());
        for position in (0..=bits).step_by(97) {
            assert_eq!(ranked.rank1(position), oracle.rank(position));
        }
        for i in (0..ranked.count()).step_by(13) {
            assert_eq!(ranked.select1(i), oracle.select(i));
        }

        // Projecting twice yields independent views over the same bytes, which
        // is what lets threads read concurrently without coordination.
        let again = spec.view(&bitmap_map, &directory_map);
        assert_eq!(again.select1(0), ranked.select1(0));
    }

    #[test]
    #[should_panic(expected = "not the file it was validated against")]
    fn a_spec_refuses_a_mapping_it_was_not_validated_against() {
        // The hazard `RankedSpec::view`'s two-mapping signature creates: both
        // files are large enough for either spec, so only identity catches the
        // swap. Getting a plausible wrong answer here would be far worse than a
        // panic.
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("data.hdt");
        let two = dir.path().join("data.hdt.perm");
        std::fs::write(&one, [0u8; 64]).unwrap();
        std::fs::write(&two, [0u8; 64]).unwrap();

        let first = map_fixture(&one);
        let second = map_fixture(&two);

        let spec = PackedSpec::new(&first, 0, 8, 64).unwrap();
        spec.view(&second);
    }

    #[test]
    fn a_spec_that_runs_past_its_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small");
        std::fs::write(&path, [0u8; 16]).unwrap();
        // SAFETY: a file this test just wrote and does not touch again.
        let mapping = map_fixture(&path);

        assert!(PackedSpec::new(&mapping, 0, 2, 64).is_ok());
        assert!(PackedSpec::new(&mapping, 0, 3, 64).is_err());
        assert!(PackedSpec::new(&mapping, 8, 2, 64).is_err());
        assert!(BitmapSpec::new(&mapping, 0, 128).is_ok());
        assert!(BitmapSpec::new(&mapping, 0, 129).is_err());
        assert!(PackedSpec::new(&mapping, 0, 1, 65).is_err());
    }

    #[test]
    fn a_directory_sized_for_a_different_bitmap_is_rejected() {
        let bits = 8192u64;
        let bytes = make_bitmap(bits, 32, 1);
        let directory = build_directory(&bytes, bits);
        let bitmap = BitmapView::new(&bytes, bits).unwrap();

        // Right bitmap, directory sized for half of it.
        let short_super = PackedArray::new(&directory.superrank, 2, 64).unwrap();
        let sub = PackedArray::new(&directory.subrank, bits / u64::from(SUB), 16).unwrap();
        assert!(RankedBitmap::new(bitmap, short_super, sub, SUPER, SUB).is_err());

        // Block widths that do not nest.
        let full_super =
            PackedArray::new(&directory.superrank, bits / u64::from(SUPER) + 1, 64).unwrap();
        assert!(RankedBitmap::new(bitmap, full_super, sub, 1000, 512).is_err());
        assert!(RankedBitmap::new(bitmap, full_super, sub, 4096, 0).is_err());
    }
}
