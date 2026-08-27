//! Memory mapping and typed views over mapped regions.
//!
//! **This is the only module in the crate allowed to write `unsafe`.** Keeping
//! the mapping surface small and audited means
//! everything above it is safe code over `&[u8]`.
//!
//! # The soundness argument
//!
//! `mmap` of a file is unsound in general, because another process can truncate
//! or rewrite the file underneath a live `&[u8]`. KGF relies on the property
//! that makes it sound here: **a published bundle version is immutable**.
//! Versions are written once under a fresh directory and never edited in
//! place; an update is a new version and a new catalog entry. On top of that,
//! [`crate::store::Store::open`] checks each sidecar's binding to its HDT before
//! any query view is exposed, so mismatched artifacts are refused at open.
//!
//! That is a *project* invariant, though, not one this crate can enforce.
//! [`Mapping::open`] therefore stays unsafe for general callers. The public
//! [`PublishedBundle`] and [`PublishedRoot`] capabilities make that premise
//! explicit: constructing one is unsafe, while opening stores and catalog
//! entries through an already-established capability is safe. The
//! crate-private `open_published` boundary below contains the only production
//! unsafe block. Everything above it is safe code over `&[u8]`.
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
//! # Specs and views
//!
//! A **spec** ([`PackedSpec`], [`BitmapSpec`], [`BytesSpec`]) is a validated
//! description of where a region is and what shape it has: offsets, lengths,
//! widths, and the arithmetic derived from them. Building one is fallible and
//! happens **once, at open**, where the file path is in hand for the error
//! message. A **view** ([`PackedArray`], [`BitmapView`], a `&[u8]`) is a spec
//! projected onto a mapping, and projecting is infallible.
//!
//! The split exists because a [`Store`](crate::store::Store) owns its
//! [`Mapping`]s and so cannot also hold views of them — that would be a
//! self-referential struct. Holding specs instead means a malformed bundle is
//! rejected by `Store::open` with a path and a remedy, rather than panicking on
//! some later request; without it, every query would re-validate and
//! `.expect()`, which is the same check moved somewhere it cannot be acted on.
//!
//! Specs are plain `Copy` data, so they are `Send + Sync` and a view costs a
//! bounds compare and a slice. Any number of threads may project the same spec
//! at once; nothing is shared but an immutable mapping.
//!
//! Past construction, accessors are infallible and bounds-check with `assert!`.
//! They are the innermost reads in the system, and threading a `Result` through
//! them would price every lookup for a condition construction already excluded.
//!
//! Projections run from a region's offset to the end of the file rather than to
//! the end of the region, which costs nothing and hands every view whatever
//! trailing slack the file has — so the widened read path above stays live
//! without any caller having to know it exists. [`BytesSpec`] is the exception,
//! because a PFC string buffer's last block is delimited by the buffer's end.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// Identifies one live [`Mapping`], so that a spec cannot be projected onto a
/// file it was not validated against.
///
/// A bundle holds several mappings of similar size — `data.hdt` and
/// `data.hdt.perm` both run to gigabytes — and [`RankedSpec`](crate::rank::RankedSpec)
/// deliberately spans two of them, since the SPO rank directories live in the
/// sidecar while the bitmaps they index live in the HDT. Swapping those two
/// arguments would pass any size check and yield plausible wrong answers, so
/// identity is what the projection asserts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingId(u64);

fn next_mapping_id() -> MappingId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    MappingId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A bundle directory whose artifacts are immutable while any derived store lives.
///
/// This is the capability that makes [`Store::open`](crate::store::Store::open)
/// safe: a path alone cannot prove the external-file invariant required by a
/// file-backed mapping, while possession of this value records that the caller
/// established it once.
#[derive(Debug, Clone)]
pub struct PublishedBundle {
    dir: PathBuf,
}

impl PublishedBundle {
    /// Assert that `dir` is a published, immutable bundle version.
    ///
    /// # Safety
    ///
    /// From construction until this capability and every derived
    /// [`Store`](crate::store::Store) have been dropped, `dir` must keep resolving
    /// to the same directory, its artifact entries and symlinks at every depth
    /// must not be replaced, and the target files must not be modified or
    /// truncated.
    pub unsafe fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The bundle-version directory.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Crate-private on purpose: a safe path-taking constructor is a safe way
    /// to reach undefined behaviour, so it may not escape this crate's own
    /// audited code (see [`crate::testing`]).
    #[cfg(test)]
    pub(crate) fn for_test(dir: &Path) -> Self {
        // SAFETY: test fixtures call this only after publishing all artifacts
        // and leave them untouched until every derived store is dropped.
        unsafe { Self::new(dir) }
    }
}

/// A catalog root whose bundle versions satisfy [`PublishedBundle`]'s invariant.
///
/// One root capability can safely derive capabilities for the version
/// directories discovered beneath it, so the catalog acknowledges the mmap
/// safety obligation once at configuration time rather than once per request.
#[derive(Debug, Clone)]
pub struct PublishedRoot {
    root: PathBuf,
}

impl PublishedRoot {
    /// Assert that every bundle version served beneath `root` is published and
    /// immutable.
    ///
    /// # Safety
    ///
    /// From construction until this capability and every derived store have
    /// been dropped, every discovered bundle path must keep resolving to the
    /// same directory, its artifact entries and symlinks at every depth must not
    /// be replaced, and the target files must not be modified or truncated.
    /// Adding unrelated entries beneath `root` is allowed.
    pub unsafe fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// The catalog root directory.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Crate-private on purpose, for [`PublishedBundle::for_test`]'s reason.
    #[cfg(test)]
    pub(crate) fn for_test(root: &Path) -> Self {
        // SAFETY: catalog fixtures call this only after publishing all bundle
        // artifacts and do not mutate their bytes while stores are live.
        unsafe { Self::new(root) }
    }

    /// Derive the capability for a version found beneath this root.
    pub(crate) fn bundle(&self, dir: PathBuf) -> PublishedBundle {
        assert!(
            dir.starts_with(&self.root),
            "catalog bundle {} is outside published root {}",
            dir.display(),
            self.root.display()
        );
        PublishedBundle { dir }
    }
}

/// Bytes needed to hold `len` entries of `width` bits.
///
/// The rounding rule belongs to the formats, so it comes from hdtc rather than
/// being restated here; only the error vocabulary is ours. Errors rather than
/// overflowing, because `len` comes from a file header and a corrupt one should
/// be rejected rather than wrapped around.
fn packed_bytes(len: u64, width: u8) -> Result<u64> {
    hdtc::format::packed_len(len, width)
        .map_err(|e| Error::Region(format!("{len} entries of {width} bits: {e}")))
}

/// A whole file mapped read-only for the lifetime of a bundle version.
#[derive(Debug)]
pub struct Mapping {
    mmap: memmap2::Mmap,
    path: PathBuf,
    id: MappingId,
}

impl Mapping {
    /// Map `path` read-only.
    ///
    /// # Safety
    ///
    /// The file must not be modified or truncated for as long as this `Mapping`
    /// lives. Nothing in this crate can check that, which is why the obligation
    /// is the caller's: reads go through `&[u8]` over the mapped pages, so a
    /// concurrent write is a data race and a truncation is a fault.
    ///
    /// KGF satisfies this by construction — a published bundle version is
    /// immutable, written once under a fresh directory and
    /// replaced only by a new version under a new name. A caller mapping
    /// anything else has to establish the equivalent.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(Error::Malformed {
                artifact: path.to_path_buf(),
                detail: "file is empty".to_owned(),
            });
        }

        // SAFETY: forwarded to this function's caller, who has undertaken that
        // the file will not change while the mapping lives.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };

        Ok(Self {
            mmap,
            path: path.to_path_buf(),
            id: next_mapping_id(),
        })
    }

    /// The mapped bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }

    /// The file this was mapped from, for error messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This mapping's identity, which specs record so they cannot be projected
    /// onto a different file.
    pub fn id(&self) -> MappingId {
        self.id
    }

    /// Length in bytes.
    fn byte_len(&self) -> u64 {
        self.mmap.len() as u64
    }

    /// The bytes from `offset` to the end of the file.
    ///
    /// Specs validate their own extent, so a projection needs only a start.
    fn from(&self, offset: u64) -> &[u8] {
        &self.mmap[offset as usize..]
    }

    /// Exactly `length` bytes from `offset`.
    ///
    /// For the one region whose *end* is load-bearing: a PFC string buffer's
    /// last block is delimited by the buffer's end, not by a following offset.
    /// Everything else projects with [`from`](Self::from) and takes its trailing
    /// slack.
    fn exact(&self, offset: u64, length: u64) -> &[u8] {
        &self.mmap[offset as usize..(offset + length) as usize]
    }
}

/// Map one artifact from a published, immutable bundle version.
///
/// This is the crate-private boundary used by [`Store`](crate::store::Store):
/// callers of the safe store API name a bundle version whose publication
/// contract includes immutability. Keeping the one unsafe
/// acknowledgement here preserves this module as the complete audited surface.
pub(crate) fn open_published(bundle: &PublishedBundle, path: &Path) -> Result<Mapping> {
    let relative = path.strip_prefix(bundle.path()).unwrap_or_else(|_| {
        panic!(
            "mapped artifact {} is outside published bundle {}",
            path.display(),
            bundle.path().display()
        )
    });
    assert!(
        relative.components().next().is_some()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "mapped artifact {} does not name a contained bundle artifact",
        path.display()
    );
    // SAFETY: constructing `bundle` established that every artifact reachable
    // beneath this directory remains immutable for the mapping's lifetime.
    unsafe { Mapping::open(path) }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn map_fixture(path: &Path) -> Mapping {
    // SAFETY: callers write a fixture in a temporary directory and leave it
    // untouched for the returned mapping's lifetime.
    unsafe { Mapping::open(path) }.expect("map fixture")
}

/// Where a packed array lives and what shape it has, validated once.
///
/// Built at open against the mapping it describes; projected to a
/// [`PackedArray`] whenever the bytes are actually needed.
#[derive(Debug, Clone, Copy)]
pub struct PackedSpec {
    mapping: MappingId,
    offset: u64,
    len: u64,
    width: u8,
    mask: u64,
}

impl PackedSpec {
    /// Validate `len` entries of `width` bits at `offset` within `mapping`.
    pub fn new(mapping: &Mapping, offset: u64, len: u64, width: u8) -> Result<Self> {
        if width > 64 {
            return Err(Error::Region(format!(
                "packed entry width {width} exceeds 64 bits"
            )));
        }
        let bytes = packed_bytes(len, width)?;
        let end = offset.checked_add(bytes).ok_or_else(|| {
            Error::Region(format!("region at {offset} of {bytes} bytes overflows u64"))
        })?;
        if end > mapping.byte_len() {
            return Err(Error::Region(format!(
                "{len} entries of {width} bits at {offset} need {bytes} bytes, \
                 past the end of {} ({} bytes)",
                mapping.path().display(),
                mapping.byte_len()
            )));
        }
        Ok(Self {
            mapping: mapping.id(),
            offset,
            len,
            width,
            mask: entry_mask(width),
        })
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

    /// Project onto the mapping this spec was validated against.
    ///
    /// Panics if given a different one. That subsumes a bounds check: the extent
    /// was validated against this exact mapping at open, and a mapping's length
    /// cannot change while it lives. Specs and their mappings sit side by side
    /// in a `Store`, so a mismatch is a bug rather than a condition to report.
    pub fn view<'a>(&self, mapping: &'a Mapping) -> PackedArray<'a> {
        assert_eq!(
            self.mapping,
            mapping.id(),
            "packed spec projected onto {}, which is not the file it was validated against",
            mapping.path().display()
        );
        PackedArray {
            bytes: mapping.from(self.offset),
            len: self.len,
            width: self.width,
            mask: self.mask,
        }
    }
}

/// `width` low bits set; `0` when `width == 0`, which makes the zero-width case
/// fall out of an ordinary read instead of needing a branch.
fn entry_mask(width: u8) -> u64 {
    if width == 0 {
        0
    } else {
        u64::MAX >> (64 - u32::from(width))
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
    mask: u64,
}

impl<'a> PackedArray<'a> {
    /// Wrap a bare slice as `len` entries of `width` bits.
    ///
    /// The form for callers that already hold a slice. Anything reached
    /// repeatedly from a `Store` should go through [`PackedSpec`] instead, so
    /// that validation happens at open where the path is known.
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
        Ok(Self {
            bytes,
            len,
            width,
            mask: entry_mask(width),
        })
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
    #[inline]
    pub fn get(&self, index: u64) -> u64 {
        assert!(
            index < self.len,
            "packed array index {index} out of range for {} entries",
            self.len
        );
        let bit_start = index * u64::from(self.width);
        let byte_start = (bit_start / 8) as usize;
        let bit_offset = bit_start % 8;

        // At most 7 + 64 = 71 bits are needed, so one 16-byte window always
        // covers an entry wherever it starts. After shifting, the entry occupies
        // the low `width` bits, so truncating to `u64` cannot lose any of it.
        let window = self.load_window(byte_start);
        ((window >> bit_offset) as u64) & self.mask
    }

    /// Sixteen bytes from `byte_start`, zero-filled if the region ends first.
    #[inline]
    fn load_window(&self, byte_start: usize) -> u128 {
        let tail = &self.bytes[byte_start..];
        match tail.first_chunk::<16>() {
            Some(chunk) => u128::from_le_bytes(*chunk),
            None => {
                // `first_chunk` failing means fewer than 16 bytes remain.
                let mut buf = [0u8; 16];
                buf[..tail.len()].copy_from_slice(tail);
                u128::from_le_bytes(buf)
            }
        }
    }
}

/// Where a bitmap lives and how long it is, validated once.
#[derive(Debug, Clone, Copy)]
pub struct BitmapSpec {
    mapping: MappingId,
    offset: u64,
    bits: u64,
}

impl BitmapSpec {
    /// Validate `bits` bits at `offset` within `mapping`.
    pub fn new(mapping: &Mapping, offset: u64, bits: u64) -> Result<Self> {
        let bytes = bits.div_ceil(8);
        let end = offset.checked_add(bytes).ok_or_else(|| {
            Error::Region(format!("region at {offset} of {bytes} bytes overflows u64"))
        })?;
        if end > mapping.byte_len() {
            return Err(Error::Region(format!(
                "{bits} bits at {offset} need {bytes} bytes, past the end of {} ({} bytes)",
                mapping.path().display(),
                mapping.byte_len()
            )));
        }
        Ok(Self {
            mapping: mapping.id(),
            offset,
            bits,
        })
    }

    /// Number of bits.
    pub fn len(&self) -> u64 {
        self.bits
    }

    /// Whether the bitmap holds no bits.
    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Project onto the mapping this spec was validated against. Panics if
    /// given a different one — see [`PackedSpec::view`].
    pub fn view<'a>(&self, mapping: &'a Mapping) -> BitmapView<'a> {
        assert_eq!(
            self.mapping,
            mapping.id(),
            "bitmap spec projected onto {}, which is not the file it was validated against",
            mapping.path().display()
        );
        BitmapView {
            bytes: mapping.from(self.offset),
            bits: self.bits,
        }
    }
}

/// Where an opaque byte region lives, validated once.
///
/// The other specs describe regions with an element structure this module
/// understands. This one describes bytes whose interpretation belongs to a
/// caller — the PFC string buffers, whose front-coded blocks [`crate::dict`]
/// decodes — so that locating and bounds-checking them still happens at open,
/// with the path in hand, rather than per query.
#[derive(Debug, Clone, Copy)]
pub struct BytesSpec {
    mapping: MappingId,
    offset: u64,
    length: u64,
}

impl BytesSpec {
    /// Validate `length` bytes at `offset` within `mapping`.
    pub fn new(mapping: &Mapping, offset: u64, length: u64) -> Result<Self> {
        let end = offset.checked_add(length).ok_or_else(|| {
            Error::Region(format!(
                "region at {offset} of {length} bytes overflows u64"
            ))
        })?;
        if end > mapping.byte_len() {
            return Err(Error::Region(format!(
                "{length} bytes at {offset} run past the end of {} ({} bytes)",
                mapping.path().display(),
                mapping.byte_len()
            )));
        }
        Ok(Self {
            mapping: mapping.id(),
            offset,
            length,
        })
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.length
    }

    /// Whether the region is empty.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Project onto the mapping this spec was validated against. Panics if
    /// given a different one — see [`PackedSpec::view`].
    ///
    /// Unlike the other projections this one ends where the region does: the
    /// bytes are self-delimiting only up to that end.
    pub fn view<'a>(&self, mapping: &'a Mapping) -> &'a [u8] {
        assert_eq!(
            self.mapping,
            mapping.id(),
            "byte spec projected onto {}, which is not the file it was validated against",
            mapping.path().display()
        );
        mapping.exact(self.offset, self.length)
    }
}

/// A bitmap inside a mapped region, LSB-first within each byte.
#[derive(Debug, Clone, Copy)]
pub struct BitmapView<'a> {
    bytes: &'a [u8],
    bits: u64,
}

impl<'a> BitmapView<'a> {
    /// Wrap a bare slice as `bits` bits.
    ///
    /// The form for callers that already hold a slice; see
    /// [`PackedArray::new`] for when to prefer a spec.
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

    /// Position of the `k`-th set bit at or after `start`, or `None` if the
    /// bitmap runs out first.
    ///
    /// Lives here rather than in [`crate::rank`] because it is the one operation
    /// that has to know the within-byte bit order, and keeping that knowledge in
    /// one file is why this type exists. `rank` supplies the bounded starting
    /// point from its directories and owns no bit layout of its own.
    pub fn select_from(&self, start: u64, k: u64) -> Option<u64> {
        // Once per `select1`, not once per element, so it is checked in every
        // build rather than only in debug: `rank` derives `start` from block
        // widths read out of a file header, and an unaligned start would make
        // the scan below begin at the containing byte and return a position off
        // by up to seven — a wrong answer where this type promises a panic.
        // `rank::directory_shape` rejects such widths at bind time; this is the
        // local statement of what that check buys.
        assert_eq!(start % 8, 0, "scans start on a byte boundary");
        let mut remaining = k;
        let mut position = start;

        while position < self.bits {
            let mut byte = self.bytes[(position / 8) as usize];
            // The last byte may carry bits past the bitmap's end. The format
            // requires them to be zero, but a reader that trusts that returns a
            // position outside its own length when they are not — a wrong
            // answer where this type promises a panic. Masking costs one branch
            // on one byte per scan.
            let left = self.bits - position;
            if left < 8 {
                byte &= (1u8 << left) - 1;
            }
            let ones = u64::from(byte.count_ones());
            if remaining < ones {
                for bit in 0..8 {
                    if byte >> bit & 1 == 1 {
                        if remaining == 0 {
                            return Some(position + bit);
                        }
                        remaining -= 1;
                    }
                }
            }
            remaining -= ones;
            position += 8;
        }
        None
    }

    /// Set bits in `range`, clamped to the bitmap's length.
    ///
    /// Used by [`crate::rank`] for the partial subblock a `rank1` ends in, which
    /// is bounded by the subblock width.
    pub fn count_ones_in(&self, range: std::ops::Range<u64>) -> u64 {
        let start = range.start;
        let end = range.end.min(self.bits);
        if start >= end {
            return 0;
        }

        let first = (start / 8) as usize;
        let last = ((end - 1) / 8) as usize;
        let low_mask = 0xFFu8 << (start % 8);
        let high_mask = 0xFFu8 >> (7 - (end - 1) % 8);

        if first == last {
            return u64::from((self.bytes[first] & low_mask & high_mask).count_ones());
        }

        // The whole bytes between the two partial ends, eight at a time: the
        // caller is a `rank1` finishing inside one subblock, so this is the
        // "at most eight u64 popcounts" the format's §7.2 costs it at.
        let middle = &self.bytes[first + 1..last];
        let mut count = u64::from((self.bytes[first] & low_mask).count_ones());
        let (words, remainder) = middle.as_chunks::<8>();
        for word in words {
            count += u64::from(u64::from_le_bytes(*word).count_ones());
        }
        for &byte in remainder {
            count += u64::from(byte.count_ones());
        }
        count + u64::from((self.bytes[last] & high_mask).count_ones())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Rng, bit};

    #[test]
    fn a_nested_bundle_artifact_is_inside_the_published_capability() {
        let temp = tempfile::tempdir().unwrap();
        let stats = temp.path().join("stats");
        std::fs::create_dir(&stats).unwrap();
        let artifact = stats.join("void.hdt");
        std::fs::write(&artifact, [1]).unwrap();

        let bundle = PublishedBundle::for_test(temp.path());
        let mapping = open_published(&bundle, &artifact).unwrap();

        assert_eq!(mapping.as_bytes(), [1]);
    }

    #[test]
    #[should_panic(expected = "does not name a contained bundle artifact")]
    fn a_lexical_escape_is_not_covered_by_the_published_capability() {
        let bundle = PublishedBundle::for_test(Path::new("/published/bundle"));
        let _ = open_published(&bundle, Path::new("/published/bundle/../elsewhere"));
    }

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

    fn values_of_width(count: usize, width: u8, seed: u64) -> Vec<u64> {
        let mut rng = Rng::new(seed);
        let mask = if width == 0 { 0 } else { (1u128 << width) - 1 };
        (0..count)
            .map(|_| (u128::from(rng.next_u64()) & mask) as u64)
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
            .filter(|index| bit(bytes, *index))
            .count() as u64
    }

    #[test]
    fn bitmap_bits_and_counts_match_a_naive_walk() {
        let mut rng = Rng::new(0xB1_7B_17);
        for bits in [
            0u64, 1, 7, 8, 9, 63, 64, 65, 511, 512, 513, 4095, 4096, 4097,
        ] {
            let mut bytes = vec![0u8; bits.div_ceil(8) as usize];
            for byte in bytes.iter_mut() {
                *byte = rng.next_u64() as u8;
            }
            // The format requires unused tail bits to be zero.
            if bits % 8 != 0 {
                let last = (bits / 8) as usize;
                bytes[last] &= (1u8 << (bits % 8)) - 1;
            }

            let view = BitmapView::new(&bytes, bits).unwrap();
            assert_eq!(view.len(), bits);
            for index in 0..bits {
                assert_eq!(
                    view.get(index),
                    bit(&bytes, index),
                    "{bits} bits, bit {index}"
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
    fn stray_tail_bits_cannot_produce_a_position_outside_the_bitmap() {
        // The format requires unused tail bits to be zero. A reader that trusts
        // that returns a position past its own length when they are not, which
        // is a wrong answer where this type promises a panic — so both scans
        // mask instead.
        for (bits, byte) in [(1u64, 0x80u8), (3, 0xF0), (5, 0xE0), (7, 0x80)] {
            let bytes = [byte];
            let view = BitmapView::new(&bytes, bits).unwrap();
            assert_eq!(view.count_ones_in(0..bits), 0, "{bits} bits of {byte:#04x}");
            assert_eq!(view.select_from(0, 0), None, "{bits} bits of {byte:#04x}");
        }

        // A real bit before the garbage is still found, at its real position.
        let view = BitmapView::new(&[0b1000_0001], 1).unwrap();
        assert_eq!(view.select_from(0, 0), Some(0));
        assert_eq!(view.count_ones_in(0..1), 1);
    }

    #[test]
    fn select_from_walks_set_bits_in_order() {
        let bytes = [0b0100_1001u8, 0b0000_0011];
        let view = BitmapView::new(&bytes, 16).unwrap();
        let found: Vec<u64> = (0..5).filter_map(|k| view.select_from(0, k)).collect();
        assert_eq!(found, vec![0, 3, 6, 8, 9]);
        assert_eq!(view.select_from(8, 0), Some(8));
        assert_eq!(view.select_from(0, 5), None);
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
