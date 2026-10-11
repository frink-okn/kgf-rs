//! Preferred labels: one per term, chosen language first.
//!
//! A term's candidates are the literal values of the release's frozen `label`
//! predicates where the term is the subject. They are ordered by **language
//! rank**, then by the predicate's place in that list, then by dictionary term
//! id, and the first of them is the label. Language dominates because the
//! wrong language is the more damaging miss: a client that asked for English
//! would rather have a second-choice predicate in English than a first-choice
//! one in German.
//!
//! Language rank is the request's language ranges in order, each matched by
//! RFC 4647 basic filtering so `en` matches `en-gb`; then literals with no
//! language tag; then every other tag. An untagged literal outranks a
//! wrong-language one on purpose: the tag asserts a language nobody asked for,
//! while no tag asserts nothing — and the strings that carry none are mostly
//! the language-neutral ones, chemical names, symbols, binomials, accessions.
//! The term-id tie-break is what makes "the" label one string from call to
//! call, which caches and response diffs depend on.
//!
//! # What it costs
//!
//! A language tag is the last thing in a literal's spelling, so the values of
//! one `(subject, predicate)` group are not grouped by language and the best
//! one cannot be found by search. The cascade reads them in order instead, and
//! stops at the first value no later one could beat. In the common case — one
//! label per predicate, in a language the request ranks first, or untagged
//! when it named none — that is one value, the same work as reading the first
//! value of the first predicate.
//!
//! The rest is bounded by an allowance of values read: `candidate_budget`, as
//! a window of the request's own beside whatever its enumeration spends,
//! because a ranked page that legitimately spends its whole candidate budget
//! would otherwise leave its labels nothing. Each value is charged as it is
//! read, and none is read past the allowance, so a request never reads more
//! label values than that, whoever chose its predicates. A term the allowance
//! cannot settle ends a response before the item that carries it — unless that
//! is the response's first item, which nothing can follow: then the request is
//! refused, because a response that carried nothing would hand back a cursor
//! that never moves.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use hdtc::format::parse_literal;
use kgf_store::dict::Dictionary;
use kgf_store::pattern::IdPattern;
use kgf_store::{Role, Store, TermId};

use crate::answer::{select, unreadable};
use crate::envelope::{ErrorCode, Problem};
use crate::term::{LiteralKind, Term};

/// One language range in RFC 4647 §2.1's basic syntax: `*`, or subtags of one
/// to eight ASCII letters or digits joined by hyphens, the first all letters.
///
/// Held lowercased, the form it is echoed in. Matching ignores case on both
/// sides, since language tags compare case-insensitively and a dictionary may
/// hold `en-GB` as readily as `en-gb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageRange(Box<str>);

/// A string that is not a basic language range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotALanguageRange;

impl LanguageRange {
    /// The wildcard, which matches every language tag and no untagged literal.
    const WILDCARD: &'static str = "*";

    /// Parse one basic language range.
    pub fn parse(text: &str) -> Result<Self, NotALanguageRange> {
        if text == Self::WILDCARD {
            return Ok(Self(Box::from(Self::WILDCARD)));
        }
        let mut subtags = text.split('-');
        let primary = subtags.next().ok_or(NotALanguageRange)?;
        let primary_ok = (1..=8).contains(&primary.len())
            && primary.bytes().all(|byte| byte.is_ascii_alphabetic());
        let rest_ok = subtags.all(|subtag| {
            (1..=8).contains(&subtag.len())
                && subtag.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if primary_ok && rest_ok {
            Ok(Self(text.to_ascii_lowercase().into_boxed_str()))
        } else {
            Err(NotALanguageRange)
        }
    }

    /// The range as the response and the cursor canonicalization spell it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// RFC 4647 §3.3.1 basic filtering: the range equals the tag, or is a
    /// prefix of it that ends where one of the tag's subtags does.
    fn matches(&self, tag: &[u8]) -> bool {
        if self.0.as_ref() == Self::WILDCARD {
            return true;
        }
        let range = self.0.as_bytes();
        tag.get(..range.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(range))
            && matches!(tag.get(range.len()), None | Some(b'-'))
    }
}

impl fmt::Display for LanguageRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A request's language preference, strongest first; empty when it named none.
///
/// Repeated ranges are dropped at construction, keeping the first, because a
/// range already tried cannot match anything new later in the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Languages(Vec<LanguageRange>);

impl Languages {
    /// The preference these ranges state, in order.
    pub fn new(ranges: impl IntoIterator<Item = LanguageRange>) -> Self {
        let mut kept: Vec<LanguageRange> = Vec::new();
        for range in ranges {
            if !kept.contains(&range) {
                kept.push(range);
            }
        }
        Self(kept)
    }

    /// The ranges, strongest first.
    pub fn ranges(&self) -> &[LanguageRange] {
        &self.0
    }

    /// Whether the request named no language.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Where a literal carrying `tag` falls in this preference.
    fn rank(&self, tag: Option<&[u8]>) -> LanguageRank {
        let requested = self.0.len() as u32;
        match tag {
            None => LanguageRank(requested),
            Some(tag) => self
                .0
                .iter()
                .position(|range| range.matches(tag))
                .map_or(LanguageRank(requested + 1), |index| {
                    LanguageRank(index as u32)
                }),
        }
    }
}

/// A candidate's language rank: lower is preferred.
///
/// `0..k` are the request's `k` ranges in order, `k` is an untagged literal,
/// and `k + 1` is every other language tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LanguageRank(u32);

impl LanguageRank {
    /// The best rank any candidate can have. A value at this rank on the
    /// predicate being read cannot be beaten by a later value of it — those
    /// have higher term ids — or by any value of a later predicate.
    const BEST: Self = Self(0);
}

/// The label a cascade chose, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferredLabel {
    value: Box<str>,
    predicate: Rc<str>,
    language: Option<Box<str>>,
    /// The literal as the dictionary spells it, datatype included, for a
    /// representation that writes the label's statement rather than its
    /// string.
    term: Box<str>,
}

impl PreferredLabel {
    /// A label made by hand, for tests that weigh one without a bundle.
    #[cfg(test)]
    pub(crate) fn new(value: &str, predicate: &str, language: Option<&str>) -> Self {
        Self {
            value: Box::from(value),
            predicate: Rc::from(predicate),
            language: language.map(Box::from),
            term: match language {
                Some(language) => format!("\"{value}\"@{language}").into_boxed_str(),
                None => format!("\"{value}\"").into_boxed_str(),
            },
        }
    }

    /// The label's lexical form.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The full IRI of the predicate whose value this is.
    pub fn predicate(&self) -> &str {
        &self.predicate
    }

    /// The literal's language tag, lowercased whatever case the dictionary
    /// stores it in; `None` for an untagged one.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    /// The literal as the dictionary spells it.
    pub fn term(&self) -> &str {
        &self.term
    }
}

/// How one term's resolution ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The cascade completed: the label, or `None` when the term has no
    /// literal value under any label predicate.
    Resolved(Option<PreferredLabel>),
    /// The term's candidates could cost more than the allowance has left, and
    /// it was not resolved at all. Never a null: a null says the term has no
    /// label, and this says nothing about the term.
    Exhausted,
}

/// What an allowance too small to settle a term means for the item that
/// carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spend {
    /// A later item: the response ends before it, and says the candidate
    /// budget ended it.
    WithinAllowance,
    /// A response's first item, or a term every response of the request
    /// carries: nothing can come before it, so the request is refused.
    First,
}

impl Spend {
    /// The spend for an item, which is the first exactly when nothing has
    /// been kept before it.
    pub fn first_if(first: bool) -> Self {
        if first {
            Self::First
        } else {
            Self::WithinAllowance
        }
    }
}

/// One request's label resolution: the cascade, and what it may still spend.
///
/// Not `Send`, like the term cache beside it, because it lives inside one
/// request's blocking task and its memos are that request's alone.
pub struct LabelCascade<'s> {
    store: &'s Store,
    dictionary: Dictionary<'s>,
    /// The label predicates this bundle holds, in cascade order, each with the
    /// full IRI a reported source names.
    predicates: Vec<(u64, Rc<str>)>,
    languages: Languages,
    /// Language rank per distinct tag. Tags are few and values are many, so
    /// matching every value against every range would be the cost of this
    /// cascade rather than a constant beside it.
    ranks: HashMap<Box<[u8]>, LanguageRank>,
    /// Values this request may read in all, for the refusal that names it.
    allowance: u64,
    /// Values this request may still read.
    remaining: u64,
    /// Each subject resolved so far, so a term repeated down a page is
    /// resolved — and charged — once.
    resolved: HashMap<u64, Option<PreferredLabel>>,
    scratch: Vec<u8>,
}

impl<'s> LabelCascade<'s> {
    /// A cascade over `predicates` — full IRIs, in declared order — that may
    /// examine `allowance` values in all.
    ///
    /// A predicate this bundle's dictionary does not hold has no values, so it
    /// is dropped here rather than probed for every term.
    pub fn new<'p>(
        store: &'s Store,
        predicates: impl IntoIterator<Item = &'p str>,
        languages: Languages,
        allowance: u64,
    ) -> Result<Self, Problem> {
        let dictionary = store.dict();
        let mut held = Vec::new();
        for iri in predicates {
            let found = dictionary
                .locate(Role::Predicate, iri.as_bytes())
                .map_err(|error| unreadable("looking a label predicate up", &error))?;
            if let Some(id) = found
                && !held.iter().any(|(seen, _): &(u64, Rc<str>)| *seen == id.0)
            {
                held.push((id.0, Rc::from(iri)));
            }
        }
        Ok(Self {
            store,
            dictionary,
            predicates: held,
            languages,
            ranks: HashMap::new(),
            allowance,
            remaining: allowance,
            resolved: HashMap::new(),
            scratch: Vec::new(),
        })
    }

    /// Resolve the preferred label of the term with subject id `subject`.
    ///
    /// `Exhausted` only for [`Spend::WithinAllowance`]: a first item the
    /// allowance cannot settle is a refusal instead.
    pub fn resolve(&mut self, subject: u64, spend: Spend) -> Result<Resolution, Problem> {
        if let Some(found) = self.resolved.get(&subject) {
            return Ok(Resolution::Resolved(found.clone()));
        }

        // Read in (predicate, term id) order, which is the cascade's order
        // within one language rank, so a value replaces the one held only by
        // ranking strictly better. A term is settled by a value at the best
        // rank — nothing after it can win — or by reading all it has.
        let mut best: Option<(LanguageRank, usize, u64)> = None;
        'groups: for index in 0..self.predicates.len() {
            if best.is_some_and(|(rank, _, _)| rank == LanguageRank::BEST) {
                break;
            }
            let selection = select(
                self.store,
                IdPattern {
                    subject: Some(subject),
                    predicate: Some(self.predicates[index].0),
                    object: None,
                },
            )?;
            let count = usize::try_from(selection.count().value).unwrap_or(usize::MAX);
            for triple in selection.page(0, count) {
                if self.remaining == 0 {
                    return match spend {
                        Spend::WithinAllowance => Ok(Resolution::Exhausted),
                        Spend::First => Err(self.unsettled()),
                    };
                }
                self.remaining -= 1;
                // A value that is not a literal is not a label candidate.
                let Some(rank) = self.rank_of(triple.object)? else {
                    continue;
                };
                if best.is_none_or(|(held, _, _)| rank < held) {
                    best = Some((rank, index, triple.object));
                }
                if rank == LanguageRank::BEST {
                    break 'groups;
                }
            }
        }

        let label = best
            .map(|(_, index, object)| self.chosen(index, object))
            .transpose()?;
        self.resolved.insert(subject, label.clone());
        Ok(Resolution::Resolved(label))
    }

    /// The refusal for a first item whose label the allowance cannot settle.
    fn unsettled(&self) -> Problem {
        Problem::new(
            ErrorCode::CapExceeded,
            format!(
                "settling the first term's label would read more than this request's \
                 candidate_budget of {} label values; name predicates with fewer values per \
                 term in `labels`, or leave labels out",
                self.allowance
            ),
        )
    }

    /// The language rank of object `id`, or `None` if it is not a literal.
    fn rank_of(&mut self, id: u64) -> Result<Option<LanguageRank>, Problem> {
        self.scratch.clear();
        let bytes = self
            .dictionary
            .extract(Role::Object, TermId(id), &mut self.scratch)
            .map_err(|error| unreadable("reading a label candidate", &error))?;
        let Some(literal) = parse_literal(bytes) else {
            return Ok(None);
        };
        let Some(tag) = literal.language else {
            return Ok(Some(self.languages.rank(None)));
        };
        if let Some(rank) = self.ranks.get(tag) {
            return Ok(Some(*rank));
        }
        let rank = self.languages.rank(Some(tag));
        self.ranks.insert(Box::from(tag), rank);
        Ok(Some(rank))
    }

    /// Materialize the winning value, which the scan established is a literal.
    fn chosen(&mut self, index: usize, object: u64) -> Result<PreferredLabel, Problem> {
        self.scratch.clear();
        let bytes = self
            .dictionary
            .extract(Role::Object, TermId(object), &mut self.scratch)
            .map_err(|error| unreadable("materializing a preferred label", &error))?;
        let text = std::str::from_utf8(bytes)
            .map_err(|error| unreadable("materializing a preferred label", &error))?;
        let Term::Literal(literal) = Term::from_dictionary(text) else {
            return Err(unreadable(
                "materializing a preferred label",
                &format_args!("object term {object} ranked as a literal and is not one"),
            ));
        };
        // Folded here whatever the dictionary stores: `Term::from_dictionary`
        // lowercases a tag, so a source reports one spelling on every bundle.
        let language = match literal.kind() {
            LiteralKind::Language(tag) => Some(Box::from(tag.as_ref())),
            LiteralKind::Plain | LiteralKind::Datatype(_) => None,
        };
        Ok(PreferredLabel {
            value: Box::from(literal.value()),
            predicate: Rc::clone(&self.predicates[index].1),
            language,
            term: Box::from(text),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(text: &str) -> LanguageRange {
        LanguageRange::parse(text).unwrap_or_else(|_| panic!("{text} is a basic language range"))
    }

    fn languages(ranges: &[&str]) -> Languages {
        Languages::new(ranges.iter().map(|text| range(text)))
    }

    #[test]
    fn a_basic_range_is_letters_then_alphanumeric_subtags_or_the_wildcard() {
        for valid in [
            "en",
            "EN",
            "en-GB",
            "zh-Hant-TW",
            "de-1996",
            "x-private",
            "*",
        ] {
            assert!(LanguageRange::parse(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "-",
            "en-",
            "-en",
            "en--gb",
            "e n",
            "1en",
            "toolongtag",
            "en-toolongsub",
            "en_GB",
            "**",
            "en-*",
            "é",
        ] {
            assert_eq!(
                LanguageRange::parse(invalid),
                Err(NotALanguageRange),
                "{invalid}"
            );
        }
        assert_eq!(range("en-GB").as_str(), "en-gb");
    }

    #[test]
    fn basic_filtering_matches_whole_subtags_without_regard_to_case() {
        assert!(range("en").matches(b"en"));
        assert!(range("en").matches(b"en-gb"));
        assert!(range("en").matches(b"EN-GB"));
        assert!(range("en-gb").matches(b"en-gb"));
        assert!(range("en-gb").matches(b"en-gb-oed"));
        assert!(!range("en").matches(b"eng"));
        assert!(!range("en-gb").matches(b"en"));
        assert!(!range("en").matches(b"fr"));
        assert!(range("*").matches(b"fr"));
    }

    #[test]
    fn rank_is_request_order_then_untagged_then_every_other_tag() {
        let preference = languages(&["fr", "en"]);
        assert_eq!(preference.rank(Some(b"fr-ca")), LanguageRank(0));
        assert_eq!(preference.rank(Some(b"en")), LanguageRank(1));
        assert_eq!(preference.rank(None), LanguageRank(2));
        assert_eq!(preference.rank(Some(b"de")), LanguageRank(3));

        // With no preference an untagged literal is the best there is, and
        // every tag ranks below it alike.
        let none = Languages::default();
        assert_eq!(none.rank(None), LanguageRank::BEST);
        assert_eq!(none.rank(Some(b"en")), none.rank(Some(b"de")));
        assert!(none.rank(None) < none.rank(Some(b"en")));

        // The wildcard is a tag preference: it outranks untagged literals but
        // does not match them.
        let wildcard = languages(&["*"]);
        assert_eq!(wildcard.rank(Some(b"de")), LanguageRank(0));
        assert_eq!(wildcard.rank(None), LanguageRank(1));
    }

    #[test]
    fn a_repeated_range_is_kept_once_in_its_first_place() {
        let preference = languages(&["en", "fr", "EN"]);
        assert_eq!(
            preference
                .ranges()
                .iter()
                .map(LanguageRange::as_str)
                .collect::<Vec<_>>(),
            ["en", "fr"]
        );
    }
}
