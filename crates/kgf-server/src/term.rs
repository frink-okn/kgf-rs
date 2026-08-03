//! RDF terms: doc 03 §3.3 syntax in, doc 03 §3.4.1 rows out.
//!
//! This is the boundary doc 20 §20.5 names. A request arrives carrying terms as
//! text; they are resolved to ids once, here; everything below runs over ids;
//! and strings reappear only while a response is serialized. Nothing between
//! those two edges should hold a term.
//!
//! # Three syntaxes, one type
//!
//! [`Term`] is the pivot between them, and they are genuinely different:
//!
//! | syntax | example | who writes it |
//! |---|---|---|
//! | request | `mondo:0005015`, `<http://…>`, `"42"^^xsd:integer` | clients (§3.3) |
//! | dictionary | `http://…/MONDO_0005015`, `"42"^^<http://…#integer>` | hdtc |
//! | response | `{"type": "iri", "value": "http://…"}` | this crate (§3.4.1) |
//!
//! Request syntax abbreviates through the manifest's prefix map, and brackets
//! an IRI so that neither form can be read as the other (§3.3, and see
//! [`Term::parse`]). Dictionary syntax never abbreviates,
//! and brackets a datatype but not a term. Conflating the two is the bug that
//! returns an empty page for data that is present, so the conversions are
//! explicit and the dictionary side is
//! [hdtc's](hdtc::format::encode_literal) rather than ours.
//!
//! # IRIs are not validated, deliberately
//!
//! A bracketed IRI is taken as given: `<relative>`, `<//example.org/a>` and
//! `<http://example.org/a b>` are all accepted, resolved against the dictionary,
//! and answered with whatever is there. This looks like a missing check and is
//! not one.
//!
//! The dictionary is the authority on what terms exist, and it holds all three.
//! hdtc's parsers accept them — verified, not assumed — so a bundle really can
//! contain an IRI that no validator would pass, and every OKN-scale graph
//! contains some. Refusing such a term at the edge would make data that is
//! present unreachable, with no way for a client to ask for it: the same
//! failure that requiring brackets (§3.3) was introduced to remove.
//!
//! The two errors are not symmetric. Rejecting wrongly loses data permanently;
//! accepting wrongly costs a less specific answer — "no rows" instead of "that
//! is not an IRI" — for a request that was going to match nothing anyway. So
//! the only refusals are shapes that cannot denote an IRI the dictionary holds:
//! an unmatched bracket, an empty `<>`, a bracket inside the IRI, and `<_:x>`,
//! which the dictionary can only have written as a blank node.
//!
//! What is worth having instead is a *diagnostic*: a bound position whose term
//! is absent makes the answer provably empty, and saying which position that
//! was is more useful than a syntax error would have been. That belongs to the
//! envelope, unit 12.
//!
//! # Percent-encoding is not handled here
//!
//! Doc 03 §3.3 requires IRIs in GET URLs to be percent-encoded, but decoding
//! belongs to the query-string parser in unit 13: a value that arrives already
//! decoded and is decoded again turns `%2520` into a space, and a term
//! containing a literal `%20` becomes a different term. [`Term::parse`] takes
//! text that has been decoded exactly once.

use hdtc::format::{XSD_STRING, encode_literal, parse_literal};
use kgf_store::dict::Dictionary;
use kgf_store::{Manifest, Role, TermId};
use serde::ser::{Serialize, SerializeMap, Serializer};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;

/// An RDF term.
///
/// Borrowed from whatever syntax it was read out of wherever that is possible;
/// a CURIE expansion and a blank node's `_:` are the cases that must allocate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term<'a> {
    /// An IRI, always in full — abbreviation is a property of request syntax,
    /// not of the term.
    Iri(Cow<'a, str>),
    /// A blank node's label, without the `_:` that introduces it.
    BlankNode(Cow<'a, str>),
    /// A literal.
    Literal(Literal<'a>),
}

/// A literal: a lexical form and at most one of a language tag and a datatype.
///
/// "At most one" is why this is not two `Option` fields. RDF gives a
/// language-tagged literal the datatype `rdf:langString` implicitly, so a term
/// carrying both is not a term with extra information — it is a term that
/// cannot exist, and [`LiteralKind`] is how it stays unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal<'a> {
    value: Cow<'a, str>,
    kind: LiteralKind<'a>,
}

/// What a [`Literal`] carries besides its lexical form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralKind<'a> {
    /// No tag: `xsd:string` by RDF 1.1, which is why it is not spelled out.
    Plain,
    /// A language tag, without its `@`.
    Language(Cow<'a, str>),
    /// A datatype IRI in full, without `^^` or brackets. Never `xsd:string` —
    /// [`Literal::typed`] folds that into [`Plain`](LiteralKind::Plain).
    Datatype(Cow<'a, str>),
}

impl<'a> Literal<'a> {
    /// A literal with no language tag and no explicit datatype.
    pub fn plain(value: impl Into<Cow<'a, str>>) -> Self {
        Self {
            value: value.into(),
            kind: LiteralKind::Plain,
        }
    }

    /// A language-tagged literal.
    ///
    /// The tag is lowercased, for the reason [`Literal::typed`] folds
    /// `xsd:string`: BCP 47 tags are case-insensitive, so `@EN` and `@en` are
    /// one term, and the dictionary holds only the folded form (the RDF parsers
    /// fold on ingest). Without this a client asking for `"Alice"@EN` is told
    /// there are no rows — true of the string it sent, false of the term it
    /// meant, and indistinguishable from an honest empty answer.
    pub fn tagged(value: impl Into<Cow<'a, str>>, language: impl Into<Cow<'a, str>>) -> Self {
        let language = language.into();
        let language = if language.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(language.to_ascii_lowercase())
        } else {
            language
        };
        Self {
            value: value.into(),
            kind: LiteralKind::Language(language),
        }
    }

    /// A typed literal.
    ///
    /// `xsd:string` becomes [`LiteralKind::Plain`], because RDF 1.1 makes `"a"`
    /// and `"a"^^xsd:string` the same term and the dictionary stores only the
    /// short form. Normalizing at construction is what lets `==` mean "the same
    /// RDF term" and keeps a lookup for the long form from missing.
    pub fn typed(value: impl Into<Cow<'a, str>>, datatype: impl Into<Cow<'a, str>>) -> Self {
        let datatype = datatype.into();
        let kind = if datatype == XSD_STRING {
            LiteralKind::Plain
        } else {
            LiteralKind::Datatype(datatype)
        };
        Self {
            value: value.into(),
            kind,
        }
    }

    /// The lexical form, unescaped.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The language tag or datatype, if any.
    pub fn kind(&self) -> &LiteralKind<'a> {
        &self.kind
    }
}

// ---------------------------------------------------------------------------
// Request syntax (doc 03 §3.3)
// ---------------------------------------------------------------------------

/// The manifest's prefix map.
///
/// Requests expand CURIEs through the forward map. HTML result pages use the
/// reverse map to make RDF terms readable; canonical JSON responses still
/// carry IRIs in full (§3.4.1). Both directions are built once per immutable
/// bundle release and shared with every request against it.
#[derive(Debug, Clone, Default)]
pub struct PrefixMap(Arc<Prefixes>);

#[derive(Debug, Default)]
struct Prefixes {
    by_prefix: BTreeMap<String, String>,
    /// `(namespace, prefix)`, longest namespace first and then prefix name.
    ///
    /// That order makes the first match the deterministic display spelling.
    by_namespace: Vec<(String, String)>,
}

impl PrefixMap {
    /// The prefixes a bundle declares.
    ///
    /// **Bundle-scoped, not request-scoped.** This copies the manifest's map,
    /// which is cheap once per open and wasteful once per request; a bundle
    /// declaring the fifty-odd prefixes an OKN graph typically does would
    /// allocate a hundred strings per request to read something that cannot
    /// change while the bundle is mapped. Build it beside the `Store` and share
    /// it.
    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self::from_prefixes(manifest.prefixes.clone())
    }

    /// The namespace `prefix` is declared to stand for.
    pub fn namespace(&self, prefix: &str) -> Option<&str> {
        self.0.by_prefix.get(prefix).map(String::as_str)
    }

    /// The manifest's preferred display spelling of `iri`, if one applies.
    ///
    /// The most specific declaration wins: a namespace covering
    /// `http://example.org/person/` is preferred to one covering
    /// `http://example.org/`. Equal namespace declarations choose the prefix
    /// whose name sorts first, so rendering never depends on insertion order.
    fn compact_iri(&self, iri: &str) -> Option<String> {
        self.0.by_namespace.iter().find_map(|(namespace, prefix)| {
            iri.strip_prefix(namespace)
                .map(|local| format!("{prefix}:{local}"))
        })
    }

    fn from_prefixes(by_prefix: BTreeMap<String, String>) -> Self {
        let mut by_namespace: Vec<_> = by_prefix
            .iter()
            .map(|(prefix, namespace)| (namespace.clone(), prefix.clone()))
            .collect();
        by_namespace.sort_by(
            |(left_namespace, left_prefix), (right_namespace, right_prefix)| {
                right_namespace
                    .len()
                    .cmp(&left_namespace.len())
                    .then_with(|| left_prefix.cmp(right_prefix))
            },
        );
        Self(Arc::new(Prefixes {
            by_prefix,
            by_namespace,
        }))
    }
}

impl FromIterator<(String, String)> for PrefixMap {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self::from_prefixes(iter.into_iter().collect())
    }
}

/// Why a request parameter is not a term.
///
/// One variant per way of being wrong, because §3.6 makes error messages agent
/// UX: an agent that is told which of these happened can fix its request, while
/// one told `bad_term_syntax` can only guess. All of them are that code on the
/// wire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TermSyntaxError {
    /// A parameter was present but empty. An *absent* parameter is a variable
    /// (§3.3) and never reaches here.
    #[error("empty term; omit the parameter entirely to leave the position unbound")]
    Empty,

    /// Not a literal, not bracketed, and with no `:` to make it a CURIE.
    #[error(
        "`{token}` is not a term: write an IRI in angle brackets (`<http://example.org/a>`), \
         a CURIE against a prefix this bundle declares (`ex:a`), or a quoted literal"
    )]
    NotIriOrLiteral {
        /// The offending token.
        token: String,
    },

    /// A CURIE whose prefix this bundle does not declare — which is also what a
    /// bare IRI looks like, so the message names that far more likely mistake
    /// first.
    #[error(
        "`{token}` is a CURIE against the undeclared prefix `{prefix}`; if it is an IRI, \
         bracket it as `<{token}>`, otherwise use a prefix this bundle's manifest declares"
    )]
    UndeclaredPrefix {
        /// The offending token.
        token: String,
        /// The prefix that is not declared.
        prefix: String,
    },

    /// One bracket, or a bracket inside the IRI. The message stays neutral
    /// about which form was intended, because a token can arrive here either
    /// as a half-bracketed IRI (`<http://x/a`) or as a CURIE that picked up a
    /// stray `>` (`ex:a>`), and guessing wrong sends the client the wrong way.
    #[error(
        "`{token}` is not wholly bracketed; an IRI is written `<http://example.org/a>`, \
         and a CURIE has no brackets at all"
    )]
    UnbalancedIri {
        /// The offending token.
        token: String,
    },

    /// `<>`. Turtle would read it as the base IRI; a request has no base.
    #[error("`<>` is empty; a request has no base IRI to resolve it against")]
    EmptyIri,

    /// `<_:x>`: a blank node wearing an IRI's brackets.
    #[error("`{token}` brackets a blank node; write it unbracketed, as `_:{label}`")]
    BracketedBlankNode {
        /// The offending token.
        token: String,
        /// The label inside.
        label: String,
    },

    /// `"a"^^` with nothing after it.
    #[error("`{token}` has an empty datatype after `^^`")]
    EmptyDatatype {
        /// The offending token.
        token: String,
    },

    /// A literal with no closing quote.
    #[error(
        "`{token}` opens a literal that is never closed; literals are `\"value\"`, \
             optionally `\"value\"@lang` or `\"value\"^^datatype`"
    )]
    UnterminatedLiteral {
        /// The offending token.
        token: String,
    },

    /// `"a"@` with nothing after it.
    #[error("`{token}` has an empty language tag after `@`")]
    EmptyLanguageTag {
        /// The offending token.
        token: String,
    },

    /// A blank node with no label.
    #[error("`{token}` has an empty blank node label after `_:`")]
    EmptyBlankNodeLabel {
        /// The offending token.
        token: String,
    },

    /// Something followed the closing quote that is neither `@` nor `^^`.
    #[error("`{token}` has `{suffix}` after its closing quote; expected `@lang` or `^^datatype`")]
    LiteralSuffix {
        /// The offending token.
        token: String,
        /// The unrecognized tail.
        suffix: String,
    },

    /// A term object whose `type` is not one of the three.
    #[error("`{found}` is not a term type; expected `iri`, `literal` or `bnode`")]
    UnknownTermType {
        /// The offending value.
        found: String,
    },

    /// A term object missing a required key, or with the wrong JSON type.
    #[error("term object is malformed: {detail}")]
    MalformedTermObject {
        /// What was wrong.
        detail: String,
    },
}

impl<'a> Term<'a> {
    /// Parse doc 03 §3.3 request syntax.
    ///
    /// `text` must already be percent-decoded — exactly once; see the module
    /// documentation.
    ///
    /// **An IRI is bracketed**: `<http://example.org/a>`, never bare. A bare
    /// token containing `:` is a CURIE and nothing else, and its prefix must be
    /// declared or the term is refused. This is Turtle's and SPARQL's rule, and
    /// it is not optional decoration — a parameter accepting both forms without
    /// a delimiter has to guess, and no guess is right. §3.3 said the opposite
    /// until this unit; `notes/plan.md` decision 10 records why it changed, of
    /// which the sharpest case is that an undeclared prefix would otherwise
    /// become a URI scheme, answering a typo with "no such term".
    ///
    /// A leading `_:` is a blank node, which §3.3 still does not mention — see
    /// `notes/plan.md`, "Questions for `../kgf`".
    pub fn parse(text: &'a str, prefixes: &PrefixMap) -> Result<Self, TermSyntaxError> {
        if text.is_empty() {
            return Err(TermSyntaxError::Empty);
        }
        if text.starts_with('"') {
            return parse_literal_syntax(text, prefixes).map(Term::Literal);
        }
        if let Some(label) = text.strip_prefix("_:") {
            if label.is_empty() {
                return Err(TermSyntaxError::EmptyBlankNodeLabel {
                    token: text.to_owned(),
                });
            }
            return Ok(Term::BlankNode(Cow::Borrowed(label)));
        }
        parse_iri_syntax(text, prefixes).map(Term::Iri)
    }

    /// Read a term out of the bytes the dictionary stores.
    ///
    /// The split is [hdtc's](hdtc::format::parse_literal) — the same code that
    /// decided what to index — rather than a second reading of
    /// `docs/text-index-format.md` §3.1.
    ///
    /// # The result is canonical, not verbatim
    ///
    /// This goes through the same normalizing constructors as parsed input, so
    /// a stored `"x"@EN` reads back as `@en` and a stored
    /// `"a"^^<…XMLSchema#string>` reads back as plain. That is deliberate:
    /// every response carries one spelling of a term, whichever bundle answered.
    /// Doc 05's federated clients compare terms across endpoints, and a bundle
    /// that spelled a language tag differently would look like it held a
    /// different term rather than the same one — the comparison silently fails
    /// instead of erroring.
    ///
    /// It costs an assumption, worth stating because nothing here enforces it:
    /// **a bundle's dictionary is expected to hold canonical terms.** hdtc's
    /// parsers fold on ingest, so bundles built the documented way do. One that
    /// does not — a dictionary literally holding `"x"@EN` — has a term that is
    /// reported as `@en` and cannot then be fetched by that name, because
    /// [`to_dictionary`](Term::to_dictionary) canonicalizes on the way back in
    /// too. Serving it verbatim instead would trade that for incoherent output
    /// across a federation, which is worse and harder to notice. Detecting it
    /// is an offline job for `kgf manifest --check`, which can afford the
    /// `O(dictionary)` scan that `Store::open` cannot (doc 20 §20.6).
    pub fn from_dictionary(term: &'a str) -> Self {
        match parse_literal(term.as_bytes()) {
            Some(literal) => {
                // Slices of a `&str` split at ASCII delimiters: still UTF-8.
                let text = |bytes: &'a [u8]| {
                    std::str::from_utf8(bytes)
                        .expect("a slice of a UTF-8 term at an ASCII boundary")
                };
                let value = text(literal.value);
                Term::Literal(match (literal.language, literal.datatype) {
                    (Some(language), _) => Literal::tagged(value, text(language)),
                    (None, Some(datatype)) => Literal::typed(value, text(datatype)),
                    (None, None) => Literal::plain(value),
                })
            }
            None => match term.strip_prefix("_:") {
                Some(label) => Term::BlankNode(Cow::Borrowed(label)),
                None => Term::Iri(Cow::Borrowed(term)),
            },
        }
    }

    /// Spell this term the way the dictionary does, for [`Dictionary::locate`].
    ///
    /// Borrows for an IRI, which is the overwhelmingly common case on a lookup
    /// path; the other two shapes have to add their punctuation back.
    pub fn to_dictionary(&self) -> Cow<'_, str> {
        match self {
            Term::Iri(iri) => Cow::Borrowed(iri.as_ref()),
            Term::BlankNode(label) => Cow::Owned(format!("_:{label}")),
            Term::Literal(literal) => {
                let (language, datatype) = match &literal.kind {
                    LiteralKind::Plain => (None, None),
                    LiteralKind::Language(language) => (Some(language.as_ref()), None),
                    LiteralKind::Datatype(datatype) => (None, Some(datatype.as_ref())),
                };
                Cow::Owned(encode_literal(&literal.value, language, datatype))
            }
        }
    }

    /// Spell this term the way a request must write it (§3.3), for building a
    /// link back into this API.
    ///
    /// The inverse of [`parse`](Term::parse), and the direction a *page* needs:
    /// an HTML row links each term to the request that asks about it, so the
    /// term has to be written in the syntax that parameter takes.
    ///
    /// Never abbreviated. A CURIE would be shorter, but it would only parse
    /// against the bundle that declared the prefix, and a link that stops
    /// working when copied to another endpoint is worse than a long one. Round
    /// trips through `parse` for every term shape, which is asserted rather
    /// than assumed.
    pub fn to_request(&self) -> String {
        match self {
            Term::Iri(iri) => format!("<{iri}>"),
            Term::BlankNode(label) => format!("_:{label}"),
            Term::Literal(literal) => match &literal.kind {
                LiteralKind::Plain => format!("\"{}\"", literal.value),
                LiteralKind::Language(language) => format!("\"{}\"@{language}", literal.value),
                // Bracketed, because a bare datatype is read as a CURIE.
                LiteralKind::Datatype(datatype) => {
                    format!("\"{}\"^^<{datatype}>", literal.value)
                }
            },
        }
    }

    /// A human-facing spelling for an HTML result cell.
    ///
    /// IRIs covered by this release's manifest are shown as CURIEs. The full
    /// IRI travels beside the label for the page's tooltip, while links keep
    /// using [`to_request`](Self::to_request) and therefore remain independent
    /// of any prefix declaration. A typed literal receives the same treatment
    /// for its datatype IRI; other term shapes are unchanged.
    pub(crate) fn into_display(self, prefixes: &PrefixMap) -> TermDisplay<'a> {
        match self {
            Term::Iri(iri) => {
                let label = prefixes
                    .compact_iri(iri.as_ref())
                    .unwrap_or_else(|| format!("<{iri}>"));
                TermDisplay {
                    label,
                    full_iri: Some(iri),
                }
            }
            Term::BlankNode(label) => TermDisplay {
                label: format!("_:{label}"),
                full_iri: None,
            },
            Term::Literal(Literal { value, kind }) => match kind {
                LiteralKind::Plain => TermDisplay {
                    label: format!("\"{value}\""),
                    full_iri: None,
                },
                LiteralKind::Language(language) => TermDisplay {
                    label: format!("\"{value}\"@{language}"),
                    full_iri: None,
                },
                LiteralKind::Datatype(datatype) => {
                    let datatype_label = prefixes
                        .compact_iri(datatype.as_ref())
                        .unwrap_or_else(|| format!("<{datatype}>"));
                    TermDisplay {
                        label: format!("\"{value}\"^^{datatype_label}"),
                        full_iri: Some(datatype),
                    }
                }
            },
        }
    }

    /// Resolve to an id in `role`'s space, or `None` if the bundle has no such
    /// term in that role.
    ///
    /// The one call that crosses into id space, and the last place a string is
    /// looked at until serialization.
    pub fn locate(
        &self,
        dictionary: &Dictionary<'_>,
        role: Role,
    ) -> kgf_store::Result<Option<TermId>> {
        dictionary.locate(role, self.to_dictionary().as_bytes())
    }
}

/// The visible spelling of one RDF term and the IRI its tooltip reveals.
pub(crate) struct TermDisplay<'a> {
    label: String,
    full_iri: Option<Cow<'a, str>>,
}

impl<'a> TermDisplay<'a> {
    pub(crate) fn into_parts(self) -> (String, Option<Cow<'a, str>>) {
        (self.label, self.full_iri)
    }
}

/// Parse a bracketed IRI or a CURIE, the pair a datatype also takes.
///
/// Brackets are **required** on an IRI, exactly as in Turtle and SPARQL, and
/// exactly because both forms are accepted here: a syntax that admits IRIs and
/// CURIEs without a delimiter has to guess which it was handed, and there is no
/// rule that guesses right. Bracketing is the delimiter, so nothing is
/// inferred — `<http://x/a>` is an IRI, `p:a` is a CURIE, and each fails
/// loudly rather than becoming the other.
fn parse_iri_syntax<'a>(
    text: &'a str,
    prefixes: &PrefixMap,
) -> Result<Cow<'a, str>, TermSyntaxError> {
    if text.starts_with('<') || text.ends_with('>') {
        let unbalanced = || TermSyntaxError::UnbalancedIri {
            token: text.to_owned(),
        };
        let iri = text
            .strip_prefix('<')
            .ok_or_else(unbalanced)?
            .strip_suffix('>')
            .ok_or_else(unbalanced)?;
        if iri.is_empty() {
            return Err(TermSyntaxError::EmptyIri);
        }
        // `<` and `>` are not IRI characters (RFC 3987 §2.2), so either one
        // inside means the brackets do not delimit what the client thought.
        if iri.contains(['<', '>']) {
            return Err(unbalanced());
        }
        // `_:x` is how the dictionary spells a blank node, and it cannot also
        // spell an IRI — the format has one encoding for both, so accepting
        // `<_:x>` would answer an IRI request with a blank node. No IRI is lost
        // by refusing: `_` cannot begin a scheme (RFC 3986 §3.1), and the term
        // is reachable under the spelling that does denote it.
        if let Some(label) = iri.strip_prefix("_:") {
            return Err(TermSyntaxError::BracketedBlankNode {
                token: text.to_owned(),
                label: label.to_owned(),
            });
        }
        return Ok(Cow::Borrowed(iri));
    }
    let Some((prefix, local)) = text.split_once(':') else {
        return Err(TermSyntaxError::NotIriOrLiteral {
            token: text.to_owned(),
        });
    };
    match prefixes.namespace(prefix) {
        Some(namespace) => Ok(Cow::Owned(format!("{namespace}{local}"))),
        None => Err(TermSyntaxError::UndeclaredPrefix {
            token: text.to_owned(),
            prefix: prefix.to_owned(),
        }),
    }
}

/// Parse `"value"`, `"value"@lang` or `"value"^^datatype`.
///
/// The closing quote is the *last* one, matching how the dictionary is read
/// (`docs/text-index-format.md` §3.1): values are not escaped anywhere in KGF,
/// so `"a "b" c"` is one literal whose value contains quotes rather than a
/// syntax error. Escaping the value instead would mean a term that round-trips
/// through a response and back into a request comes out different.
fn parse_literal_syntax<'a>(
    text: &'a str,
    prefixes: &PrefixMap,
) -> Result<Literal<'a>, TermSyntaxError> {
    let Some(close) = text.rfind('"').filter(|close| *close > 0) else {
        return Err(TermSyntaxError::UnterminatedLiteral {
            token: text.to_owned(),
        });
    };
    let value = &text[1..close];
    let suffix = &text[close + 1..];

    if suffix.is_empty() {
        return Ok(Literal::plain(value));
    }
    if let Some(language) = suffix.strip_prefix('@') {
        if language.is_empty() {
            return Err(TermSyntaxError::EmptyLanguageTag {
                token: text.to_owned(),
            });
        }
        return Ok(Literal::tagged(value, language));
    }
    if let Some(datatype) = suffix.strip_prefix("^^") {
        if datatype.is_empty() {
            return Err(TermSyntaxError::EmptyDatatype {
                token: text.to_owned(),
            });
        }
        return Ok(Literal::typed(value, parse_iri_syntax(datatype, prefixes)?));
    }
    Err(TermSyntaxError::LiteralSuffix {
        token: text.to_owned(),
        suffix: suffix.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Response syntax (doc 03 §3.4.1)
// ---------------------------------------------------------------------------

impl Serialize for Term<'_> {
    /// Doc 03 §3.4.1's term object, written straight into the serializer.
    ///
    /// No intermediate `Value`: a page is `limit` rows of up to three terms, and
    /// a map allocated per term is a map allocated per term.
    ///
    /// The keys are §3.4.1's — `iri` and `lang`. SPARQL Results JSON spells
    /// those `uri` and `xml:lang`, so this is *not* SRJ despite the resemblance;
    /// `format=srj` is a separate serialization and belongs to unit 14. See
    /// `notes/plan.md`, "Questions for `../kgf`".
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        match self {
            Term::Iri(iri) => {
                map.serialize_entry("type", "iri")?;
                map.serialize_entry("value", iri.as_ref())?;
            }
            Term::BlankNode(label) => {
                map.serialize_entry("type", "bnode")?;
                map.serialize_entry("value", label.as_ref())?;
            }
            Term::Literal(literal) => {
                map.serialize_entry("type", "literal")?;
                map.serialize_entry("value", literal.value.as_ref())?;
                match &literal.kind {
                    LiteralKind::Plain => {}
                    LiteralKind::Language(language) => {
                        map.serialize_entry("lang", language.as_ref())?;
                    }
                    LiteralKind::Datatype(datatype) => {
                        map.serialize_entry("datatype", datatype.as_ref())?;
                    }
                }
            }
        }
        map.end()
    }
}

impl<'a> Term<'a> {
    /// Read the term-object form doc 03 §3.3 accepts in request bodies.
    ///
    /// No prefix map: the term object is the form responses use, where IRIs are
    /// always full, and §3.3 offers it as the way out of escaping and ambiguity.
    /// Re-admitting CURIEs here would put the ambiguity back.
    ///
    /// Every key must be one this understands. Ignoring the rest is what turns
    /// a SPARQL Results JSON object — `xml:lang` rather than `lang` — into a
    /// *different term* that resolves and answers, rather than into an error;
    /// §3.4.1's claim of SRJ compatibility guarantees clients will send them.
    pub fn from_json(value: &'a serde_json::Value) -> Result<Self, TermSyntaxError> {
        let malformed = |detail: &str| TermSyntaxError::MalformedTermObject {
            detail: detail.to_owned(),
        };
        let object = value
            .as_object()
            .ok_or_else(|| malformed("not an object"))?;
        let string = |key: &str| -> Result<Option<&'a str>, TermSyntaxError> {
            match object.get(key) {
                None => Ok(None),
                Some(serde_json::Value::String(text)) => Ok(Some(text.as_str())),
                Some(_) => Err(malformed(&format!("`{key}` is not a string"))),
            }
        };

        let kind = string("type")?.ok_or_else(|| malformed("no `type`"))?;
        let value = string("value")?.ok_or_else(|| malformed("no `value`"))?;

        // A term object is closed. The two SRJ spellings get their own remedy,
        // since a client sending them is not confused, just reading the other
        // spec (see `notes/plan.md`, question 9).
        let permitted: &[&str] = match kind {
            "literal" => &["type", "value", "lang", "datatype"],
            _ => &["type", "value"],
        };
        if let Some(unknown) = object.keys().find(|key| !permitted.contains(&key.as_str())) {
            return Err(match unknown.as_str() {
                "xml:lang" => {
                    malformed("`xml:lang` is SPARQL Results JSON; this form spells it `lang`")
                }
                "uri" => malformed("`uri` is SPARQL Results JSON; this form spells the type `iri`"),
                other if kind == "literal" => {
                    malformed(&format!("`{other}` is not a key of a literal term object"))
                }
                other => malformed(&format!(
                    "`{other}` is not a key of an `{kind}` term object"
                )),
            });
        }

        match kind {
            // The label is bare, as this crate and SRJ both write it. `_:b1`
            // would encode to `_:_:b1`, and cannot simply be stripped: a
            // dictionary may hold a blank node whose label really is `_:b1`.
            "bnode" if value.starts_with("_:") => {
                Err(malformed("a `bnode` value is the bare label, without `_:`"))
            }
            "iri" => Ok(Term::Iri(Cow::Borrowed(value))),
            "bnode" => Ok(Term::BlankNode(Cow::Borrowed(value))),
            "literal" => match (string("lang")?, string("datatype")?) {
                (Some(_), Some(_)) => Err(malformed("both `lang` and `datatype`")),
                (Some(language), None) => Ok(Term::Literal(Literal::tagged(value, language))),
                (None, Some(datatype)) => Ok(Term::Literal(Literal::typed(value, datatype))),
                (None, None) => Ok(Term::Literal(Literal::plain(value))),
            },
            other => Err(TermSyntaxError::UnknownTermType {
                found: other.to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The per-request cache
// ---------------------------------------------------------------------------

/// A term that is in the dictionary but is not a term.
#[derive(Debug, thiserror::Error)]
pub enum DictionaryTermError {
    /// The dictionary could not be read.
    #[error(transparent)]
    Read(#[from] kgf_store::Error),

    /// A stored term is not UTF-8, so the bundle is not serving RDF.
    #[error(
        "{} term {id} is not valid UTF-8; the bundle's dictionary is corrupt",
        role_name(*role)
    )]
    NotUtf8 {
        /// Which id space.
        role: Role,
        /// The offending id.
        id: u64,
    },
}

/// A role as it should read in a message.
///
/// Not `{role:?}`: that would make operator-visible text a function of the
/// variant names in another crate, so renaming one silently rewords this.
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Subject => "subject",
        Role::Predicate => "predicate",
        Role::Object => "object",
    }
}

/// Terms already materialized while answering one request.
///
/// A page repeats terms heavily — `s ? ?` has one subject for every row, and a
/// predicate is shared by most of them — so the same PFC block is otherwise
/// decoded once per occurrence rather than once per term.
///
/// This lives here and not in [`kgf_store`] on purpose (doc 20 §20.5, and the
/// crate's rule 4): a cache inside `Store` would be shared across threads and
/// would need a lock on the read path. One per request needs neither, because
/// it is owned by the request.
///
/// UTF-8 is validated once per distinct term as it enters, which is what lets
/// [`Term::from_dictionary`] be infallible.
///
/// # Not `Send`
///
/// The `Rc` makes the whole cache thread-local, so a cache must be created and
/// dropped **inside** the blocking closure that does the request's store work,
/// never built outside and moved in — `spawn_blocking` requires `Send`. That is
/// the intended shape anyway (a request's terms belong to that request), and
/// the alternative costs an atomic increment per row on the hot path to buy a
/// sharing nobody wants. Recorded because otherwise it is discovered as a
/// compile error in unit 13 and "fixed" by reaching for `Arc`.
#[derive(Debug, Default)]
pub struct TermCache {
    entries: HashMap<(Role, u64), Entry>,
    scratch: Vec<u8>,
}

/// A materialized term, and how much room it takes in a response.
#[derive(Debug, Clone)]
struct Entry {
    text: Rc<str>,
    serialized: u64,
}

impl TermCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The term for `id`, extracting it if this is the first time it is asked
    /// for.
    ///
    /// The `Rc` is what makes a hit free: the caller holds a term without
    /// borrowing the cache, so the three terms of a triple can be alive at once
    /// while the cache is still available for the next row.
    pub fn resolve(
        &mut self,
        dictionary: &Dictionary<'_>,
        role: Role,
        id: TermId,
    ) -> Result<Rc<str>, DictionaryTermError> {
        Ok(self.measured(dictionary, role, id)?.0)
    }

    /// The same term, with the bytes its term object occupies (§3.4.1).
    ///
    /// Memoized with the term rather than computed per use, which is the whole
    /// point of measuring here: doc 03 §3.5's `max_response_bytes` has to be
    /// weighed once per *row*, and a page repeats terms heavily — `s ? ?` has
    /// one subject for every row and a predicate shared by most of them. A page
    /// of 10 000 rows over 500 distinct terms serializes 500 term objects to
    /// size itself instead of 30 000.
    ///
    /// The number is what `serde_json` writes for [`Term`]'s own `Serialize`,
    /// taken through a counting sink so nothing is allocated to weigh it — so
    /// it cannot drift from the encoding, because it *is* the encoding.
    pub fn measured(
        &mut self,
        dictionary: &Dictionary<'_>,
        role: Role,
        id: TermId,
    ) -> Result<(Rc<str>, u64), DictionaryTermError> {
        if let Some(entry) = self.entries.get(&(role, id.0)) {
            return Ok((Rc::clone(&entry.text), entry.serialized));
        }
        self.scratch.clear();
        let bytes = dictionary.extract(role, id, &mut self.scratch)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| DictionaryTermError::NotUtf8 { role, id: id.0 })?;
        let serialized = serialized_bytes(&Term::from_dictionary(text));
        let text: Rc<str> = Rc::from(text);
        self.entries.insert(
            (role, id.0),
            Entry {
                text: Rc::clone(&text),
                serialized,
            },
        );
        Ok((text, serialized))
    }

    /// How many distinct terms have been materialized.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been materialized yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The bytes a term object occupies, without producing them.
fn serialized_bytes(term: &Term<'_>) -> u64 {
    struct Counter(u64);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    // A term object is strings and ASCII punctuation, so the only way this
    // fails is a `Serialize` impl that errors — and a term's is this module's.
    serde_json::to_writer(&mut counter, term).expect("a term serializes");
    counter.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgf_store::hdt::HdtLayout;
    use kgf_store::map::Mapping;
    use kgf_store::testing::{Fixture, TINY_NT};

    /// A golden bundle's dictionary, kept alive with the mapping it views.
    struct Golden {
        // Held, not used: dropping the fixture removes the temporary directory,
        // and an mmap outliving its unlinked file is a POSIX detail rather than
        // something a test should lean on.
        _fixture: Fixture,
        hdt: Mapping,
        layout: HdtLayout,
    }

    impl Golden {
        fn build(source: &str) -> Self {
            let fixture = Fixture::build(source);
            let hdt = fixture.map_hdt();
            let layout = HdtLayout::parse(&hdt).expect("parse HDT");
            Self {
                _fixture: fixture,
                hdt,
                layout,
            }
        }

        fn dictionary(&self) -> Dictionary<'_> {
            self.layout.dictionary().view(&self.hdt)
        }
    }

    fn prefixes(pairs: &[(&str, &str)]) -> PrefixMap {
        pairs
            .iter()
            .map(|(prefix, namespace)| ((*prefix).to_owned(), (*namespace).to_owned()))
            .collect()
    }

    #[test]
    fn display_compaction_prefers_the_longest_namespace_then_prefix_name() {
        let prefixes = prefixes(&[
            ("ex", "http://example.org/"),
            ("person", "http://example.org/person/"),
            ("human", "http://example.org/person/"),
        ]);

        let (label, full_iri) = Term::from_dictionary("http://example.org/person/alice")
            .into_display(&prefixes)
            .into_parts();
        assert_eq!(label, "human:alice");
        assert_eq!(full_iri.as_deref(), Some("http://example.org/person/alice"));

        let (label, full_iri) = Term::from_dictionary("https://elsewhere.example/alice")
            .into_display(&prefixes)
            .into_parts();
        assert_eq!(label, "<https://elsewhere.example/alice>");
        assert_eq!(full_iri.as_deref(), Some("https://elsewhere.example/alice"));
    }

    #[test]
    fn display_compaction_applies_to_a_literal_datatype() {
        let prefixes = prefixes(&[("xsd", "http://www.w3.org/2001/XMLSchema#")]);
        let (label, full_iri) =
            Term::from_dictionary("\"31\"^^<http://www.w3.org/2001/XMLSchema#integer>")
                .into_display(&prefixes)
                .into_parts();

        assert_eq!(label, "\"31\"^^xsd:integer");
        assert_eq!(
            full_iri.as_deref(),
            Some("http://www.w3.org/2001/XMLSchema#integer")
        );
    }

    #[test]
    fn a_prefix_map_clone_shares_its_precomputed_indexes() {
        let prefixes = prefixes(&[("ex", "http://example.org/")]);
        let cloned = prefixes.clone();
        assert!(Arc::ptr_eq(&prefixes.0, &cloned.0));
    }

    /// Write a term in doc 03 §3.3 request syntax.
    ///
    /// The test oracle, so deliberately not [`Term::to_dictionary`] nor anything
    /// else the module under test uses: this is a second reading of §3.3, and a
    /// round trip is only evidence if the two directions were written
    /// independently.
    fn as_request_syntax(term: &Term<'_>) -> String {
        match term {
            Term::Iri(iri) => format!("<{iri}>"),
            Term::BlankNode(label) => format!("_:{label}"),
            Term::Literal(literal) => match literal.kind() {
                LiteralKind::Plain => format!("\"{}\"", literal.value()),
                LiteralKind::Language(language) => {
                    format!("\"{}\"@{}", literal.value(), language)
                }
                LiteralKind::Datatype(datatype) => {
                    format!("\"{}\"^^<{}>", literal.value(), datatype)
                }
            },
        }
    }

    /// The same term written with a CURIE wherever `namespace` covers it.
    ///
    /// The other spelling of the same thing, so that the round trip proves both
    /// forms name one term rather than only that brackets survive.
    fn as_curie_syntax(term: &Term<'_>, prefix: &str, namespace: &str) -> Option<String> {
        match term {
            Term::Iri(iri) => iri
                .strip_prefix(namespace)
                .map(|local| format!("{prefix}:{local}")),
            Term::Literal(literal) => match literal.kind() {
                LiteralKind::Datatype(datatype) => datatype
                    .strip_prefix(namespace)
                    .map(|local| format!("\"{}\"^^{prefix}:{local}", literal.value())),
                _ => None,
            },
            Term::BlankNode(_) => None,
        }
    }

    /// Every term in the golden bundle, in every role it occupies.
    fn every_term(dictionary: &Dictionary<'_>) -> Vec<(Role, TermId, String)> {
        let counts = dictionary.counts();
        let mut cache = TermCache::new();
        let mut terms = Vec::new();
        for role in [Role::Subject, Role::Predicate, Role::Object] {
            for id in 1..=counts.len(role) {
                let text = cache
                    .resolve(dictionary, role, TermId(id))
                    .expect("extract term");
                terms.push((role, TermId(id), text.to_string()));
            }
        }
        assert!(terms.len() >= 12, "the fixture should have more terms");
        terms
    }

    #[test]
    fn a_dictionary_term_survives_the_trip_out_to_a_client_and_back() {
        let golden = Golden::build(TINY_NT);
        let dictionary = golden.dictionary();

        let (prefix, namespace) = ("ex", "http://example.org/");
        let prefixes = prefixes(&[(prefix, namespace)]);

        let (mut iris, mut bnodes, mut plain, mut tagged, mut typed) = (0, 0, 0, 0, 0);
        let mut curies = 0;
        for (role, id, stored) in every_term(&dictionary) {
            let term = Term::from_dictionary(&stored);
            match &term {
                Term::Iri(_) => iris += 1,
                Term::BlankNode(_) => bnodes += 1,
                Term::Literal(literal) => match literal.kind() {
                    LiteralKind::Plain => plain += 1,
                    LiteralKind::Language(_) => tagged += 1,
                    LiteralKind::Datatype(_) => typed += 1,
                },
            }

            // Out to the client and back through request syntax.
            let request = as_request_syntax(&term);
            // And the crate's own writer agrees with that independent reading
            // of §3.3, which is what makes a link on a page a valid request.
            assert_eq!(term.to_request(), request, "to_request for {stored}");
            let reparsed = Term::parse(&request, &prefixes)
                .unwrap_or_else(|error| panic!("{request} does not parse back: {error}"));
            assert_eq!(reparsed, term, "request syntax round trip for {stored}");

            // The CURIE spelling of the same term must name the same term, in
            // the datatype position as well as the term position.
            if let Some(curie) = as_curie_syntax(&term, prefix, namespace) {
                curies += 1;
                let from_curie = Term::parse(&curie, &prefixes)
                    .unwrap_or_else(|error| panic!("{curie} does not parse: {error}"));
                assert_eq!(from_curie, term, "CURIE spelling of {stored}");
            }

            // Out to the client and back through the JSON term object.
            let json = serde_json::to_value(&term).expect("serialize term");
            let from_json = Term::from_json(&json)
                .unwrap_or_else(|error| panic!("{json} does not parse back: {error}"));
            assert_eq!(from_json, term, "term object round trip for {stored}");

            // And the spelling the store sees is byte-for-byte what it holds,
            // which is the property that decides whether a lookup finds a term
            // that is present.
            assert_eq!(reparsed.to_dictionary(), stored, "dictionary spelling");
            assert_eq!(
                reparsed.locate(&dictionary, role).expect("locate"),
                Some(id),
                "{stored} must resolve to the id it was extracted from"
            );
        }

        assert!(
            iris > 0 && bnodes > 0 && plain > 0 && tagged > 0 && typed > 0 && curies > 0,
            "the fixture must cover every term shape, saw \
             {iris} IRIs, {bnodes} blank nodes, {plain}/{tagged}/{typed} literals, \
             {curies} CURIE spellings"
        );
    }

    #[test]
    fn a_curie_expands_only_against_a_declared_prefix() {
        let prefixes = prefixes(&[
            ("ex", "http://example.org/"),
            ("mondo", "http://purl.obolibrary.org/obo/MONDO_"),
        ]);

        assert_eq!(
            Term::parse("ex:alice", &prefixes).unwrap(),
            Term::Iri(Cow::Borrowed("http://example.org/alice"))
        );
        assert_eq!(
            Term::parse("mondo:0005015", &prefixes).unwrap(),
            Term::Iri(Cow::Borrowed(
                "http://purl.obolibrary.org/obo/MONDO_0005015"
            ))
        );
        // An undeclared prefix is refused. It must not become an IRI whose
        // scheme is `wat`, which is the reading that makes an unresolvable
        // CURIE quietly denote something — a term the bundle does not contain
        // and the client did not ask for.
        assert_eq!(
            Term::parse("wat:0005015", &prefixes),
            Err(TermSyntaxError::UndeclaredPrefix {
                token: "wat:0005015".to_owned(),
                prefix: "wat".to_owned(),
            })
        );
        // Which applies to a bare IRI too: it is a CURIE against `http`.
        assert!(matches!(
            Term::parse("http://example.org/alice", &prefixes),
            Err(TermSyntaxError::UndeclaredPrefix { .. })
        ));
        // Bracketed, it is an IRI, and no expansion applies.
        assert_eq!(
            Term::parse("<http://example.org/alice>", &prefixes).unwrap(),
            Term::Iri(Cow::Borrowed("http://example.org/alice"))
        );
    }

    #[test]
    fn a_prefix_named_for_a_scheme_is_no_longer_a_hazard() {
        // §3.3 advises datasets not to declare a prefix that collides with a
        // URI scheme, because under its rule the declaration would capture
        // every IRI using that scheme. Requiring brackets removes the hazard
        // rather than warning about it: the two forms cannot be confused, so
        // both spellings stay available and mean different things.
        let prefixes = prefixes(&[("http", "http://example.org/broken#")]);

        assert_eq!(
            Term::parse("http:alice", &prefixes).unwrap(),
            Term::Iri(Cow::Owned("http://example.org/broken#alice".to_owned())),
            "the CURIE expands, as declared"
        );
        assert_eq!(
            Term::parse("<http://example.org/alice>", &prefixes).unwrap(),
            Term::Iri(Cow::Borrowed("http://example.org/alice")),
            "and the IRI is untouched by the collision"
        );
    }

    #[test]
    fn literal_shapes_parse() {
        let prefixes = prefixes(&[("xsd", "http://www.w3.org/2001/XMLSchema#")]);

        assert_eq!(
            Term::parse("\"Diabetes mellitus\"@en", &prefixes).unwrap(),
            Term::Literal(Literal::tagged("Diabetes mellitus", "en"))
        );
        // A datatype takes both forms, on the same terms as any other IRI slot.
        for spelling in [
            "\"42\"^^xsd:integer",
            "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>",
        ] {
            assert_eq!(
                Term::parse(spelling, &prefixes).unwrap(),
                Term::Literal(Literal::typed(
                    "42",
                    "http://www.w3.org/2001/XMLSchema#integer"
                )),
                "parsing {spelling}"
            );
        }
        assert!(matches!(
            Term::parse("\"42\"^^nope:integer", &prefixes),
            Err(TermSyntaxError::UndeclaredPrefix { .. })
        ));
        assert_eq!(
            Term::parse("\"plain\"", &prefixes).unwrap(),
            Term::Literal(Literal::plain("plain"))
        );
        // Unescaped: the closing quote is the last one, so inner quotes are data.
        assert_eq!(
            Term::parse("\"a \"b\" c\"", &prefixes).unwrap(),
            Term::Literal(Literal::plain("a \"b\" c"))
        );
        assert_eq!(
            Term::parse("\"\"", &prefixes).unwrap(),
            Term::Literal(Literal::plain(""))
        );
    }

    #[test]
    fn a_language_tag_finds_its_term_whatever_case_it_arrives_in() {
        // The fixture holds `"Alice"@en`. BCP 47 is case-insensitive and the
        // builders fold on ingest, so every spelling below is the same term and
        // must reach the same id. Getting this wrong returns zero rows for data
        // that is present, which is why it is checked against a real dictionary
        // rather than against `to_dictionary` alone.
        let golden = Golden::build(TINY_NT);
        let dictionary = golden.dictionary();
        let prefixes = PrefixMap::default();

        let canonical = Term::parse("\"Alice\"@en", &prefixes).unwrap();
        let expected = canonical
            .locate(&dictionary, Role::Object)
            .expect("locate")
            .expect("the fixture holds \"Alice\"@en");

        for spelling in ["\"Alice\"@EN", "\"Alice\"@En", "\"Alice\"@eN"] {
            let term = Term::parse(spelling, &prefixes).unwrap();
            assert_eq!(term, canonical, "{spelling} is the same term as @en");
            assert_eq!(
                term.locate(&dictionary, Role::Object).expect("locate"),
                Some(expected),
                "{spelling} must reach the same id"
            );
        }

        // Subtags fold too, and a tag that is already lowercase is untouched.
        assert_eq!(
            Term::parse("\"x\"@en-GB", &prefixes)
                .unwrap()
                .to_dictionary(),
            "\"x\"@en-gb"
        );
    }

    #[test]
    fn a_stored_term_is_reported_in_canonical_form() {
        // Reading the dictionary canonicalizes rather than echoing bytes, so
        // one term has one spelling in a response whichever bundle answered —
        // doc 05's clients compare terms across endpoints. hdtc-built bundles
        // are already canonical, so this is checked directly rather than
        // through a fixture, which cannot produce the non-canonical input.
        let folded = Term::from_dictionary("\"x\"@EN");
        assert_eq!(folded, Term::Literal(Literal::tagged("x", "en")));
        assert_eq!(
            serde_json::to_string(&folded).unwrap(),
            r#"{"type":"literal","value":"x","lang":"en"}"#
        );

        let implicit = Term::from_dictionary("\"a\"^^<http://www.w3.org/2001/XMLSchema#string>");
        assert_eq!(implicit, Term::Literal(Literal::plain("a")));
        assert_eq!(
            serde_json::to_string(&implicit).unwrap(),
            r#"{"type":"literal","value":"a"}"#
        );

        // The cost, stated so it is a decision and not a surprise: a bundle
        // holding non-canonical terms has terms it cannot be asked for.
        assert_eq!(folded.to_dictionary(), "\"x\"@en");
        assert_eq!(implicit.to_dictionary(), "\"a\"");
    }

    #[test]
    fn the_two_spellings_of_a_plain_literal_are_one_term() {
        let prefixes = prefixes(&[("xsd", "http://www.w3.org/2001/XMLSchema#")]);
        let implicit = Term::parse("\"a\"", &prefixes).unwrap();
        let explicit = Term::parse("\"a\"^^xsd:string", &prefixes).unwrap();
        assert_eq!(implicit, explicit);
        assert_eq!(explicit.to_dictionary(), "\"a\"");
    }

    #[test]
    fn malformed_input_is_an_error_rather_than_a_plausible_term() {
        let prefixes = prefixes(&[("ex", "http://example.org/")]);
        let cases: [(&str, TermSyntaxError); 11] = [
            ("", TermSyntaxError::Empty),
            (
                "alice",
                TermSyntaxError::NotIriOrLiteral {
                    token: "alice".to_owned(),
                },
            ),
            ("<>", TermSyntaxError::EmptyIri),
            (
                "<http://example.org/alice",
                TermSyntaxError::UnbalancedIri {
                    token: "<http://example.org/alice".to_owned(),
                },
            ),
            (
                "http://example.org/alice>",
                TermSyntaxError::UnbalancedIri {
                    token: "http://example.org/alice>".to_owned(),
                },
            ),
            (
                "<http://example.org/a<b>",
                TermSyntaxError::UnbalancedIri {
                    token: "<http://example.org/a<b>".to_owned(),
                },
            ),
            (
                "\"unterminated",
                TermSyntaxError::UnterminatedLiteral {
                    token: "\"unterminated".to_owned(),
                },
            ),
            (
                "\"a\"@",
                TermSyntaxError::EmptyLanguageTag {
                    token: "\"a\"@".to_owned(),
                },
            ),
            (
                "_:",
                TermSyntaxError::EmptyBlankNodeLabel {
                    token: "_:".to_owned(),
                },
            ),
            (
                "\"a\"!en",
                TermSyntaxError::LiteralSuffix {
                    token: "\"a\"!en".to_owned(),
                    suffix: "!en".to_owned(),
                },
            ),
            // Names the datatype rather than reporting an empty token, which
            // told an agent nothing about which part of the term was missing.
            (
                "\"42\"^^",
                TermSyntaxError::EmptyDatatype {
                    token: "\"42\"^^".to_owned(),
                },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                Term::parse(input, &prefixes),
                Err(expected),
                "parsing {input:?}"
            );
        }

        // The bracket rules reach the datatype position too, which is the other
        // place an IRI or a CURIE may appear.
        assert!(matches!(
            Term::parse(
                "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer",
                &prefixes
            ),
            Err(TermSyntaxError::UnbalancedIri { .. })
        ));
    }

    #[test]
    fn a_term_object_is_written_the_way_doc_03_writes_one() {
        let json = |term: &Term<'_>| serde_json::to_string(term).unwrap();

        assert_eq!(
            json(&Term::Iri(Cow::Borrowed("http://example.org/a"))),
            r#"{"type":"iri","value":"http://example.org/a"}"#
        );
        assert_eq!(
            json(&Term::BlankNode(Cow::Borrowed("b1"))),
            r#"{"type":"bnode","value":"b1"}"#
        );
        assert_eq!(
            json(&Term::Literal(Literal::plain("Alice"))),
            r#"{"type":"literal","value":"Alice"}"#
        );
        assert_eq!(
            json(&Term::Literal(Literal::tagged("atrazine", "en"))),
            r#"{"type":"literal","value":"atrazine","lang":"en"}"#
        );
        assert_eq!(
            json(&Term::Literal(Literal::typed("30", "http://x/int"))),
            r#"{"type":"literal","value":"30","datatype":"http://x/int"}"#
        );
    }

    #[test]
    fn a_malformed_term_object_is_refused() {
        let cases = [
            serde_json::json!("http://example.org/a"),
            serde_json::json!({"value": "http://example.org/a"}),
            serde_json::json!({"type": "iri"}),
            serde_json::json!({"type": "uri", "value": "http://example.org/a"}),
            serde_json::json!({"type": "iri", "value": 42}),
            serde_json::json!({"type": "literal", "value": "a", "lang": "en", "datatype": "http://x"}),
        ];
        for case in &cases {
            assert!(Term::from_json(case).is_err(), "{case} should be refused");
        }
    }

    #[test]
    fn a_term_object_key_that_is_not_understood_is_refused() {
        // The dangerous case is not a typo, it is a client reading SPARQL
        // Results JSON: dropping `xml:lang` silently yields a *plain* literal,
        // which resolves and answers with rows for a different term.
        let srj = serde_json::json!({"type": "literal", "value": "Alice", "xml:lang": "en"});
        let error = Term::from_json(&srj).expect_err("SRJ spelling must not be read as plain");
        assert!(
            error.to_string().contains("`lang`"),
            "the message must name the spelling to use, got: {error}"
        );

        for case in [
            serde_json::json!({"type": "literal", "value": "a", "langauge": "en"}),
            serde_json::json!({"type": "iri", "value": "http://x/a", "lang": "en"}),
            serde_json::json!({"type": "bnode", "value": "b1", "datatype": "http://x"}),
        ] {
            assert!(
                Term::from_json(&case).is_err(),
                "{case} carries a key it should not"
            );
        }

        // A label already wearing its prefix is refused rather than stripped:
        // a dictionary may hold a blank node whose label really is `_:b1`.
        assert!(Term::from_json(&serde_json::json!({"type": "bnode", "value": "_:b1"})).is_err());
        assert_eq!(
            Term::from_json(&serde_json::json!({"type": "bnode", "value": "b1"})).unwrap(),
            Term::BlankNode(Cow::Borrowed("b1"))
        );
    }

    #[test]
    fn a_bracketed_blank_node_is_refused_rather_than_resolved() {
        // `<_:b1>` would encode to `_:b1` and locate the blank node, answering
        // an IRI request with a term of another kind. The dictionary has one
        // encoding for both, so the only fix is to refuse the spelling that
        // cannot mean what it says.
        let error = Term::parse("<_:b1>", &PrefixMap::default())
            .expect_err("a bracketed blank node must not resolve as an IRI");
        assert!(
            error.to_string().contains("_:b1"),
            "the message must name the unbracketed form, got: {error}"
        );
        assert_eq!(
            Term::parse("_:b1", &PrefixMap::default()).unwrap(),
            Term::BlankNode(Cow::Borrowed("b1"))
        );
    }

    #[test]
    fn the_cache_materializes_each_term_once() {
        let golden = Golden::build(TINY_NT);
        let dictionary = golden.dictionary();

        let mut cache = TermCache::new();
        assert!(cache.is_empty());

        let first = cache
            .resolve(&dictionary, Role::Predicate, TermId(1))
            .unwrap();
        let again = cache
            .resolve(&dictionary, Role::Predicate, TermId(1))
            .unwrap();
        assert_eq!(first, again);
        assert!(Rc::ptr_eq(&first, &again), "a hit must not re-materialize");
        assert_eq!(cache.len(), 1);

        // The role is part of the key: the shared section gives subject and
        // object id 1 the same string, but they are different entries and a
        // cache keyed on the id alone would answer the wrong one for a bundle
        // where they differ.
        cache
            .resolve(&dictionary, Role::Subject, TermId(1))
            .unwrap();
        cache.resolve(&dictionary, Role::Object, TermId(1)).unwrap();
        assert_eq!(cache.len(), 3);

        // Three terms of a row can be held at once, which is the reason for the
        // `Rc` rather than a borrow of the cache.
        let subject = cache
            .resolve(&dictionary, Role::Subject, TermId(1))
            .unwrap();
        let predicate = cache
            .resolve(&dictionary, Role::Predicate, TermId(1))
            .unwrap();
        let object = cache.resolve(&dictionary, Role::Object, TermId(1)).unwrap();
        assert!(!subject.is_empty() && !predicate.is_empty() && !object.is_empty());
    }

    #[test]
    fn an_id_outside_the_dictionary_is_an_error_not_a_term() {
        let golden = Golden::build(TINY_NT);
        let dictionary = golden.dictionary();

        let mut cache = TermCache::new();
        let beyond = dictionary.counts().len(Role::Predicate) + 1;
        assert!(matches!(
            cache.resolve(&dictionary, Role::Predicate, TermId(beyond)),
            Err(DictionaryTermError::Read(_))
        ));
        assert!(cache.is_empty(), "a failed lookup must not be cached");
    }

    #[test]
    fn an_absent_term_resolves_to_no_id_rather_than_failing() {
        let golden = Golden::build(TINY_NT);
        let dictionary = golden.dictionary();

        let term = Term::parse("<http://example.org/nobody>", &PrefixMap::default()).unwrap();
        assert_eq!(term.locate(&dictionary, Role::Subject).unwrap(), None);
    }
}
