//! Shared test support.
//!
//! Only the *oracles* in each module's tests must avoid sharing code with the
//! implementation they check. Sharing between test modules is fine, and worth
//! doing: every module still to be written wants a deterministic generator and
//! a way to read a bit out of a byte slice.

/// SplitMix64 — a deterministic generator, so a failure reproduces.
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next value.
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Whether bit `index` of `bytes` is set, LSB-first within each byte.
///
/// The one-line definition of the encoding every oracle in the crate walks.
pub fn bit(bytes: &[u8], index: u64) -> bool {
    bytes[(index / 8) as usize] >> (index % 8) & 1 == 1
}

/// Map a file a test has just written and will not touch again.
///
/// [`Mapping::open`](crate::map::Mapping::open) is `unsafe` because the caller
/// must guarantee the file does not change while mapped. Tests satisfy that by
/// writing a fixture into a `tempdir` and leaving it alone, so the obligation is
/// discharged once here rather than at every call site — keeping the crate's
/// `unsafe` surface to `map` plus this one test-only wrapper.
#[allow(unsafe_code)]
pub fn map_fixture(path: &std::path::Path) -> crate::map::Mapping {
    // SAFETY: the caller has just written this file into a temporary directory
    // and does not modify or truncate it for the mapping's lifetime.
    unsafe { crate::map::Mapping::open(path) }.expect("map fixture")
}
