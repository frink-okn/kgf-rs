//! Memory mapping and typed views over mapped regions.
//!
//! **This is the only module in the crate allowed to write `unsafe`.** Doc 20
//! §20.9 makes that an obligation: the mapping surface is small and audited, and
//! everything above it is safe code over `&[u8]`.
//!
//! # The soundness argument
//!
//! `mmap` of a file is unsound in general, because another process can truncate
//! or rewrite the file underneath a live `&[u8]`. KGF relies on the property
//! that makes it sound here: **a published bundle version is immutable** (doc 04
//! §4.6). Versions are written once under a fresh directory and never edited in
//! place; an update is a new version and a new catalog entry. On top of that,
//! [`crate::store::Store::open`] checks each sidecar's binding to its HDT before
//! any region is read, so a swapped file is caught rather than mapped.
//!
//! Anything that maps a file *not* covered by that guarantee does not belong in
//! this crate.
//!
//! # One element reader; the difference is framing
//!
//! `.hdt.perm` guarantees every payload region is 64-byte aligned and that a
//! full word may be loaded at any offset inside it without running past end of
//! file (`permutation-index-format.md` §2.1). `data.hdt` guarantees neither: its
//! `LogArray` and `Bitmap` sections carry preambles, begin at arbitrary byte
//! offsets, and the last of them ends at EOF.
//!
//! That difference is **framing** — where a payload starts and how its shape is
//! discovered — and it is handled by the callers, [`crate::perm`] reading a
//! section directory and [`crate::hdt`] walking preambles. Element extraction is
//! identical in both, so there is one [`PackedArray`], and it is correct at the
//! tail by construction: reads take a 16-byte window when the backing slice has
//! room and assemble from what remains when it does not. The mapped-load
//! guarantee then means sidecar reads never take the second path, without that
//! being a property callers have to know or a flag they have to set.
//!
//! # Where errors live
//!
//! Shapes are checked **once, at construction**: a view that exists is a view
//! whose backing bytes are big enough for the entries it claims. Accessors are
//! therefore infallible and bounds-check with `assert!` — they are the innermost
//! reads in the system, and threading a `Result` through them would price every
//! lookup for a condition that construction already excluded.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Bytes needed to hold `len` entries of `width` bits.
///
/// Returns an error rather than overflowing: `len` comes from a file header, and
/// a corrupt one should be rejected rather than wrapped around.
fn packed_bytes(len: u64, width: u8) -> Result<u64> {
    len.checked_mul(u64::from(width))
        .map(|bits| bits.div_ceil(8))
        .ok_or_else(|| Error::Region(format!("{len} entries of {width} bits overflows u64")))
}

/// A whole file mapped read-only for the lifetime of a bundle version.
#[derive(Debug)]
pub struct Mapping {
    mmap: memmap2::Mmap,
    path: PathBuf,
}

impl Mapping {
    /// Map `path` read-only.
    ///
    /// The caller must have established that the file belongs to an immutable
    /// published bundle version — see the soundness argument above.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(Error::Malformed {
                artifact: path.to_path_buf(),
                detail: "file is empty".to_owned(),
            });
        }

        // SAFETY: the mapped bytes are only sound while the file's contents stay
        // put. That is the module-level invariant: bundle versions are published
        // once and never edited in place, so no writer exists for this file. A
        // caller that maps something outside that guarantee breaks this, which is
        // why `Mapping::open` is reached only through `Store::open`.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };

        Ok(Self {
            mmap,
            path: path.to_path_buf(),
        })
    }

    /// The mapped bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }

    /// File length in bytes.
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Whether the mapping is empty. Never true — [`Mapping::open`] rejects
    /// empty files — but required alongside [`Mapping::len`].
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// The file this was mapped from, for error messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A sub-slice of the mapping, checked against its length.
    ///
    /// Regions are located by a section directory or a preamble walk, both of
    /// which come from the file itself, so a range that does not fit means the
    /// file is lying about its own shape.
    pub fn region(&self, offset: u64, length: u64) -> Result<&[u8]> {
        let end = offset.checked_add(length).ok_or_else(|| {
            Error::Region(format!(
                "region at {offset} of {length} bytes overflows u64"
            ))
        })?;
        let total = self.mmap.len() as u64;
        if end > total {
            return Err(Error::Region(format!(
                "region [{offset}, {end}) runs past the end of {} ({total} bytes)",
                self.path.display()
            )));
        }
        Ok(&self.mmap[offset as usize..end as usize])
    }

    /// A region extended by whatever slack the file has after it, up to `slack`
    /// bytes.
    ///
    /// Element reads widen to 16 bytes where they can (see the module docs), so
    /// handing a view its trailing slack keeps the fast path live. Passing a
    /// region without slack is always correct, just marginally slower at the
    /// last entry or two.
    pub fn region_with_slack(&self, offset: u64, length: u64, slack: u64) -> Result<&[u8]> {
        self.region(offset, length)?;
        let total = self.mmap.len() as u64;
        let end = (offset + length).saturating_add(slack).min(total);
        Ok(&self.mmap[offset as usize..end as usize])
    }
}

/// A fixed-width packed integer array inside a mapped region.
///
/// Entry `i` begins at bit `i * width`, LSB-first, with `width` in `0..=64`;
/// `width == 0` means every entry is zero. This is the encoding shared by
/// `.hdt.perm`'s packed regions and HDT's `LogArray` payloads.
#[derive(Debug, Clone, Copy)]
pub struct PackedArray<'a> {
    bytes: &'a [u8],
    len: u64,
    width: u8,
}

impl<'a> PackedArray<'a> {
    /// Wrap `bytes` as `len` entries of `width` bits.
    ///
    /// `bytes` must be at least `ceil(len * width / 8)` long; anything past that
    /// is slack the reader may use to widen its loads, and need not exist.
    pub fn new(bytes: &'a [u8], len: u64, width: u8) -> Result<Self> {
        if width > 64 {
            return Err(Error::Region(format!(
                "packed entry width {width} exceeds 64 bits"
            )));
        }
        let needed = packed_bytes(len, width)?;
        if (bytes.len() as u64) < needed {
            return Err(Error::Region(format!(
                "{len} entries of {width} bits need {needed} bytes, region has {}",
                bytes.len()
            )));
        }
        Ok(Self { bytes, len, width })
    }

    /// Number of entries.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the array holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bits per entry.
    pub fn width(&self) -> u8 {
        self.width
    }

    /// Entry `index`.
    ///
    /// Panics if `index >= len()`. Callers hold ranges derived from the same
    /// headers that sized this view, so an out-of-range index is a bug here, not
    /// a condition to report.
    pub fn get(&self, index: u64) -> u64 {
        assert!(
            index < self.len,
            "packed array index {index} out of range for {} entries",
            self.len
        );
        if self.width == 0 {
            return 0;
        }
        let bit_start = index * u64::from(self.width);
        let byte_start = (bit_start / 8) as usize;
        let bit_offset = bit_start % 8;

        // At most 7 + 64 = 71 bits are needed, so one 16-byte window always
        // covers an entry wherever it starts.
        let window = self.load_window(byte_start);
        let mask = (1u128 << self.width) - 1;
        ((window >> bit_offset) & mask) as u64
    }

    /// Sixteen bytes from `byte_start`, zero-filled if the region ends first.
    #[inline]
    fn load_window(&self, byte_start: usize) -> u128 {
        match self.bytes.get(byte_start..byte_start + 16) {
            Some(chunk) => u128::from_le_bytes(chunk.try_into().expect("16-byte chunk")),
            None => {
                let mut buf = [0u8; 16];
                let tail = &self.bytes[byte_start..];
                let take = tail.len().min(16);
                buf[..take].copy_from_slice(&tail[..take]);
                u128::from_le_bytes(buf)
            }
        }
    }
}

/// A bitmap inside a mapped region, LSB-first within each byte.
#[derive(Debug, Clone, Copy)]
pub struct BitmapView<'a> {
    bytes: &'a [u8],
    bits: u64,
}

impl<'a> BitmapView<'a> {
    /// Wrap a region as `bits` bits.
    pub fn new(bytes: &'a [u8], bits: u64) -> Result<Self> {
        let needed = bits.div_ceil(8);
        if (bytes.len() as u64) < needed {
            return Err(Error::Region(format!(
                "{bits} bits need {needed} bytes, region has {}",
                bytes.len()
            )));
        }
        Ok(Self { bytes, bits })
    }

    /// Number of bits.
    pub fn len(&self) -> u64 {
        self.bits
    }

    /// Whether the bitmap holds no bits.
    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Whether bit `index` is set. Panics if `index >= len()`.
    pub fn get(&self, index: u64) -> bool {
        assert!(
            index < self.bits,
            "bitmap index {index} out of range for {} bits",
            self.bits
        );
        let byte = self.bytes[(index / 8) as usize];
        byte >> (index % 8) & 1 == 1
    }

    /// The byte holding bit `index * 8`, for callers scanning a bounded run.
    pub fn byte(&self, index: usize) -> u8 {
        self.bytes[index]
    }

    /// Set bits in `range`, clamped to the bitmap's length.
    ///
    /// Used by [`crate::rank`] for the partial subblock a `rank1` ends in, which
    /// is bounded by the subblock width.
    pub fn count_ones_in(&self, range: std::ops::Range<u64>) -> u64 {
        let start = range.start.min(self.bits);
        let end = range.end.min(self.bits);
        if start >= end {
            return 0;
        }

        let first = (start / 8) as usize;
        let last = ((end - 1) / 8) as usize;
        let low_mask = 0xFFu8 << (start % 8);
        let high_bit = (end - 1) % 8;
        let high_mask = if high_bit == 7 {
            0xFFu8
        } else {
            (1u8 << (high_bit + 1)) - 1
        };

        if first == last {
            return u64::from((self.bytes[first] & low_mask & high_mask).count_ones());
        }

        let mut count = u64::from((self.bytes[first] & low_mask).count_ones());
        for &byte in &self.bytes[first + 1..last] {
            count += u64::from(byte.count_ones());
        }
        count + u64::from((self.bytes[last] & high_mask).count_ones())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `values` at `width` bits each, one bit at a time.
    ///
    /// Deliberately the slowest possible implementation: it is the oracle the
    /// real reader is checked against, so it shares no code with it.
    fn encode_naive(values: &[u64], width: u8) -> Vec<u8> {
        let mut bits = Vec::new();
        for &value in values {
            for bit in 0..width {
                bits.push(value >> bit & 1 == 1);
            }
        }
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (index, set) in bits.iter().enumerate() {
            if *set {
                bytes[index / 8] |= 1 << (index % 8);
            }
        }
        bytes
    }

    /// A cheap deterministic generator; no dev-dependency for four call sites.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // SplitMix64.
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    fn values_of_width(count: usize, width: u8, seed: u64) -> Vec<u64> {
        let mut rng = Rng(seed);
        let mask = if width == 0 { 0 } else { (1u128 << width) - 1 };
        (0..count)
            .map(|_| (u128::from(rng.next()) & mask) as u64)
            .collect()
    }

    #[test]
    fn every_width_round_trips_at_every_index() {
        for width in 0..=64u8 {
            let values = values_of_width(37, width, 0xC0FFEE + u64::from(width));
            let bytes = encode_naive(&values, width);
            let array = PackedArray::new(&bytes, values.len() as u64, width).unwrap();

            assert_eq!(array.len(), values.len() as u64);
            assert_eq!(array.width(), width);
            for (index, expected) in values.iter().enumerate() {
                assert_eq!(
                    array.get(index as u64),
                    *expected,
                    "width {width}, index {index}"
                );
            }
        }
    }

    #[test]
    fn the_last_entries_read_correctly_with_no_slack() {
        // The region ends exactly at the payload, so `get` cannot widen its load
        // and must assemble from what remains. Widths near 64 are the ones that
        // straddle the most bytes.
        for width in [1u8, 7, 8, 25, 57, 63, 64] {
            let values = values_of_width(9, width, u64::from(width));
            let bytes = encode_naive(&values, width);
            assert_eq!(bytes.len() as u64, packed_bytes(9, width).unwrap());

            let array = PackedArray::new(&bytes, 9, width).unwrap();
            for (index, expected) in values.iter().enumerate() {
                assert_eq!(array.get(index as u64), *expected, "width {width}");
            }
        }
    }

    #[test]
    fn slack_does_not_change_what_is_read() {
        let values = values_of_width(20, 25, 7);
        let mut bytes = encode_naive(&values, 25);
        let tight = PackedArray::new(&bytes, 20, 25).unwrap();
        let tight: Vec<u64> = (0..20).map(|i| tight.get(i)).collect();

        bytes.extend_from_slice(&[0xAB; 32]);
        let slacked = PackedArray::new(&bytes, 20, 25).unwrap();
        let slacked: Vec<u64> = (0..20).map(|i| slacked.get(i)).collect();

        assert_eq!(tight, values);
        assert_eq!(tight, slacked);
    }

    #[test]
    fn zero_width_entries_are_all_zero() {
        let array = PackedArray::new(&[], 1000, 0).unwrap();
        assert_eq!(array.len(), 1000);
        for index in [0, 1, 499, 999] {
            assert_eq!(array.get(index), 0);
        }
    }

    #[test]
    fn an_empty_array_is_empty_at_any_width() {
        for width in [0u8, 1, 25, 64] {
            let array = PackedArray::new(&[], 0, width).unwrap();
            assert!(array.is_empty());
            assert_eq!(array.len(), 0);
        }
    }

    #[test]
    fn a_region_too_small_for_its_entries_is_rejected() {
        let bytes = [0u8; 3];
        // 8 entries of 25 bits need 25 bytes.
        assert!(PackedArray::new(&bytes, 8, 25).is_err());
        // Exactly enough is accepted.
        let exact = vec![0u8; 25];
        assert!(PackedArray::new(&exact, 8, 25).is_ok());
    }

    #[test]
    fn widths_beyond_64_are_rejected() {
        let bytes = [0u8; 64];
        assert!(PackedArray::new(&bytes, 1, 65).is_err());
    }

    #[test]
    fn absurd_lengths_are_rejected_rather_than_overflowing() {
        let bytes = [0u8; 8];
        assert!(PackedArray::new(&bytes, u64::MAX, 64).is_err());
    }

    fn naive_count(bytes: &[u8], bits: u64, range: std::ops::Range<u64>) -> u64 {
        (range.start..range.end.min(bits))
            .filter(|bit| bytes[(bit / 8) as usize] >> (bit % 8) & 1 == 1)
            .count() as u64
    }

    #[test]
    fn bitmap_bits_and_counts_match_a_naive_walk() {
        let mut rng = Rng(0xB1_7B_17);
        for bits in [
            0u64, 1, 7, 8, 9, 63, 64, 65, 511, 512, 513, 4095, 4096, 4097,
        ] {
            let mut bytes = vec![0u8; bits.div_ceil(8) as usize];
            for byte in bytes.iter_mut() {
                *byte = rng.next() as u8;
            }
            // The format requires unused tail bits to be zero.
            if bits % 8 != 0 {
                let last = (bits / 8) as usize;
                bytes[last] &= (1u8 << (bits % 8)) - 1;
            }

            let view = BitmapView::new(&bytes, bits).unwrap();
            assert_eq!(view.len(), bits);
            for bit in 0..bits {
                assert_eq!(
                    view.get(bit),
                    bytes[(bit / 8) as usize] >> (bit % 8) & 1 == 1,
                    "{bits} bits, bit {bit}"
                );
            }

            for start in 0..bits.min(70) {
                for end in start..bits.min(70) {
                    assert_eq!(
                        view.count_ones_in(start..end),
                        naive_count(&bytes, bits, start..end),
                        "{bits} bits, range {start}..{end}"
                    );
                }
            }
            assert_eq!(
                view.count_ones_in(0..bits),
                naive_count(&bytes, bits, 0..bits)
            );
        }
    }

    #[test]
    fn counting_clamps_to_the_bitmaps_length() {
        let bytes = [0xFFu8; 4];
        let view = BitmapView::new(&bytes, 20).unwrap();
        assert_eq!(view.count_ones_in(0..1000), 20);
        assert_eq!(view.count_ones_in(16..1000), 4);
        assert_eq!(view.count_ones_in(1000..2000), 0);
        assert_eq!(view.count_ones_in(10..10), 0);
    }

    #[test]
    fn a_region_too_small_for_its_bits_is_rejected() {
        let bytes = [0u8; 2];
        assert!(BitmapView::new(&bytes, 17).is_err());
        assert!(BitmapView::new(&bytes, 16).is_ok());
    }
}
