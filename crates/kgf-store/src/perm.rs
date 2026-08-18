//! The permutation sidecar, `data.hdt.perm`.
//!
//! Twenty core sections: POS and OPS as implicit-level-1 `BitmapTriples`, plus
//! rank directories for the host HDT's own SPO bitmaps. Both permutations'
//! `ArrayZ` payloads are subject ids, since both orderings end in S.
//!
//! # What this module does and does not do
//!
//! It **reads the section directory through `hdtc::format`** and then maps the
//! regions itself. hdtc's [`PermutationIndex`]
//! owns the header parse, the version and flag checks, and the binding to the
//! HDT; duplicating that here is the drift risk docs 17–18 already record. Its
//! `triples()` path is a seek-based reader for hdtc's CLI and is not used.
//!
//! Section payloads are bare packed regions at 64-byte-aligned absolute
//! offsets with no preamble, and the format guarantees a full `u64` may be
//! loaded anywhere inside one (`permutation-index-format.md` §2.1). Locating
//! them is therefore a directory read rather than a preamble walk — the
//! difference from [`crate::hdt`]. Element access is the same
//! [`PackedArray`](crate::map::PackedArray) either way.

use std::path::Path;

use hdtc::format::{
    PermutationComponent, PermutationIndex, PermutationIndexOpenError, PermutationSection,
    PermutationSectionKind,
};

use crate::Role;
use crate::dict::Dictionary;
use crate::error::{Error, Result};
use crate::hdt::{BitmapTriples, HdtLayout, TriplesLayout};
use crate::map::{BitmapSpec, Mapping, PackedSpec};
use crate::rank::RankedSpec;

/// Specs for one implicit-level-1 `BitmapTriples` ordering.
#[derive(Debug, Clone, Copy)]
struct PermutationSpec {
    layout: TriplesLayout,
    bitmap_y: RankedSpec,
    bitmap_z: RankedSpec,
}

impl PermutationSpec {
    fn sidecar(
        index: &PermutationIndex,
        mapping: &Mapping,
        component: PermutationComponent,
    ) -> Result<Self> {
        let array_y = section(index, component, PermutationSectionKind::ArrayY)?;
        let bitmap_y = section(index, component, PermutationSectionKind::BitmapY)?;
        let array_z = section(index, component, PermutationSectionKind::ArrayZ)?;
        let bitmap_z = section(index, component, PermutationSectionKind::BitmapZ)?;

        let layout = with_artifact(
            mapping,
            TriplesLayout::new(
                packed(mapping, array_y)?,
                bitmap(mapping, bitmap_y)?,
                packed(mapping, array_z)?,
                bitmap(mapping, bitmap_z)?,
            ),
        )?;
        let ranked_y = ranked(
            *layout.bitmap_y(),
            index,
            mapping,
            component,
            PermutationSectionKind::BitmapYSuperrank,
            PermutationSectionKind::BitmapYSubrank,
        )?;
        let ranked_z = ranked(
            *layout.bitmap_z(),
            index,
            mapping,
            component,
            PermutationSectionKind::BitmapZSuperrank,
            PermutationSectionKind::BitmapZSubrank,
        )?;

        Ok(Self {
            layout,
            bitmap_y: ranked_y,
            bitmap_z: ranked_z,
        })
    }

    fn spo(index: &PermutationIndex, sidecar: &Mapping, layout: TriplesLayout) -> Result<Self> {
        let bitmap_y = ranked(
            *layout.bitmap_y(),
            index,
            sidecar,
            PermutationComponent::Spo,
            PermutationSectionKind::BitmapYSuperrank,
            PermutationSectionKind::BitmapYSubrank,
        )?;
        let bitmap_z = ranked(
            *layout.bitmap_z(),
            index,
            sidecar,
            PermutationComponent::Spo,
            PermutationSectionKind::BitmapZSuperrank,
            PermutationSectionKind::BitmapZSubrank,
        )?;
        Ok(Self {
            layout,
            bitmap_y,
            bitmap_z,
        })
    }

    fn view<'a>(&self, data: &'a Mapping, directory: &'a Mapping) -> BitmapTriples<'a> {
        BitmapTriples::new(
            self.layout.array_y().view(data),
            self.bitmap_y.view(data, directory),
            self.layout.array_z().view(data),
            self.bitmap_z.view(data, directory),
        )
    }
}

/// The mapped HDT and permutation sidecar, plus specs validated against them.
///
/// This owns both mappings because SPO's arrays and bitmaps live in the HDT
/// while its rank directories live in the sidecar. Keeping the coupled files
/// together also makes projecting a spec onto the wrong bundle impossible for
/// callers.
#[derive(Debug)]
pub struct Permutations {
    hdt: Mapping,
    sidecar: Mapping,
    hdt_identity_digest: [u8; 32],
    hdt_layout: HdtLayout,
    triples: u64,
    pos: PermutationSpec,
    ops: PermutationSpec,
    spo: PermutationSpec,
}

impl Permutations {
    /// Bind a mapped HDT to its mapped canonical permutation sidecar.
    ///
    /// The header and directory are parsed by hdtc, which also verifies the
    /// binding to the HDT (suffix length, triple count, dictionary counts).
    /// Full CRC verification is off the open path by design — it belongs to
    /// publish and to `kgf verify` (doc 20 §20.6).
    ///
    /// The caller must have created both mappings under the immutable-file
    /// obligation documented by [`Mapping::open`](crate::map::Mapping::open).
    pub fn open(hdt: Mapping, sidecar: Mapping) -> Result<Self> {
        let hdt_layout = HdtLayout::parse(&hdt)?;
        let index =
            PermutationIndex::open(sidecar.path(), hdt.path()).map_err(|error| match error {
                PermutationIndexOpenError::Binding { source } => Error::ArtifactBindingMismatch {
                    artifact: sidecar.path().to_path_buf(),
                    hdt: hdt.path().to_path_buf(),
                    detail: format!("{source:#}"),
                },
                PermutationIndexOpenError::Sidecar { source } => Error::Format(source.context(
                    format!("opening permutation index {}", sidecar.path().display()),
                )),
                PermutationIndexOpenError::Source { source } => Error::Format(
                    source.context(format!("validating source HDT {}", hdt.path().display())),
                ),
            })?;
        let pos = PermutationSpec::sidecar(&index, &sidecar, PermutationComponent::Pos)?;
        let ops = PermutationSpec::sidecar(&index, &sidecar, PermutationComponent::Ops)?;
        let spo = PermutationSpec::spo(&index, &sidecar, *hdt_layout.spo())?;
        let triples = index.header().triples;
        let hdt_identity_digest = index.header().source_digest;

        let permutations = Self {
            hdt,
            sidecar,
            hdt_identity_digest,
            hdt_layout,
            triples,
            pos,
            ops,
            spo,
        };
        permutations.check_level1_key_spaces()?;
        Ok(permutations)
    }

    /// Check that each permutation closes exactly as many level-1 groups as the
    /// dictionary has terms in that role.
    ///
    /// Level 1 is implicit, so this is the one relationship between a
    /// permutation's *payload* and the dictionary beside it, and it is what
    /// makes [`crate::pattern::resolve`]'s id validation — which bounds ids by
    /// the dictionary — sufficient to keep `level2_range` in range.
    ///
    /// Three `u64` loads: every rank directory already stores its population
    /// count as its sentinel entry. This is bounded, size-independent open-time
    /// I/O, rather than a payload scan; it prevents a malformed bundle from
    /// moving its failure into a request.
    /// Payload CRCs are deliberately off the open path (doc 20 §20.6), so
    /// without this check a bundle whose directory disagrees with the
    /// dictionary opens cleanly and then panics inside a request.
    fn check_level1_key_spaces(&self) -> Result<()> {
        let counts = self.hdt_layout.dictionary().counts();
        for (role, keys, artifact) in [
            (Role::Subject, self.spo().level1_count(), self.hdt.path()),
            (
                Role::Predicate,
                self.pos().level1_count(),
                self.sidecar.path(),
            ),
            (Role::Object, self.ops().level1_count(), self.sidecar.path()),
        ] {
            let terms = counts.len(role);
            if keys != terms {
                return Err(Error::Malformed {
                    artifact: artifact.to_path_buf(),
                    detail: format!(
                        "level-1 bitmap closes {keys} groups, but the dictionary has \
                         {terms} terms in the {role:?} id space"
                    ),
                });
            }
        }
        Ok(())
    }

    /// The POS permutation: predicate → objects → subjects.
    pub fn pos(&self) -> BitmapTriples<'_> {
        self.pos.view(&self.sidecar, &self.sidecar)
    }

    /// The OPS permutation: object → predicates → subjects.
    ///
    /// OPS rather than OSP because the hot object-rooted operation is
    /// *predicate-filtered* resolution — `(?, p ∈ roles, o)` for `/search` hit
    /// resolution, the `/labels` cascade, `ranges/` row recovery, and reverse
    /// star hydration. OSP degrades that to a scan of all subjects of the
    /// object, unbounded on exactly the hub literals where bounds matter most
    /// (doc 20 §20.2).
    pub fn ops(&self) -> BitmapTriples<'_> {
        self.ops.view(&self.sidecar, &self.sidecar)
    }

    /// The SPO permutation from the host HDT, using sidecar rank directories.
    pub fn spo(&self) -> BitmapTriples<'_> {
        self.spo.view(&self.hdt, &self.sidecar)
    }

    /// Total triples in every permutation.
    pub fn triples(&self) -> u64 {
        self.triples
    }

    /// SHA-256 identity of the host HDT's dictionary and triples.
    ///
    /// This is the source digest recorded by the required permutation
    /// sidecar. Query opening validates its cheap structural binding to the
    /// HDT; publication verification establishes the full cryptographic
    /// binding. It deliberately excludes the mutable HDT header; see
    /// `hdtc/docs/permutation-index-format.md` §9.
    pub fn hdt_identity_digest(&self) -> [u8; 32] {
        self.hdt_identity_digest
    }

    /// Path of the mapped host HDT.
    pub(crate) fn hdt_path(&self) -> &Path {
        self.hdt.path()
    }

    /// The dictionary projected from the host HDT.
    ///
    /// The layout and mapping remain encapsulated here so a caller cannot
    /// accidentally project one bundle's dictionary spec onto another
    /// bundle's bytes.
    pub fn dict(&self) -> Dictionary<'_> {
        self.hdt_layout.dictionary().view(&self.hdt)
    }

    /// The dictionary's per-role term counts, from the four PFC preambles.
    ///
    /// Public because a manifest records these (doc 04 §4.3) and
    /// [`crate::manifest::BundleFacts`] must reach them without a `Store`, which
    /// would require the manifest it is being used to write.
    pub fn dict_counts(&self) -> &crate::dict::DictCounts {
        self.hdt_layout.dictionary().counts()
    }
}

fn section(
    index: &PermutationIndex,
    component: PermutationComponent,
    kind: PermutationSectionKind,
) -> Result<&PermutationSection> {
    let section_type = component.section_type(kind);
    index
        .sections()
        .binary_search_by_key(&section_type, |candidate| candidate.section_type)
        .map(|position| &index.sections()[position])
        .map_err(|_| Error::Malformed {
            artifact: index.path().to_path_buf(),
            detail: format!("missing permutation-index section {section_type:#06x}"),
        })
}

fn packed(mapping: &Mapping, section: &PermutationSection) -> Result<PackedSpec> {
    with_artifact(
        mapping,
        PackedSpec::new(
            mapping,
            section.offset,
            section.entry_count,
            section.bits_per_entry,
        ),
    )
}

fn bitmap(mapping: &Mapping, section: &PermutationSection) -> Result<BitmapSpec> {
    with_artifact(
        mapping,
        BitmapSpec::new(mapping, section.offset, section.entry_count),
    )
}

fn ranked(
    bitmap: BitmapSpec,
    index: &PermutationIndex,
    directory: &Mapping,
    component: PermutationComponent,
    superrank_kind: PermutationSectionKind,
    subrank_kind: PermutationSectionKind,
) -> Result<RankedSpec> {
    let superrank = section(index, component, superrank_kind)?;
    let subrank = section(index, component, subrank_kind)?;
    let header = index.header();
    with_artifact(
        directory,
        RankedSpec::new(
            bitmap,
            packed(directory, superrank)?,
            packed(directory, subrank)?,
            header.superblock_bits,
            header.subblock_bits,
        ),
    )
}

fn with_artifact<T>(mapping: &Mapping, result: Result<T>) -> Result<T> {
    result.map_err(|error| Error::Malformed {
        artifact: mapping.path().to_path_buf(),
        detail: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Role;
    use crate::testing::{Fixture, TINY_NT, map_fixture};

    #[test]
    fn every_permutation_projects_with_the_shapes_and_id_spaces_hdtc_declared() {
        let fixture = Fixture::build(TINY_NT);
        let index = PermutationIndex::open(&fixture.perm_path(), &fixture.hdt_path()).unwrap();
        let header = index.header().clone();
        let permutations =
            Permutations::open(fixture.map_hdt(), fixture.map_perm()).expect("bind permutations");

        assert_eq!(permutations.triples(), header.triples);
        assert_eq!(permutations.hdt_identity_digest(), header.source_digest);
        assert_eq!(permutations.pos.layout.pairs(), header.pos_pairs);
        assert_eq!(permutations.ops.layout.pairs(), header.ops_pairs);
        assert_eq!(
            permutations.spo.layout.pairs(),
            permutations.hdt_layout.spo().pairs()
        );
        for spec in [&permutations.pos, &permutations.ops, &permutations.spo] {
            assert_eq!(spec.layout.triples(), header.triples);
        }

        assert_permutation(
            &permutations.pos,
            &permutations.sidecar,
            &permutations.sidecar,
            header.predicates,
            header.objects,
            header.subjects,
        );
        assert_permutation(
            &permutations.ops,
            &permutations.sidecar,
            &permutations.sidecar,
            header.objects,
            header.predicates,
            header.subjects,
        );
        assert_permutation(
            &permutations.spo,
            &permutations.hdt,
            &permutations.sidecar,
            header.subjects,
            header.predicates,
            header.objects,
        );

        // Public projection assembles the same shared traversal type for all
        // three sources without invoking hdtc's seek-based triples reader.
        let _ = permutations.pos();
        let _ = permutations.ops();
        let _ = permutations.spo();
    }

    fn assert_permutation(
        spec: &PermutationSpec,
        data: &Mapping,
        directory: &Mapping,
        level1_count: u64,
        level2_max: u64,
        level3_max: u64,
    ) {
        let array_y = spec.layout.array_y().view(data);
        let array_z = spec.layout.array_z().view(data);
        for position in 0..array_y.len() {
            assert!((1..=level2_max).contains(&array_y.get(position)));
        }
        for position in 0..array_z.len() {
            assert!((1..=level3_max).contains(&array_z.get(position)));
        }

        let bitmap_y = spec.bitmap_y.view(data, directory);
        let bitmap_z = spec.bitmap_z.view(data, directory);
        assert_eq!(bitmap_y.count(), level1_count);
        assert_eq!(bitmap_z.count(), spec.layout.pairs());
        assert_eq!(bitmap_y.len(), spec.layout.pairs());
        assert_eq!(bitmap_z.len(), spec.layout.triples());
        assert!(bitmap_y.bitmap().get(bitmap_y.len() - 1));
        assert!(bitmap_z.bitmap().get(bitmap_z.len() - 1));
    }

    #[test]
    fn a_sidecar_for_a_different_hdt_is_refused_before_any_view_is_built() {
        let first = Fixture::build(TINY_NT);
        let second = Fixture::build(concat!(
            "<http://example.org/s> <http://example.org/p> <http://example.org/o> .\n",
            "<http://example.org/s> <http://example.org/q> \"value\" .\n",
        ));

        let error = Permutations::open(second.map_hdt(), first.map_perm())
            .expect_err("foreign sidecar must be refused");
        match error {
            Error::ArtifactBindingMismatch {
                artifact,
                hdt,
                detail,
            } => {
                assert_eq!(artifact, first.perm_path());
                assert_eq!(hdt, second.hdt_path());
                assert!(detail.contains("permutation/HDT"), "{detail}");
            }
            other => panic!("expected a binding mismatch, got {other:#}"),
        }
    }

    #[test]
    fn a_truncated_sidecar_is_refused_by_name() {
        let fixture = Fixture::build(TINY_NT);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.hdt.perm");
        let bytes = std::fs::read(fixture.perm_path()).unwrap();
        std::fs::write(&path, &bytes[..300]).unwrap();

        let error = Permutations::open(fixture.map_hdt(), map_fixture(&path))
            .expect_err("truncated sidecar must be refused");
        assert!(error.to_string().contains("data.hdt.perm"), "{error:#}");
        assert!(matches!(error, Error::Format(_)));
    }

    #[test]
    fn a_directory_that_disagrees_with_the_dictionary_is_refused_at_open() {
        // Payload CRCs are off the open path (doc 20 §20.6), so a directory
        // that no longer describes its bitmap opens cleanly unless something
        // cheap catches it. Overwriting POS's superrank sentinel — the entry
        // `count()` reads — is exactly that case: without the check the bundle
        // opens and `resolve(? p ?)` panics inside `level2_range` on the last
        // predicate id.
        let fixture = Fixture::build(TINY_NT);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data.hdt.perm");
        let mut bytes = std::fs::read(fixture.perm_path()).unwrap();

        let index = PermutationIndex::open(&fixture.perm_path(), &fixture.hdt_path()).unwrap();
        let sentinel = {
            let section_type =
                PermutationComponent::Pos.section_type(PermutationSectionKind::BitmapYSuperrank);
            let section = index
                .sections()
                .iter()
                .find(|section| section.section_type == section_type)
                .expect("POS superrank section");
            (section.offset + (section.entry_count - 1) * 8) as usize
        };
        let predicates = index.header().predicates;
        bytes[sentinel..sentinel + 8].copy_from_slice(&(predicates + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let error = Permutations::open(fixture.map_hdt(), map_fixture(&path))
            .expect_err("a directory that miscounts its keys must be refused");
        match error {
            Error::Malformed { artifact, detail } => {
                assert_eq!(artifact, path);
                assert!(detail.contains("Predicate id space"), "{detail}");
                assert!(
                    detail.contains(&format!("closes {} groups", predicates + 1)),
                    "{detail}"
                );
            }
            other => panic!("expected a malformed-artifact error, got {other:#}"),
        }
    }

    #[test]
    fn dictionary_and_permutation_counts_stay_bound_to_one_hdt() {
        let fixture = Fixture::build(TINY_NT);
        let permutations = Permutations::open(fixture.map_hdt(), fixture.map_perm()).unwrap();
        let counts = permutations.hdt_layout.dictionary().counts();

        assert_eq!(
            counts.len(Role::Subject),
            permutations
                .spo
                .bitmap_y
                .view(&permutations.hdt, &permutations.sidecar)
                .count()
        );
        assert_eq!(
            counts.len(Role::Predicate),
            permutations
                .pos
                .bitmap_y
                .view(&permutations.sidecar, &permutations.sidecar)
                .count()
        );
        assert_eq!(
            counts.len(Role::Object),
            permutations
                .ops
                .bitmap_y
                .view(&permutations.sidecar, &permutations.sidecar)
                .count()
        );
    }
}
