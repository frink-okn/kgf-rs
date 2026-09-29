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
    /// decides a tie between equally weighted codings: zstd compresses smaller
    /// than gzip does, and several times faster.
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
    /// **Identity unless the client prefers a coding to it.** Identity is
    /// acceptable to every client that has not excluded it (RFC 9110 §12.5.3),
    /// and a tie goes to it: a coded body has no `Content-Length`, no
    /// `Accept-Ranges`, and only a weak validator, so a browser shows no
    /// progress and restarts an interrupted download from zero. Every browser,
    /// Python's `requests`, Go's client, and `curl --compressed` send
    /// `Accept-Encoding` by default, at equal weight for every coding, so a
    /// server that preferred compression would give all of them a download
    /// they cannot resume, of an HDT that compresses by about half. A client
    /// that wants the smaller transfer says so with weights —
    /// `Accept-Encoding: zstd, identity;q=0.5` — and between codings at equal
    /// weight zstd wins.
    ///
    /// No header is identity. When identity is excluded and nothing offered
    /// here is acceptable, the response is sent uncoded anyway, which the RFC
    /// permits and which is more useful to the client than a `406`.
    pub(crate) fn negotiate(accept_encoding: Option<&str>) -> Self {
        let Some(header) = accept_encoding else {
            return Self::Identity;
        };
        let weights = Weights::parse(header);
        let identity = weights.identity();
        let mut chosen = (Self::Identity, identity);
        for coding in Self::COMPRESSED {
            let weight = weights.of(coding).unwrap_or(0);
            if weight > chosen.1 {
                chosen = (coding, weight);
            }
        }
        chosen.0
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

    /// Identity's weight: its own, else the wildcard's, else fully acceptable,
    /// since only `identity;q=0` or an unqualified `*;q=0` excludes it.
    fn identity(&self) -> u32 {
        self.identity.or(self.any).unwrap_or(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ContentCoding::{Gzip, Identity, Zstd};

    #[test]
    fn what_real_clients_send_by_default_gets_a_resumable_download() {
        for header in [
            // No header: plain `curl`, `wget`, `aria2c`.
            None,
            // A current browser.
            Some("gzip, deflate, br, zstd"),
            // `curl --compressed`, Python `requests`, Go.
            Some("deflate, gzip, br, zstd"),
            Some("gzip, deflate"),
            Some("gzip"),
            Some("zstd"),
            Some("*"),
            Some("br"),
            Some("identity"),
            Some(""),
        ] {
            assert_eq!(ContentCoding::negotiate(header), Identity, "{header:?}");
        }
    }

    #[test]
    fn a_coding_is_chosen_only_when_weighted_above_identity() {
        assert_eq!(ContentCoding::negotiate(Some("zstd, identity;q=0.5")), Zstd);
        assert_eq!(ContentCoding::negotiate(Some("gzip, identity;q=0.5")), Gzip);
        assert_eq!(
            ContentCoding::negotiate(Some("x-gzip, identity;q=0.5")),
            Gzip
        );
        assert_eq!(ContentCoding::negotiate(Some("GZIP, IDENTITY;Q=0.5")), Gzip);
        assert_eq!(
            ContentCoding::negotiate(Some("gzip;q=0.5, identity")),
            Identity
        );
        assert_eq!(ContentCoding::negotiate(Some("*;q=0, gzip")), Gzip);
        assert_eq!(
            ContentCoding::negotiate(Some("identity;q=0, zstd;q=0.1")),
            Zstd
        );
        // Between codings, the weights decide first and this server's order
        // breaks a tie.
        assert_eq!(
            ContentCoding::negotiate(Some("zstd;q=0.5, gzip, identity;q=0.1")),
            Gzip
        );
        assert_eq!(
            ContentCoding::negotiate(Some("gzip, zstd, identity;q=0.1")),
            Zstd
        );
        // Zero is a refusal, and everything refused is answered uncoded anyway.
        assert_eq!(
            ContentCoding::negotiate(Some("zstd;q=0, gzip;q=0, identity;q=0")),
            Identity
        );
        assert_eq!(
            ContentCoding::negotiate(Some("*, zstd;q=0, identity;q=0.5")),
            Gzip
        );
    }

    #[test]
    fn a_malformed_weight_drops_its_element_and_only_its_element() {
        let negotiate =
            |header: &str| ContentCoding::negotiate(Some(&format!("{header}, identity;q=0.5")));
        assert_eq!(negotiate("zstd;q=2, gzip"), Gzip);
        assert_eq!(negotiate("zstd;q, gzip"), Gzip);
        assert_eq!(negotiate("zstd;q=0.5x"), Identity);
        // The first occurrence of a coding is the one that counts.
        assert_eq!(negotiate("zstd;q=0, zstd"), Identity);
    }
}
