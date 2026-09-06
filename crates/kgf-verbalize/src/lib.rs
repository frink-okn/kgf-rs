//! Verbalization: a graph node becomes the text it is embedded from.
//!
//! An embedding model needs a string. This module makes one per root node:
//! a `label:` line, then one `predicate: value` line per selected outgoing
//! edge, values named by their labels rather than expanded. It is the read
//! half of the embedding pipeline; the model half stays outside this
//! workspace and consumes the records this module writes.
//!
//! # It runs in id space
//!
//! A config names classes and predicates as IRIs. [`Bound::bind`] resolves
//! every one of them against a bundle's dictionary once; after that the walk,
//! the filters, the per-predicate limit, and label resolution all operate on
//! ids over the mapped permutations, and a string is materialized only when it
//! is about to be written into the text. Predicate ids are assigned in
//! dictionary order, so "predicates in IRI order" costs nothing: it is the
//! order the star already arrives in.
//!
//! # What a root's text is
//!
//! 1. Enumerate roots: the subjects of `? rdf:type <class>`, blank nodes
//!    skipped.
//! 2. Read the root's star `s ? ?`, group by predicate, drop predicates on the
//!    ignore list or off a declared allow list.
//! 3. Per predicate, keep the `predicate_limit` values with the smallest
//!    [`text::stable_score`] — a uniform sample keyed on the strings, so an
//!    unchanged node samples the same values in every build — and emit them
//!    in score order.
//! 4. Name each value: a literal's lexical form; an IRI's label through the
//!    profile for its class, then the label cascade, then its humanized
//!    fragment. Blank-node values are skipped.
//! 5. Name the root the same way, except that its target's `label_template`
//!    is tried after profiles and before the cascade.
//!
//! The cascade is the target's own `label_predicates`, in the order written,
//! followed by the bundle's `label` role — the predicates `/labels` answers
//! with — as the fallback. The config goes first because its author has seen
//! the graph: a generic label predicate can carry boilerplate ("Journal
//! article about data") beside a domain predicate that carries the real name,
//! and only the config can say which to prefer. The config may also name
//! description or identifier predicates a label endpoint should never return
//! but a text is glad of. The bundle's role is what a config that says nothing
//! gets.
//!
//! # Where this crate sits
//!
//! Between the store and the server. It has two callers that are peers — the
//! build stage writing records, and the `/verbalize` route rendering a few
//! roots on request — and neither owns it. It knows nothing of HTTP, so a
//! build never links the server to run it, and it is testable headless
//! against fixture bundles.
//!
//! # Caches, and why they are here
//!
//! A predicate's display name and a mentioned node's label are recomputed for
//! every root that touches them, and a class node like `Treatment Provider` is
//! touched by every root of that class. Both are memoized per target for the
//! life of a [`Verbalizer`], as is every materialized term. Caches live here
//! and not in the store for the reason every other cache does: the store's
//! read path holds no lock, and a verbalizer is one thread's work over one
//! bundle.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod text;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hdtc::format::parse_literal;
use kgf_store::dict::Dictionary;
use kgf_store::pattern::{IdPattern, Selection};
use kgf_store::{Role, Store, TermId};
use serde::Serialize;

pub use config::{Config, ConfigError, Resolved};

use self::config::{Template, TemplatePart};
use self::text::{RDF_TYPE, fallback_label, humanize, normalize_label, stable_score, text_digest};

/// Why verbalization stopped.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bundle could not be read.
    #[error("reading the bundle: {0}")]
    Store(#[from] kgf_store::Error),
    /// A dictionary term was not UTF-8, which a conforming bundle never
    /// produces.
    #[error("term {id} in the {role:?} space is not UTF-8")]
    NotUtf8 {
        /// Which id space.
        role: Role,
        /// The id.
        id: u64,
    },
}

/// Materialized terms, memoized for the life of a run.
///
/// The `Rc` is what makes a hit free: a caller holds a term without borrowing
/// the cache, so several terms of one triple are alive at once while the
/// cache serves the next.
#[derive(Default)]
struct Terms {
    entries: HashMap<(Role, u64), Rc<str>>,
    scratch: Vec<u8>,
}

impl Terms {
    fn resolve(
        &mut self,
        dictionary: &Dictionary<'_>,
        role: Role,
        id: u64,
    ) -> Result<Rc<str>, Error> {
        if let Some(text) = self.entries.get(&(role, id)) {
            return Ok(Rc::clone(text));
        }
        self.scratch.clear();
        let bytes = dictionary.extract(role, TermId(id), &mut self.scratch)?;
        let text: Rc<str> = std::str::from_utf8(bytes)
            .map_err(|_| Error::NotUtf8 { role, id })?
            .into();
        self.entries.insert((role, id), Rc::clone(&text));
        Ok(text)
    }
}

/// What kind of term a dictionary string spells.
///
/// The dictionary writes IRIs as their bytes, blank nodes as `_:label`, and
/// literals quoted; a literal's lexical form is what a text wants, and the
/// language tag or datatype is not.
enum TermKind<'a> {
    Iri(&'a str),
    BlankNode,
    Literal(&'a str),
}

fn classify(text: &str) -> TermKind<'_> {
    match parse_literal(text.as_bytes()) {
        // A slice of a `&str` at an ASCII delimiter is still UTF-8.
        Some(literal) => TermKind::Literal(
            std::str::from_utf8(literal.value)
                .expect("a slice of a UTF-8 term at an ASCII boundary"),
        ),
        None if text.starts_with("_:") => TermKind::BlankNode,
        None => TermKind::Iri(text),
    }
}

/// A config's IRI absent from the bundle it was bound to.
///
/// Not an error: a class with no members or a predicate the graph never uses
/// verbalizes to nothing, correctly. But it is the most common config mistake
/// — a typo, a `http` for an `https` — so it is reported rather than silent.
/// Reported once per IRI and place: an IRI written under `defaults` reaches
/// every target, and is named where it was written rather than once per
/// target it reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unknown {
    /// Where in the config, as a dotted key path.
    pub at: String,
    /// The IRI the bundle does not have.
    pub iri: String,
}

/// A resolved config bound to one bundle: every IRI it names, as that bundle's
/// ids. Built once per bundle; a [`Verbalizer`] borrows it.
#[derive(Debug)]
pub struct Bound {
    /// `rdf:type`'s predicate id, or `None` in a graph with no types at all.
    rdf_type: Option<u64>,
    targets: Vec<BoundTarget>,
    /// Profiles by the object id of the class they apply to.
    profiles: HashMap<u64, BoundTemplate>,
    unknown: Vec<Unknown>,
}

#[derive(Debug)]
struct BoundTarget {
    name: String,
    /// The class as an object id, or `None` if the bundle lacks the IRI.
    class: Option<u64>,
    /// Label predicates: the target's own first, then the bundle's role.
    cascade: Vec<u64>,
    ignore: HashSet<u64>,
    /// `Some` when an allow list was declared, even if every IRI in it was
    /// unknown: a declared allow list that matches nothing walks nothing.
    include: Option<HashSet<u64>>,
    predicate_limit: usize,
    template: Option<BoundTemplate>,
}

#[derive(Debug)]
struct BoundTemplate {
    parts: Vec<BoundPart>,
}

#[derive(Debug)]
enum BoundPart {
    Text(String),
    /// A field's predicate id; `None` when the bundle lacks the predicate, in
    /// which case the placeholder renders empty.
    Field(Option<u64>),
}

impl Bound {
    /// Resolve a config against a bundle.
    ///
    /// `label_role` is the bundle's declared label cascade, strongest first.
    /// IRIs the dictionary does not hold are recorded in
    /// [`unknown`](Self::unknown) and bind to nothing.
    pub fn bind(store: &Store, resolved: &Resolved, label_role: &[String]) -> Result<Self, Error> {
        let dictionary = store.dict();
        let mut unknown = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut locate = |role: Role, at: String, iri: &str| -> Result<Option<u64>, Error> {
            let id = dictionary.locate(role, iri.as_bytes())?.map(|id| id.0);
            if id.is_none() && seen.insert((at.clone(), iri.to_owned())) {
                unknown.push(Unknown {
                    at,
                    iri: iri.to_owned(),
                });
            }
            Ok(id)
        };
        // Where a list entry was written: under `defaults` if it is there,
        // else under the target.
        let list_at = |target: &str, list: &str, written_in_defaults: bool| {
            if written_in_defaults {
                format!("defaults.{list}")
            } else {
                format!("targets.{target}.{list}")
            }
        };

        let rdf_type = dictionary
            .locate(Role::Predicate, RDF_TYPE.as_bytes())?
            .map(|id| id.0);

        let mut profiles = HashMap::with_capacity(resolved.profiles.len());
        for profile in &resolved.profiles {
            let at = format!("profiles.{}", profile.name);
            let Some(class) = locate(Role::Object, format!("{at}.type"), &profile.class)? else {
                continue;
            };
            let template = bind_template(&profile.template, &profile.fields, &at, &mut locate)?;
            profiles.insert(class, template);
        }

        let mut targets = Vec::with_capacity(resolved.targets.len());
        for target in &resolved.targets {
            let at = format!("targets.{}", target.name);
            let class = locate(Role::Object, format!("{at}.type"), &target.class)?;

            let mut cascade = Vec::new();
            for iri in &target.label_predicates {
                let at = list_at(
                    &target.name,
                    "label_predicates",
                    resolved.defaults.label_predicates.contains(iri),
                );
                if let Some(id) = locate(Role::Predicate, at, iri)? {
                    push_unique(&mut cascade, id);
                }
            }
            for iri in label_role {
                // The bundle's own role: absence is the manifest's problem, not
                // this config's, so it is not reported here.
                if let Some(id) = dictionary.locate(Role::Predicate, iri.as_bytes())? {
                    push_unique(&mut cascade, id.0);
                }
            }

            let mut ignore = HashSet::new();
            for iri in &target.ignore_predicates {
                let at = list_at(
                    &target.name,
                    "ignore_predicates",
                    resolved.defaults.ignore_predicates.contains(iri),
                );
                if let Some(id) = locate(Role::Predicate, at, iri)? {
                    ignore.insert(id);
                }
            }
            let include = if target.include_predicates.is_empty() {
                None
            } else {
                let mut include = HashSet::new();
                for iri in &target.include_predicates {
                    let at = list_at(
                        &target.name,
                        "include_predicates",
                        resolved.defaults.include_predicates.contains(iri),
                    );
                    if let Some(id) = locate(Role::Predicate, at, iri)? {
                        include.insert(id);
                    }
                }
                Some(include)
            };

            let template = match &target.label_template {
                Some((template, fields)) => {
                    Some(bind_template(template, fields, &at, &mut locate)?)
                }
                None => None,
            };

            targets.push(BoundTarget {
                name: target.name.clone(),
                class,
                cascade,
                ignore,
                include,
                predicate_limit: target.predicate_limit as usize,
                template,
            });
        }

        Ok(Self {
            rdf_type,
            targets,
            profiles,
            unknown,
        })
    }

    /// The config's IRIs this bundle does not hold.
    pub fn unknown(&self) -> &[Unknown] {
        &self.unknown
    }

    /// Target names, in the order [`Verbalizer`] indexes them.
    pub fn targets(&self) -> impl ExactSizeIterator<Item = &str> {
        self.targets.iter().map(|target| target.name.as_str())
    }
}

fn bind_template(
    template: &Template,
    fields: &std::collections::BTreeMap<String, String>,
    at: &str,
    locate: &mut impl FnMut(Role, String, &str) -> Result<Option<u64>, Error>,
) -> Result<BoundTemplate, Error> {
    let mut parts = Vec::with_capacity(template.parts().len());
    for part in template.parts() {
        parts.push(match part {
            TemplatePart::Text(text) => BoundPart::Text(text.clone()),
            TemplatePart::Field(field) => {
                let iri = &fields[field];
                BoundPart::Field(locate(
                    Role::Predicate,
                    format!("{at}.fields.{field}"),
                    iri,
                )?)
            }
        });
    }
    Ok(BoundTemplate { parts })
}

fn push_unique(ids: &mut Vec<u64>, id: u64) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// One root, verbalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The root's IRI.
    pub iri: String,
    /// Its display label, which is also the text's first line.
    pub label: String,
    /// The text to embed.
    pub text: String,
    /// Whether the root's star was cut at a [`star budget`](Verbalizer::with_star_budget)
    /// before every edge was seen, so the text is a bounded approximation of
    /// what an unbudgeted run would write.
    pub truncated: bool,
    /// How many of the root's walked predicates had more values than
    /// `predicate_limit`, so that a sample of them stands in for the rest.
    /// The tuning signal for the limit: a predicate sampled down on every
    /// root is one to raise the limit for, exclude, or allow-list around.
    pub limited: u32,
}

/// A root's text and what the walk left out of it.
struct BuiltText {
    text: String,
    truncated: bool,
    limited: u32,
}

/// A node as the store knows it: which role's id space it lives in.
#[derive(Debug, Clone, Copy)]
enum Node {
    Subject(u64),
    Object(u64),
    Predicate(u64),
}

/// Verbalizes roots of one bundle under one bound config.
///
/// Holds the per-run caches; make one per bundle and reuse it across every
/// root. Not `Sync`: the caches are single-threaded by design.
pub struct Verbalizer<'a> {
    store: &'a Store,
    dictionary: Dictionary<'a>,
    bound: &'a Bound,
    /// The most edges of one root's star that are read; `None` reads them all.
    star_budget: Option<usize>,
    terms: Terms,
    /// A predicate's display name, per target (the cascade differs).
    predicate_names: HashMap<(u64, usize), Rc<str>>,
    /// A mentioned node's label, by object id, per target.
    object_labels: HashMap<(u64, usize), Rc<str>>,
    /// A predicate IRI's subject id, when it has one, for labelling the
    /// predicate itself.
    predicate_subjects: HashMap<u64, Option<u64>>,
}

impl<'a> Verbalizer<'a> {
    /// A verbalizer over `store` under `bound`, with empty caches.
    pub fn new(store: &'a Store, bound: &'a Bound) -> Self {
        Self {
            store,
            dictionary: store.dict(),
            bound,
            star_budget: None,
            terms: Terms::default(),
            predicate_names: HashMap::new(),
            object_labels: HashMap::new(),
            predicate_subjects: HashMap::new(),
        }
    }

    /// Read at most `edges` of any one root's star.
    ///
    /// A build reads every edge, because `predicate_limit` selects from all
    /// of a predicate's values. A bounded-cost server cannot: one root with a
    /// million edges would be a million-row read behind a request that asked
    /// for one text. Past the budget the star is cut and the text is marked
    /// [`truncated`](Rendered::truncated).
    pub fn with_star_budget(mut self, edges: usize) -> Self {
        self.star_budget = Some(edges);
        self
    }

    /// The subject ids of every member of `target`'s class, ascending.
    ///
    /// Blank-node members are included; [`verbalize`](Self::verbalize) skips
    /// them. An empty result for a class the bundle lacks is the correct
    /// answer, and [`Bound::unknown`] is where that is reported. This
    /// enumerates the whole class and is for a build; a server draws
    /// positions through [`root_count`](Self::root_count) and
    /// [`root_at`](Self::root_at) instead.
    pub fn roots(&self, target: usize) -> Result<Vec<u64>, Error> {
        Ok(match self.members(target)? {
            Some(selection) => selection
                .page(0, usize::MAX)
                .map(|triple| triple.subject)
                .collect(),
            None => Vec::new(),
        })
    }

    /// How many members `target`'s class has, blank nodes included. A range
    /// width, not an enumeration.
    pub fn root_count(&self, target: usize) -> Result<u64, Error> {
        Ok(self
            .members(target)?
            .map_or(0, |selection| selection.count().value))
    }

    /// The member of `target`'s class at `position` in ascending subject
    /// order. One rank descent; `position` must be below
    /// [`root_count`](Self::root_count).
    pub fn root_at(&self, target: usize, position: u64) -> Result<u64, Error> {
        let selection = self
            .members(target)?
            .expect("a position below the count means the class is bound");
        Ok(selection.at(position).subject)
    }

    /// `? rdf:type <class>` for the target, or `None` when the bundle lacks
    /// the predicate or the class.
    fn members(&self, target: usize) -> Result<Option<Selection<'a>>, Error> {
        let (Some(rdf_type), Some(class)) = (self.bound.rdf_type, self.bound.targets[target].class)
        else {
            return Ok(None);
        };
        Ok(Some(self.store.resolve(IdPattern {
            subject: None,
            predicate: Some(rdf_type),
            object: Some(class),
        })?))
    }

    /// Verbalize one root of `target`, or `None` if it is a blank node.
    pub fn verbalize(&mut self, target: usize, subject: u64) -> Result<Option<Rendered>, Error> {
        let iri = self.resolve(Role::Subject, subject)?;
        if iri.starts_with("_:") {
            return Ok(None);
        }
        let label = self.display_label(Node::Subject(subject), target, true)?;
        let built = self.build_text(subject, &iri, target, &label)?;
        Ok(Some(Rendered {
            iri: iri.to_string(),
            label,
            text: built.text,
            truncated: built.truncated,
            limited: built.limited,
        }))
    }

    /// Verbalize a root named by IRI, or `None` if the bundle has no such
    /// subject.
    pub fn verbalize_iri(&mut self, target: usize, iri: &str) -> Result<Option<Rendered>, Error> {
        match self.dictionary.locate(Role::Subject, iri.as_bytes())? {
            Some(subject) => self.verbalize(target, subject.0),
            None => Ok(None),
        }
    }

    fn resolve(&mut self, role: Role, id: u64) -> Result<Rc<str>, Error> {
        self.terms.resolve(&self.dictionary, role, id)
    }

    /// The text: the label line, then one line per selected edge, with what
    /// the walk had to leave out.
    fn build_text(
        &mut self,
        root: u64,
        root_iri: &str,
        target: usize,
        label: &str,
    ) -> Result<BuiltText, Error> {
        let mut lines = vec![format!("label: {label}")];
        let (star, truncated) = self.star(root)?;
        let limit = self.bound.targets[target].predicate_limit;
        let mut limited = 0u32;

        let mut i = 0;
        while i < star.len() {
            let predicate = star[i].0;
            let mut j = i;
            while j < star.len() && star[j].0 == predicate {
                j += 1;
            }
            let objects = &star[i..j];
            i = j;

            if !self.walks(target, predicate) {
                continue;
            }

            // Every group is sorted by score, not only the ones over the limit:
            // the order values appear in is part of the text.
            let predicate_iri = self.resolve(Role::Predicate, predicate)?;
            let mut scored = Vec::with_capacity(objects.len());
            for &(_, object) in objects {
                let key = self.score_key(object)?;
                scored.push((stable_score(root_iri, &predicate_iri, &key), object));
            }
            scored.sort_unstable();
            if scored.len() > limit {
                limited += 1;
                scored.truncate(limit);
            }

            let predicate_name = self.predicate_name(predicate, target)?;
            for (_, object) in scored {
                let Some(value) = self.object_text(object, target)? else {
                    continue;
                };
                lines.push(format!("{predicate_name}: {value}"));
            }
        }
        Ok(BuiltText {
            text: lines.join("\n"),
            truncated,
            limited,
        })
    }

    fn walks(&self, target: usize, predicate: u64) -> bool {
        let target = &self.bound.targets[target];
        if let Some(include) = &target.include
            && !include.contains(&predicate)
        {
            return false;
        }
        !target.ignore.contains(&predicate)
    }

    /// The object's string as [`text::stable_score`] keys it: a literal's
    /// lexical form, an IRI, or a blank node with its `_:`.
    fn score_key(&mut self, object: u64) -> Result<String, Error> {
        let text = self.resolve(Role::Object, object)?;
        Ok(match classify(&text) {
            TermKind::Literal(value) => value.to_owned(),
            TermKind::Iri(_) | TermKind::BlankNode => text.to_string(),
        })
    }

    /// What a value contributes to a line, or `None` when it contributes no
    /// line: a blank node, or a label that came out empty.
    fn object_text(&mut self, object: u64, target: usize) -> Result<Option<String>, Error> {
        let text = self.resolve(Role::Object, object)?;
        if text.starts_with("_:") {
            return Ok(None);
        }
        let label = self.display_label(Node::Object(object), target, false)?;
        Ok((!label.is_empty()).then_some(label))
    }

    /// `s ? ?` as `(predicate, object)` pairs, grouped by predicate in id
    /// order, and whether the star budget cut it short.
    fn star(&self, subject: u64) -> Result<(Vec<(u64, u64)>, bool), Error> {
        let selection = self.store.resolve(IdPattern {
            subject: Some(subject),
            predicate: None,
            object: None,
        })?;
        let budget = self.star_budget.unwrap_or(usize::MAX);
        let mut star: Vec<(u64, u64)> = selection
            .page(0, budget.saturating_add(1))
            .map(|triple| (triple.predicate, triple.object))
            .collect();
        let truncated = star.len() > budget;
        star.truncate(budget);
        Ok((star, truncated))
    }

    /// The objects of `subject predicate ?`.
    fn objects(&self, subject: u64, predicate: u64) -> Result<Vec<u64>, Error> {
        let selection = self.store.resolve(IdPattern {
            subject: Some(subject),
            predicate: Some(predicate),
            object: None,
        })?;
        Ok(selection
            .page(0, usize::MAX)
            .map(|triple| triple.object)
            .collect())
    }

    /// The subject id a node has when it appears as a subject, if it does.
    fn subject_of(&mut self, node: Node) -> Result<Option<u64>, Error> {
        Ok(match node {
            Node::Subject(id) => Some(id),
            Node::Object(id) => {
                let counts = self.dictionary.counts();
                let section = counts.section_id(Role::Object, TermId(id))?;
                counts.role_id(Role::Subject, section).map(|id| id.0)
            }
            Node::Predicate(id) => {
                if let Some(found) = self.predicate_subjects.get(&id) {
                    return Ok(*found);
                }
                let iri = self.resolve(Role::Predicate, id)?;
                let found = self
                    .dictionary
                    .locate(Role::Subject, iri.as_bytes())?
                    .map(|id| id.0);
                self.predicate_subjects.insert(id, found);
                found
            }
        })
    }

    /// The node's name for a text: literal value; else the first profile,
    /// template, or cascade entry that yields one; else the IRI's fragment.
    ///
    /// `as_root` admits the target's `label_template`, which names a node as
    /// itself rather than as something mentioned.
    fn display_label(&mut self, node: Node, target: usize, as_root: bool) -> Result<String, Error> {
        if let Node::Object(id) = node
            && let Some(label) = self.object_labels.get(&(id, target))
        {
            return Ok(label.to_string());
        }
        let label = self.compute_display_label(node, target, as_root)?;
        if let Node::Object(id) = node {
            self.object_labels
                .insert((id, target), Rc::from(label.as_str()));
        }
        Ok(label)
    }

    fn compute_display_label(
        &mut self,
        node: Node,
        target: usize,
        as_root: bool,
    ) -> Result<String, Error> {
        let (role, id) = match node {
            Node::Subject(id) => (Role::Subject, id),
            Node::Object(id) => (Role::Object, id),
            Node::Predicate(id) => (Role::Predicate, id),
        };
        let text = self.resolve(role, id)?;
        let iri = match classify(&text) {
            TermKind::Literal(value) => return Ok(normalize_label(value)),
            TermKind::BlankNode => return Ok(text.to_string()),
            TermKind::Iri(iri) => iri.to_owned(),
        };

        if let Some(subject) = self.subject_of(node)? {
            if !self.bound.profiles.is_empty()
                && let Some(rdf_type) = self.bound.rdf_type
            {
                for class in self.objects(subject, rdf_type)? {
                    let Some(template) = self.bound.profiles.get(&class) else {
                        continue;
                    };
                    if let Some(label) = self.render(template, subject, target)? {
                        return Ok(label);
                    }
                }
            }
            if as_root
                && let Some(template) = &self.bound.targets[target].template
                && let Some(label) = self.render(template, subject, target)?
            {
                return Ok(label);
            }
            if let Some(label) = self.first_literal(subject, target)? {
                return Ok(normalize_label(&label));
            }
        }
        Ok(fallback_label(&iri))
    }

    /// A template filled from `subject`'s direct values, or `None` when it
    /// renders to nothing.
    fn render(
        &mut self,
        template: &BoundTemplate,
        subject: u64,
        target: usize,
    ) -> Result<Option<String>, Error> {
        let mut out = String::new();
        for part in &template.parts {
            match part {
                BoundPart::Text(text) => out.push_str(text),
                BoundPart::Field(None) => {}
                BoundPart::Field(Some(predicate)) => {
                    if let Some(value) = self.first_direct_value(subject, *predicate, target)? {
                        out.push_str(&value);
                    }
                }
            }
        }
        let label = normalize_label(&out);
        Ok((!label.is_empty()).then_some(label))
    }

    /// The lexicographically smallest label among `subject predicate ?`'s
    /// non-blank values.
    fn first_direct_value(
        &mut self,
        subject: u64,
        predicate: u64,
        target: usize,
    ) -> Result<Option<String>, Error> {
        let mut best: Option<String> = None;
        for object in self.objects(subject, predicate)? {
            let Some(label) = self.object_text(object, target)? else {
                continue;
            };
            if best.as_ref().is_none_or(|best| label < *best) {
                best = Some(label);
            }
        }
        Ok(best)
    }

    /// The first literal value along the target's cascade.
    fn first_literal(&mut self, subject: u64, target: usize) -> Result<Option<Rc<str>>, Error> {
        for i in 0..self.bound.targets[target].cascade.len() {
            let predicate = self.bound.targets[target].cascade[i];
            for object in self.objects(subject, predicate)? {
                let text = self.resolve(Role::Object, object)?;
                if let TermKind::Literal(value) = classify(&text) {
                    return Ok(Some(Rc::from(value)));
                }
            }
        }
        Ok(None)
    }

    /// How a predicate reads at the head of a line: its label along the
    /// cascade, else its fragment, humanized and lowercased.
    fn predicate_name(&mut self, predicate: u64, target: usize) -> Result<Rc<str>, Error> {
        if let Some(name) = self.predicate_names.get(&(predicate, target)) {
            return Ok(Rc::clone(name));
        }
        let iri = self.resolve(Role::Predicate, predicate)?;
        let label = match self.subject_of(Node::Predicate(predicate))? {
            Some(subject) => self.first_literal(subject, target)?,
            None => None,
        };
        let name: Rc<str> = match label {
            Some(label) => humanize(&label).to_lowercase().into(),
            None => fallback_label(&iri).to_lowercase().into(),
        };
        self.predicate_names
            .insert((predicate, target), Rc::clone(&name));
        Ok(name)
    }
}

/// One line of records output: a text and the roots that produced it.
///
/// The field order is part of the contract: the embedding stage that consumes
/// these lines is a separate program, and a record must read the same from
/// either side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    /// Up to `max_iris` of the roots that produced this text, ascending.
    pub iris: Vec<String>,
    /// The first root's label.
    pub label: String,
    /// The text.
    pub embedding_text: String,
    /// How many roots produced this text, counting past the `iris` cap.
    pub iri_count: u64,
}

/// Groups verbalized roots by identical text into [`Record`]s.
#[derive(Debug)]
pub struct Grouper {
    by_digest: HashMap<[u8; 32], Record>,
    max_iris: usize,
}

impl Grouper {
    /// A grouper keeping at most `max_iris` IRIs per record.
    pub fn new(max_iris: usize) -> Self {
        Self {
            by_digest: HashMap::new(),
            max_iris: max_iris.max(1),
        }
    }

    /// Add one root. A record keeps its `max_iris` smallest IRIs whatever
    /// order roots arrive in, so the output does not depend on it.
    pub fn add(&mut self, rendered: Rendered) {
        let digest = text_digest(&rendered.text);
        let Some(record) = self.by_digest.get_mut(&digest) else {
            self.by_digest.insert(
                digest,
                Record {
                    iris: vec![rendered.iri],
                    label: rendered.label,
                    embedding_text: rendered.text,
                    iri_count: 1,
                },
            );
            return;
        };
        if record.iris.contains(&rendered.iri) {
            return;
        }
        record.iri_count += 1;
        if record.iris.len() < self.max_iris {
            record.iris.push(rendered.iri);
            record.iris.sort_unstable();
        } else if let Some(last) = record.iris.last_mut()
            && rendered.iri < *last
        {
            *last = rendered.iri;
            record.iris.sort_unstable();
        }
    }

    /// How many distinct texts so far.
    pub fn len(&self) -> usize {
        self.by_digest.len()
    }

    /// Whether nothing has been added.
    pub fn is_empty(&self) -> bool {
        self.by_digest.is_empty()
    }

    /// The records, ordered by text and then by IRIs.
    pub fn finish(self) -> Vec<Record> {
        let mut records: Vec<Record> = self.by_digest.into_values().collect();
        records.sort_unstable_by(|a, b| {
            a.embedding_text
                .cmp(&b.embedding_text)
                .then_with(|| a.iris.cmp(&b.iris))
        });
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(iri: &str, text: &str) -> Rendered {
        Rendered {
            iri: iri.to_owned(),
            label: "l".to_owned(),
            text: text.to_owned(),
            truncated: false,
            limited: 0,
        }
    }

    #[test]
    fn identical_text_groups_and_keeps_the_smallest_iris() {
        let mut grouper = Grouper::new(2);
        grouper.add(rendered("http://x/c", "same"));
        grouper.add(rendered("http://x/a", "same"));
        grouper.add(rendered("http://x/b", "same"));
        grouper.add(rendered("http://x/a", "same"));
        grouper.add(rendered("http://x/z", "other"));
        let records = grouper.finish();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].embedding_text, "other");
        assert_eq!(records[1].iris, ["http://x/a", "http://x/b"]);
        assert_eq!(records[1].iri_count, 3);
    }

    #[test]
    fn records_serialize_with_a_fixed_field_order() {
        let mut grouper = Grouper::new(10);
        grouper.add(rendered("http://x/a", "label: A"));
        let json = serde_json::to_string(&grouper.finish()[0]).unwrap();
        assert_eq!(
            json,
            r#"{"iris":["http://x/a"],"label":"l","embedding_text":"label: A","iri_count":1}"#
        );
    }
}
