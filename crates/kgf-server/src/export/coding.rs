//! The content coding of a whole-body download, from `Accept-Encoding`.
//!
//! Implemented here rather than left to the compression layer every other
//! route passes through, because a download has to *know* its coding before it
//! answers: the coding decides the validator, whether `Content-Length` and
//! `Repr-Digest` can be sent, and which of those a `304` repeats. A layer that
//! compresses after the handler has answered can do none of that.

use crate::representation::quality_thousandths;

/// A content coding a download may be sent in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentCoding {
    /// The artifact's own bytes.
    Identity,
    /// Zstandard (RFC 8878).
    Zstd,
    /// gzip (RFC 1952).
    Gzip,
}

impl ContentCoding {
    /// The compressed codings in this server's order of preference, which
    /// decides a tie between equally weighted codings: zstd compresses N-Triples
    /// smaller than gzip does, and several times faster.
    const COMPRESSED: [Self; 2] = [Self::Zstd, Self::Gzip];

    /// The token as it appears in `Content-Encoding` and in access records.
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Zstd => "zstd",
            Self::Gzip => "gzip",
        }
    }

    /// The coding a full-body response takes for this `Accept-Encoding`.
    ///
    /// No header means identity. RFC 9110 §12.5.3 reads an absent field as
    /// "anything is acceptable", but a client that did not ask is a client that
    /// may not decode: `curl` without `--compressed` would save compressed bytes
    /// under an `.hdt` name.
    ///
    /// A listed coding is chosen over identity unless identity is itself listed
    /// with a higher weight: unlisted, identity is acceptable but only as the
    /// fallback. When nothing is acceptable — identity excluded and no coding
    /// here offered — the response is sent uncoded anyway, which the RFC
    /// permits and which is more useful to the client than a `406`.
    pub(crate) fn negotiate(accept_encoding: Option<&str>) -> Self {
        let Some(header) = accept_encoding else {
            return Self::Identity;
        };
        let weights = Weights::parse(header);
        let mut best: Option<(Self, u32)> = None;
        for coding in Self::COMPRESSED {
            let Some(weight) = weights.of(coding).filter(|weight| *weight > 0) else {
                continue;
            };
            if best.is_none_or(|(_, held)| weight > held) {
                best = Some((coding, weight));
            }
        }
        match (best, weights.identity()) {
            (Some((_, weight)), Some(identity)) if identity > weight => Self::Identity,
            (Some((coding, _)), _) => coding,
            (None, _) => Self::Identity,
        }
    }
}

/// The weights one `Accept-Encoding` gives the codings this server knows.
#[derive(Debug, Default)]
struct Weights {
    zstd: Option<u32>,
    gzip: Option<u32>,
    identity: Option<u32>,
    any: Option<u32>,
}

impl Weights {
    /// Read the list; the first occurrence of a coding is the one that counts.
    ///
    /// An element whose weight is outside RFC 9110 §12.4.2's grammar is
    /// skipped whole, rather than taken at a default weight it did not ask for.
    /// `x-gzip` is `gzip` (§8.4.1.3). Codings this server does not produce are
    /// irrelevant and ignored.
    fn parse(header: &str) -> Self {
        let mut weights = Self::default();
        for element in header.split(',') {
            let mut parts = element.split(';');
            let coding = parts.next().unwrap_or_default().trim_matches([' ', '\t']);
            if coding.is_empty() {
                continue;
            }
            let mut weight = Some(1000);
            for parameter in parts {
                let Some((name, value)) = parameter.split_once('=') else {
                    weight = None;
                    break;
                };
                if name.trim_matches([' ', '\t']).eq_ignore_ascii_case("q") {
                    weight = quality_thousandths(value.trim_matches([' ', '\t']));
                }
            }
            let Some(weight) = weight else {
                continue;
            };
            let slot = if coding.eq_ignore_ascii_case("zstd") {
                &mut weights.zstd
            } else if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
                &mut weights.gzip
            } else if coding.eq_ignore_ascii_case("identity") {
                &mut weights.identity
            } else if coding == "*" {
                &mut weights.any
            } else {
                continue;
            };
            slot.get_or_insert(weight);
        }
        weights
    }

    /// A compressed coding's weight: its own, else the wildcard's.
    fn of(&self, coding: ContentCoding) -> Option<u32> {
        let own = match coding {
            ContentCoding::Zstd => self.zstd,
            ContentCoding::Gzip => self.gzip,
            ContentCoding::Identity => self.identity,
        };
        own.or(self.any)
    }

    /// Identity's weight when the header states one, directly or through the
    /// wildcard; `None` when it is only implicitly acceptable.
    fn identity(&self) -> Option<u32> {
        self.identity.or(self.any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ContentCoding::{Gzip, Identity, Zstd};

    #[test]
    fn what_real_clients_send_gets_the_coding_they_can_decode() {
        let cases = [
            // No header: plain `curl`, `wget`, `aria2c`.
            (None, Identity),
            // A current browser.
            (Some("gzip, deflate, br, zstd"), Zstd),
            // `curl --compressed` built without zstd, and Python `requests`.
            (Some("deflate, gzip"), Gzip),
            (Some("gzip, deflate"), Gzip),
            (Some("br"), Identity),
            (Some("identity"), Identity),
            (Some(""), Identity),
            (Some("*"), Zstd),
        ];
        for (header, expected) in cases {
            assert_eq!(ContentCoding::negotiate(header), expected, "{header:?}");
        }
    }

    #[test]
    fn weights_decide_before_this_servers_preference() {
        assert_eq!(ContentCoding::negotiate(Some("zstd;q=0.5, gzip")), Gzip);
        assert_eq!(
            ContentCoding::negotiate(Some("gzip;q=0.5, zstd;q=0.5")),
            Zstd
        );
        assert_eq!(ContentCoding::negotiate(Some("GZIP;Q=0.9")), Gzip);
        assert_eq!(ContentCoding::negotiate(Some("x-gzip")), Gzip);
        // Zero is a refusal, not a low preference.
        assert_eq!(
            ContentCoding::negotiate(Some("zstd;q=0, gzip;q=0")),
            Identity
        );
        assert_eq!(ContentCoding::negotiate(Some("*;q=0, gzip")), Gzip);
        assert_eq!(ContentCoding::negotiate(Some("*, zstd;q=0")), Gzip);
    }

    #[test]
    fn identity_wins_only_when_it_is_weighted_above_the_coding() {
        assert_eq!(ContentCoding::negotiate(Some("gzip;q=0.1")), Gzip);
        assert_eq!(
            ContentCoding::negotiate(Some("gzip;q=0.5, identity")),
            Identity
        );
        assert_eq!(ContentCoding::negotiate(Some("gzip, identity")), Gzip);
        assert_eq!(ContentCoding::negotiate(Some("gzip;q=0.5, *;q=0.8")), Zstd);
        // Everything refused: the RFC lets the server send identity anyway.
        assert_eq!(ContentCoding::negotiate(Some("identity;q=0")), Identity);
    }

    #[test]
    fn a_malformed_weight_drops_its_element_and_only_its_element() {
        assert_eq!(ContentCoding::negotiate(Some("zstd;q=2, gzip")), Gzip);
        assert_eq!(ContentCoding::negotiate(Some("zstd;q, gzip")), Gzip);
        assert_eq!(ContentCoding::negotiate(Some("zstd;q=0.5x")), Identity);
        // The first occurrence of a coding is the one that counts.
        assert_eq!(ContentCoding::negotiate(Some("zstd;q=0, zstd")), Identity);
    }
}
