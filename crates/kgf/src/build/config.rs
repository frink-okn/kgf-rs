//! The on-disk shape of `build.yaml`.
//!
//! This module models the file, not the build. Everything here is optional,
//! permissive, and shaped for a human or a template to write; the precise,
//! fully-defaulted, validated form is [`super::plan`], and nothing outside this
//! module should read a `Config` field directly. That split is the point: a
//! rendered config is the boundary where a registry's YAML becomes this
//! program's problem, so it is where strings become types.
//!
//! Unknown fields are rejected. A build config is machine-rendered from a
//! registry entry, and the failure mode that
//! matters is a key silently doing nothing across forty knowledge graphs.
//! `schema` exists so that a config written for a later `kgf` fails with that
//! sentence rather than with a list of unrecognised keys.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The only config schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// A parsed `build.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Config schema version. Must equal [`SCHEMA_VERSION`].
    pub schema: u32,

    /// Who the dataset is.
    pub dataset: Dataset,

    /// How to read the dataset: prefixes, predicate roles, namespaces.
    #[serde(default)]
    pub semantics: Semantics,

    /// What the bundle carries.
    #[serde(default)]
    pub contents: Contents,

    /// Limits for the external builders.
    #[serde(default)]
    pub resources: Resources,

    /// Derived-triple components. Recognized, not yet supported.
    ///
    /// Named rather than left to `deny_unknown_fields` so that a config written
    /// against the planned component DAG fails with an explanation instead of
    /// "unknown field `components`". Claiming the key now stays additive: when
    /// the DAG lands, the refusal becomes an implementation and no config that
    /// works today breaks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<serde_norway::Value>,

    /// Which components merge into `data.hdt`. As above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<serde_norway::Value>,
}

/// Identity and description.
///
/// `id` and `iri` are identity and cannot be defaulted. The rest is description
/// that a bundle is better for carrying and still valid without. It lives in
/// the manifest so a bundle copied away from its host still says what it is.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    /// Host-local slug: the directory name and the URL path component.
    pub id: String,
    /// Globally stable dataset identity, used by registries and citations.
    pub iri: String,
    /// Short human title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Longer human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// License identifier or URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Project homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Who publishes the dataset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Publisher identity, from the registry entry's contacts.
pub struct Publisher {
    /// Display name.
    pub name: String,
    /// Contact address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
}

/// An interpretation of the data, frozen into this version's manifest.
///
/// None of it changes a byte of any artifact, but all of it changes what an
/// answer means, so
/// it is versioned with the data rather than overlaid at serve time.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Semantics {
    /// Prefix bindings for CURIE syntax in parameters. Layered
    /// last over `contents.stats.prefix_tables`, so a per-KG binding wins over
    /// the shared table.
    #[serde(default)]
    pub prefixes: BTreeMap<String, String>,

    /// Predicate roles, strongest first, as full IRIs.
    ///
    /// Omitted entirely, the manifest's standard profile applies. Present, it
    /// replaces that profile rather than extending it: a KG whose labels are on
    /// a bespoke predicate usually needs the generic ones *out* of the cascade,
    /// not under it.
    #[serde(default)]
    pub roles: BTreeMap<String, Vec<String>>,

    /// Prefixes this dataset is authoritative for, named from `prefixes`.
    #[serde(default)]
    pub authoritative_namespaces: Vec<String>,
}

/// What the bundle carries, and the knobs that change those bytes.
///
/// Keys are named for bundle directory entries, not for the hdtc
/// subcommands that produce them today. The config describes the bundle; which
/// tool builds it is this program's business.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contents {
    /// The required POS/OPS permutation sidecar.
    #[serde(default)]
    pub perm: Perm,
    /// The full-text index over literals.
    #[serde(default)]
    pub text: Text,
    /// Membership filters and overlap sketches.
    #[serde(default)]
    pub filters: Filters,
    /// Exact role key sets.
    #[serde(default)]
    pub keysets: Keysets,
    /// The tier-1 description set.
    #[serde(default)]
    pub stats: Stats,
}

/// `data.hdt.perm`. Required by rule 1 and so carries no enable flag.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Perm {
    /// Optional permutation-to-SPO position maps: `pos`, `ops`, or both.
    #[serde(default)]
    pub position_maps: Vec<String>,
}

/// `data.hdt.text/`. The one optional family, and the expensive one.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Text {
    /// Build it. `search` and `o.text` are absent from a bundle without it.
    #[serde(default = "crate::build::config::yes")]
    pub enabled: bool,
    /// Skip literals whose lexical form exceeds this many bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_literal_bytes: Option<u64>,
    /// Datatype IRIs to skip on top of hdtc's value-space defaults.
    #[serde(default)]
    pub exclude_datatypes: Vec<String>,
    /// Index every datatype, dropping those defaults.
    #[serde(default)]
    pub index_all_datatypes: bool,
    /// Language to stem untagged literals as, or `none` to leave them unstemmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untagged_language: Option<String>,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            enabled: true,
            max_literal_bytes: None,
            exclude_datatypes: Vec::new(),
            index_all_datatypes: false,
            untagged_language: None,
        }
    }
}

/// `filters/`. Always built as complete role families.
///
/// No `roles` key and no `k`. Each family is all-or-nothing — both filter roles
/// or neither, both sketch roles or neither — and MinHash `k` is fixed at 65,536
/// federation-wide. Neither could be expressed here
/// except to express a nonconforming bundle.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    /// Binary fuse fingerprint width: 8 or 16.
    ///
    /// The one knob here. BinaryFuse16 is the default; BinaryFuse8 is supported
    /// but discouraged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_bits: Option<u8>,

    /// Bottom-k MinHash capacity. Recognized, and only ever [`SKETCH_K`].
    ///
    /// Named rather than rejected as an unknown field for two reasons. A
    /// config that sets it should fail with the invariant's reason instead of "unknown
    /// field `k`" — this is a value someone will reasonably try to tune. And
    /// the resolved plan prints it, so the plan must parse back as a config for
    /// its digest to serve as a canonical config identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<u32>,
}

/// `keysets/`. Always built, with no size threshold.
///
/// No `roles` key, for a second reason on top of the one above: the KGF profile
/// fixes the disjoint trio and excludes hdtc's experimental
/// `terms` role, because predicate IRIs would make every pair of knowledge
/// graphs "overlap" through `rdfs:label`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Keysets {
    /// `elias-fano` or `raw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// `stats/`. Always built: `/void` and `/summary` are core profile.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    /// Prefix tables, layered with later files winning, then
    /// `semantics.prefixes` last of all.
    ///
    /// The layered map is the bundle's prefix map, not only the namespace
    /// inventory's: the manifest declares it, requests resolve CURIEs against
    /// it, pages compact IRIs with it, and the inventory counts against it and
    /// publishes its digest. A flat `prefix: namespace` file, JSON or YAML by
    /// extension; the registry's shared table is the usual base.
    #[serde(default)]
    pub prefix_tables: Vec<PathBuf>,
}

/// Limits handed to the external builders.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Soft memory limit, as hdtc spells it: `4G`, `2000M`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,
    /// Parent for the per-invocation temporary directories.
    ///
    /// Per invocation, never shared: sharing one temp directory across
    /// concurrent `hdtc` processes has produced key
    /// sets that were structurally perfect and held another graph's keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<PathBuf>,
    /// Indexing threads, where the builder takes a count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    /// Refuse to publish a bundle larger than this, as `4G` or `900M`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bundle_bytes: Option<String>,
}

pub(super) fn yes() -> bool {
    true
}
