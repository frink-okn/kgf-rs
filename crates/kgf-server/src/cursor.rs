//! Cursor tokens.
//!
//! **A cursor is a position**, not a snapshot. There is no server-side state: a
//! token names where an enumeration stopped, and resuming seeks straight there.
//! No-loss and no-duplication (doc 03 §3.6) follow from positional resume
//! against immutable data, and rank on the permutation's bitmaps re-derives
//! group context in `O(1)` — which is precisely why the rank directories are
//! persisted (doc 20 §20.7).
//!
//! # Why a token rather than `offset=`
//!
//! Worth stating plainly, because for six of the eight patterns a position *is*
//! a result offset — [`Selection::page`](kgf_store::pattern::Selection::page)
//! takes exactly that — and for those the token is packaging. It earns its keep
//! three other ways:
//!
//! - **Positions that are not offsets.** `s ? o` resumes on the last predicate
//!   id returned, because no permutation makes its result contiguous
//!   ([`PositionSpace::Predicate`]). Bindings QUERY will resume on an (input
//!   row, offset) pair, and a budgeted scan on a scan position plus an
//!   accumulated lower bound.
//! - **Request binding.** A versioned URL pins the *data*; nothing pins the
//!   *request*. `?p=rdfs:label&cursor=X` and `?p=rdfs:label&s=ex:a&cursor=X` are
//!   the same path with the same offset, and a bare offset would silently page
//!   into a different result set. [`CanonicalRequest`] is what closes that.
//! - **Opacity.** A published offset becomes a contract clients index into, and
//!   random access has a different cost profile from paging —
//!   [`Selection::at`](kgf_store::pattern::Selection::at) for `s ? o` walks the
//!   whole bounded probe, which is why doc 03 §3.4.7 forbids `/sample` from
//!   calling it per sample.
//!
//! [`Cursor::digest_prefix`] is the weakest of the four and is documented as
//! such rather than as the reason: doc 04 §4.6 makes versioned URLs immutable,
//! so a client paging `/{dataset}/v/{version}/…` cannot drift onto other data.
//! What it still catches is a client that rebuilds page-two URLs from `latest/`
//! rather than from the resolved version, and a resume against a mirror serving
//! different bytes under the same label.
//!
//! # Revisability
//!
//! Doc 20 §20.7 calls the token stable from the first release. No release has
//! happened, and the service is expected to run for a long time before one
//! does, so this encoding is *documented* rather than frozen — the leading
//! version byte is how it moves once tokens are outstanding, and until then a
//! format change is a format change. What must not drift meanwhile is the
//! enumeration order the position indexes, which doc 20 §20.2's table fixes and
//! `kgf-store` implements.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kgf_store::pattern::{Permutation, Selection};
use sha2::{Digest, Sha256};

/// Version byte prefixing every token.
pub const TOKEN_VERSION: u8 = 1;

/// Bytes of a fixed-layout token before the optional trailers.
const FIXED_LEN: usize = 29;

/// A token that does not address this data and this request.
///
/// Lives here rather than in `kgf_store`: a cursor is HTTP-facing state, and
/// the crate boundary exists to keep that vocabulary out of storage code (doc
/// 20 §20.4). The store has no notion of a token to go stale — it enumerates
/// from a position it is handed.
///
/// Every way a token can be rejected is this one condition, deliberately: a
/// malformed token, a token for another bundle version, a token for another
/// operation, and a token for another request are all answered `stale_cursor`
/// (doc 03 §3.6) rather than distinguished, so that a client learns nothing
/// about data it did not query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stale cursor")]
pub struct StaleCursor;

/// The operation a token was issued by.
///
/// Carried so that a token cannot be replayed against a different operation
/// over the same data and request shape. `/sample` is deliberately absent: it
/// draws `n` members of a result set and never pages, so it has no position to
/// resume from (doc 03 §3.4.7).
///
/// **These discriminants are wire values.** `Count` issues a token for the
/// budgeted text scans of doc 03 §3.4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Operation {
    /// `GET|QUERY /fragment`.
    Fragment = 1,
    /// `GET|QUERY /count`, for scanning counts that exhaust their budget.
    Count = 2,
    /// `GET /describe`.
    Describe = 3,
    /// `GET /schema`.
    Schema = 4,
}

impl Operation {
    /// The wire value.
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Fragment),
            2 => Some(Self::Count),
            3 => Some(Self::Describe),
            4 => Some(Self::Schema),
            _ => None,
        }
    }
}

/// What a cursor's [`position`](Cursor::position) counts.
///
/// Not "which permutation the request used": for `s ? o` the planner may probe
/// either endpoint and both routes emit in ascending predicate order (doc 20
/// §20.2.1), so recording the route would make a legitimate route switch look
/// like a mismatch. What matters is the *space the number lives in*, and for
/// `s ? o` that is the predicate id space rather than any permutation's
/// enumeration.
///
/// This also carries `/describe`'s phase for free. `direction=both` is two
/// enumerations, out-triples (`s ? ?`, SPO) then in-triples (`? ? o`, OPS), and
/// they land in different spaces — so a token says which half it stopped in
/// without a field for it.
///
/// **These discriminants are wire values.** The mapping from `kgf_store`'s
/// internal enum is explicit in [`PositionSpace::of`] for exactly that reason:
/// adding a permutation there must force a decision here rather than silently
/// renumbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PositionSpace {
    /// Offset into a contiguous subject-rooted range.
    Spo = 1,
    /// Offset into a contiguous predicate-rooted range.
    Pos = 2,
    /// Offset into a contiguous object-rooted range.
    Ops = 3,
    /// The last predicate id returned by an `s ? o` enumeration.
    Predicate = 4,
    /// Rank in a text query's hit list, with the offset inside one hit's
    /// statements in [`Cursor::scan_position`].
    ///
    /// The one space that is not a position in doc 20 §20.2's enumeration
    /// order, because a ranked result has none: BM25 orders literals, not
    /// triples. It resumes safely for the reason the order is stable anyway —
    /// a published index is immutable and hdtc breaks score ties on ascending
    /// object id, so re-running a query and skipping is the same enumeration
    /// every time.
    ///
    /// Two numbers rather than one because a hit fans out: an object id
    /// resolves to every statement carrying that literal, so a page can stop in
    /// the middle of one. A single flat offset would resume by re-expanding
    /// every hit before it, which is work proportional to how deep the client
    /// has paged rather than to the page.
    TextRank = 5,
    /// Opaque hdtc text-index scan position, with the accumulated statement
    /// count in [`Cursor::scan_position`].
    TextScan = 6,
    /// Zero-based offset in one immediate `/schema` child collection.
    SchemaChild = 7,
    /// Byte offset in the persisted `/schema` class-relation projection.
    ClassRelation = 8,
}

impl PositionSpace {
    /// The wire value.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The space a resolved selection's positions live in.
    ///
    /// The one place that decides this, so a handler cannot pair a position with
    /// the wrong reading of it.
    ///
    /// [`TextRank`](PositionSpace::TextRank) and
    /// [`TextScan`](PositionSpace::TextScan) are deliberately unreachable from
    /// here: a text-filtered request is not one selection but an index walk and
    /// a selection per matching literal, so the space is a property of the
    /// *operation* rather than of anything this function can see.
    pub fn of(selection: &Selection<'_>) -> Self {
        if selection.subject_object_route().is_some() {
            return Self::Predicate;
        }
        match selection.permutation() {
            Permutation::Spo => Self::Spo,
            Permutation::Pos => Self::Pos,
            Permutation::Ops => Self::Ops,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Spo),
            2 => Some(Self::Pos),
            3 => Some(Self::Ops),
            4 => Some(Self::Predicate),
            5 => Some(Self::TextRank),
            6 => Some(Self::TextScan),
            7 => Some(Self::SchemaChild),
            8 => Some(Self::ClassRelation),
            _ => None,
        }
    }
}

/// The canonicalized form of a request, for binding a token to it.
///
/// # What belongs in it
///
/// The parameters that determine *which triples* are in the result set, and
/// nothing else:
///
/// - **In:** the pattern positions, and any filter that narrows the set.
/// - **Out:** `limit`, because a client may change page size between pages and
///   a position does not depend on it; `format`, which selects a serialization
///   of the same rows; and `cursor` itself.
///
/// Doc 03 §3.6 requires the binding without enumerating its contents, so this
/// rule is a decision recorded here — see `notes/plan.md`, Questions for
/// `../kgf` item 7. It matters beyond this implementation only if resuming
/// against a *mirror* is meant to work, since two servers would then have to
/// canonicalize identically.
///
/// # Encoding
///
/// Keys are sorted and every key and value is length-prefixed, so that no two
/// distinct parameter sets can hash alike — without the lengths, `a=b&c` and a
/// value containing a separator could collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRequest {
    operation: Operation,
    params: BTreeMap<String, String>,
}

impl CanonicalRequest {
    /// Start from the operation the request addresses.
    pub fn new(operation: Operation) -> Self {
        Self {
            operation,
            params: BTreeMap::new(),
        }
    }

    /// Add a result-determining parameter. Order of calls does not matter.
    #[must_use]
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.params.insert(key.to_owned(), value.to_owned());
        self
    }

    /// Add a parameter only when the request carried one.
    #[must_use]
    pub fn with_opt(self, key: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.with(key, value),
            None => self,
        }
    }

    /// The operation this request addresses.
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The 8-byte hash a token carries.
    pub fn hash(&self) -> [u8; 8] {
        let mut hasher = Sha256::new();
        hasher.update(self.operation.as_u16().to_le_bytes());
        for (key, value) in &self.params {
            hasher.update((key.len() as u64).to_le_bytes());
            hasher.update(key.as_bytes());
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        truncate(hasher.finalize().as_slice())
    }
}

/// Everything a token must match to be usable: this bundle version, this
/// operation, this request.
///
/// Bundling the three makes it impossible for a handler to check two and forget
/// the third. The bundle half is computed once when a version is opened, not per
/// request — [`BundleBinding`] is that half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorBinding {
    digest_prefix: [u8; 8],
    operation: Operation,
    request_hash: [u8; 8],
}

impl CursorBinding {
    /// Bind to a bundle version and a canonicalized request.
    pub fn new(bundle: &BundleBinding, request: &CanonicalRequest) -> Self {
        Self {
            digest_prefix: bundle.digest_prefix,
            operation: request.operation(),
            request_hash: request.hash(),
        }
    }
}

/// The bundle-version half of a binding, derived once at open.
///
/// A manifest's `content_digest` is `algorithm:hex` (doc 04 §4.3) and the token
/// carries a prefix of the digest itself, so the hex is decoded once here rather
/// than on every request. A digest that will not parse is a malformed manifest,
/// which is worth failing on at open rather than per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleBinding {
    digest_prefix: [u8; 8],
}

impl BundleBinding {
    /// Derive from a manifest's `content_digest`.
    ///
    /// Returns `None` if the digest is not `algorithm:hex` with at least eight
    /// bytes of hex.
    pub fn from_content_digest(content_digest: &str) -> Option<Self> {
        let (_algorithm, hex) = content_digest.split_once(':')?;
        let mut digest_prefix = [0u8; 8];
        for (byte, pair) in digest_prefix.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            *byte = (hi * 16 + lo) as u8;
        }
        // `chunks_exact` yields nothing past the end, so a short digest would
        // leave trailing zeros rather than failing. Check the length instead.
        if hex.len() < 16 {
            return None;
        }
        Some(Self { digest_prefix })
    }
}

/// A token this server produced, ready to hand to a client.
///
/// Only [`Cursor::encode`] builds one, so it is always non-empty URL-safe
/// base64 — safe in a query string and safe as a header value, which matters
/// because `KGF-Next-Cursor` (doc 03 §3.6) puts it in one. An arbitrary string
/// there could be empty, giving a client a continuation that continues nothing,
/// or could carry CR/LF, which is header injection.
///
/// Client-supplied tokens are *not* this type: [`Cursor::decode`] takes a plain
/// `&str`, because untrusted input has no invariant to preserve.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CursorToken(String);

impl CursorToken {
    /// The token text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CursorToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A decoded cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// Prefix of the bundle's content digest; a mismatch is `stale_cursor`.
    pub digest_prefix: [u8; 8],
    /// Which operation issued this token.
    pub operation: Operation,
    /// Hash of the canonicalized request; a mismatch is `stale_cursor`.
    pub request_hash: [u8; 8],
    /// What [`position`](Cursor::position) counts.
    pub space: PositionSpace,
    /// Position in the operation's enumeration order.
    ///
    /// **Not validated against the result set here** — this module has no store.
    /// The operation consuming the cursor validates it in the space it names:
    /// selection cardinality, predicate id space, ranked hit, or hdtc scan.
    pub position: u64,
    /// Row index, for bindings operations.
    pub binding_index: Option<u32>,
    /// Secondary position: an offset within a ranked hit, or an accumulated
    /// count for an unranked text scan.
    pub scan_position: Option<u64>,
}

impl Cursor {
    /// A cursor resuming `binding`'s enumeration at `position`.
    pub fn at(binding: &CursorBinding, space: PositionSpace, position: u64) -> Self {
        Self {
            digest_prefix: binding.digest_prefix,
            operation: binding.operation,
            request_hash: binding.request_hash,
            space,
            position,
            binding_index: None,
            scan_position: None,
        }
    }

    /// A cursor resuming one input row of a bindings fragment.
    pub fn at_binding(
        binding: &CursorBinding,
        binding_index: u32,
        space: PositionSpace,
        position: u64,
    ) -> Self {
        Self {
            binding_index: Some(binding_index),
            ..Self::at(binding, space, position)
        }
    }

    /// A cursor resuming a text-filtered enumeration at hit `rank`, `offset`
    /// statements into that hit.
    ///
    /// Separate from [`at`](Cursor::at) because the two numbers are one
    /// position: a rank without an offset would restart a hit the page was
    /// halfway through, and an offset without a rank means nothing at all.
    pub fn at_rank(binding: &CursorBinding, rank: u64, offset: u64) -> Self {
        Self {
            scan_position: Some(offset),
            ..Self::at(binding, PositionSpace::TextRank, rank)
        }
    }

    /// A resumable unranked text scan and its accumulated statement count.
    pub fn at_text_scan(binding: &CursorBinding, position: u64, accumulated: u64) -> Self {
        Self {
            scan_position: Some(accumulated),
            ..Self::at(binding, PositionSpace::TextScan, position)
        }
    }

    /// Resume one immediate `/schema` child collection at a zero-based offset.
    pub fn at_schema_child(binding: &CursorBinding, offset: u64) -> Self {
        Self::at(binding, PositionSpace::SchemaChild, offset)
    }

    /// Resume the `/schema` class-relation projection at an artifact byte offset.
    pub fn at_class_relation(binding: &CursorBinding, byte_offset: u64) -> Self {
        Self::at(binding, PositionSpace::ClassRelation, byte_offset)
    }

    /// Encode to the opaque token clients round-trip.
    ///
    /// Fixed layout, little-endian, then URL-safe base64 without padding: 29
    /// bytes and 39 characters for an M1 token. Varints would save about five
    /// bytes and cost a parser that has to be right about every length; a token
    /// this short is not worth it.
    pub fn encode(&self) -> CursorToken {
        let mut bytes = Vec::with_capacity(FIXED_LEN + 12);
        bytes.push(TOKEN_VERSION);
        bytes.extend_from_slice(&self.operation.as_u16().to_le_bytes());
        bytes.push(self.space.as_u8());
        bytes.push(self.flags());
        bytes.extend_from_slice(&self.digest_prefix);
        bytes.extend_from_slice(&self.request_hash);
        bytes.extend_from_slice(&self.position.to_le_bytes());
        if let Some(binding_index) = self.binding_index {
            bytes.extend_from_slice(&binding_index.to_le_bytes());
        }
        if let Some(scan_position) = self.scan_position {
            bytes.extend_from_slice(&scan_position.to_le_bytes());
        }
        CursorToken(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Decode a token, rejecting anything not addressed to this data, this
    /// operation, and this request.
    ///
    /// The space is *not* checked against the request: a handler that resolved a
    /// selection should compare [`PositionSpace::of`] itself, since only it
    /// knows what the request resolved to. Everything checkable without a store
    /// is checked here.
    pub fn decode(token: &str, binding: &CursorBinding) -> Result<Self, StaleCursor> {
        let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| StaleCursor)?;
        if bytes.len() < FIXED_LEN || bytes[0] != TOKEN_VERSION {
            return Err(StaleCursor);
        }

        let operation =
            Operation::from_u16(u16::from_le_bytes([bytes[1], bytes[2]])).ok_or(StaleCursor)?;
        let space = PositionSpace::from_u8(bytes[3]).ok_or(StaleCursor)?;

        let flags = bytes[4];
        if flags & !(HAS_BINDING_INDEX | HAS_SCAN_POSITION) != 0 {
            return Err(StaleCursor);
        }

        let digest_prefix: [u8; 8] = bytes[5..13].try_into().map_err(|_| StaleCursor)?;
        let request_hash: [u8; 8] = bytes[13..21].try_into().map_err(|_| StaleCursor)?;
        let position = u64::from_le_bytes(bytes[21..29].try_into().map_err(|_| StaleCursor)?);

        let mut rest = &bytes[FIXED_LEN..];
        let binding_index = if flags & HAS_BINDING_INDEX != 0 {
            Some(u32::from_le_bytes(take(&mut rest, 4)?))
        } else {
            None
        };
        let scan_position = if flags & HAS_SCAN_POSITION != 0 {
            Some(u64::from_le_bytes(take(&mut rest, 8)?))
        } else {
            None
        };
        // Trailing bytes mean this is not a token this build wrote.
        if !rest.is_empty() {
            return Err(StaleCursor);
        }

        // Constant-time comparison is not called for: both operands are derived
        // from data the client already supplied or already has.
        if digest_prefix != binding.digest_prefix
            || operation != binding.operation
            || request_hash != binding.request_hash
        {
            return Err(StaleCursor);
        }

        // Optional trailers are not independent state. Each current position
        // space has one exact shape; accepting another lets an edited token
        // silently restart a ranked hit or reinterpret a scan accumulator.
        let shape_is_valid = match space {
            PositionSpace::TextRank | PositionSpace::TextScan => {
                binding_index.is_none() && scan_position.is_some()
            }
            PositionSpace::SchemaChild | PositionSpace::ClassRelation => {
                binding_index.is_none() && scan_position.is_none()
            }
            PositionSpace::Spo
            | PositionSpace::Pos
            | PositionSpace::Ops
            | PositionSpace::Predicate => scan_position.is_none(),
        };
        if !shape_is_valid {
            return Err(StaleCursor);
        }

        Ok(Self {
            digest_prefix,
            operation,
            request_hash,
            space,
            position,
            binding_index,
            scan_position,
        })
    }

    fn flags(&self) -> u8 {
        let mut flags = 0;
        if self.binding_index.is_some() {
            flags |= HAS_BINDING_INDEX;
        }
        if self.scan_position.is_some() {
            flags |= HAS_SCAN_POSITION;
        }
        flags
    }
}

const HAS_BINDING_INDEX: u8 = 0b0000_0001;
const HAS_SCAN_POSITION: u8 = 0b0000_0010;

fn take<const N: usize>(rest: &mut &[u8], n: usize) -> Result<[u8; N], StaleCursor> {
    if rest.len() < n {
        return Err(StaleCursor);
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    head.try_into().map_err(|_| StaleCursor)
}

fn truncate(digest: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgf_store::pattern::{IdPattern, resolve};
    use kgf_store::perm::Permutations;
    use kgf_store::testing::{Fixture, TINY_NT};
    use kgf_store::{IdTriple, Role};

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str =
        "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn bundle() -> BundleBinding {
        BundleBinding::from_content_digest(DIGEST).expect("a well-formed digest")
    }

    fn request() -> CanonicalRequest {
        CanonicalRequest::new(Operation::Fragment).with("p", "rdfs:label")
    }

    fn binding() -> CursorBinding {
        CursorBinding::new(&bundle(), &request())
    }

    fn sample() -> Cursor {
        Cursor::at(&binding(), PositionSpace::Pos, 4_294_967_400)
    }

    /// Every id in a role's space, plus unbound.
    fn options(len: u64) -> Vec<Option<u64>> {
        std::iter::once(None).chain((1..=len).map(Some)).collect()
    }

    #[test]
    fn a_token_round_trips_through_every_field() {
        let cursor = sample();
        assert_eq!(
            Cursor::decode(cursor.encode().as_str(), &binding()),
            Ok(cursor)
        );

        for cursor in [
            Cursor::at_binding(&binding(), u32::MAX, PositionSpace::Predicate, 0),
            Cursor::at_rank(&binding(), 7, u64::MAX),
            Cursor::at_text_scan(&binding(), 42, 1_000),
            Cursor::at_schema_child(&binding(), 99),
            Cursor::at_class_relation(&binding(), 4_096),
        ] {
            assert_eq!(
                Cursor::decode(cursor.encode().as_str(), &binding()),
                Ok(cursor)
            );
        }

        // A trailer is part of its position space's shape, not an optional
        // field an edited token may add or remove.
        let mut unexpected = sample();
        unexpected.scan_position = Some(7);
        assert_eq!(
            Cursor::decode(unexpected.encode().as_str(), &binding()),
            Err(StaleCursor)
        );
        let mut incomplete = Cursor::at_rank(&binding(), 7, 0);
        incomplete.scan_position = None;
        assert_eq!(
            Cursor::decode(incomplete.encode().as_str(), &binding()),
            Err(StaleCursor)
        );
        let impossible = Cursor::at_binding(&binding(), 3, PositionSpace::SchemaChild, 7);
        assert_eq!(
            Cursor::decode(impossible.encode().as_str(), &binding()),
            Err(StaleCursor)
        );

        // An M1 token is short enough to sit in a URL without comment, and its
        // alphabet is what makes `CursorToken` safe in a `KGF-Next-Cursor`
        // header as well as in a query string.
        let token = sample().encode();
        assert_eq!(token.as_str().len(), 39);
        assert!(
            token
                .as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn every_rejection_is_the_same_rejection() {
        let token = sample().encode();
        let binding = binding();
        assert!(Cursor::decode(token.as_str(), &binding).is_ok());

        let mut raw = URL_SAFE_NO_PAD.decode(token.as_str()).unwrap();
        let reencode = |bytes: &[u8]| URL_SAFE_NO_PAD.encode(bytes);

        // Malformed in every way the format can be malformed.
        let mut bad = vec![
            "not base64!!".to_owned(),
            String::new(),
            reencode(&raw[..FIXED_LEN - 1]),
            reencode(&[raw.clone(), vec![0u8]].concat()),
        ];
        for (index, value) in [(0, 2u8), (3, 9u8), (4, 0b1000_0000u8)] {
            let mut tampered = raw.clone();
            tampered[index] = value;
            bad.push(reencode(&tampered));
        }
        // An unknown operation id.
        raw[1] = 99;
        bad.push(reencode(&raw));

        for token in bad {
            assert_eq!(
                Cursor::decode(token.as_str(), &binding),
                Err(StaleCursor),
                "token {token:?} should be stale"
            );
        }

        // And addressed to the wrong data, operation, or request — a client
        // must not be able to tell these apart from the above.
        let other_bundle = BundleBinding::from_content_digest(OTHER_DIGEST).unwrap();
        for wrong in [
            CursorBinding::new(&other_bundle, &request()),
            CursorBinding::new(&bundle(), &CanonicalRequest::new(Operation::Count)),
            CursorBinding::new(
                &bundle(),
                &CanonicalRequest::new(Operation::Fragment).with("p", "rdfs:comment"),
            ),
            CursorBinding::new(
                &bundle(),
                &CanonicalRequest::new(Operation::Fragment)
                    .with("p", "rdfs:label")
                    .with("s", "ex:a"),
            ),
        ] {
            assert_eq!(Cursor::decode(token.as_str(), &wrong), Err(StaleCursor));
        }
    }

    #[test]
    fn a_canonical_request_hashes_by_content_not_by_call_order() {
        let one = CanonicalRequest::new(Operation::Fragment)
            .with("p", "rdfs:label")
            .with("s", "ex:a");
        let other = CanonicalRequest::new(Operation::Fragment)
            .with("s", "ex:a")
            .with("p", "rdfs:label");
        assert_eq!(one.hash(), other.hash());

        // Length prefixes: without them these two would concatenate alike.
        let split_one = CanonicalRequest::new(Operation::Fragment).with("a", "bc");
        let split_other = CanonicalRequest::new(Operation::Fragment).with("ab", "c");
        assert_ne!(split_one.hash(), split_other.hash());

        // The operation is part of the hash as well as a separate field.
        assert_ne!(
            CanonicalRequest::new(Operation::Fragment).hash(),
            CanonicalRequest::new(Operation::Describe).hash()
        );

        // `with_opt` on an absent parameter is not the same as an empty one.
        let absent = CanonicalRequest::new(Operation::Fragment).with_opt("s", None);
        let empty = CanonicalRequest::new(Operation::Fragment).with("s", "");
        assert_ne!(absent.hash(), empty.hash());
    }

    #[test]
    fn a_bundle_binding_needs_a_parseable_digest() {
        assert!(BundleBinding::from_content_digest(DIGEST).is_some());
        assert_ne!(
            BundleBinding::from_content_digest(DIGEST),
            BundleBinding::from_content_digest(OTHER_DIGEST)
        );

        // Two digests agreeing in their first eight bytes bind alike; that is
        // the documented consequence of carrying a prefix.
        assert_eq!(
            BundleBinding::from_content_digest("sha256:0123456789abcdef0000000000000000"),
            BundleBinding::from_content_digest("sha256:0123456789abcdefffffffffffffffff")
        );

        for bad in [
            "0123456789abcdef",        // no algorithm
            "sha256:0123456789abcde",  // under eight bytes of hex
            "sha256:",                 // no hex at all
            "sha256:zzzzzzzzzzzzzzzz", // not hex
            "sha256:0123456789abcdeg", // one bad nibble
        ] {
            assert!(
                BundleBinding::from_content_digest(bad).is_none(),
                "{bad} should not parse"
            );
        }
    }

    /// The property doc 20 §20.9 asks for, composed with the codec: for every
    /// pattern shape and every stopping point, a token round-trips to a position
    /// that resumes with exactly the remaining rows.
    ///
    /// `kgf-store` already proves that positional resume yields the suffix. What
    /// this adds is that encoding and decoding a position does not change it —
    /// including for `s ? o`, whose position is a predicate id rather than an
    /// offset, which is exactly the case a codec is most likely to get wrong.
    #[test]
    fn a_token_resumes_exactly_where_the_page_stopped() {
        let fixture = Fixture::build(TINY_NT);
        let perms =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("open permutations");
        let counts = *perms.dict_counts();
        let bundle = bundle();

        let mut shapes = 0;
        let mut subject_object_shapes = 0;
        for subject in options(counts.len(Role::Subject)) {
            for predicate in options(counts.len(Role::Predicate)) {
                for object in options(counts.len(Role::Object)) {
                    let selection = resolve(
                        &perms,
                        IdPattern {
                            subject,
                            predicate,
                            object,
                        },
                    )
                    .expect("every id is in range");

                    let space = PositionSpace::of(&selection);
                    if space == PositionSpace::Predicate {
                        subject_object_shapes += 1;
                    }
                    shapes += 1;

                    let request = CanonicalRequest::new(Operation::Fragment)
                        .with_opt("s", subject.map(|id| id.to_string()).as_deref())
                        .with_opt("p", predicate.map(|id| id.to_string()).as_deref())
                        .with_opt("o", object.map(|id| id.to_string()).as_deref());
                    let binding = CursorBinding::new(&bundle, &request);

                    let all: Vec<IdTriple> = selection.page(0, usize::MAX).collect();
                    for stop in 0..=all.len() {
                        let position = resume_position(space, &all, stop);
                        let token = Cursor::at(&binding, space, position).encode();

                        let decoded = Cursor::decode(token.as_str(), &binding)
                            .expect("a token this request just issued");
                        assert_eq!(decoded.space, space);
                        assert_eq!(decoded.position, position);

                        let resumed: Vec<IdTriple> =
                            selection.page(decoded.position, usize::MAX).collect();
                        assert_eq!(
                            resumed,
                            all[stop..],
                            "resuming {space:?} at {position} after {stop} of {} rows",
                            all.len()
                        );
                    }
                }
            }
        }
        assert!(
            shapes > 0 && subject_object_shapes > 0,
            "both routes covered"
        );
    }

    /// Where a page that stopped after `stop` rows resumes from.
    ///
    /// Mirrors `Selection::page`'s contract: a result offset for the contiguous
    /// patterns, and for `s ? o` the last predicate id returned, with zero
    /// denoting the beginning.
    fn resume_position(space: PositionSpace, all: &[IdTriple], stop: usize) -> u64 {
        match space {
            PositionSpace::Predicate => stop.checked_sub(1).map_or(0, |i| all[i].predicate),
            _ => stop as u64,
        }
    }
}
