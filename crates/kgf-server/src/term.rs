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
//! an IRI so that neither form can be read as the other — see [`Term::parse`],
//! which deviates from §3.3 as written. Dictionary syntax never abbreviates,
//! and brackets a datatype but not a term. Conflating the two is the bug that
//! returns an empty page for data that is present, so the conversions are
//! explicit and the dictionary side is
//! [hdtc's](hdtc::format::encode_literal) rather than ours.
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
    pub fn tagged(value: impl Into<Cow<'a, str>>, language: impl Into<Cow<'a, str>>) -> Self {
        Self {
            value: value.into(),
            kind: LiteralKind::Language(language.into()),
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

/// The manifest's prefix map, used to expand CURIEs in request parameters.
///
/// Expansion is one-way. Responses carry IRIs in full (§3.4.1), so there is no
/// abbreviating direction to keep consistent with this one.
#[derive(Debug, Clone, Default)]
pub struct PrefixMap(BTreeMap<String, String>);

impl PrefixMap {
    /// The prefixes a bundle declares.
    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self(manifest.prefixes.clone())
    }

    /// The namespace `prefix` is declared to stand for.
    pub fn namespace(&self, prefix: &str) -> Option<&str> {
        self.0.get(prefix).map(String::as_str)
    }

    /// Whether any prefix is declared.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, String)> for PrefixMap {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
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

    /// One bracket, or a bracket inside the IRI.
    #[error(
        "`{token}` has an unmatched `<` or `>`; write an IRI either bare \
         (`http://example.org/a`) or wholly bracketed (`<http://example.org/a>`)"
    )]
    UnbalancedIri {
        /// The offending token.
        token: String,
    },

    /// `<>`. Turtle would read it as the base IRI; a request has no base.
    #[error("`<>` is empty; a request has no base IRI to resolve it against")]
    EmptyIri,

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
    /// declared. This is Turtle's and SPARQL's rule and it is not optional
    /// decoration — a parameter that accepts both forms without a delimiter has
    /// to guess, and every guessing rule is wrong for some dataset. Doc 03
    /// §3.3's own rule ("a token parses as a CURIE only when its prefix is
    /// declared; otherwise as an IRI") makes a term's meaning depend on the
    /// manifest of the bundle it is sent to, so the same string denotes
    /// different things at different endpoints, and a bundle declaring `http:`
    /// has IRIs that no request can name at all.
    ///
    /// This deviates from §3.3 as written; see `notes/plan.md`, "Questions for
    /// `../kgf`", which records it as a decision for the doc rather than an
    /// implementation liberty.
    ///
    /// A leading `_:` is a blank node, also unmentioned by §3.3.
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

    /// Give up the borrow, so the term can outlive the text it was read from.
    pub fn into_owned(self) -> Term<'static> {
        match self {
            Term::Iri(iri) => Term::Iri(Cow::Owned(iri.into_owned())),
            Term::BlankNode(label) => Term::BlankNode(Cow::Owned(label.into_owned())),
            Term::Literal(literal) => Term::Literal(Literal {
                value: Cow::Owned(literal.value.into_owned()),
                kind: match literal.kind {
                    LiteralKind::Plain => LiteralKind::Plain,
                    LiteralKind::Language(language) => {
                        LiteralKind::Language(Cow::Owned(language.into_owned()))
                    }
                    LiteralKind::Datatype(datatype) => {
                        LiteralKind::Datatype(Cow::Owned(datatype.into_owned()))
                    }
                },
            }),
        }
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
        match kind {
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
    #[error("{role:?} term {id} is not valid UTF-8; the bundle's dictionary is corrupt")]
    NotUtf8 {
        /// Which id space.
        role: Role,
        /// The offending id.
        id: u64,
    },
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
#[derive(Debug, Default)]
pub struct TermCache {
    entries: HashMap<(Role, u64), Rc<str>>,
    scratch: Vec<u8>,
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
        if let Some(term) = self.entries.get(&(role, id.0)) {
            return Ok(Rc::clone(term));
        }
        self.scratch.clear();
        let bytes = dictionary.extract(role, id, &mut self.scratch)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| DictionaryTermError::NotUtf8 { role, id: id.0 })?;
        let term: Rc<str> = Rc::from(text);
        self.entries.insert((role, id.0), Rc::clone(&term));
        Ok(term)
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
        let cases: [(&str, TermSyntaxError); 10] = [
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
