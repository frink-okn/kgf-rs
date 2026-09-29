//! `Range: bytes=…`, parsed and then resolved against an artifact's length.
//!
//! Two steps because they fail differently. A header that does not parse is
//! ignored — RFC 9110 §14.2 has a server treat an invalid range set as absent
//! and send the whole representation — while one that parses but names nothing
//! inside the artifact is a `416`. Collapsing the two would either refuse
//! requests the RFC says to answer or answer requests it says to refuse.

use std::ops::Range;

/// Range specs beyond which a request is refused rather than read further.
///
/// A header is bounded by the HTTP stack's own limits, but those allow tens of
/// thousands of specs; nothing legitimate sends more than a handful.
const MAX_SPECS: usize = 256;

/// Parts one `multipart/byteranges` response may carry after coalescing.
///
/// RFC 9110 §14.2 lets a server reject "many small ranges" as a likely denial
/// of service. The cap is on parts *after* merging, so a client asking for
/// many adjacent slivers is served one span rather than refused.
pub(crate) const MAX_PARTS: usize = 32;

/// Gap below which two requested spans are served as one part.
///
/// A part costs a boundary line and two header lines — around this many bytes —
/// so sending the gap is cheaper than framing another part, which is the
/// coalescing RFC 9110 §15.3.7 permits.
const COALESCE_GAP: u64 = 128;

/// One `range-spec`, not yet resolved against a length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSpec {
    /// `first-last`, or `first-` to the end.
    From { first: u64, last: Option<u64> },
    /// `-length`: the final `length` bytes.
    Suffix(u64),
}

/// A `bytes` range set as the client wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ByteRangeSet {
    specs: Vec<RangeSpec>,
    /// The client sent more than [`MAX_SPECS`]; the rest were not read.
    excessive: bool,
}

/// What a range set selects from an artifact of a known length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RangeSelection {
    /// One contiguous span: a single-part `206`.
    One(Range<u64>),
    /// Two or more disjoint spans, ascending: `multipart/byteranges`.
    Several(Vec<Range<u64>>),
    /// No spec names a byte inside the artifact.
    Unsatisfiable,
    /// More parts than this server serves in one response.
    Excessive,
}

impl ByteRangeSet {
    /// Parse a `Range` field value.
    ///
    /// `None` means the header is to be ignored: a unit other than `bytes`, or a
    /// set outside RFC 9110 §14.1.2's grammar — which includes a spec whose last
    /// position precedes its first. Empty list elements are skipped, as §5.6.1
    /// asks of every list-valued field.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let (unit, set) = value.split_once('=')?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return None;
        }
        let mut specs = Vec::new();
        for element in set.split(',') {
            let element = element.trim_matches([' ', '\t']);
            if element.is_empty() {
                continue;
            }
            if specs.len() == MAX_SPECS {
                return Some(Self {
                    specs,
                    excessive: true,
                });
            }
            specs.push(RangeSpec::parse(element)?);
        }
        (!specs.is_empty()).then_some(Self {
            specs,
            excessive: false,
        })
    }

    /// How many specs the client sent, as far as they were read.
    pub(crate) fn len(&self) -> usize {
        self.specs.len()
    }

    /// Resolve against an artifact of `len` bytes.
    ///
    /// Overlapping spans, and spans closer than a part's framing, are merged;
    /// merging needs an order, so a multipart response lists its parts
    /// ascending rather than in request order.
    pub(crate) fn select(&self, len: u64) -> RangeSelection {
        if self.excessive {
            return RangeSelection::Excessive;
        }
        let mut spans: Vec<Range<u64>> = self
            .specs
            .iter()
            .filter_map(|spec| spec.span(len))
            .collect();
        spans.sort_unstable_by_key(|span| span.start);
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(previous) if span.start <= previous.end.saturating_add(COALESCE_GAP) => {
                    previous.end = previous.end.max(span.end);
                }
                _ => merged.push(span),
            }
        }
        match merged.len() {
            0 => RangeSelection::Unsatisfiable,
            1 => RangeSelection::One(merged.remove(0)),
            parts if parts > MAX_PARTS => RangeSelection::Excessive,
            _ => RangeSelection::Several(merged),
        }
    }
}

impl RangeSpec {
    fn parse(element: &str) -> Option<Self> {
        let (first, last) = element.split_once('-')?;
        if first.is_empty() {
            return Some(Self::Suffix(position(last)?));
        }
        let first = position(first)?;
        let last = match last {
            "" => None,
            last => Some(position(last)?),
        };
        if last.is_some_and(|last| last < first) {
            return None;
        }
        Some(Self::From { first, last })
    }

    /// The half-open span this spec selects, or `None` if it is unsatisfiable.
    ///
    /// A last position past the end is clamped to it, and a suffix longer than
    /// the artifact is the whole artifact — both are satisfiable requests for
    /// less than they say (RFC 9110 §14.1.2).
    fn span(self, len: u64) -> Option<Range<u64>> {
        match self {
            Self::From { first, last } => (first < len).then(|| {
                let end = last.map_or(len, |last| last.saturating_add(1).min(len));
                first..end
            }),
            Self::Suffix(length) => {
                (length > 0 && len > 0).then(|| len.saturating_sub(length)..len)
            }
        }
    }
}

/// `1*DIGIT`, saturating rather than failing on overflow.
///
/// Saturation is exact for every use: a first position past `u64::MAX` is past
/// any artifact's end, a last position that large clamps to the end anyway, and
/// a suffix that long selects everything.
fn position(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(text.bytes().fold(0u64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(header: &str, len: u64) -> RangeSelection {
        ByteRangeSet::parse(header)
            .unwrap_or_else(|| panic!("{header:?} should parse"))
            .select(len)
    }

    #[test]
    fn the_three_spec_forms_select_what_rfc_9110_says() {
        assert_eq!(select("bytes=0-499", 1000), RangeSelection::One(0..500));
        assert_eq!(select("bytes=500-", 1000), RangeSelection::One(500..1000));
        assert_eq!(select("bytes=-200", 1000), RangeSelection::One(800..1000));
        // A last position past the end clamps; a suffix past the start is all.
        assert_eq!(
            select("bytes=900-5000", 1000),
            RangeSelection::One(900..1000)
        );
        assert_eq!(select("bytes=-5000", 1000), RangeSelection::One(0..1000));
        assert_eq!(
            select("bytes=999-999", 1000),
            RangeSelection::One(999..1000)
        );
    }

    #[test]
    fn nothing_inside_the_artifact_is_unsatisfiable() {
        assert_eq!(select("bytes=1000-", 1000), RangeSelection::Unsatisfiable);
        assert_eq!(
            select("bytes=1000-2000", 1000),
            RangeSelection::Unsatisfiable
        );
        assert_eq!(select("bytes=-0", 1000), RangeSelection::Unsatisfiable);
        assert_eq!(select("bytes=0-0", 0), RangeSelection::Unsatisfiable);
        // One satisfiable spec among unsatisfiable ones is served.
        assert_eq!(
            select("bytes=5000-, 10-19", 1000),
            RangeSelection::One(10..20)
        );
    }

    #[test]
    fn a_header_outside_the_grammar_is_ignored_rather_than_refused() {
        for header in [
            "bytes=5-3",
            "bytes=",
            "bytes=-",
            "bytes=a-b",
            "bytes=1-2-3",
            "bytes = 0-1",
            "items=0-1",
            "0-1",
            "bytes=+1-2",
            "bytes=0x10-",
            "bytes=,,",
        ] {
            assert_eq!(ByteRangeSet::parse(header), None, "{header:?}");
        }
        // The unit is case-insensitive, and empty list elements are skipped.
        assert!(ByteRangeSet::parse("BYTES=0-1").is_some());
        assert_eq!(select("bytes=, 0-1 ,", 10), RangeSelection::One(0..2));
    }

    #[test]
    fn huge_positions_saturate_to_the_answer_they_imply() {
        let huge = "99999999999999999999999999";
        assert_eq!(
            select(&format!("bytes={huge}-"), 10),
            RangeSelection::Unsatisfiable
        );
        assert_eq!(
            select(&format!("bytes=2-{huge}"), 10),
            RangeSelection::One(2..10)
        );
        assert_eq!(
            select(&format!("bytes=-{huge}"), 10),
            RangeSelection::One(0..10)
        );
    }

    #[test]
    fn several_spans_are_merged_ordered_and_capped() {
        // Overlapping and adjacent spans become one part.
        assert_eq!(
            select("bytes=0-99, 50-149, 150-199", 10_000),
            RangeSelection::One(0..200)
        );
        // Spans closer than a part's framing are merged across the gap.
        assert_eq!(
            select("bytes=0-9, 100-109", 10_000),
            RangeSelection::One(0..110)
        );
        // Distant spans stay parts, ascending whatever order they were asked in.
        assert_eq!(
            select("bytes=5000-5009, 0-9", 10_000),
            RangeSelection::Several(vec![0..10, 5000..5010])
        );

        let many: Vec<String> = (0..=MAX_PARTS as u64)
            .map(|part| format!("{}-{}", part * 1000, part * 1000 + 9))
            .collect();
        let header = format!("bytes={}", many.join(","));
        assert_eq!(select(&header, 1_000_000), RangeSelection::Excessive);
        // The cap is on parts served, so the same count of adjacent slivers is
        // one span.
        let slivers: Vec<String> = (0..=MAX_PARTS as u64)
            .map(|part| format!("{}-{}", part * 10, part * 10 + 9))
            .collect();
        let header = format!("bytes={}", slivers.join(","));
        assert_eq!(
            select(&header, 1_000_000),
            RangeSelection::One(0..(MAX_PARTS as u64 + 1) * 10)
        );

        let flood = vec!["0-0"; MAX_SPECS + 1].join(",");
        assert_eq!(
            select(&format!("bytes={flood}"), 10),
            RangeSelection::Excessive
        );
    }

    #[test]
    fn a_selection_covers_every_requested_byte_and_nothing_outside_the_artifact() {
        // Exhaustive over small artifacts and every two-spec combination of a
        // representative set of specs: the served spans must be disjoint,
        // ascending, inside the artifact, and cover each byte some spec asked
        // for — the property a client reassembling a file depends on.
        let specs = [
            "0-0", "0-3", "2-5", "4-", "7-7", "-1", "-3", "-20", "3-100", "9-",
        ];
        for len in 0..12u64 {
            for a in specs {
                for b in specs {
                    let header = format!("bytes={a},{b}");
                    let set = ByteRangeSet::parse(&header).unwrap();
                    let wanted: std::collections::BTreeSet<u64> = set
                        .specs
                        .iter()
                        .filter_map(|spec| spec.span(len))
                        .flatten()
                        .collect();
                    let spans = match set.select(len) {
                        RangeSelection::One(span) => vec![span],
                        RangeSelection::Several(spans) => spans,
                        RangeSelection::Unsatisfiable => Vec::new(),
                        RangeSelection::Excessive => {
                            panic!("{header}: two specs are not excessive")
                        }
                    };
                    for pair in spans.windows(2) {
                        assert!(pair[0].end < pair[1].start, "{header} at {len}: {spans:?}");
                    }
                    let served: std::collections::BTreeSet<u64> =
                        spans.iter().cloned().flatten().collect();
                    assert!(served.iter().all(|&byte| byte < len), "{header} at {len}");
                    assert!(wanted.is_subset(&served), "{header} at {len}");
                    assert_eq!(wanted.is_empty(), spans.is_empty(), "{header} at {len}");
                }
            }
        }
    }
}
