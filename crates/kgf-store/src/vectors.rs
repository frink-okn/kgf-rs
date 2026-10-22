//! The `vectors/` sidecar: one vector per embedded entity, and the ANN index
//! over them.
//!
//! A build verbalizes a chosen set of subjects into texts, deduplicates the
//! texts, and embeds each distinct one. The result is a set of **records** —
//! one per distinct text, numbered `0..records` in the order the texts were
//! written — and a many-to-one relation from **entities** (subject ids) to
//! records. Two files carry that into the bundle:
//!
//! - `vectors/index.usearch`: the ANN index, with record `i` stored under key
//!   `i`. Mapped and read in place by the library that wrote it ([`MappedIndex`]).
//! - `vectors/entities.idmap`: the id map, this crate's own format, in id
//!   space: which entities have a vector and which record each one is; which
//!   entities each record names; and each record's text digest, so a text
//!   rendered later can be checked against the one that was embedded.
//!
//! The canonical vectors (`vectors/embeddings.parquet`) and the entity table
//! (`vectors/roots.parquet`) are also artifacts of the sidecar, served as bytes
//! and never read here: everything a query needs is in the two files above.
//!
//! # The id map's layout
//!
//! Little-endian throughout; every region is 8-byte aligned. After a fixed
//! header ([`HEADER_LEN`] bytes) come five regions, in this order:
//!
//! | region | entries | each | holds |
//! |---|---|---|---|
//! | ids | entities | u64 | every embedded entity's subject id, strictly increasing |
//! | rows | entities | u64 | the record of the entity at the same position in `ids` |
//! | offsets | records + 1 | u64 | where each record's entities begin in `members`; `offsets[0] = 0`, `offsets[records] = entities` |
//! | members | entities | u64 | entities grouped by record: record `r` names `members[offsets[r]..offsets[r+1]]` |
//! | digests | records | 32 bytes | the SHA-256 of record `r`'s text |
//!
//! `ids`/`rows` and `offsets`/`members` are the same relation in both
//! directions, each laid out for the lookup that reads it: entity → record is a
//! binary search over `ids`; record → entities is two offset reads and a slice.
//!
//! The header carries the identity digest of the `data.hdt` the entities
//! belong to, so an id map copied beside another HDT is refused at open rather
//! than answering with ids from the wrong dictionary.
//!
//! # What open checks, and what it leaves to verification
//!
//! Open reads the header, checks the file is exactly as long as the header's
//! counts require, reads the two sentinel offsets, and checks the index's size
//! and width against the header — a fixed number of reads however many
//! entities there are. It does not walk the regions: that an entity's record
//! is in range, that `offsets` is monotone, that the two directions agree, is
//! [`Vectors::verify`]'s pass, run by `kgf manifest --check`. The accessors
//! bound-check with `assert!` as every mapped accessor in this crate does.

use std::io::Write;
use std::path::Path;

use crate::TermId;
use crate::error::{Error, Result};
use crate::indexed::IndexedHdt;
use crate::map::{BytesSpec, MappedIndex, Mapping, PackedSpec, PublishedBundle, open_published};

/// The id map's first eight bytes.
pub const MAGIC: &[u8; 8] = b"KGFIDMAP";

/// The layout this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;

/// Bytes before the first region.
pub const HEADER_LEN: u64 = 152;

/// Bytes of a text digest.
pub const DIGEST_LEN: u64 = 32;

/// A record: one distinct embedded text, and the index key of its vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row(pub u64);

/// One result of a similarity search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// The record found.
    pub row: Row,
    /// Its distance from the query under the index's metric: for cosine,
    /// `1 - similarity`, so `0` is identical.
    pub distance: f32,
}

/// What the id map's header records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMapHeader {
    /// Vector width.
    pub dimensions: u32,
    /// Records: distinct texts, index keys `0..records`.
    pub records: u64,
    /// Entities: embedded subjects, summed over records.
    pub entities: u64,
    /// The most entities any one record names.
    pub max_entities_per_record: u64,
    /// Identity digest of the `data.hdt` whose subject ids these are.
    pub hdt_identity: [u8; 32],
    /// The vector space, as the embedding stage names it: the SHA-256 over
    /// the model, width, metric and normalization, so two bundles' vectors
    /// are comparable exactly when this agrees.
    pub convention_id: [u8; 32],
    /// SHA-256 of the `embeddings.parquet` the index and this map were built
    /// from. Not checked at open — it is a pass over the whole file — but by
    /// `kgf manifest`, like every other whole-file digest.
    pub parent_digest: [u8; 32],
}

impl IdMapHeader {
    fn parse(mapping: &Mapping) -> Result<Self> {
        let bytes = mapping.as_bytes();
        let malformed = |detail: String| Error::Malformed {
            artifact: mapping.path().to_path_buf(),
            detail,
        };
        if bytes.len() < HEADER_LEN as usize {
            return Err(malformed(format!(
                "{} bytes is shorter than the {HEADER_LEN}-byte header",
                bytes.len()
            )));
        }
        if &bytes[0..8] != MAGIC {
            return Err(malformed("not an id map: wrong magic".to_owned()));
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let digest_at =
            |at: usize| -> [u8; 32] { bytes[at..at + 32].try_into().expect("32 bytes") };
        let version = u32_at(8);
        if version != FORMAT_VERSION {
            return Err(malformed(format!(
                "id map format {version}; this build reads format {FORMAT_VERSION}"
            )));
        }
        Ok(Self {
            dimensions: u32_at(12),
            records: u64_at(16),
            entities: u64_at(24),
            max_entities_per_record: u64_at(32),
            hdt_identity: digest_at(40),
            convention_id: digest_at(72),
            parent_digest: digest_at(104),
        })
    }

    fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        out.write_all(MAGIC)?;
        out.write_all(&FORMAT_VERSION.to_le_bytes())?;
        out.write_all(&self.dimensions.to_le_bytes())?;
        out.write_all(&self.records.to_le_bytes())?;
        out.write_all(&self.entities.to_le_bytes())?;
        out.write_all(&self.max_entities_per_record.to_le_bytes())?;
        out.write_all(&self.hdt_identity)?;
        out.write_all(&self.convention_id)?;
        out.write_all(&self.parent_digest)?;
        // Reserved to the end of the header.
        out.write_all(&[0; (HEADER_LEN - 136) as usize])
    }

    /// Where each region begins, and the file's total length.
    fn layout(&self) -> Result<Layout> {
        let ids = HEADER_LEN;
        let words = |count: u64| count.checked_mul(8);
        let rows = words(self.entities).and_then(|n| ids.checked_add(n));
        let offsets = rows.and_then(|at| words(self.entities).and_then(|n| at.checked_add(n)));
        let members = offsets.and_then(|at| {
            self.records
                .checked_add(1)
                .and_then(words)
                .and_then(|n| at.checked_add(n))
        });
        let digests = members.and_then(|at| words(self.entities).and_then(|n| at.checked_add(n)));
        let end = digests.and_then(|at| {
            self.records
                .checked_mul(DIGEST_LEN)
                .and_then(|n| at.checked_add(n))
        });
        match (rows, offsets, members, digests, end) {
            (Some(rows), Some(offsets), Some(members), Some(digests), Some(end)) => Ok(Layout {
                ids,
                rows,
                offsets,
                members,
                digests,
                end,
            }),
            _ => Err(Error::Region(format!(
                "{} records and {} entities overflow the file length",
                self.records, self.entities
            ))),
        }
    }
}

/// Region offsets, derived from the header's counts.
struct Layout {
    ids: u64,
    rows: u64,
    offsets: u64,
    members: u64,
    digests: u64,
    end: u64,
}

/// The id map's regions, validated against its mapping at open.
#[derive(Debug, Clone, Copy)]
struct IdMapSpec {
    ids: PackedSpec,
    rows: PackedSpec,
    offsets: PackedSpec,
    members: PackedSpec,
    digests: BytesSpec,
}

/// The `vectors/` sidecar, open: the id map and the index together.
pub struct Vectors {
    header: IdMapHeader,
    spec: IdMapSpec,
    entities: Mapping,
    index: MappedIndex,
}

impl std::fmt::Debug for Vectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vectors")
            .field("entities", &self.entities.path())
            .field("index", &self.index)
            .field("records", &self.header.records)
            .field("entities_count", &self.header.entities)
            .finish()
    }
}

impl Vectors {
    /// Open the sidecar's two files and bind them to each other and to `hdt`.
    ///
    /// `search_contexts` is how many searches may run at once; see
    /// [`MappedIndex::view`].
    pub(crate) fn open(
        bundle: &PublishedBundle,
        entities: &Path,
        index: &Path,
        hdt: &IndexedHdt,
        search_contexts: usize,
    ) -> Result<Self> {
        let entities = open_published(bundle, entities)?;
        let header = IdMapHeader::parse(&entities)?;
        let malformed = |detail: String| Error::Malformed {
            artifact: entities.path().to_path_buf(),
            detail,
        };

        if header.hdt_identity != hdt.permutations().hdt_identity_digest() {
            return Err(Error::ArtifactBindingMismatch {
                artifact: entities.path().to_path_buf(),
                hdt: hdt.path().to_path_buf(),
                detail: "the id map records the identity digest of a different HDT".to_owned(),
            });
        }

        let layout = header.layout().map_err(|e| malformed(e.to_string()))?;
        let length = entities.as_bytes().len() as u64;
        if layout.end != length {
            return Err(malformed(format!(
                "{} records and {} entities need {} bytes; the file has {length}",
                header.records, header.entities, layout.end
            )));
        }
        let packed = |offset: u64, len: u64| {
            PackedSpec::new(&entities, offset, len, 64).map_err(|e| malformed(e.to_string()))
        };
        let spec = IdMapSpec {
            ids: packed(layout.ids, header.entities)?,
            rows: packed(layout.rows, header.entities)?,
            offsets: packed(layout.offsets, header.records + 1)?,
            members: packed(layout.members, header.entities)?,
            digests: BytesSpec::new(&entities, layout.digests, header.records * DIGEST_LEN)
                .map_err(|e| malformed(e.to_string()))?,
        };
        let offsets = spec.offsets.view(&entities);
        let (first, last) = (offsets.get(0), offsets.get(header.records));
        if first != 0 || last != header.entities {
            return Err(malformed(format!(
                "record offsets run from {first} to {last}; expected 0 to {}",
                header.entities
            )));
        }

        let index = MappedIndex::view(open_published(bundle, index)?, search_contexts)?;
        let viewed = index.index();
        if viewed.size() as u64 != header.records
            || viewed.dimensions() as u64 != u64::from(header.dimensions)
        {
            return Err(Error::Malformed {
                artifact: index.path().to_path_buf(),
                detail: format!(
                    "the index holds {} vectors of {} dimensions; the id map beside it records {} of {}",
                    viewed.size(),
                    viewed.dimensions(),
                    header.records,
                    header.dimensions
                ),
            });
        }

        Ok(Self {
            header,
            spec,
            entities,
            index,
        })
    }

    /// What the id map's header records.
    pub fn header(&self) -> &IdMapHeader {
        &self.header
    }

    /// Vector width.
    pub fn dimensions(&self) -> usize {
        self.header.dimensions as usize
    }

    /// Records: distinct embedded texts, and index keys.
    pub fn records(&self) -> u64 {
        self.header.records
    }

    /// Embedded entities, over all records.
    pub fn entities(&self) -> u64 {
        self.header.entities
    }

    /// How far a search looks: the index's stored search expansion, the
    /// bound on candidates one probe visits.
    pub fn expansion_search(&self) -> usize {
        self.index.index().expansion_search()
    }

    /// Links per node in the index's graph, the other build parameter a
    /// probe's cost depends on.
    pub fn connectivity(&self) -> usize {
        self.index.index().connectivity()
    }

    fn malformed(&self, detail: String) -> Error {
        Error::Malformed {
            artifact: self.entities.path().to_path_buf(),
            detail,
        }
    }

    fn checked_row(&self, row: Row) -> Result<Row> {
        if row.0 < self.header.records {
            Ok(row)
        } else {
            Err(self.malformed(format!(
                "record {} is past the {} records",
                row.0, self.header.records
            )))
        }
    }

    /// The record `entity`'s vector is, if it was embedded.
    ///
    /// `entity` is a subject id. One binary search over the sorted ids.
    ///
    /// The accessors here report a map whose regions disagree with its
    /// header as [`Error::Malformed`] rather than asserting: open reads the
    /// header and sentinels only, so a region can be wrong on a map that
    /// opened, and a request must not take the process down over it.
    pub fn row_of(&self, entity: TermId) -> Result<Option<Row>> {
        let ids = self.spec.ids.view(&self.entities);
        let (mut low, mut high) = (0, ids.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match ids.get(mid).cmp(&entity.0) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    let row = Row(self.spec.rows.view(&self.entities).get(mid));
                    return self.checked_row(row).map(Some);
                }
            }
        }
        Ok(None)
    }

    /// The entities whose text is `row`'s, as subject ids, in id order.
    ///
    /// Several when identical texts were deduplicated into one record; the
    /// header's `max_entities_per_record` bounds how many.
    pub fn entities_of(&self, row: Row) -> Result<impl Iterator<Item = TermId> + '_> {
        let row = self.checked_row(row)?;
        let offsets = self.spec.offsets.view(&self.entities);
        let (start, end) = (offsets.get(row.0), offsets.get(row.0 + 1));
        if start >= end || end > self.header.entities {
            return Err(self.malformed(format!(
                "record {} spans {start}..{end} of {} entities",
                row.0, self.header.entities
            )));
        }
        let members = self.spec.members.view(&self.entities);
        Ok((start..end).map(move |i| TermId(members.get(i))))
    }

    /// The SHA-256 of the text `row` was embedded from.
    pub fn digest_of(&self, row: Row) -> Result<[u8; 32]> {
        let row = self.checked_row(row)?;
        let at = (row.0 * DIGEST_LEN) as usize;
        Ok(
            self.spec.digests.view(&self.entities)[at..at + DIGEST_LEN as usize]
                .try_into()
                .expect("32 bytes"),
        )
    }

    /// `row`'s stored vector, at the index's precision widened to `f32`.
    pub fn vector(&self, row: Row) -> Result<Vec<f32>> {
        let row = self.checked_row(row)?;
        let mut vector = vec![0f32; self.dimensions()];
        let found = self
            .index
            .index()
            .get(row.0, &mut vector)
            .map_err(|e| Error::Malformed {
                artifact: self.index.path().to_path_buf(),
                detail: format!("no vector under key {}: {e}", row.0),
            })?;
        if found != 1 {
            return Err(Error::Malformed {
                artifact: self.index.path().to_path_buf(),
                detail: format!(
                    "{found} vectors under key {}; a key holds exactly one",
                    row.0
                ),
            });
        }
        Ok(vector)
    }

    /// The `k` records nearest `query`, nearest first.
    ///
    /// One probe of the index at its stored search expansion; the library
    /// fails rather than waits if more searches are running than contexts
    /// were reserved at open, and that failure is reported, not retried.
    pub fn similar(&self, query: &[f32], k: usize) -> Result<Vec<Hit>> {
        if query.len() != self.dimensions() {
            return Err(Error::QueryDimension {
                expected: self.dimensions(),
                found: query.len(),
            });
        }
        let matches = self
            .index
            .index()
            .search(query, k)
            .map_err(|e| Error::VectorSearch(e.to_string()))?;
        Ok(matches
            .keys
            .into_iter()
            .zip(matches.distances)
            .map(|(key, distance)| Hit {
                row: Row(key),
                distance,
            })
            .collect())
    }

    /// Walk every region and check the invariants open takes on trust.
    ///
    /// `subjects` is the size of the subject id space, so an id past the
    /// dictionary is caught. Linear in entities: for `kgf manifest --check`,
    /// never the open path.
    pub fn verify(&self, subjects: u64) -> Result<()> {
        let malformed = |detail: String| Error::Malformed {
            artifact: self.entities.path().to_path_buf(),
            detail,
        };
        let ids = self.spec.ids.view(&self.entities);
        let rows = self.spec.rows.view(&self.entities);
        let offsets = self.spec.offsets.view(&self.entities);
        let members = self.spec.members.view(&self.entities);

        let mut previous = 0;
        for i in 0..self.header.entities {
            let id = ids.get(i);
            if id <= previous {
                return Err(malformed(format!(
                    "entity ids are not strictly increasing at position {i}: {previous} then {id}"
                )));
            }
            if id > subjects {
                return Err(malformed(format!(
                    "entity id {id} is past the {subjects} subjects of the dictionary"
                )));
            }
            if rows.get(i) >= self.header.records {
                return Err(malformed(format!(
                    "entity {id} is in record {}, of {}",
                    rows.get(i),
                    self.header.records
                )));
            }
            previous = id;
        }

        let mut widest = 0;
        for row in 0..self.header.records {
            let (start, end) = (offsets.get(row), offsets.get(row + 1));
            if start >= end || end > self.header.entities {
                return Err(malformed(format!(
                    "record {row} spans {start}..{end}; every record names at least one entity \
                     and at most {}",
                    self.header.entities
                )));
            }
            widest = widest.max(end - start);
            let mut previous = 0;
            for i in start..end {
                let member = members.get(i);
                if member <= previous {
                    return Err(malformed(format!(
                        "record {row}'s entities are not strictly increasing: {previous} then {member}"
                    )));
                }
                let placed = self.row_of(TermId(member))?;
                if placed != Some(Row(row)) {
                    return Err(malformed(format!(
                        "record {row} names entity {member}, which the entity table puts in {placed:?}"
                    )));
                }
                previous = member;
            }
        }
        if widest != self.header.max_entities_per_record {
            return Err(malformed(format!(
                "the widest record names {widest} entities; the header says {}",
                self.header.max_entities_per_record
            )));
        }
        // Open checked the index's size; that its keys are exactly
        // `0..records` is what makes a record its key.
        let index = self.index.index();
        for key in 0..self.header.records {
            if index.count(key) != 1 {
                return Err(Error::Malformed {
                    artifact: self.index.path().to_path_buf(),
                    detail: format!(
                        "key {key} holds {} vectors; every key below {} holds one",
                        index.count(key),
                        self.header.records
                    ),
                });
            }
        }
        Ok(())
    }
}

/// One record as the id-map writer takes it: in record order, so the `n`th
/// row given is record `n`.
#[derive(Debug, Clone)]
pub struct IdMapRow {
    /// The SHA-256 of the record's text.
    pub digest: [u8; 32],
    /// The subject ids whose text it is, in any order; at least one.
    pub entities: Vec<TermId>,
}

/// What the writer was told that it cannot derive from the rows.
#[derive(Debug, Clone, Copy)]
pub struct IdMapIdentity {
    /// Vector width.
    pub dimensions: u32,
    /// Identity digest of the `data.hdt` the ids belong to.
    pub hdt_identity: [u8; 32],
    /// The vector space, from the embedding stage's footer.
    pub convention_id: [u8; 32],
    /// SHA-256 of the `embeddings.parquet` the rows came from.
    pub parent_digest: [u8; 32],
}

/// What was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMapSummary {
    /// Records written.
    pub records: u64,
    /// Entities over all records.
    pub entities: u64,
    /// The most entities one record names.
    pub max_entities_per_record: u64,
}

/// Write an id map for `rows`, record `n` being the `n`th row.
///
/// Refuses a row with no entities, an id of zero, and an entity named by two
/// rows: each is a map that would answer wrongly rather than a map with less
/// in it. Holds every (entity, record) pair in memory to sort them — sixteen
/// bytes an entity — which is the one thing here that grows with the output.
pub fn write_id_map(
    out: &mut impl Write,
    identity: IdMapIdentity,
    rows: impl IntoIterator<Item = IdMapRow>,
) -> Result<IdMapSummary> {
    let mut by_id: Vec<(u64, u64)> = Vec::new();
    let mut offsets: Vec<u64> = vec![0];
    let mut members: Vec<u64> = Vec::new();
    let mut digests: Vec<u8> = Vec::new();
    let mut widest = 0u64;
    for (row, record) in rows.into_iter().enumerate() {
        let row = row as u64;
        let mut entities: Vec<u64> = record.entities.iter().map(|id| id.0).collect();
        if entities.is_empty() {
            return Err(Error::IdMapInput(format!("record {row} names no entity")));
        }
        if entities.contains(&0) {
            return Err(Error::IdMapInput(format!("record {row} names entity id 0")));
        }
        entities.sort_unstable();
        entities.dedup();
        widest = widest.max(entities.len() as u64);
        by_id.extend(entities.iter().map(|&id| (id, row)));
        members.extend_from_slice(&entities);
        offsets.push(members.len() as u64);
        digests.extend_from_slice(&record.digest);
    }
    by_id.sort_unstable();
    if let Some(pair) = by_id.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(Error::IdMapInput(format!(
            "entity {} is named by records {} and {}",
            pair[0].0, pair[0].1, pair[1].1
        )));
    }

    let header = IdMapHeader {
        dimensions: identity.dimensions,
        records: offsets.len() as u64 - 1,
        entities: by_id.len() as u64,
        max_entities_per_record: widest,
        hdt_identity: identity.hdt_identity,
        convention_id: identity.convention_id,
        parent_digest: identity.parent_digest,
    };
    header.write(out)?;
    for (id, _) in &by_id {
        out.write_all(&id.to_le_bytes())?;
    }
    for (_, row) in &by_id {
        out.write_all(&row.to_le_bytes())?;
    }
    for offset in &offsets {
        out.write_all(&offset.to_le_bytes())?;
    }
    for member in &members {
        out.write_all(&member.to_le_bytes())?;
    }
    out.write_all(&digests)?;
    Ok(IdMapSummary {
        records: header.records,
        entities: header.entities,
        max_entities_per_record: header.max_entities_per_record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BundleFacts, Capability};
    use crate::store::{OpenOptions, Store, artifact};
    use crate::testing::{Fixture, fixture_digest, fixture_vector, published_bundle};
    use crate::{Role, TermId};

    const SOURCE: &str = "\
<http://example.org/a> <http://example.org/type> <http://example.org/T> .
<http://example.org/b> <http://example.org/type> <http://example.org/T> .
<http://example.org/c> <http://example.org/type> <http://example.org/T> .
<http://example.org/d> <http://example.org/type> <http://example.org/T> .
<http://example.org/e> <http://example.org/type> <http://example.org/T> .
<http://example.org/f> <http://example.org/label> \"not embedded\" .
<http://example.org/T> <http://example.org/label> \"a class\" .
";

    /// Four records over five of the six subjects: two texts shared.
    const GROUPS: &[&[&str]] = &[
        &["http://example.org/a"],
        &["http://example.org/b", "http://example.org/c"],
        &["http://example.org/d", "http://example.org/e"],
        &["http://example.org/T"],
    ];

    fn open(fixture: &Fixture) -> Store {
        Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default().with_search_contexts(4),
        )
        .expect("open")
    }

    fn subject(store: &Store, iri: &str) -> TermId {
        store
            .dict()
            .locate(Role::Subject, iri.as_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("{iri}"))
    }

    #[test]
    fn every_entity_round_trips_and_every_row_maps_back() {
        let fixture = Fixture::build(SOURCE).with_vectors(GROUPS);
        let store = open(&fixture);
        let vectors = store.vectors().expect("sidecar");

        assert_eq!((vectors.records(), vectors.entities()), (4, 6));
        assert_eq!(vectors.header().max_entities_per_record, 2);
        for (row, group) in GROUPS.iter().enumerate() {
            let mut ids: Vec<TermId> = group.iter().map(|iri| subject(&store, iri)).collect();
            ids.sort();
            for &id in &ids {
                assert_eq!(vectors.row_of(id).unwrap(), Some(Row(row as u64)), "{id:?}");
            }
            let members: Vec<TermId> = vectors.entities_of(Row(row as u64)).unwrap().collect();
            assert_eq!(members, ids, "record {row}");
            assert_eq!(
                vectors.digest_of(Row(row as u64)).unwrap(),
                fixture_digest(row as u64)
            );
        }
        assert_eq!(
            vectors
                .row_of(subject(&store, "http://example.org/f"))
                .unwrap(),
            None
        );
        assert_eq!(vectors.row_of(TermId(u64::MAX)).unwrap(), None);
        assert!(vectors.entities_of(Row(4)).is_err());
        assert!(vectors.digest_of(Row(4)).is_err());
        assert!(vectors.vector(Row(4)).is_err());
        vectors
            .verify(store.dict().counts().len(Role::Subject))
            .expect("a fixture verifies");
    }

    #[test]
    fn a_record_is_nearest_its_own_vector() {
        let fixture = Fixture::build(SOURCE).with_vectors(GROUPS);
        let store = open(&fixture);
        let vectors = store.vectors().expect("sidecar");

        for row in 0..4 {
            let stored = vectors.vector(Row(row)).unwrap();
            for (got, want) in stored.iter().zip(fixture_vector(row)) {
                assert!((got - want).abs() < 1e-3, "{got} vs {want}");
            }
            let hits = vectors.similar(&stored, 3).unwrap();
            assert_eq!(hits.len(), 3);
            assert_eq!(hits[0].row, Row(row));
            assert!(hits[0].distance.abs() < 1e-3);
            assert!(hits.windows(2).all(|w| w[0].distance <= w[1].distance));
        }
        let error = vectors.similar(&[0.5; 3], 1).unwrap_err();
        assert!(
            matches!(
                error,
                Error::QueryDimension {
                    expected: 8,
                    found: 3
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn the_sidecar_declares_its_capability_and_its_files() {
        let fixture = Fixture::build(SOURCE).with_vectors(GROUPS);

        let facts = BundleFacts::read(&published_bundle(fixture.bundle_path())).unwrap();

        let capabilities: Vec<Capability> = facts.capabilities().collect();
        assert!(
            capabilities.contains(&Capability::Vectors),
            "{capabilities:?}"
        );
        let names: Vec<&str> = facts.artifact_names().collect();
        assert!(names.contains(&artifact::VECTORS_INDEX), "{names:?}");
        assert!(names.contains(&artifact::VECTORS_ENTITIES), "{names:?}");
        assert!(!names.contains(&artifact::VECTORS_EMBEDDINGS), "{names:?}");
        let plain = Fixture::build(SOURCE);
        let facts = BundleFacts::read(&published_bundle(plain.bundle_path())).unwrap();
        let capabilities: Vec<Capability> = facts.capabilities().collect();
        assert!(
            !capabilities.contains(&Capability::Vectors),
            "{capabilities:?}"
        );
    }

    #[test]
    fn an_index_without_its_id_map_is_refused_naming_the_id_map() {
        let fixture = Fixture::build(SOURCE).with_vectors(GROUPS);
        std::fs::remove_file(fixture.bundle_path().join(artifact::VECTORS_ENTITIES)).unwrap();

        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .expect_err("refused");

        assert!(
            matches!(&error, Error::MissingRequiredArtifact { artifact, .. } if artifact == artifact::VECTORS_ENTITIES),
            "{error}"
        );
    }

    #[test]
    fn vectors_without_their_index_are_refused() {
        let fixture = Fixture::build(SOURCE);
        let dir = fixture.bundle_path().join("vectors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("embeddings.parquet"), b"PAR1").unwrap();

        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .expect_err("refused");

        assert!(matches!(error, Error::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_sidecar_of_another_hdt_is_refused_at_open() {
        let embedded = Fixture::build(SOURCE).with_vectors(GROUPS);
        let other = Fixture::build(
            "<http://example.org/x> <http://example.org/y> <http://example.org/z> .\n",
        );
        let dir = other.bundle_path().join("vectors");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [artifact::VECTORS_INDEX, artifact::VECTORS_ENTITIES] {
            std::fs::copy(
                embedded.bundle_path().join(name),
                other.bundle_path().join(name),
            )
            .unwrap();
        }

        let error = Store::open(
            &published_bundle(other.bundle_path()),
            OpenOptions::default(),
        )
        .expect_err("refused");

        assert!(
            matches!(error, Error::ArtifactBindingMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn an_id_map_disagreeing_with_its_index_is_refused_naming_both_counts() {
        let fixture = Fixture::build(SOURCE).with_vectors(GROUPS);
        let (identity, rows) = {
            let store = open(&fixture);
            let identity = IdMapIdentity {
                dimensions: 8,
                hdt_identity: store.hdt_identity_digest(),
                convention_id: [0; 32],
                parent_digest: [0; 32],
            };
            // Three records against the index's four.
            let rows: Vec<IdMapRow> = GROUPS[..3]
                .iter()
                .enumerate()
                .map(|(n, group)| IdMapRow {
                    digest: fixture_digest(n as u64),
                    entities: group.iter().map(|iri| subject(&store, iri)).collect(),
                })
                .collect();
            (identity, rows)
        };
        let mut bytes = Vec::new();
        write_id_map(&mut bytes, identity, rows).unwrap();
        std::fs::write(
            fixture.bundle_path().join(artifact::VECTORS_ENTITIES),
            bytes,
        )
        .unwrap();

        let error = Store::open(
            &published_bundle(fixture.bundle_path()),
            OpenOptions::default(),
        )
        .expect_err("refused");

        let message = error.to_string();
        assert!(
            message.contains("4 vectors") && message.contains("records 3"),
            "{message}"
        );
    }

    #[test]
    fn verify_walks_what_open_does_not() {
        let fixture = Fixture::build(SOURCE).with_vectors(GROUPS);
        let path = fixture.bundle_path().join(artifact::VECTORS_ENTITIES);
        let good = std::fs::read(&path).unwrap();
        let subjects = {
            let store = open(&fixture);
            store.dict().counts().len(Role::Subject)
        };
        let ids_at = HEADER_LEN as usize;
        let rows_at = ids_at + 48;
        let members_at = rows_at + 48 + 40;

        type Corruption<'a> = Box<dyn Fn(&mut Vec<u8>) + 'a>;
        let corruptions: [(&str, Corruption<'_>); 4] = [
            (
                "entity ids are not strictly increasing",
                Box::new(|b| {
                    let first = b[ids_at..ids_at + 8].to_vec();
                    b[ids_at + 8..ids_at + 16].copy_from_slice(&first);
                }),
            ),
            (
                "is past the",
                Box::new(|b| {
                    b[ids_at + 40..ids_at + 48].copy_from_slice(&(subjects + 1).to_le_bytes())
                }),
            ),
            (
                "is in record",
                Box::new(|b| b[rows_at..rows_at + 8].copy_from_slice(&99u64.to_le_bytes())),
            ),
            (
                "the entity table puts in",
                Box::new(|b| {
                    let (x, y) = (
                        b[members_at..members_at + 8].to_vec(),
                        b[members_at + 8..members_at + 16].to_vec(),
                    );
                    b[members_at..members_at + 8].copy_from_slice(&y);
                    b[members_at + 8..members_at + 16].copy_from_slice(&x);
                }),
            ),
        ];
        for (expected, corrupt) in corruptions {
            let mut bytes = good.clone();
            corrupt(&mut bytes);
            std::fs::write(&path, &bytes).unwrap();
            let store = open(&fixture);
            let error = store.vectors().unwrap().verify(subjects).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "wanted {expected:?}: {error}"
            );
        }
    }

    #[test]
    fn the_writer_refuses_what_no_map_can_hold() {
        let identity = IdMapIdentity {
            dimensions: 8,
            hdt_identity: [0; 32],
            convention_id: [0; 32],
            parent_digest: [0; 32],
        };
        let row = |ids: &[u64]| IdMapRow {
            digest: [0; 32],
            entities: ids.iter().map(|&i| TermId(i)).collect(),
        };
        let write = |rows: Vec<IdMapRow>| write_id_map(&mut Vec::new(), identity, rows);

        assert!(
            write(vec![row(&[1]), row(&[])])
                .unwrap_err()
                .to_string()
                .contains("record 1 names no entity")
        );
        assert!(
            write(vec![row(&[0])])
                .unwrap_err()
                .to_string()
                .contains("entity id 0")
        );
        assert!(
            write(vec![row(&[1, 2]), row(&[2])])
                .unwrap_err()
                .to_string()
                .contains("entity 2 is named by records 0 and 1")
        );
        let summary = write(vec![row(&[3, 1, 1]), row(&[2])]).unwrap();
        assert_eq!(
            summary,
            IdMapSummary {
                records: 2,
                entities: 3,
                max_entities_per_record: 2
            }
        );
    }

    #[test]
    fn rebuilding_an_index_from_its_own_stored_vectors_is_bit_identical() {
        // What a vector is after storage is the f16 the index keeps; feeding
        // that back into a fresh index must store exactly the same f16, so
        // the index's precision is a fixed point and the metric does not
        // renormalize on insert.
        let build = |vectors: &[Vec<f32>]| {
            let index = usearch::Index::new(&usearch::IndexOptions {
                dimensions: 8,
                metric: usearch::MetricKind::Cos,
                quantization: usearch::ScalarKind::F16,
                ..Default::default()
            })
            .unwrap();
            index.reserve(vectors.len()).unwrap();
            for (key, vector) in vectors.iter().enumerate() {
                index.add(key as u64, vector).unwrap();
            }
            index
        };
        let stored = |index: &usearch::Index, key: u64| {
            let mut out = vec![0f32; 8];
            index.get(key, &mut out).unwrap();
            out
        };
        let originals: Vec<Vec<f32>> = (0..64).map(fixture_vector).collect();
        let first = build(&originals);
        let once: Vec<Vec<f32>> = (0..64).map(|k| stored(&first, k)).collect();
        let second = build(&once);
        for key in 0..64 {
            let (a, b) = (stored(&first, key), stored(&second, key));
            assert_eq!(
                a.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                b.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                "key {key}"
            );
        }
    }
}
