//! The permutation sidecar, `data.hdt.perm`.
//!
//! Twenty core sections: POS and OPS as implicit-level-1 `BitmapTriples`
//! (`0x0101`–`0x0108`, `0x0201`–`0x0208`), plus rank directories for the host
//! HDT's own SPO bitmaps (`0x0305`–`0x0308`). Both permutations' `ArrayZ`
//! payloads are subject ids, since both orderings end in S.
//!
//! # What this module does and does not do
//!
//! It **reads the section directory through `hdtc::format`** and then maps the
//! regions itself. hdtc's [`PermutationIndex`](hdtc::format::PermutationIndex)
//! owns the header parse, the version and flag checks, and the binding to the
//! HDT; duplicating that here is the drift risk docs 17–18 already record. Its
//! `triples()` path is a seek-based reader for hdtc's CLI and is not used.
//!
//! Section payloads are bare packed regions at 64-byte-aligned absolute
//! offsets with no preamble, and the format guarantees a full `u64` may be
//! loaded anywhere inside one (`permutation-index-format.md` §2.1). Locating
//! them is therefore a directory read rather than a preamble walk — the
//! difference from [`crate::hdt`]. Element access is the same
//! [`PackedArray`](crate::map::PackedArray) either way.

use crate::error::Result;
use crate::hdt::BitmapTriples;

/// Section type constants: `(component << 8) | kind` (format §5).
pub mod section {
    /// POS component.
    pub const POS: u32 = 0x01;
    /// OPS component.
    pub const OPS: u32 = 0x02;
    /// SPO component — rank directories only; the bitmaps live in the HDT.
    pub const SPO: u32 = 0x03;

    /// Level-2 value array.
    pub const ARRAY_Y: u32 = 0x01;
    /// Bitmap over `ArrayY` positions.
    pub const BITMAP_Y: u32 = 0x02;
    /// Level-3 value array.
    pub const ARRAY_Z: u32 = 0x03;
    /// Bitmap over `ArrayZ` positions.
    pub const BITMAP_Z: u32 = 0x04;
    /// `BitmapY` superblock ranks.
    pub const BITMAP_Y_SUPERRANK: u32 = 0x05;
    /// `BitmapY` subblock ranks.
    pub const BITMAP_Y_SUBRANK: u32 = 0x06;
    /// `BitmapZ` superblock ranks.
    pub const BITMAP_Z_SUPERRANK: u32 = 0x07;
    /// `BitmapZ` subblock ranks.
    pub const BITMAP_Z_SUBRANK: u32 = 0x08;

    /// Compose a section type.
    pub const fn id(component: u32, kind: u32) -> u32 {
        (component << 8) | kind
    }
}

/// The mapped permutation sidecar: its mapping, plus the specs validated
/// against it at open.
#[derive(Debug)]
pub struct Permutations {
    _triples: u64,
}

impl Permutations {
    /// Open the sidecar beside `hdt_path`, mapping its regions.
    ///
    /// The header and directory are parsed by hdtc, which also verifies the
    /// binding to the HDT (suffix length, triple count, dictionary counts).
    /// Full CRC verification is off the open path by design — it belongs to
    /// publish and to `kgf verify` (doc 20 §20.6).
    pub fn open(_hdt_path: &std::path::Path) -> Result<Self> {
        todo!(
            "hdtc::format::PermutationIndex::open, then build a spec per directory entry \
             — validation only, no payload read"
        )
    }

    /// The POS permutation: predicate → objects → subjects.
    pub fn pos(&self) -> BitmapTriples<'_> {
        todo!("project the specs for sections 0x0101..0x0108")
    }

    /// The OPS permutation: object → predicates → subjects.
    ///
    /// OPS rather than OSP because the hot object-rooted operation is
    /// *predicate-filtered* resolution — `(?, p ∈ roles, o)` for `/search` hit
    /// resolution, the `/labels` cascade, `ranges/` row recovery, and reverse
    /// star hydration. OSP degrades that to a scan of all subjects of the
    /// object, unbounded on exactly the hub literals where bounds matter most
    /// (doc 20 §20.2).
    pub fn ops(&self) -> BitmapTriples<'_> {
        todo!("project the specs for sections 0x0201..0x0208")
    }
}
