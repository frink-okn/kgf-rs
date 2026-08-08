//! Offline proof for the recoverable description indexes.
//!
//! Nothing in this module runs from [`Store::open`](crate::Store::open). The
//! serving path performs only bounded range checks; build and publication tools
//! call [`verify_description_indexes`] to scan the complete TSVs and prove that
//! the manifest metadata and indexed VoID graph agree.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::indexed::IndexedHdt;
use crate::manifest::Manifest;
use crate::map::PublishedBundle;
use crate::pattern::IdPattern;
use crate::store::{ArtifactSet, description_set_disagreement};
use crate::{Role, TermId};

use super::{
    CountPredicates, DescriptionStore, MappedTsv, SchemaRow, StatsView, VOID_CLASS,
    VOID_CLASS_PARTITION, VOID_PROPERTY, VOID_PROPERTY_PARTITION, VOID_TRIPLES, VOIDEXT_DATATYPE,
    VOIDEXT_DATATYPE_PARTITION, VOIDEXT_OBJECT_CLASS_PARTITION, compare_fields, fields,
    integer_object, malformed, object_to_subject, parse_schema_row, reject_extra_fields,
    required_field,
};
use crate::error::Result;

const SCHEMA_HEADER: &[u8] = b"view\tkind\tclass\tpredicate\tdatatype\tsubject_id\n";
const RELATIONS_HEADER: &[u8] = b"view\tsubject_class\tpredicate\tobject_class\ttriples\n";

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const VOID_DATASET: &str = "http://rdfs.org/ns/void#Dataset";
const VOID_SUBSET: &str = "http://rdfs.org/ns/void#subset";

/// Fully verify a candidate manifest's description indexes against its bundle.
///
/// The candidate need not be the manifest currently stored in the bundle. This
/// is the build/publication boundary: it scans both TSVs and traverses the VoID
/// graph, while [`crate::Store::open`] remains bounded and size-independent.
pub fn verify_description_indexes(bundle: &PublishedBundle, manifest: &Manifest) -> Result<()> {
    let dir = bundle.path();
    manifest.validate(dir)?;
    let artifacts = ArtifactSet::resolve(dir)?;
    let entries = manifest.description_artifacts();
    match (artifacts.description.as_ref(), entries) {
        (Some(_), Some(entries)) => {
            let description = DescriptionStore::open(bundle, &artifacts, entries)?;
            description.verify_indexes()
        }
        (None, None) => Ok(()),
        (Some(_), None) => Err(description_set_disagreement(
            dir,
            "the description files are present, but the candidate manifest lists none of them; \
             rebuild the description set with `kgf build`",
        )),
        (None, Some(_)) => Err(description_set_disagreement(
            dir,
            "the candidate manifest lists the description artifacts, but the files are absent; \
             regenerate it with `kgf manifest` or rebuild them with `kgf build`",
        )),
    }
}

type SelectorIndex = BTreeMap<StatsView, BTreeMap<VerifiedSelector, TermId>>;
type RelationIndex = BTreeMap<StatsView, BTreeMap<RelationKey, u64>>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum VerifiedSelector {
    Dataset,
    Class(String),
    Property {
        class: Option<String>,
        predicate: String,
    },
    Datatype {
        class: Option<String>,
        predicate: String,
        datatype: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RelationKey {
    subject_class: String,
    predicate: String,
    object_class: String,
}

struct RelationRow<'a> {
    view: &'a str,
    subject_class: &'a str,
    predicate: &'a str,
    object_class: &'a str,
    triples: &'a str,
}

impl DescriptionStore {
    /// Fully verify both description indexes against their manifest directory
    /// and the indexed VoID graph.
    ///
    /// This is an offline build/publication operation: it scans every TSV row,
    /// allocates selector maps proportional to the description, and queries the
    /// VoID graph. [`DescriptionStore::open`](super::DescriptionStore::open)
    /// deliberately does none of that work.
    pub fn verify_indexes(&self) -> Result<()> {
        let selectors = verify_schema_table(&self.schema_nodes, &self.void)?;
        verify_schema_bindings(&self.void, &selectors, self.schema_nodes.path())?;
        verify_count_facts(&self.void)?;
        let relations = verify_relation_table(&self.class_relations)?;
        let expected = expected_relations(&self.void, &selectors, self.class_relations.path())?;
        compare_relations(&relations, &expected, self.class_relations.path())
    }
}

fn verify_count_facts(void: &IndexedHdt) -> Result<()> {
    let mut subjects = BTreeSet::new();
    for predicate in CountPredicates::new(void.dict())?.all() {
        let Some(predicate_id) = predicate.id else {
            continue;
        };
        subjects.clear();
        let values = void.resolve(IdPattern {
            subject: None,
            predicate: Some(predicate_id.0),
            object: None,
        })?;
        for index in 0..values.count().value {
            let triple = values.at(index);
            if !subjects.insert(triple.subject) {
                return Err(malformed(
                    void.path(),
                    format!(
                        "partition subject {} has multiple values for <{}>",
                        triple.subject, predicate.iri
                    ),
                ));
            }
            integer_object(void, TermId(triple.object), void.path())?;
        }
    }
    Ok(())
}

impl MappedTsv {
    fn verify_rows(
        &self,
        header: &[u8],
        mut visit: impl FnMut(&StatsView, u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let bytes = self.mapping.as_bytes();
        if !bytes.starts_with(header) {
            return Err(malformed(
                self.path(),
                format!(
                    "header is not {:?}",
                    String::from_utf8_lossy(header.trim_ascii_end())
                ),
            ));
        }

        let mut cursor = header.len() as u64;
        let mut observed_max = header.len() as u64;
        for (view_name, view) in &self.views {
            if view.offset != cursor {
                return Err(malformed(
                    self.path(),
                    format!(
                        "view {:?} starts at byte {}, expected contiguous offset {cursor}",
                        manifest_view_name(view_name),
                        view.offset,
                    ),
                ));
            }

            let mut rows = 0u64;
            while cursor < view.end() {
                let row = self.row_at(cursor, view.end())?;
                observed_max = observed_max.max(row.next - cursor);
                visit(view_name, cursor, row.bytes)?;
                cursor = row.next;
                rows = rows
                    .checked_add(1)
                    .ok_or_else(|| malformed(self.path(), "row count overflows u64".to_owned()))?;
            }
            if cursor != view.end() {
                return Err(malformed(
                    self.path(),
                    format!(
                        "view {:?} ends inside a row at byte {}",
                        manifest_view_name(view_name),
                        view.end()
                    ),
                ));
            }
            if rows != view.rows {
                return Err(malformed(
                    self.path(),
                    format!(
                        "view {:?} records {} rows but contains {rows}",
                        manifest_view_name(view_name),
                        view.rows
                    ),
                ));
            }
        }

        if cursor != bytes.len() as u64 {
            return Err(malformed(
                self.path(),
                format!(
                    "view ranges cover bytes through {cursor}, but the artifact has {} bytes",
                    bytes.len()
                ),
            ));
        }
        if observed_max != self.max_row_bytes as u64 {
            return Err(malformed(
                self.path(),
                format!(
                    "manifest records max_row_bytes {}, actual maximum complete row is {observed_max}",
                    self.max_row_bytes
                ),
            ));
        }
        Ok(())
    }
}

fn verify_schema_table(table: &MappedTsv, void: &IndexedHdt) -> Result<SelectorIndex> {
    let maximum = void.dict_counts().len(Role::Subject);
    let mut indexes: SelectorIndex = table
        .views
        .keys()
        .cloned()
        .map(|view| (view, BTreeMap::new()))
        .collect();
    let mut previous_view: Option<StatsView> = None;
    let mut previous_key: Option<[String; 4]> = None;

    table.verify_rows(SCHEMA_HEADER, |view, offset, bytes| {
        let row = parse_schema_row(bytes, table.path())?;
        require_row_view(row.view, view, table.path(), offset)?;
        if previous_view.as_ref() != Some(view) {
            previous_view = Some(view.clone());
            previous_key = None;
        }
        if let Some(previous) = &previous_key {
            let previous = previous.each_ref().map(String::as_str);
            if compare_fields(previous, row.key()) != Ordering::Less {
                return Err(malformed(
                    table.path(),
                    format!(
                        "schema selector at byte {offset} is not strictly ordered after the previous row"
                    ),
                ));
            }
        }
        previous_key = Some(row.key().map(str::to_owned));

        let selector = verified_selector(&row, table.path(), offset)?;
        let subject = parse_subject_id(row.subject_id, maximum, table.path(), offset)?;
        let index = indexes
            .get_mut(view)
            .expect("every declared view was pre-populated");
        if index.insert(selector, subject).is_some() {
            return Err(malformed(
                table.path(),
                format!("duplicate schema selector at byte {offset}"),
            ));
        }
        Ok(())
    })?;

    Ok(indexes)
}

fn verified_selector(row: &SchemaRow<'_>, path: &Path, offset: u64) -> Result<VerifiedSelector> {
    let optional_class = || {
        (!row.class.is_empty())
            .then(|| expanded_iri(row.class, "class", path, offset))
            .transpose()
    };
    match row.kind {
        "dataset"
            if row.class.is_empty() && row.predicate.is_empty() && row.datatype.is_empty() =>
        {
            Ok(VerifiedSelector::Dataset)
        }
        "class" if !row.class.is_empty() && row.predicate.is_empty() && row.datatype.is_empty() => {
            Ok(VerifiedSelector::Class(expanded_iri(
                row.class, "class", path, offset,
            )?))
        }
        "property" if !row.predicate.is_empty() && row.datatype.is_empty() => {
            Ok(VerifiedSelector::Property {
                class: optional_class()?,
                predicate: expanded_iri(row.predicate, "predicate", path, offset)?,
            })
        }
        "datatype" if !row.predicate.is_empty() && !row.datatype.is_empty() => {
            Ok(VerifiedSelector::Datatype {
                class: optional_class()?,
                predicate: expanded_iri(row.predicate, "predicate", path, offset)?,
                datatype: expanded_iri(row.datatype, "datatype", path, offset)?,
            })
        }
        "dataset" | "class" | "property" | "datatype" => Err(malformed(
            path,
            format!(
                "schema row at byte {offset} has fields incompatible with kind {:?}",
                row.kind
            ),
        )),
        _ => Err(malformed(
            path,
            format!(
                "schema row at byte {offset} has unknown kind {:?}",
                row.kind
            ),
        )),
    }
}

fn parse_subject_id(value: &str, maximum: u64, path: &Path, offset: u64) -> Result<TermId> {
    let subject = value.parse::<u64>().map_err(|error| {
        malformed(
            path,
            format!("subject_id {value:?} at byte {offset} is not an unsigned decimal: {error}"),
        )
    })?;
    if subject == 0 || subject > maximum {
        return Err(malformed(
            path,
            format!(
                "subject_id {subject} at byte {offset} is outside the VoID subject id space 1..={maximum}"
            ),
        ));
    }
    Ok(TermId(subject))
}

fn expanded_iri(value: &str, field: &str, path: &Path, offset: u64) -> Result<String> {
    oxiri::Iri::parse(value).map_err(|error| {
        malformed(
            path,
            format!("{field} {value:?} at byte {offset} is not an absolute IRI: {error}"),
        )
    })?;
    Ok(value.to_owned())
}

fn verify_schema_bindings(void: &IndexedHdt, indexes: &SelectorIndex, path: &Path) -> Result<()> {
    let queryable = indexes
        .get(&StatsView::Queryable)
        .and_then(|index| index.get(&VerifiedSelector::Dataset))
        .copied()
        .ok_or_else(|| malformed(path, "queryable view has no dataset selector".to_owned()))?;

    for (view, index) in indexes {
        let root = index
            .get(&VerifiedSelector::Dataset)
            .copied()
            .ok_or_else(|| {
                malformed(
                    path,
                    format!(
                        "view {:?} has no dataset selector",
                        manifest_view_name(view)
                    ),
                )
            })?;
        ensure_named_triple(void, root, RDF_TYPE, VOID_DATASET, path, "dataset type")?;
        if *view != StatsView::Queryable {
            ensure_link(void, queryable, VOID_SUBSET, root, path, "view root")?;
        }

        for (selector, node) in index {
            ensure_named_triple(void, *node, RDF_TYPE, VOID_DATASET, path, "partition type")?;
            match selector {
                VerifiedSelector::Dataset => {}
                VerifiedSelector::Class(class) => {
                    ensure_named_triple(void, *node, VOID_CLASS, class, path, "class selector")?;
                    ensure_link(
                        void,
                        root,
                        VOID_CLASS_PARTITION,
                        *node,
                        path,
                        "class partition",
                    )?;
                }
                VerifiedSelector::Property { class, predicate } => {
                    let parent_selector =
                        class.as_ref().map_or(VerifiedSelector::Dataset, |class| {
                            VerifiedSelector::Class(class.clone())
                        });
                    let parent = required_selector(index, &parent_selector, view, path)?;
                    ensure_named_triple(
                        void,
                        *node,
                        VOID_PROPERTY,
                        predicate,
                        path,
                        "property selector",
                    )?;
                    ensure_link(
                        void,
                        parent,
                        VOID_PROPERTY_PARTITION,
                        *node,
                        path,
                        "property partition",
                    )?;
                }
                VerifiedSelector::Datatype {
                    class,
                    predicate,
                    datatype,
                } => {
                    let parent_selector = VerifiedSelector::Property {
                        class: class.clone(),
                        predicate: predicate.clone(),
                    };
                    let parent = required_selector(index, &parent_selector, view, path)?;
                    ensure_named_triple(
                        void,
                        *node,
                        VOIDEXT_DATATYPE,
                        datatype,
                        path,
                        "datatype selector",
                    )?;
                    ensure_link(
                        void,
                        parent,
                        VOIDEXT_DATATYPE_PARTITION,
                        *node,
                        path,
                        "datatype partition",
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn required_selector(
    index: &BTreeMap<VerifiedSelector, TermId>,
    selector: &VerifiedSelector,
    view: &StatsView,
    path: &Path,
) -> Result<TermId> {
    index.get(selector).copied().ok_or_else(|| {
        malformed(
            path,
            format!(
                "view {:?} is missing parent selector {selector:?}",
                manifest_view_name(view)
            ),
        )
    })
}

fn verify_relation_table(table: &MappedTsv) -> Result<RelationIndex> {
    let mut indexes: RelationIndex = table
        .views
        .keys()
        .cloned()
        .map(|view| (view, BTreeMap::new()))
        .collect();
    let mut previous_view: Option<StatsView> = None;
    let mut previous: Option<(u64, RelationKey)> = None;

    table.verify_rows(RELATIONS_HEADER, |view, offset, bytes| {
        let row = parse_verified_relation(bytes, table.path())?;
        require_row_view(row.view, view, table.path(), offset)?;
        if previous_view.as_ref() != Some(view) {
            previous_view = Some(view.clone());
            previous = None;
        }

        let key = RelationKey {
            subject_class: expanded_iri(
                row.subject_class,
                "subject_class",
                table.path(),
                offset,
            )?,
            predicate: expanded_iri(row.predicate, "predicate", table.path(), offset)?,
            object_class: expanded_iri(
                row.object_class,
                "object_class",
                table.path(),
                offset,
            )?,
        };
        let triples = row.triples.parse::<u64>().map_err(|error| {
            malformed(
                table.path(),
                format!(
                    "triples {:?} at byte {offset} is not an unsigned decimal: {error}",
                    row.triples
                ),
            )
        })?;
        if let Some((previous_triples, previous_key)) = &previous
            && (*previous_triples < triples
                || (*previous_triples == triples && previous_key >= &key))
        {
            return Err(malformed(
                table.path(),
                format!(
                    "class relation at byte {offset} is not in triples-descending, IRI-ascending order"
                ),
            ));
        }
        previous = Some((triples, key.clone()));

        let index = indexes
            .get_mut(view)
            .expect("every declared view was pre-populated");
        if index.insert(key, triples).is_some() {
            return Err(malformed(
                table.path(),
                format!("duplicate class relation at byte {offset}"),
            ));
        }
        Ok(())
    })?;
    Ok(indexes)
}

fn parse_verified_relation<'a>(bytes: &'a [u8], path: &Path) -> Result<RelationRow<'a>> {
    let mut fields = fields(bytes, path)?;
    let row = RelationRow {
        view: required_field(&mut fields, 0, 5, path)?,
        subject_class: required_field(&mut fields, 1, 5, path)?,
        predicate: required_field(&mut fields, 2, 5, path)?,
        object_class: required_field(&mut fields, 3, 5, path)?,
        triples: required_field(&mut fields, 4, 5, path)?,
    };
    reject_extra_fields(fields, 5, path)?;
    Ok(row)
}

fn expected_relations(
    void: &IndexedHdt,
    selectors: &SelectorIndex,
    path: &Path,
) -> Result<RelationIndex> {
    let mut cache: BTreeMap<TermId, BTreeMap<String, u64>> = BTreeMap::new();
    let mut expected = BTreeMap::new();
    for (view, index) in selectors {
        let mut relations = BTreeMap::new();
        for (selector, property_node) in index {
            let VerifiedSelector::Property {
                class: Some(class),
                predicate,
            } = selector
            else {
                continue;
            };
            let targets = if let Some(targets) = cache.get(property_node) {
                targets.clone()
            } else {
                let targets = typed_targets(void, *property_node, path)?;
                cache.insert(*property_node, targets.clone());
                targets
            };
            for (object_class, triples) in targets {
                let key = RelationKey {
                    subject_class: class.clone(),
                    predicate: predicate.clone(),
                    object_class,
                };
                if relations.insert(key.clone(), triples).is_some() {
                    return Err(malformed(
                        path,
                        format!(
                            "VoID graph yields duplicate class relation {key:?} in view {:?}",
                            manifest_view_name(view)
                        ),
                    ));
                }
            }
        }
        expected.insert(view.clone(), relations);
    }
    Ok(expected)
}

fn typed_targets(
    void: &IndexedHdt,
    property: TermId,
    path: &Path,
) -> Result<BTreeMap<String, u64>> {
    let mut targets = BTreeMap::new();
    for child_object in object_ids(void, property, VOIDEXT_OBJECT_CLASS_PARTITION, path)? {
        let child = object_to_subject(void, child_object, path, "object-class partition")?;
        let classes = object_ids(void, child, VOID_CLASS, path)?;
        if classes.is_empty() {
            continue;
        }
        if classes.len() != 1 {
            return Err(malformed(
                path,
                format!(
                    "object-class partition subject {} has {} void:class values",
                    child.0,
                    classes.len()
                ),
            ));
        }
        let object_class = object_iri(void, classes[0], path, "object class")?;
        let counts = object_ids(void, child, VOID_TRIPLES, path)?;
        if counts.len() != 1 {
            return Err(malformed(
                path,
                format!(
                    "object-class partition subject {} has {} void:triples values",
                    child.0,
                    counts.len()
                ),
            ));
        }
        let triples = integer_object(void, counts[0], path)?;
        if targets.insert(object_class.clone(), triples).is_some() {
            return Err(malformed(
                path,
                format!(
                    "property partition subject {} repeats object class {object_class}",
                    property.0
                ),
            ));
        }
    }
    Ok(targets)
}

fn compare_relations(actual: &RelationIndex, expected: &RelationIndex, path: &Path) -> Result<()> {
    if actual.keys().ne(expected.keys()) {
        return Err(malformed(
            path,
            "schema and class-relation indexes do not contain the same views".to_owned(),
        ));
    }
    for (view, expected_rows) in expected {
        let actual_rows = actual
            .get(view)
            .expect("view key sets were compared immediately above");
        for (key, expected_count) in expected_rows {
            match actual_rows.get(key) {
                Some(actual_count) if actual_count == expected_count => {}
                Some(actual_count) => {
                    return Err(malformed(
                        path,
                        format!(
                            "class relation {key:?} in view {:?} records {actual_count} triples, VoID records {expected_count}",
                            manifest_view_name(view)
                        ),
                    ));
                }
                None => {
                    return Err(malformed(
                        path,
                        format!(
                            "class relation {key:?} from VoID is missing in view {:?}",
                            manifest_view_name(view)
                        ),
                    ));
                }
            }
        }
        if let Some(extra) = actual_rows
            .keys()
            .find(|key| !expected_rows.contains_key(*key))
        {
            return Err(malformed(
                path,
                format!(
                    "class relation {extra:?} in view {:?} is absent from VoID",
                    manifest_view_name(view)
                ),
            ));
        }
    }
    Ok(())
}

fn require_row_view(row_view: &str, declared: &StatsView, path: &Path, offset: u64) -> Result<()> {
    let expected = manifest_view_name(declared);
    if row_view != expected {
        return Err(malformed(
            path,
            format!(
                "row at byte {offset} belongs to view {row_view:?}, manifest range declares {expected:?}"
            ),
        ));
    }
    Ok(())
}

fn manifest_view_name(view: &StatsView) -> Cow<'_, str> {
    match view {
        StatsView::Design => Cow::Borrowed("design"),
        StatsView::Queryable => Cow::Borrowed("queryable"),
        StatsView::Component(component) => Cow::Owned(format!("component:{}", component.as_str())),
    }
}

fn ensure_named_triple(
    void: &IndexedHdt,
    subject: TermId,
    predicate: &str,
    object: &str,
    path: &Path,
    context: &str,
) -> Result<()> {
    if !has_named_triple(void, subject, predicate, object)? {
        return Err(malformed(
            path,
            format!(
                "{context} subject {} does not state <{predicate}> <{object}> in stats/void.hdt",
                subject.0
            ),
        ));
    }
    Ok(())
}

fn ensure_link(
    void: &IndexedHdt,
    parent: TermId,
    predicate: &str,
    child: TermId,
    path: &Path,
    context: &str,
) -> Result<()> {
    let dictionary = void.dict();
    let mut buffer = Vec::new();
    let child_term = dictionary.extract(Role::Subject, child, &mut buffer)?;
    let child_object = dictionary.locate(Role::Object, child_term)?;
    let linked = if let (Some(predicate), Some(child_object)) = (
        dictionary.locate(Role::Predicate, predicate.as_bytes())?,
        child_object,
    ) {
        void.resolve(IdPattern {
            subject: Some(parent.0),
            predicate: Some(predicate.0),
            object: Some(child_object.0),
        })?
        .count()
        .value
            == 1
    } else {
        false
    };
    if !linked {
        return Err(malformed(
            path,
            format!(
                "{context} subject {} is not linked from parent {} by <{predicate}>",
                child.0, parent.0
            ),
        ));
    }
    Ok(())
}

fn has_named_triple(
    void: &IndexedHdt,
    subject: TermId,
    predicate: &str,
    object: &str,
) -> Result<bool> {
    let dictionary = void.dict();
    let Some(predicate) = dictionary.locate(Role::Predicate, predicate.as_bytes())? else {
        return Ok(false);
    };
    let Some(object) = dictionary.locate(Role::Object, object.as_bytes())? else {
        return Ok(false);
    };
    Ok(void
        .resolve(IdPattern {
            subject: Some(subject.0),
            predicate: Some(predicate.0),
            object: Some(object.0),
        })?
        .count()
        .value
        == 1)
}

fn object_ids(
    void: &IndexedHdt,
    subject: TermId,
    predicate: &str,
    path: &Path,
) -> Result<Vec<TermId>> {
    let dictionary = void.dict();
    let Some(predicate) = dictionary.locate(Role::Predicate, predicate.as_bytes())? else {
        return Ok(Vec::new());
    };
    let selection = void.resolve(IdPattern {
        subject: Some(subject.0),
        predicate: Some(predicate.0),
        object: None,
    })?;
    let count = selection.count().value;
    let capacity = usize::try_from(count).map_err(|_| {
        malformed(
            path,
            format!("VoID result of {count} rows does not fit this platform"),
        )
    })?;
    let mut objects = Vec::with_capacity(capacity);
    for index in 0..count {
        objects.push(TermId(selection.at(index).object));
    }
    Ok(objects)
}

fn object_iri(void: &IndexedHdt, object: TermId, path: &Path, context: &str) -> Result<String> {
    let dictionary = void.dict();
    let mut buffer = Vec::new();
    let term = dictionary.extract(Role::Object, object, &mut buffer)?;
    let value = std::str::from_utf8(term)
        .map_err(|error| malformed(path, format!("{context} is not UTF-8: {error}")))?;
    expanded_iri(value, context, path, 0)
}
