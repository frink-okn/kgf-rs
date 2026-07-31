//! Cursor tokens.
//!
//! **A cursor is a position**, not a snapshot. It encodes the content digest
//! prefix, the operation id, a canonical-request hash, the permutation, and the
//! position — plus a binding index for bindings operations and a scan position
//! for candidate-budgeted scans. Resume is `O(1)`: the position seeks directly,
//! and rank on the permutation's bitmaps re-derives group context, which is
//! precisely why the rank directories are persisted.
//!
//! No-loss and no-duplication (doc 03 §3.6) follow from positional resume
//! against immutable data. A digest or request-hash mismatch is `stale_cursor` —
//! never a silently different answer.
//!
//! # Stable from the first release
//!
//! There is no unstable-token phase. The token's meaning is fixed by the
//! enumeration order in doc 20 §20.2's table, which the format already
//! determines, so there is nothing about it left to discover (doc 07 §7.5
//! item 24). The leading version byte exists for genuine format evolution, not
//! as a licence to change the encoding during development.

#![allow(missing_docs)]

/// Version byte prefixing every token.
pub const TOKEN_VERSION: u8 = 1;

/// The permutation a token's `position` indexes.
///
/// Carried explicitly rather than re-derived from the request, because
/// `position` means a different thing in each permutation and `s ? o` may switch
/// routes between pages (doc 20 §20.2.1). A mismatch against what the request
/// resolves to is `stale_cursor`, not a silently reinterpreted offset.
///
/// **These discriminants are wire values.** They are written into tokens that
/// clients round-trip, so they are fixed once the encoding below is implemented
/// and are not free to follow `kgf_store`'s internal enum. The mapping to
/// [`kgf_store::pattern::Permutation`] is explicit in
/// [`CursorPermutation::from_permutation`] for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CursorPermutation {
    /// Subject-rooted.
    Spo = 1,
    /// Predicate-rooted.
    Pos = 2,
    /// Object-rooted.
    Ops = 3,
}

impl CursorPermutation {
    /// The wire value.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Map from the store's internal enum. Deliberately exhaustive, so adding a
    /// permutation there forces a decision about its wire value here.
    pub fn from_permutation(permutation: kgf_store::pattern::Permutation) -> Self {
        use kgf_store::pattern::Permutation;
        match permutation {
            Permutation::Spo => Self::Spo,
            Permutation::Pos => Self::Pos,
            Permutation::Ops => Self::Ops,
        }
    }
}

/// A decoded cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Prefix of the bundle's content digest; a mismatch is `stale_cursor`.
    pub digest_prefix: [u8; 8],
    /// Which operation issued this token.
    pub operation: u16,
    /// Hash of the canonicalized request; a mismatch is `stale_cursor`.
    pub request_hash: [u8; 8],
    /// Which permutation [`position`](Cursor::position) indexes (doc 20 §20.7).
    pub permutation: CursorPermutation,
    /// Position in the operation's enumeration order.
    pub position: u64,
    /// Row index, for bindings operations.
    pub binding_index: Option<u32>,
    /// Scan position, for candidate-budgeted scans.
    pub scan_position: Option<u64>,
}

impl Cursor {
    /// Encode to the opaque token clients round-trip.
    pub fn encode(&self) -> String {
        todo!("version byte, fields, then URL-safe base64")
    }

    /// Decode a token, rejecting anything not addressed to this data and request.
    pub fn decode(_token: &str, _digest: &[u8], _request_hash: &[u8]) -> Option<Self> {
        todo!("decode, then compare digest prefix and request hash")
    }
}
