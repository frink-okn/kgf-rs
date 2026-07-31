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
//! walking those preambles with `hdtc::format`'s *skip* forms, which read shapes
//! without touching payloads. Nothing on this path may call hdtc's materializing
//! readers ([`crate::map`] explains the element access that results).
//!
//! The rank directories for this file's two bitmaps live in `data.hdt.perm`
//! (its component `0x03`), since standard HDT has nowhere to put them.

use crate::error::Result;
use crate::map::{BitmapView, PackedArray};
use crate::rank::RankedBitmap;

/// Byte offsets and shapes of the sections inside a mapped `data.hdt`.
///
/// Produced by walking the file's control info and section preambles with
/// `hdtc::format`, once, at open.
#[derive(Debug, Clone)]
pub struct HdtLayout {
    /// Total triples, from the header.
    pub triples: u64,
}

impl HdtLayout {
    /// Walk a mapped HDT and record where everything is.
    ///
    /// Header parse only: no payload byte is read.
    pub fn parse(_bytes: &[u8]) -> Result<Self> {
        todo!("use hdtc::format to walk control info, dictionary, and triples sections")
    }
}

/// One BitmapTriples permutation with an implicit level 1.
///
/// Serves SPO from `data.hdt` and POS/OPS from `data.hdt.perm` — the same
/// traversal over differently sourced views.
#[derive(Debug, Clone, Copy)]
pub struct BitmapTriples<'a> {
    _array_y: PackedArray<'a>,
    _bitmap_y: RankedBitmap<'a>,
    _array_z: PackedArray<'a>,
    _bitmap_z: RankedBitmap<'a>,
}

impl<'a> BitmapTriples<'a> {
    /// Assemble a permutation from its four regions and their directories.
    pub fn new(
        array_y: PackedArray<'a>,
        bitmap_y: RankedBitmap<'a>,
        array_z: PackedArray<'a>,
        bitmap_z: RankedBitmap<'a>,
    ) -> Self {
        Self {
            _array_y: array_y,
            _bitmap_y: bitmap_y,
            _array_z: array_z,
            _bitmap_z: bitmap_z,
        }
    }

    /// The half-open `ArrayY` range holding level-1 key `first`'s level-2 values.
    ///
    /// Two select operations. `first` is 1-based, as HDT ids are.
    pub fn level2_range(&self, _first: u64) -> std::ops::Range<u64> {
        todo!("select1 on BitmapY around the level-1 group")
    }

    /// The half-open `ArrayZ` range for the level-2 entry at `y_position`.
    pub fn level3_range(&self, _y_position: u64) -> std::ops::Range<u64> {
        todo!("select1 on BitmapZ around the level-2 group")
    }

    /// Binary search for `value` within a sorted `ArrayY` range.
    ///
    /// Sorted-within-group is normative in every permutation, which is what
    /// makes this legal.
    pub fn find_level2(&self, _range: std::ops::Range<u64>, _value: u64) -> Option<u64> {
        todo!("binary search ArrayY over the range")
    }

    /// Binary search for `value` within a sorted `ArrayZ` range.
    pub fn find_level3(&self, _range: std::ops::Range<u64>, _value: u64) -> Option<u64> {
        todo!("binary search ArrayZ over the range")
    }

    /// The level-1 key owning `y_position`, by rank.
    pub fn level1_of(&self, _y_position: u64) -> u64 {
        todo!("rank1 on BitmapY")
    }

    /// The `ArrayY` position owning `z_position`, by rank.
    pub fn level2_of(&self, _z_position: u64) -> u64 {
        todo!("rank1 on BitmapZ")
    }

    /// Raw level-3 value at a position — the innermost read on every hot path.
    pub fn level3_at(&self, _z_position: u64) -> u64 {
        todo!("PackedArray::get on ArrayZ")
    }
}

/// A bitmap paired with the view it indexes, for the SPO case where the two
/// live in different files.
#[derive(Debug, Clone, Copy)]
pub struct ForeignBitmap<'a> {
    /// The bitmap, inside `data.hdt`.
    pub bitmap: BitmapView<'a>,
}
