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
//! With superblock width `B = 4096` bits and subblock width `b = 512`
//! (`permutation-index-format.md` §7.2 — read from the header, not assumed):
//!
//! - `superrank[k]`: `u64`, set bits before bit `min(k * B, L)`, with
//!   `ceil(L / B) + 1` entries. The final entry is the total population count,
//!   which is what answers `rank1(L)` in one load.
//! - `subrank[j]`: `u16`, set bits from the start of the containing superblock
//!   to bit `min(j * b, L)`, with `ceil(L / b)` entries. Every eighth entry is
//!   zero. Values are bounded by `B - b = 3584`.
//!
//! `rank1` is constant time: one `u64` load, one `u16` load, and at most eight
//! `u64` popcounts. `select1` is a binary search over superblocks plus the same
//! bounded scan. Version 1 stores no select samples; if select ever profiles
//! hot, the format reserves the extension point (doc 20 §20.10).

use crate::error::Result;
use crate::map::{BitmapView, PackedArray};

/// A bitmap together with the directory that indexes it.
///
/// The bitmap and its directory need not live in the same file: the SPO
/// directories index bitmaps inside `data.hdt`.
#[derive(Debug, Clone, Copy)]
pub struct RankedBitmap<'a> {
    _bitmap: BitmapView<'a>,
    _superrank: PackedArray<'a>,
    _subrank: PackedArray<'a>,
    _superblock_bits: u32,
    _subblock_bits: u32,
}

impl<'a> RankedBitmap<'a> {
    /// Bind a bitmap to its directory.
    ///
    /// Returns an error if the directory is not sized for the bitmap — a
    /// mismatch means the sidecar does not describe this HDT, which is a
    /// binding failure rather than a recoverable condition.
    pub fn new(
        _bitmap: BitmapView<'a>,
        _superrank: PackedArray<'a>,
        _subrank: PackedArray<'a>,
        _superblock_bits: u32,
        _subblock_bits: u32,
    ) -> Result<Self> {
        todo!("check ceil(L/B)+1 and ceil(L/b) against the directory lengths")
    }

    /// Set bits strictly before `position`.
    ///
    /// The domain includes `position == len()`, which is how a half-open range
    /// ending at the bitmap's end is counted; that case reads the sentinel
    /// rather than indexing one past the end of `subrank`.
    pub fn rank1(&self, _position: u64) -> u64 {
        todo!("superrank[p/B] + subrank[p/b] + popcount of the partial subblock")
    }

    /// Position of the `i`-th set bit, zero-based.
    pub fn select1(&self, _i: u64) -> u64 {
        todo!("binary search superrank, then subrank within the superblock, then scan")
    }

    /// Total set bits.
    pub fn count(&self) -> u64 {
        todo!("read the superrank sentinel")
    }
}
