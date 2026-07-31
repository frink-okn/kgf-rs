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
//! # Two packed-array readers, not one
//!
//! `.hdt.perm` guarantees that every payload region is 64-byte aligned and that
//! a full `u64` may be loaded at any offset inside it without running past end
//! of file (`permutation-index-format.md` §2.1). `data.hdt` guarantees neither:
//! its `LogArray` and `Bitmap` sections carry preambles, begin at arbitrary byte
//! offsets, and the last of them ends at EOF. So the HDT-side reader needs a
//! tail-safe path that the sidecar-side reader does not, and conflating them is
//! how you get a segfault on the last section of every bundle.

use std::path::Path;

use crate::error::Result;

/// A whole file mapped read-only for the lifetime of a bundle version.
#[derive(Debug)]
pub struct Mapping {
    // Held for its `Drop`; the bytes are reached through `as_bytes`.
    _inner: (),
}

impl Mapping {
    /// Map `path` read-only.
    ///
    /// The caller must have established that the file belongs to an immutable
    /// published bundle version — see the soundness argument above.
    pub fn open(_path: &Path) -> Result<Self> {
        todo!("map the file read-only and record its length")
    }

    /// The mapped bytes.
    pub fn as_bytes(&self) -> &[u8] {
        todo!("expose the mapping as a slice")
    }
}

/// A fixed-width packed integer array inside a mapped region.
///
/// Entry `i` begins at bit `i * width`, LSB-first, with `width` in `0..=64`;
/// `width == 0` means every entry is zero. This is the encoding shared by
/// `.hdt.perm`'s packed regions and HDT's `LogArray` payloads — the difference
/// between them is framing and tail safety, not element layout.
#[derive(Debug, Clone, Copy)]
pub struct PackedArray<'a> {
    _bytes: &'a [u8],
    _len: u64,
    _width: u8,
}

impl<'a> PackedArray<'a> {
    /// A view over a `.hdt.perm` region, which may be read a `u64` at a time
    /// anywhere inside it (§2.1's mapped-load guarantee).
    pub fn aligned(_bytes: &'a [u8], _len: u64, _width: u8) -> Result<Self> {
        todo!("validate the region against len * width and wrap it")
    }

    /// A view over an HDT `LogArray` payload, whose final entries must be read
    /// without loading past the end of the mapping.
    pub fn tail_safe(_bytes: &'a [u8], _len: u64, _width: u8) -> Result<Self> {
        todo!("wrap with the tail-safe accessor selected")
    }

    /// Number of entries.
    pub fn len(&self) -> u64 {
        self._len
    }

    /// Whether the array holds no entries.
    pub fn is_empty(&self) -> bool {
        self._len == 0
    }

    /// Entry `index`. Panics if out of bounds — callers hold ranges derived from
    /// the same headers that sized this view.
    pub fn get(&self, _index: u64) -> u64 {
        todo!("extract width bits at index * width, LSB-first")
    }
}

/// A bitmap inside a mapped region, LSB-first within each byte.
#[derive(Debug, Clone, Copy)]
pub struct BitmapView<'a> {
    _bytes: &'a [u8],
    _bits: u64,
}

impl<'a> BitmapView<'a> {
    /// Wrap a region of `bits` bits.
    pub fn new(_bytes: &'a [u8], _bits: u64) -> Result<Self> {
        todo!("validate ceil(bits / 8) against the region and wrap it")
    }

    /// Whether bit `index` is set.
    pub fn get(&self, _index: u64) -> bool {
        todo!("load the containing byte and test the bit")
    }

    /// Number of bits.
    pub fn len(&self) -> u64 {
        self._bits
    }

    /// Whether the bitmap is empty.
    pub fn is_empty(&self) -> bool {
        self._bits == 0
    }
}
