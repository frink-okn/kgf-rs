//! Mapped access to the bundle's description indexes.
//!
//! A [`DescriptionStore`] owns the indexed VoID HDT and the two TSV mappings,
//! while the manifest contributes only a small typed directory of byte ranges.
//! Opening therefore reads no TSV row and allocates nothing proportional to the
//! description size. Schema lookup binary-searches one declared view block;
//! class-relation paging walks rows from a resumable byte boundary.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;

use crate::error::{Error, Result};
use crate::indexed::IndexedHdt;
use crate::manifest::{ArtifactEntry, ArtifactView, DescriptionArtifactEntries};
use crate::map::{BytesSpec, Mapping, MappingId, PublishedBundle, open_published};
use crate::store::ArtifactSet;
#[cfg(test)]
use crate::store::artifact;
use crate::{Role, TermId};

/// A manifest component identifier used by a description view.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentId(String);

impl ComponentId {
    /// Parse a non-empty component identifier.
    pub fn new(id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        (!id.is_empty()).then_some(Self(id))
    }

    /// The component identifier without the manifest's `component:` prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One description layer published by the bundle.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatsView {
    /// The canonical component's designed schema.
    Design,
    /// The merged graph whose counts agree with query operations.
    Queryable,
    /// One published component.
    Component(ComponentId),
}

impl StatsView {
    /// Construct a component view, refusing an empty component identifier.
    pub fn component(id: impl Into<String>) -> Option<Self> {
        ComponentId::new(id).map(Self::Component)
    }

    fn from_manifest_key(key: &str) -> Option<Self> {
        match key {
            "design" => Some(Self::Design),
            "queryable" => Some(Self::Queryable),
            _ => key
                .strip_prefix("component:")
                .and_then(ComponentId::new)
                .map(Self::Component),
        }
    }
}

/// A semantic selector whose shape is valid for `/schema`.
///
/// The enum makes invalid combinations such as a datatype without a predicate
/// unrepresentable. IRI syntax is an edge concern; callers pass the expanded
/// strings used in the checked TSV index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaSelector<'a> {
    /// The dataset root.
    Dataset,
    /// One class partition.
    Class {
        /// Expanded class IRI.
        class: &'a str,
    },
    /// A dataset- or class-scoped property partition.
    Property {
        /// Expanded class IRI for a class-scoped property.
        class: Option<&'a str>,
        /// Expanded predicate IRI.
        predicate: &'a str,
    },
    /// A dataset- or class-scoped datatype partition.
    Datatype {
        /// Expanded class IRI for a class-scoped datatype.
        class: Option<&'a str>,
        /// Expanded predicate IRI.
        predicate: &'a str,
        /// Expanded datatype IRI.
        datatype: &'a str,
    },
}

impl SchemaSelector<'_> {
    fn key(&self) -> [&str; 4] {
        match *self {
            Self::Dataset => ["dataset", "", "", ""],
            Self::Class { class } => ["class", class, "", ""],
            Self::Property { class, predicate } => ["property", class.unwrap_or(""), predicate, ""],
            Self::Datatype {
                class,
                predicate,
                datatype,
            } => ["datatype", class.unwrap_or(""), predicate, datatype],
        }
    }
}

/// A schema selector resolved to the final VoID HDT's subject id space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaNode {
    /// One-based subject dictionary id in `stats/void.hdt`.
    pub subject: TermId,
}

/// Optional filters over the persisted class-relation order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassRelationFilter<'a> {
    /// Expanded subject-class IRI to retain.
    pub class: Option<&'a str>,
    /// Expanded predicate IRI to retain.
    pub predicate: Option<&'a str>,
}

impl ClassRelationFilter<'_> {
    fn matches(&self, row: &ClassRelation<'_>) -> bool {
        self.class.is_none_or(|class| class == row.subject_class)
            && self
                .predicate
                .is_none_or(|predicate| predicate == row.predicate)
    }

    fn is_empty(&self) -> bool {
        self.class.is_none() && self.predicate.is_none()
    }
}

/// One observed class relation from the persisted flat projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassRelation<'a> {
    /// Expanded subject-class IRI.
    pub subject_class: &'a str,
    /// Expanded predicate IRI.
    pub predicate: &'a str,
    /// Expanded object-class IRI.
    pub object_class: &'a str,
    /// Number of observed triples contributing to this relation.
    pub triples: u64,
}

/// A validated byte boundary from which class-relation paging may resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassRelationPosition {
    mapping: MappingId,
    view_start: u64,
    view_end: u64,
    offset: u64,
}

impl ClassRelationPosition {
    /// Absolute byte offset in `stats/class-relations.tsv`.
    pub fn byte_offset(self) -> u64 {
        self.offset
    }
}

/// Why one class-relation page stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassRelationStop {
    /// The selected view has no rows left.
    Complete,
    /// The requested number of matching rows was returned.
    RowLimit,
    /// A filtered scan examined its caller-supplied candidate limit.
    ScanLimit,
}

/// One bounded page from the persisted class-relation order.
#[derive(Debug, PartialEq, Eq)]
pub struct ClassRelationPage<'a> {
    /// Matching rows, in the artifact's global count-descending order.
    pub items: Vec<ClassRelation<'a>>,
    /// Boundary for the next page; absent only when the view is exhausted.
    pub next: Option<ClassRelationPosition>,
    /// The condition that ended this page.
    pub stop: ClassRelationStop,
    /// Rows examined, including filtered-out candidates.
    pub examined: usize,
}

/// The mapped, immutable description surface for one bundle version.
pub struct DescriptionStore {
    void: IndexedHdt,
    schema_nodes: MappedTsv,
    class_relations: MappedTsv,
}

impl std::fmt::Debug for DescriptionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DescriptionStore")
            .field("void", &self.void)
            .field("schema_views", &self.schema_nodes.views.len())
            .field("class_relation_views", &self.class_relations.views.len())
            .finish()
    }
}

impl DescriptionStore {
    pub(crate) fn open(
        bundle: &PublishedBundle,
        artifacts: &ArtifactSet,
        entries: DescriptionArtifactEntries<'_>,
    ) -> Result<Self> {
        let description = artifacts
            .description
            .as_ref()
            .expect("DescriptionStore opens only for a resolved description set");
        let void = artifacts
            .open_description(bundle)?
            .expect("the resolved description set has a VoID pair");
        let schema_nodes = open_tsv(bundle, &description.schema_nodes, entries.schema_nodes)?;
        let class_relations = open_tsv(
            bundle,
            &description.class_relations,
            entries.class_relations,
        )?;

        Ok(Self {
            void,
            schema_nodes,
            class_relations,
        })
    }

    /// Select one published description view.
    ///
    /// `None` distinguishes an unknown component from a valid view whose
    /// semantic selector has no matching node.
    pub fn view(&self, view: &StatsView) -> Option<DescriptionView<'_>> {
        let schema = self.schema_nodes.views.get(view)?;
        let relations = self.class_relations.views.get(view)?;
        Some(DescriptionView {
            store: self,
            schema: *schema,
            relations: *relations,
        })
    }
}

/// Borrowed operations over one selected description layer.
#[derive(Debug, Clone, Copy)]
pub struct DescriptionView<'a> {
    store: &'a DescriptionStore,
    schema: ViewSpec,
    relations: ViewSpec,
}

impl<'a> DescriptionView<'a> {
    /// Number of selector rows recorded for this view.
    pub fn schema_rows(&self) -> u64 {
        self.schema.rows
    }

    /// Number of class-relation rows recorded for this view.
    pub fn class_relation_rows(&self) -> u64 {
        self.relations.rows
    }

    /// Resolve a semantic selector through the mapped selector index.
    pub fn schema_node(&self, selector: SchemaSelector<'_>) -> Result<Option<SchemaNode>> {
        let target = selector.key();
        let Some(row) = self
            .store
            .schema_nodes
            .find_schema_row(self.schema, target)?
        else {
            return Ok(None);
        };
        let subject = row.subject_id.parse::<u64>().map_err(|error| {
            malformed(
                self.store.schema_nodes.path(),
                format!(
                    "subject_id {:?} is not an unsigned decimal: {error}",
                    row.subject_id
                ),
            )
        })?;
        let maximum = self.store.void.dict_counts().len(Role::Subject);
        if subject == 0 || subject > maximum {
            return Err(malformed(
                self.store.schema_nodes.path(),
                format!("subject_id {subject} is outside the VoID subject id space 1..={maximum}"),
            ));
        }
        Ok(Some(SchemaNode {
            subject: TermId(subject),
        }))
    }

    /// Validate a decoded cursor offset against this view in constant time.
    pub fn class_relation_position(&self, offset: u64) -> Option<ClassRelationPosition> {
        self.store.class_relations.position(self.relations, offset)
    }

    /// Page the persisted class-relation order.
    ///
    /// `row_limit` bounds returned matches. `scan_limit` is consulted only when
    /// a filter is present; an unfiltered page examines exactly the rows it
    /// returns and therefore already costs `O(row_limit)`.
    pub fn class_relations(
        &self,
        filter: ClassRelationFilter<'_>,
        from: Option<ClassRelationPosition>,
        row_limit: NonZeroUsize,
        scan_limit: NonZeroUsize,
    ) -> Result<ClassRelationPage<'a>> {
        let table = &self.store.class_relations;
        let mut position = from.unwrap_or_else(|| table.start_position(self.relations));
        if position.mapping != table.mapping.id()
            || position.view_start != self.relations.offset
            || position.view_end != self.relations.end()
        {
            return Err(Error::Region(
                "class-relation position belongs to a different mapped view".to_owned(),
            ));
        }

        let mut items = Vec::new();
        let mut examined = 0usize;
        let filtered = !filter.is_empty();
        while position.offset < self.relations.end() {
            if items.len() == row_limit.get() {
                return Ok(relation_page(
                    items,
                    position,
                    ClassRelationStop::RowLimit,
                    examined,
                ));
            }
            if filtered && examined == scan_limit.get() {
                return Ok(relation_page(
                    items,
                    position,
                    ClassRelationStop::ScanLimit,
                    examined,
                ));
            }

            let row = table.row_at(position.offset, self.relations.end())?;
            position.offset = row.next;
            examined += 1;
            let relation = parse_relation_row(row.bytes, table.path())?;
            if filter.matches(&relation) {
                items.push(relation);
            }
        }

        Ok(ClassRelationPage {
            items,
            next: None,
            stop: ClassRelationStop::Complete,
            examined,
        })
    }
}

fn relation_page<'a>(
    items: Vec<ClassRelation<'a>>,
    next: ClassRelationPosition,
    stop: ClassRelationStop,
    examined: usize,
) -> ClassRelationPage<'a> {
    ClassRelationPage {
        items,
        next: Some(next),
        stop,
        examined,
    }
}

fn open_tsv(bundle: &PublishedBundle, path: &Path, entry: &ArtifactEntry) -> Result<MappedTsv> {
    let mapping = open_published(bundle, path)?;
    MappedTsv::open(mapping, entry)
}

#[derive(Debug)]
struct MappedTsv {
    mapping: Mapping,
    max_row_bytes: usize,
    views: BTreeMap<StatsView, ViewSpec>,
}

impl MappedTsv {
    fn open(mapping: Mapping, entry: &ArtifactEntry) -> Result<Self> {
        let actual = mapping.as_bytes().len() as u64;
        if entry.bytes != actual {
            return Err(malformed(
                mapping.path(),
                format!(
                    "manifest records {} bytes, but the artifact contains {actual}",
                    entry.bytes
                ),
            ));
        }
        let max_row_bytes = entry
            .max_row_bytes
            .expect("Manifest::validate requires TSV max_row_bytes");
        let max_row_bytes = usize::try_from(max_row_bytes).map_err(|_| {
            malformed(
                mapping.path(),
                "max_row_bytes does not fit this platform".to_owned(),
            )
        })?;
        if max_row_bytes == 0 {
            return Err(malformed(
                mapping.path(),
                "max_row_bytes must be greater than zero".to_owned(),
            ));
        }

        let mut views = BTreeMap::new();
        for (name, view) in &entry.views {
            let parsed = StatsView::from_manifest_key(name)
                .expect("Manifest::validate accepts only typed description view names");
            let bytes = BytesSpec::new(&mapping, view.offset, view.bytes).map_err(|error| {
                malformed(
                    mapping.path(),
                    format!("view {name:?} is out of range: {error}"),
                )
            })?;
            views.insert(parsed, ViewSpec::new(*view, bytes));
        }

        Ok(Self {
            mapping,
            max_row_bytes,
            views,
        })
    }

    fn path(&self) -> &Path {
        self.mapping.path()
    }

    fn start_position(&self, view: ViewSpec) -> ClassRelationPosition {
        ClassRelationPosition {
            mapping: self.mapping.id(),
            view_start: view.offset,
            view_end: view.end(),
            offset: view.offset,
        }
    }

    fn position(&self, view: ViewSpec, offset: u64) -> Option<ClassRelationPosition> {
        if offset < view.offset || offset > view.end() {
            return None;
        }
        if offset != view.offset
            && offset != view.end()
            && self.mapping.as_bytes().get(offset as usize - 1) != Some(&b'\n')
        {
            return None;
        }
        Some(ClassRelationPosition {
            mapping: self.mapping.id(),
            view_start: view.offset,
            view_end: view.end(),
            offset,
        })
    }

    fn find_schema_row<'a>(
        &'a self,
        view: ViewSpec,
        target: [&str; 4],
    ) -> Result<Option<SchemaRow<'a>>> {
        let mut left = view.offset;
        let mut right = view.end();
        while left < right {
            let probe = left + (right - left) / 2;
            let Some(start) = self.row_start_at_or_after(probe, right) else {
                right = probe;
                continue;
            };
            if start >= right {
                right = probe;
                continue;
            }
            let row = self.row_at(start, view.end())?;
            let parsed = parse_schema_row(row.bytes, self.path())?;
            match compare_fields(parsed.key(), target) {
                Ordering::Less => left = row.next,
                Ordering::Greater => right = start,
                Ordering::Equal => return Ok(Some(parsed)),
            }
        }
        Ok(None)
    }

    fn row_start_at_or_after(&self, probe: u64, before: u64) -> Option<u64> {
        if probe >= before {
            return None;
        }
        if probe == 0 || self.mapping.as_bytes().get(probe as usize - 1) == Some(&b'\n') {
            return Some(probe);
        }
        let limit = probe.saturating_add(self.max_row_bytes as u64).min(before);
        let bytes = &self.mapping.as_bytes()[probe as usize..limit as usize];
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|relative| probe + relative as u64 + 1)
    }

    fn row_at<'a>(&'a self, start: u64, view_end: u64) -> Result<Row<'a>> {
        let limit = start
            .saturating_add(self.max_row_bytes as u64)
            .min(view_end);
        let bytes = &self.mapping.as_bytes()[start as usize..limit as usize];
        let newline = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| {
                malformed(
                    self.path(),
                    format!(
                        "row at byte {start} has no newline within max_row_bytes ({})",
                        self.max_row_bytes
                    ),
                )
            })?;
        Ok(Row {
            bytes: &bytes[..newline],
            next: start + newline as u64 + 1,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ViewSpec {
    offset: u64,
    bytes: BytesSpec,
    rows: u64,
}

impl ViewSpec {
    fn new(view: ArtifactView, bytes: BytesSpec) -> Self {
        Self {
            offset: view.offset,
            bytes,
            rows: view.rows,
        }
    }

    fn end(self) -> u64 {
        self.offset + self.bytes.len()
    }
}

struct Row<'a> {
    bytes: &'a [u8],
    next: u64,
}

struct SchemaRow<'a> {
    kind: &'a str,
    class: &'a str,
    predicate: &'a str,
    datatype: &'a str,
    subject_id: &'a str,
}

impl SchemaRow<'_> {
    fn key(&self) -> [&str; 4] {
        [self.kind, self.class, self.predicate, self.datatype]
    }
}

fn parse_schema_row<'a>(bytes: &'a [u8], path: &Path) -> Result<SchemaRow<'a>> {
    let mut fields = fields(bytes, path)?;
    let _view = required_field(&mut fields, 0, 6, path)?;
    let row = SchemaRow {
        kind: required_field(&mut fields, 1, 6, path)?,
        class: required_field(&mut fields, 2, 6, path)?,
        predicate: required_field(&mut fields, 3, 6, path)?,
        datatype: required_field(&mut fields, 4, 6, path)?,
        subject_id: required_field(&mut fields, 5, 6, path)?,
    };
    reject_extra_fields(fields, 6, path)?;
    Ok(row)
}

fn parse_relation_row<'a>(bytes: &'a [u8], path: &Path) -> Result<ClassRelation<'a>> {
    let mut fields = fields(bytes, path)?;
    let _view = required_field(&mut fields, 0, 5, path)?;
    let subject_class = required_field(&mut fields, 1, 5, path)?;
    let predicate = required_field(&mut fields, 2, 5, path)?;
    let object_class = required_field(&mut fields, 3, 5, path)?;
    let triples = required_field(&mut fields, 4, 5, path)?;
    reject_extra_fields(fields, 5, path)?;
    let triples = triples.parse::<u64>().map_err(|error| {
        malformed(
            path,
            format!("triples {triples:?} is not an unsigned decimal: {error}"),
        )
    })?;
    Ok(ClassRelation {
        subject_class,
        predicate,
        object_class,
        triples,
    })
}

fn fields<'a>(bytes: &'a [u8], path: &Path) -> Result<std::str::Split<'a, char>> {
    let row = std::str::from_utf8(bytes)
        .map_err(|error| malformed(path, format!("row is not UTF-8: {error}")))?;
    Ok(row.split('\t'))
}

fn required_field<'a>(
    fields: &mut std::str::Split<'a, char>,
    index: usize,
    expected: usize,
    path: &Path,
) -> Result<&'a str> {
    fields
        .next()
        .ok_or_else(|| malformed(path, format!("row has {index} fields, expected {expected}")))
}

fn reject_extra_fields(
    mut fields: std::str::Split<'_, char>,
    expected: usize,
    path: &Path,
) -> Result<()> {
    let Some(_) = fields.next() else {
        return Ok(());
    };
    let found = expected + 1 + fields.count();
    Err(malformed(
        path,
        format!("row has {found} fields, expected {expected}"),
    ))
}

fn compare_fields<const N: usize>(left: [&str; N], right: [&str; N]) -> Ordering {
    left.into_iter()
        .zip(right)
        .find_map(|(left, right)| {
            let ordering = left.as_bytes().cmp(right.as_bytes());
            (ordering != Ordering::Equal).then_some(ordering)
        })
        .unwrap_or(Ordering::Equal)
}

fn malformed(path: &Path, detail: String) -> Error {
    Error::Malformed {
        artifact: path.to_path_buf(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Counts, Formats, Manifest};
    use crate::store::{OpenOptions, Store};
    use crate::testing::{
        CLASS_RELATIONS_HEADER, Fixture, SCHEMA_NODES_HEADER, TINY_NT, published_bundle,
    };

    struct PublishedDescription {
        alice: u64,
        bob: u64,
    }

    fn publish_description(fixture: &Fixture) -> PublishedDescription {
        let ids = {
            let indexed = IndexedHdt::open(fixture.map_hdt(), fixture.map_perm()).unwrap();
            let dictionary = indexed.dict();
            let id = |term: &[u8]| {
                dictionary
                    .locate(Role::Subject, term)
                    .unwrap()
                    .expect("fixture subject")
                    .0
            };
            (
                id(b"http://example.org/alice"),
                id(b"http://example.org/bob"),
            )
        };

        let bundle = fixture.bundle_path();
        let mut schema = SCHEMA_NODES_HEADER.to_vec();
        let mut schema_views = BTreeMap::new();
        append_view(
            &mut schema,
            &mut schema_views,
            "design",
            &[
                format!("class\thttps://example.org/A\t\t\t{}", ids.0),
                format!("dataset\t\t\t\t{}", ids.1),
                format!(
                    "datatype\t\thttps://example.org/p1\thttps://example.org/Type\t{}",
                    ids.0
                ),
                format!("property\t\thttps://example.org/p1\t\t{}", ids.1),
            ],
        );
        append_view(
            &mut schema,
            &mut schema_views,
            "queryable",
            &[format!("dataset\t\t\t\t{}", ids.0)],
        );
        append_view(
            &mut schema,
            &mut schema_views,
            "component:canonical",
            &[format!("dataset\t\t\t\t{}", ids.1)],
        );
        let schema_max = max_complete_row(&schema);

        let mut relations = CLASS_RELATIONS_HEADER.to_vec();
        let mut relation_views = BTreeMap::new();
        append_view(
            &mut relations,
            &mut relation_views,
            "design",
            &[
                "https://example.org/A\thttps://example.org/p1\thttps://example.org/B\t50"
                    .to_owned(),
                "https://example.org/A\thttps://example.org/p2\thttps://example.org/C\t30"
                    .to_owned(),
                "https://example.org/D\thttps://example.org/p1\thttps://example.org/E\t10"
                    .to_owned(),
            ],
        );
        append_view(
            &mut relations,
            &mut relation_views,
            "queryable",
            &[
                "https://example.org/Q\thttps://example.org/p\thttps://example.org/R\t60"
                    .to_owned(),
            ],
        );
        append_view(
            &mut relations,
            &mut relation_views,
            "component:canonical",
            &[
                "https://example.org/C\thttps://example.org/p\thttps://example.org/D\t20"
                    .to_owned(),
            ],
        );
        let relation_max = max_complete_row(&relations);
        fixture.add_description_artifacts(&schema, &relations);

        let mut artifacts = BTreeMap::new();
        for name in [
            artifact::HDT,
            artifact::PERM,
            artifact::VOID_HDT,
            artifact::VOID_PERM,
            artifact::NAMESPACES,
            artifact::SUMMARY_JSON,
            artifact::SUMMARY_MD,
        ] {
            artifacts.insert(name.to_owned(), checksum_entry(&bundle.join(name)));
        }
        artifacts.insert(
            artifact::SCHEMA_NODES.to_owned(),
            tsv_entry(schema.len() as u64, schema_max, schema_views),
        );
        artifacts.insert(
            artifact::CLASS_RELATIONS.to_owned(),
            tsv_entry(relations.len() as u64, relation_max, relation_views),
        );

        let manifest = Manifest {
            id: "example".to_owned(),
            dataset_iri: None,
            version: "v1".to_owned(),
            content_digest: "sha256:00".to_owned(),
            created: None,
            formats: Formats::default(),
            title: None,
            description: None,
            license: None,
            homepage: None,
            publisher: None,
            counts: Counts {
                triples: 8,
                subjects: 3,
                predicates: 5,
                objects: 8,
            },
            capabilities: BTreeMap::new(),
            prefixes: BTreeMap::new(),
            predicate_roles: BTreeMap::new(),
            artifacts,
            previous_version: None,
        };
        manifest.validate(bundle).unwrap();
        std::fs::write(
            bundle.join(artifact::MANIFEST),
            manifest.to_json_bytes().unwrap(),
        )
        .unwrap();

        PublishedDescription {
            alice: ids.0,
            bob: ids.1,
        }
    }

    fn append_view(
        bytes: &mut Vec<u8>,
        views: &mut BTreeMap<String, ArtifactView>,
        view: &str,
        rows: &[String],
    ) {
        let offset = bytes.len() as u64;
        for row in rows {
            bytes.extend_from_slice(view.as_bytes());
            bytes.push(b'\t');
            bytes.extend_from_slice(row.as_bytes());
            bytes.push(b'\n');
        }
        views.insert(
            view.to_owned(),
            ArtifactView {
                offset,
                bytes: bytes.len() as u64 - offset,
                rows: rows.len() as u64,
            },
        );
    }

    fn max_complete_row(bytes: &[u8]) -> u64 {
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .map(<[u8]>::len)
            .max()
            .unwrap() as u64
    }

    fn checksum_entry(path: &Path) -> ArtifactEntry {
        ArtifactEntry::checksum(std::fs::metadata(path).unwrap().len(), "00")
    }

    fn tsv_entry(
        bytes: u64,
        max_row_bytes: u64,
        views: BTreeMap<String, ArtifactView>,
    ) -> ArtifactEntry {
        let mut entry = ArtifactEntry::checksum(bytes, "00");
        entry.parents = vec![artifact::VOID_HDT.to_owned()];
        entry.max_row_bytes = Some(max_row_bytes);
        entry.views = views;
        entry
    }

    fn opened_description(fixture: &Fixture) -> Store {
        let published = published_bundle(fixture.bundle_path());
        Store::open(&published, OpenOptions::default()).unwrap()
    }

    #[test]
    fn row_boundary_search_finds_every_variable_width_key_and_no_gap() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("schema-nodes.tsv");
        let mut bytes = b"view\tkind\tclass\tpredicate\tdatatype\tsubject_id\n".to_vec();
        let offset = bytes.len() as u64;
        let keys: Vec<_> = (0..129)
            .map(|index| format!("{index:04}{}", "x".repeat(index % 17)))
            .collect();
        for key in &keys {
            bytes.extend_from_slice(format!("design\tclass\t{key}\t\t\t1\n").as_bytes());
        }
        let view = ArtifactView {
            offset,
            bytes: bytes.len() as u64 - offset,
            rows: keys.len() as u64,
        };
        let max_row_bytes = max_complete_row(&bytes);
        std::fs::write(&path, &bytes).unwrap();
        let mapping = crate::testing::map_fixture(&path);
        let table = MappedTsv::open(
            mapping,
            &tsv_entry(
                bytes.len() as u64,
                max_row_bytes,
                BTreeMap::from([("design".to_owned(), view)]),
            ),
        )
        .unwrap();
        let view = *table.views.get(&StatsView::Design).unwrap();

        for key in &keys {
            let target = ["class", key.as_str(), "", ""];
            let found = table.find_schema_row(view, target).unwrap();
            assert!(found.is_some(), "missing key {key:?}");
        }
        for index in 0..129 {
            let missing = format!("{index:04}!");
            let target = ["class", missing.as_str(), "", ""];
            let found = table.find_schema_row(view, target).unwrap();
            assert!(found.is_none(), "invented gap key {missing:?}");
        }
    }

    #[test]
    fn semantic_selectors_binary_search_each_declared_view() {
        let fixture = Fixture::build(TINY_NT);
        let expected = publish_description(&fixture);
        let store = opened_description(&fixture);
        let description = store.description().expect("tier-1 description");

        let design = description.view(&StatsView::Design).unwrap();
        assert_eq!(design.schema_rows(), 4);
        assert_eq!(design.class_relation_rows(), 3);
        assert_eq!(
            design.schema_node(SchemaSelector::Dataset).unwrap(),
            Some(SchemaNode {
                subject: TermId(expected.bob)
            })
        );
        assert_eq!(
            design
                .schema_node(SchemaSelector::Class {
                    class: "https://example.org/A"
                })
                .unwrap(),
            Some(SchemaNode {
                subject: TermId(expected.alice)
            })
        );
        assert_eq!(
            design
                .schema_node(SchemaSelector::Property {
                    class: None,
                    predicate: "https://example.org/p1"
                })
                .unwrap(),
            Some(SchemaNode {
                subject: TermId(expected.bob)
            })
        );
        assert_eq!(
            design
                .schema_node(SchemaSelector::Datatype {
                    class: None,
                    predicate: "https://example.org/p1",
                    datatype: "https://example.org/Type"
                })
                .unwrap(),
            Some(SchemaNode {
                subject: TermId(expected.alice)
            })
        );
        assert_eq!(
            design
                .schema_node(SchemaSelector::Property {
                    class: Some("https://example.org/A"),
                    predicate: "https://example.org/absent"
                })
                .unwrap(),
            None
        );
        for absent in [
            SchemaSelector::Class {
                class: "https://example.org/0-before",
            },
            SchemaSelector::Class {
                class: "https://example.org/B-between",
            },
            SchemaSelector::Property {
                class: None,
                predicate: "https://example.org/z-after",
            },
        ] {
            assert_eq!(design.schema_node(absent).unwrap(), None);
        }

        let component = description
            .view(&StatsView::component("canonical").unwrap())
            .unwrap();
        assert_eq!(
            component.schema_node(SchemaSelector::Dataset).unwrap(),
            Some(SchemaNode {
                subject: TermId(expected.bob)
            })
        );
        assert!(
            description
                .view(&StatsView::component("unknown").unwrap())
                .is_none()
        );
    }

    #[test]
    fn class_relations_page_and_resume_at_exact_row_boundaries() {
        let fixture = Fixture::build(TINY_NT);
        publish_description(&fixture);
        let store = opened_description(&fixture);
        let view = store
            .description()
            .unwrap()
            .view(&StatsView::Design)
            .unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        let many = NonZeroUsize::new(100).unwrap();

        let first = view
            .class_relations(ClassRelationFilter::default(), None, two, many)
            .unwrap();
        assert_eq!(
            first
                .items
                .iter()
                .map(|row| row.triples)
                .collect::<Vec<_>>(),
            [50, 30]
        );
        assert_eq!(first.stop, ClassRelationStop::RowLimit);
        assert_eq!(first.examined, 2);
        let next = first.next.expect("another relation remains");
        assert_eq!(view.class_relation_position(next.byte_offset()), Some(next));
        assert_eq!(view.class_relation_position(next.byte_offset() + 1), None);

        let second = view
            .class_relations(ClassRelationFilter::default(), Some(next), two, many)
            .unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|row| row.triples)
                .collect::<Vec<_>>(),
            [10]
        );
        assert_eq!(second.stop, ClassRelationStop::Complete);
        assert_eq!(second.next, None);
    }

    #[test]
    fn filtered_scans_keep_global_order_and_resume_without_loss() {
        let fixture = Fixture::build(TINY_NT);
        publish_description(&fixture);
        let store = opened_description(&fixture);
        let view = store
            .description()
            .unwrap()
            .view(&StatsView::Design)
            .unwrap();
        let many = NonZeroUsize::new(100).unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        let filter = ClassRelationFilter {
            class: None,
            predicate: Some("https://example.org/p1"),
        };

        let first = view.class_relations(filter, None, many, two).unwrap();
        assert_eq!(
            first
                .items
                .iter()
                .map(|row| row.triples)
                .collect::<Vec<_>>(),
            [50]
        );
        assert_eq!(first.examined, 2);
        assert_eq!(first.stop, ClassRelationStop::ScanLimit);

        let second = view.class_relations(filter, first.next, many, two).unwrap();
        assert_eq!(
            second
                .items
                .iter()
                .map(|row| row.triples)
                .collect::<Vec<_>>(),
            [10]
        );
        assert_eq!(second.stop, ClassRelationStop::Complete);
        assert_eq!(second.examined, 1);
    }

    #[test]
    fn an_out_of_range_selector_subject_is_refused_as_a_malformed_index() {
        let fixture = Fixture::build(TINY_NT);
        let expected = publish_description(&fixture);
        let path = fixture.bundle_path().join(artifact::SCHEMA_NODES);
        let mut bytes = std::fs::read(&path).unwrap();
        let needle = format!(
            "design\tclass\thttps://example.org/A\t\t\t{}\n",
            expected.alice
        );
        let start = bytes
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .expect("class selector row");
        let id_start = start + needle.len() - expected.alice.to_string().len() - 1;
        bytes[id_start..id_start + expected.alice.to_string().len()].fill(b'0');
        std::fs::write(&path, bytes).unwrap();

        let store = opened_description(&fixture);
        let view = store
            .description()
            .unwrap()
            .view(&StatsView::Design)
            .unwrap();
        match view
            .schema_node(SchemaSelector::Class {
                class: "https://example.org/A",
            })
            .expect_err("zero is not a VoID subject id")
        {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, path);
                assert!(detail.contains("outside the VoID subject id space"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
