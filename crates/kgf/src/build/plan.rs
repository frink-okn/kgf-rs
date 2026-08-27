//! The resolved, validated build plan.
//!
//! [`config`](super::config) models what a person or a template writes;
//! this module models what the build will do. Resolution happens once, at the
//! start, and everything after it reads types rather than re-checking strings:
//! an IRI here has parsed, a role's predicates are full IRIs that are not
//! accidental CURIEs, a memory limit is a number of bytes, and a dataset id is
//! a legal directory name and URL path component.
//!
//! Two plan types, because there are two questions. [`ConfigPlan`] is everything
//! the config alone determines, and is what `--check-config` answers — a
//! registry can validate an entry without knowing where a bundle would go.
//! [`BundlePlan`] adds the per-build facts (where the input is, where the output
//! goes, what to record about provenance) and exists only when building.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use kgf_store::manifest::validate_predicate_role_iri;
use serde::Serialize;

use super::config;

/// The MinHash capacity every conforming bundle publishes at (doc 17 §17.2).
///
/// Not a default and not a knob. Comparing two sketches truncates both to
/// `min(k_A, k_B)`, so one bundle publishing at a smaller `k` caps the
/// resolution of *every pair it participates in*: raising it unilaterally buys
/// nothing and lowering it degrades other people's numbers. §17.2.1 calls it a
/// federation constant for exactly that reason, so it is not representable in a
/// config.
pub const SKETCH_K: u32 = 65536;

/// The sketch roles a conforming bundle publishes.
///
/// Doc 17 §17.3 makes each family all-or-nothing — both filter roles or
/// neither, both sketch roles or neither — so this is the profile rather than a
/// default, and it is not configurable.
pub const SKETCH_ROLES: &str = "subjects,objects";

/// The key-set roles the `kgf-keyset/1` profile fixes (doc 18 §18.4).
///
/// The disjoint trio rather than the overlapping pair: measured across the 40
/// OKN graphs it costs 2.53 GB against 3.87 GB, and every composite view is a
/// streaming merge of two sections. hdtc's experimental `terms` role is
/// deliberately absent — it carries predicate IRIs, which would make every pair
/// of knowledge graphs "overlap" through `rdfs:label`.
pub const KEYSET_ROLES: &str = "subjects-only,objects-only,shared";

/// A host-local dataset slug.
///
/// It is simultaneously a directory name under the bundle root and the first
/// path component of every URL for this dataset, so it is constrained to what
/// is unambiguous in both. The leading-dot rule is not cosmetic: a build stages
/// into a dot-prefixed sibling of its output, and `Catalog::scan` walks
/// `{root}/{dataset}/{version}` without knowing which directories are still
/// being written.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct DatasetId(String);

/// A human-facing release label, and the version directory's name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct VersionLabel(String);

fn parse_path_component(kind: &str, value: &str) -> Result<String> {
    ensure!(!value.is_empty(), "a {kind} may not be empty");
    ensure!(
        value.len() <= 128,
        "a {kind} may not exceed 128 bytes; {value:?} is {}",
        value.len()
    );
    ensure!(
        !value.starts_with('.'),
        "a {kind} may not begin with `.`; {value:?} would be taken for an \
         in-progress build directory rather than a published one"
    );
    if let Some(bad) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        bail!(
            "a {kind} may contain only ASCII letters, digits, `-`, `_` and `.`; \
             {value:?} contains {bad:?}"
        );
    }
    Ok(value.to_owned())
}

impl FromStr for DatasetId {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        parse_path_component("dataset id", value).map(Self)
    }
}

impl FromStr for VersionLabel {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        parse_path_component("version label", value).map(Self)
    }
}

impl DatasetId {
    /// The slug as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl VersionLabel {
    /// The label as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatasetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for VersionLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A globally stable dataset identity: an absolute IRI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DatasetIri(String);

impl FromStr for DatasetIri {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        oxrdf::NamedNode::new(value)
            .with_context(|| format!("dataset IRI {value:?} is not an absolute RDF IRI"))?;
        Ok(Self(value.to_owned()))
    }
}

impl DatasetIri {
    /// The IRI as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A byte count written the way hdtc spells it: `4G`, `2000M`, `512K`, `4096`.
///
/// Serialized back as that spelling rather than as a raw count, so the resolved
/// plan `--check-config` prints is a document that parses as a config again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(into = "String")]
pub struct ByteSize(u64);

impl From<ByteSize> for String {
    fn from(size: ByteSize) -> String {
        size.to_hdtc_arg()
    }
}

impl FromStr for ByteSize {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        ensure!(!trimmed.is_empty(), "a byte size may not be empty");
        let (digits, scale) = match trimmed.as_bytes()[trimmed.len() - 1] {
            b'K' | b'k' => (&trimmed[..trimmed.len() - 1], 1024),
            b'M' | b'm' => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
            b'G' | b'g' => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
            b'T' | b't' => (&trimmed[..trimmed.len() - 1], 1024_u64.pow(4)),
            _ => (trimmed, 1),
        };
        let count: u64 = digits.trim().parse().with_context(|| {
            format!("byte size {value:?} is not a number optionally suffixed K, M, G or T")
        })?;
        let bytes = count
            .checked_mul(scale)
            .with_context(|| format!("byte size {value:?} overflows"))?;
        ensure!(bytes > 0, "byte size {value:?} must be greater than zero");
        Ok(Self(bytes))
    }
}

impl ByteSize {
    /// The count in bytes.
    pub fn bytes(self) -> u64 {
        self.0
    }

    /// Spelled back the way an hdtc argument wants it.
    pub fn to_hdtc_arg(self) -> String {
        for (suffix, scale) in [("G", 1024_u64.pow(3)), ("M", 1024 * 1024), ("K", 1024)] {
            if self.0.is_multiple_of(scale) {
                return format!("{}{suffix}", self.0 / scale);
            }
        }
        self.0.to_string()
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hdtc_arg())
    }
}

/// A permutation-to-SPO position map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionMap {
    /// The POS permutation's map back to SPO positions.
    Pos,
    /// The OPS permutation's.
    Ops,
}

impl FromStr for PositionMap {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pos" => Ok(Self::Pos),
            "ops" => Ok(Self::Ops),
            other => bail!("position map must be `pos` or `ops`, not {other:?}"),
        }
    }
}

impl PositionMap {
    /// The name hdtc's `--position-maps` list uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pos => "pos",
            Self::Ops => "ops",
        }
    }
}

/// Binary fuse fingerprint width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(into = "u8")]
pub enum FilterBits {
    /// BinaryFuse8: about half the bytes, a 1/256 false-positive rate.
    Eight,
    /// BinaryFuse16, the KGF emission (doc 18 §18.2).
    Sixteen,
}

impl From<FilterBits> for u8 {
    fn from(bits: FilterBits) -> u8 {
        match bits {
            FilterBits::Eight => 8,
            FilterBits::Sixteen => 16,
        }
    }
}

impl TryFrom<u8> for FilterBits {
    type Error = anyhow::Error;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            8 => Ok(Self::Eight),
            16 => Ok(Self::Sixteen),
            other => bail!("filter_bits must be 8 or 16, not {other}"),
        }
    }
}

/// Key-set payload encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeysetEncoding {
    /// Elias-Fano, ~4.4–5.8 bytes per key. Doc 18 §18.4's standard emission.
    EliasFano,
    /// A raw sorted `u64` array, 8 bytes per key.
    Raw,
}

impl FromStr for KeysetEncoding {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "elias-fano" => Ok(Self::EliasFano),
            "raw" => Ok(Self::Raw),
            other => bail!("keyset encoding must be `elias-fano` or `raw`, not {other:?}"),
        }
    }
}

impl KeysetEncoding {
    /// The name hdtc's `--encoding` uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EliasFano => "elias-fano",
            Self::Raw => "raw",
        }
    }
}

/// How to stem literals carrying no language tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(into = "String")]
pub enum UntaggedLanguage {
    /// Leave them unstemmed.
    None,
    /// Stem them as this language.
    As(String),
}

impl FromStr for UntaggedLanguage {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        if value == "none" {
            return Ok(Self::None);
        }
        ensure!(
            !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "untagged_language must be `none` or a language tag, not {value:?}"
        );
        Ok(Self::As(value.to_owned()))
    }
}

impl From<UntaggedLanguage> for String {
    fn from(language: UntaggedLanguage) -> String {
        language.as_str().to_owned()
    }
}

impl UntaggedLanguage {
    /// The value hdtc's `--untagged-language` takes.
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "none",
            Self::As(tag) => tag,
        }
    }
}

/// Identity and description, resolved.
#[derive(Debug, Clone, Serialize)]
pub struct Dataset {
    /// Host-local slug.
    pub id: DatasetId,
    /// Globally stable identity.
    pub iri: DatasetIri,
    /// Short human title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Longer human description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// License identifier or URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Project homepage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Who publishes the dataset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<Publisher>,
}

/// Publisher identity.
#[derive(Debug, Clone, Serialize)]
pub struct Publisher {
    /// Display name.
    pub name: String,
    /// Contact address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
}

/// Prefixes, roles and authoritative namespaces, validated against each other.
#[derive(Debug, Clone, Serialize)]
pub struct Semantics {
    /// Prefix bindings for CURIE syntax in parameters.
    pub prefixes: BTreeMap<String, String>,
    /// Empty means "the manifest's standard profile", not "no roles".
    pub roles: BTreeMap<String, Vec<String>>,
    /// Prefixes this dataset is authoritative for.
    pub authoritative_namespaces: Vec<String>,
}

/// What the bundle will carry.
#[derive(Debug, Clone, Serialize)]
pub struct Contents {
    /// The required permutation sidecar.
    pub perm: Perm,
    /// `None` builds no text index, so the bundle declares no `search`.
    ///
    /// Serialized as `{"enabled": false}` rather than omitted, because a
    /// resolved plan is meant to parse as a config again and an omitted `text`
    /// would come back enabled — silently turning the expensive step on in the
    /// one case where someone deliberately turned it off.
    #[serde(serialize_with = "serialize_text")]
    pub text: Option<Text>,
    /// Membership filters and overlap sketches.
    pub filters: Filters,
    /// Exact role key sets.
    pub keysets: Keysets,
    /// The tier-1 description set.
    pub stats: Stats,
}

fn serialize_text<S: serde::Serializer>(
    text: &Option<Text>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    match text {
        Some(text) => text.serialize(serializer),
        None => {
            let mut disabled = serializer.serialize_struct("Text", 1)?;
            disabled.serialize_field("enabled", &false)?;
            disabled.end()
        }
    }
}

/// `data.hdt.perm`.
#[derive(Debug, Clone, Serialize)]
pub struct Perm {
    /// Permutation-to-SPO position maps to include, sorted and deduplicated.
    pub position_maps: Vec<PositionMap>,
}

/// `data.hdt.text/`.
#[derive(Debug, Clone, Serialize)]
pub struct Text {
    /// Skip literals whose lexical form exceeds this many bytes.
    pub max_literal_bytes: u64,
    /// Datatype IRIs to skip on top of hdtc's value-space defaults.
    pub exclude_datatypes: Vec<String>,
    /// Index every datatype, dropping those defaults.
    pub index_all_datatypes: bool,
    /// How to stem literals carrying no language tag.
    pub untagged_language: UntaggedLanguage,
}

/// `filters/`.
#[derive(Debug, Clone, Serialize)]
pub struct Filters {
    /// Bottom-k MinHash capacity. Always [`SKETCH_K`].
    pub k: u32,
    /// Binary fuse fingerprint width.
    pub filter_bits: FilterBits,
}

/// `keysets/`.
#[derive(Debug, Clone, Serialize)]
pub struct Keysets {
    /// Payload encoding.
    pub encoding: KeysetEncoding,
}

/// `stats/`.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Prefix tables, layered with later files winning.
    pub prefix_tables: Vec<PathBuf>,
}

/// Limits handed to the external builders.
#[derive(Debug, Clone, Serialize)]
pub struct Resources {
    /// Soft memory limit for each builder's internal buffers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<ByteSize>,
    /// Parent for the per-invocation temporary directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<PathBuf>,
    /// Indexing threads, where the builder takes a count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    /// Refuse to publish a bundle larger than this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bundle_bytes: Option<ByteSize>,
}

/// Everything the config alone determines.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigPlan {
    /// The config schema version this was resolved from.
    pub schema: u32,
    /// Identity and description.
    pub dataset: Dataset,
    /// Prefixes, roles, and authoritative namespaces.
    pub semantics: Semantics,
    /// What the bundle will carry.
    pub contents: Contents,
    /// Limits for the external builders.
    pub resources: Resources,
}

/// hdtc's own defaults, restated here so a resolved plan is complete rather than
/// partly deferred to whichever hdtc happens to run. Where a value is also a KGF
/// convention rather than merely a default, the doc that fixes it is cited: a
/// future hdtc changing its default must not silently change a bundle.
mod defaults {
    /// hdtc `text --max-literal-bytes`.
    pub const MAX_LITERAL_BYTES: u64 = 4096;
    /// hdtc `text --untagged-language`.
    pub const UNTAGGED_LANGUAGE: &str = "en";
    /// hdtc `sketch --filter-bits`; doc 18 §18.2 sizes the corpus at Fuse16.
    pub const FILTER_BITS: u8 = 16;
    /// hdtc `keyset --encoding`; doc 18 §18.4's `kgf-keyset/1` standard emission.
    pub const KEYSET_ENCODING: &str = "elias-fano";
}

impl ConfigPlan {
    /// Resolve and validate a parsed config.
    pub fn resolve(config: config::Config) -> Result<Self> {
        ensure!(
            config.schema == config::SCHEMA_VERSION,
            "build config schema {} is not supported; this kgf understands schema {}",
            config.schema,
            config::SCHEMA_VERSION
        );

        // Refused rather than ignored. A bundle whose config declares components
        // and whose artifacts contain none would be described as a plain bundle
        // — the statistics, the graph identities, and the entailment flags doc
        // 04 §4.3 hangs off each component all silently absent.
        for (field, value) in [
            ("components", &config.components),
            ("publish", &config.publish),
        ] {
            ensure!(
                value.is_none(),
                "build config declares `{field}`, but this build has no component \
                 DAG: it merges no derived components, binds no per-component graph \
                 identity, and produces no per-component statistics (doc 04 §4.4). \
                 Remove it, or build the components with their own tools and pass \
                 the merged result as `--input`"
            );
        }

        let dataset = resolve_dataset(config.dataset)?;
        let semantics = resolve_semantics(config.semantics)?;
        let contents = resolve_contents(config.contents)?;
        let resources = resolve_resources(config.resources)?;

        Ok(Self {
            schema: config.schema,
            dataset,
            semantics,
            contents,
            resources,
        })
    }
}

fn resolve_dataset(dataset: config::Dataset) -> Result<Dataset> {
    Ok(Dataset {
        id: dataset
            .id
            .parse()
            .context("dataset.id is not a usable slug")?,
        iri: dataset.iri.parse().context("dataset.iri is not usable")?,
        title: non_empty(dataset.title, "dataset.title")?,
        description: non_empty(dataset.description, "dataset.description")?,
        license: non_empty(dataset.license, "dataset.license")?,
        homepage: non_empty(dataset.homepage, "dataset.homepage")?,
        publisher: dataset
            .publisher
            .map(|publisher| -> Result<Publisher> {
                ensure!(
                    !publisher.name.trim().is_empty(),
                    "dataset.publisher.name may not be blank"
                );
                Ok(Publisher {
                    name: publisher.name,
                    contact: non_empty(publisher.contact, "dataset.publisher.contact")?,
                })
            })
            .transpose()?,
    })
}

/// A field a template rendered as an empty string is a field the registry did
/// not have. Writing `"title": ""` into a manifest would publish that omission
/// as a fact.
fn non_empty(value: Option<String>, field: &str) -> Result<Option<String>> {
    match value {
        Some(value) if value.trim().is_empty() => {
            ensure!(
                value.is_empty(),
                "{field} is whitespace; omit it rather than blanking it"
            );
            Ok(None)
        }
        other => Ok(other),
    }
}

fn resolve_semantics(semantics: config::Semantics) -> Result<Semantics> {
    for (prefix, expansion) in &semantics.prefixes {
        ensure!(
            !prefix.is_empty()
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "prefix name {prefix:?} is not a usable CURIE prefix"
        );
        oxiri::Iri::parse(expansion.as_str()).map_err(|error| {
            anyhow::anyhow!(
                "prefix {prefix:?} expands to {expansion:?}, which is not an IRI: {error}"
            )
        })?;
    }

    for (role, predicates) in &semantics.roles {
        ensure!(
            !role.is_empty()
                && role
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "role name {role:?} must be a lowercase token; it is spelled into `role=` \
             on the wire (doc 19 §19.1)"
        );
        ensure!(
            !predicates.is_empty(),
            "role {role:?} declares no predicates; omit the role rather than \
             declaring it empty"
        );
        for predicate in predicates {
            validate_predicate_role_iri(predicate, &semantics.prefixes).map_err(|error| {
                anyhow::anyhow!("role {role:?} predicate {predicate:?} is unusable: {error}")
            })?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for predicate in predicates {
            ensure!(
                seen.insert(predicate),
                "role {role:?} lists {predicate:?} twice; a cascade is ordered, so a \
                 repeat is either a typo or a lost intention"
            );
        }
    }

    for namespace in &semantics.authoritative_namespaces {
        ensure!(
            semantics.prefixes.contains_key(namespace),
            "authoritative namespace {namespace:?} is not one of the declared prefixes"
        );
    }

    // Seeded with the bindings every manifest declares, so the resolved plan
    // shows the map the build will actually use. Two reasons this belongs here
    // rather than being left to the manifest writer: `--check-config` would
    // otherwise print a prefix map the bundle does not have, and the namespace
    // inventory runs *before* the manifest exists — `hdtc namespaces` requires
    // at least one `--prefixes` table, so a config declaring none had nothing
    // to pass it and failed at the last step of an otherwise valid build.
    //
    // Seeded first, so an explicit binding overrides rather than collides,
    // exactly as `crate::manifest::prefixes` does.
    let mut prefixes: BTreeMap<String, String> = crate::manifest::WELL_KNOWN_PREFIXES
        .iter()
        .map(|(prefix, namespace)| ((*prefix).to_owned(), (*namespace).to_owned()))
        .collect();
    prefixes.extend(semantics.prefixes);

    Ok(Semantics {
        prefixes,
        roles: semantics.roles,
        authoritative_namespaces: semantics.authoritative_namespaces,
    })
}

fn resolve_contents(contents: config::Contents) -> Result<Contents> {
    let mut position_maps = Vec::new();
    for value in &contents.perm.position_maps {
        let parsed: PositionMap = value
            .parse()
            .context("contents.perm.position_maps is unusable")?;
        ensure!(
            !position_maps.contains(&parsed),
            "contents.perm.position_maps lists {value:?} twice"
        );
        position_maps.push(parsed);
    }
    position_maps.sort();

    let text = contents
        .text
        .enabled
        .then(|| -> Result<Text> {
            for datatype in &contents.text.exclude_datatypes {
                oxrdf::NamedNode::new(datatype).with_context(|| {
                    format!("contents.text.exclude_datatypes entry {datatype:?} is not an IRI")
                })?;
            }
            ensure!(
                !contents.text.index_all_datatypes || contents.text.exclude_datatypes.is_empty(),
                "contents.text sets index_all_datatypes and also excludes datatypes; \
                 the two contradict"
            );
            Ok(Text {
                max_literal_bytes: contents
                    .text
                    .max_literal_bytes
                    .unwrap_or(defaults::MAX_LITERAL_BYTES),
                exclude_datatypes: contents.text.exclude_datatypes.clone(),
                index_all_datatypes: contents.text.index_all_datatypes,
                untagged_language: contents
                    .text
                    .untagged_language
                    .as_deref()
                    .unwrap_or(defaults::UNTAGGED_LANGUAGE)
                    .parse()
                    .context("contents.text.untagged_language is unusable")?,
            })
        })
        .transpose()?;
    if let Some(text) = &text {
        ensure!(
            text.max_literal_bytes > 0,
            "contents.text.max_literal_bytes must be greater than zero"
        );
    }

    if let Some(k) = contents.filters.k {
        ensure!(
            k == SKETCH_K,
            "contents.filters.k is {k}, but the KGF profile fixes it at {SKETCH_K} \
             federation-wide (doc 17 §17.2). Comparing two sketches truncates both to \
             the smaller `k`, so a bundle published at {k} would cap the resolution of \
             every pair it takes part in — raising it unilaterally buys nothing and \
             lowering it degrades other publishers' numbers. Remove the key"
        );
    }
    let filters = Filters {
        k: SKETCH_K,
        filter_bits: FilterBits::try_from(
            contents
                .filters
                .filter_bits
                .unwrap_or(defaults::FILTER_BITS),
        )
        .context("contents.filters.filter_bits is unusable")?,
    };

    let keysets = Keysets {
        encoding: contents
            .keysets
            .encoding
            .as_deref()
            .unwrap_or(defaults::KEYSET_ENCODING)
            .parse()
            .context("contents.keysets.encoding is unusable")?,
    };

    Ok(Contents {
        perm: Perm { position_maps },
        text,
        filters,
        keysets,
        stats: Stats {
            prefix_tables: contents.stats.prefix_tables,
        },
    })
}

fn resolve_resources(resources: config::Resources) -> Result<Resources> {
    Ok(Resources {
        memory_limit: parse_size(resources.memory_limit, "resources.memory_limit")?,
        temp_dir: resources.temp_dir,
        threads: match resources.threads {
            Some(0) => bail!("resources.threads must be greater than zero, or omitted for auto"),
            other => other,
        },
        max_bundle_bytes: parse_size(resources.max_bundle_bytes, "resources.max_bundle_bytes")?,
    })
}

fn parse_size(value: Option<String>, field: &str) -> Result<Option<ByteSize>> {
    value
        .map(|value| {
            value
                .parse::<ByteSize>()
                .context(format!("{field} is unusable"))
        })
        .transpose()
}

/// The per-build facts that are flags rather than config.
#[derive(Debug, Clone, Serialize)]
pub struct BundlePlan {
    /// Everything the config determined.
    #[serde(flatten)]
    pub config: ConfigPlan,
    /// This release's label, and the output directory's name.
    pub version: VersionLabel,
    /// The version this one supersedes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<VersionLabel>,
    /// Where this bundle's triples come from.
    pub input: Input,
    /// The version directory to publish into.
    pub output: PathBuf,
    /// What to record about how this build was run.
    pub provenance: Provenance,
}

/// Where this bundle's triples come from.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Input {
    /// An HDT another pipeline already built. The OKN path: kace leaves
    /// `hdt/graph.hdt` in LakeFS and that file is exactly the input.
    Hdt {
        /// The HDT to adopt.
        path: PathBuf,
        /// Move it rather than copying. The downloaded copy is scratch, and on a
        /// large graph the copy is a full HDT-sized read and write.
        adopt: bool,
    },
    /// RDF, normalized and built here by `hdtc create --perm`.
    Rdf {
        /// The RDF inputs, in the order hdtc will read them.
        paths: Vec<PathBuf>,
    },
}

/// Recorded in the manifest, never acted on. Provenance, not identity: doc 04
/// §4.3 is emphatic that `content_digest` covers published bytes and not build
/// inputs.
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    /// Where the input came from. Unverifiable here, and passed through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Asserted digest of the input, to check against the bytes actually read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
    /// OCI reference of the builder image. Unverifiable here, and passed through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builder_image: Option<String>,
}

impl BundlePlan {
    /// The version directory this build publishes into.
    pub fn output(&self) -> &Path {
        &self.output
    }
}

/// Check that an `--out` path agrees with the identity the config declares.
///
/// `kgf manifest` infers `{root}/{dataset}/{version}` from the path; this does
/// the converse, because here the config is the authority and the path is the
/// claim. A mismatch is refused rather than resolved: publishing `dreamkg` into
/// a directory named `dream-kg` produces a bundle the catalog serves under a
/// slug its own manifest disagrees with.
pub fn resolve_output(
    out: &Path,
    id: &DatasetId,
    version: Option<&VersionLabel>,
) -> Result<(PathBuf, VersionLabel)> {
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .context("--out has no final path component to read a version label from")?;
    let from_path: VersionLabel = name
        .parse()
        .context("--out's final path component is not a usable version label")?;
    let version = match version {
        Some(explicit) => {
            ensure!(
                explicit == &from_path,
                "--version {explicit} contradicts --out, whose final component is {from_path}"
            );
            explicit.clone()
        }
        None => from_path,
    };

    if let Some(parent) = out
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
    {
        ensure!(
            parent == id.as_str(),
            "--out places the bundle under {parent:?}, but dataset.id is {id}; \
             the catalog layout is {{root}}/{{dataset}}/{{version}}"
        );
    }

    Ok((out.to_path_buf(), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(yaml: &str) -> Result<ConfigPlan> {
        ConfigPlan::resolve(serde_norway::from_str(yaml)?)
    }

    const MINIMAL: &str = "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n";

    /// The defaults are the federation's measured conventions, not whatever
    /// hdtc happens to default to today. A future hdtc changing one must change
    /// this test, which is the point of restating them.
    #[test]
    fn defaults_are_the_documented_conventions() {
        let plan = resolve(MINIMAL).unwrap();
        assert_eq!(plan.contents.filters.k, 65536);
        assert_eq!(plan.contents.filters.filter_bits, FilterBits::Sixteen);
        assert_eq!(plan.contents.keysets.encoding, KeysetEncoding::EliasFano);
        let text = plan.contents.text.expect("text is built by default");
        assert_eq!(text.max_literal_bytes, 4096);
        assert_eq!(text.untagged_language, UntaggedLanguage::As("en".into()));
        assert!(plan.contents.perm.position_maps.is_empty());
        assert!(
            plan.semantics.roles.is_empty(),
            "an empty role map means the manifest's standard profile"
        );
    }

    /// `--check-config` prints a fully-defaulted plan, and that document must
    /// parse as a config again — otherwise it is a report rather than a
    /// canonical form, and a caller cannot store the resolved version.
    #[test]
    fn a_resolved_plan_parses_as_a_config_again() {
        for source in [
            MINIMAL,
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\ncontents: {text: {enabled: false}}\n",
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\nresources: {memory_limit: 8G}\n",
        ] {
            let first = resolve(source).unwrap();
            let json = serde_json::to_string(&first).unwrap();
            let again =
                ConfigPlan::resolve(serde_json::from_str(&json).unwrap()).unwrap_or_else(|error| {
                    panic!("resolved plan did not re-parse: {error:#}\n{json}")
                });
            assert_eq!(json, serde_json::to_string(&again).unwrap());
        }
    }

    /// Turning a text index off must survive the round trip. Coming back
    /// enabled would silently reinstate the expensive step in the one case
    /// where someone deliberately declined it.
    #[test]
    fn a_declined_text_index_stays_declined() {
        let plan = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\ncontents: {text: {enabled: false}}\n",
        )
        .unwrap();
        assert!(plan.contents.text.is_none());
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            json["contents"]["text"],
            serde_json::json!({"enabled": false})
        );
        let again = ConfigPlan::resolve(serde_json::from_value(json).unwrap()).unwrap();
        assert!(again.contents.text.is_none());
    }

    /// Doc 17 §17.3 makes each family all-or-nothing and doc 18 §18.1 states
    /// key sets are published unconditionally, so there is no key to turn them
    /// off with. `deny_unknown_fields` is what enforces that.
    /// `k` is representable so it can be *refused with a reason* — doc 17
    /// §17.2 makes it a federation constant, and someone will reasonably try to
    /// tune it. It is also why the resolved plan still round-trips.
    #[test]
    fn a_minhash_capacity_other_than_the_federation_constant_is_refused() {
        let error = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
             contents: {filters: {k: 4096}}\n",
        )
        .expect_err("a per-bundle k must be refused");
        assert!(format!("{error:#}").contains("§17.2"), "{error:#}");

        let stated = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
             contents: {filters: {k: 65536}}\n",
        )
        .expect("restating the constant is harmless");
        assert_eq!(stated.contents.filters.k, SKETCH_K);
    }

    #[test]
    fn filters_and_keysets_cannot_be_switched_off() {
        for source in [
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\ncontents: {filters: {enabled: false}}\n",
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\ncontents: {keysets: {enabled: false}}\n",
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\ncontents: {keysets: {roles: [terms]}}\n",
        ] {
            let error = resolve(source).expect_err("must be refused");
            assert!(
                format!("{error:#}").contains("unknown field"),
                "expected an unknown-field refusal, got {error:#}"
            );
        }
    }

    #[test]
    fn byte_sizes_parse_and_spell_back() {
        for (source, bytes, spelled) in [
            ("4G", 4 * 1024 * 1024 * 1024, "4G"),
            ("2000M", 2000 * 1024 * 1024, "2000M"),
            ("512K", 512 * 1024, "512K"),
            ("4096", 4096, "4K"),
            (" 8G ", 8 * 1024 * 1024 * 1024, "8G"),
        ] {
            let size: ByteSize = source.parse().unwrap();
            assert_eq!(size.bytes(), bytes, "{source}");
            assert_eq!(size.to_hdtc_arg(), spelled, "{source}");
            assert_eq!(spelled.parse::<ByteSize>().unwrap(), size);
        }
        for bad in ["", "0", "4 gigs", "-1", "G", "4P"] {
            assert!(bad.parse::<ByteSize>().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_role_predicate_must_be_a_full_iri_and_may_not_repeat() {
        let curie = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
             semantics: {prefixes: {ex: 'http://e.org/'}, roles: {label: ['ex:name']}}\n",
        )
        .expect_err("a declared CURIE must be refused");
        assert!(format!("{curie:#}").contains("ex:name"));

        let repeated = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
             semantics: {roles: {label: ['http://e.org/n', 'http://e.org/n']}}\n",
        )
        .expect_err("a repeated predicate must be refused");
        assert!(format!("{repeated:#}").contains("twice"));

        let empty = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\nsemantics: {roles: {label: []}}\n",
        )
        .expect_err("an empty role must be refused");
        assert!(format!("{empty:#}").contains("declares no predicates"));
    }

    #[test]
    fn an_authoritative_namespace_must_be_a_declared_prefix() {
        assert!(
            resolve(
                "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
                 semantics: {authoritative_namespaces: [nope]}\n"
            )
            .is_err()
        );
        assert!(
            resolve(
                "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
                 semantics: {prefixes: {ok: 'http://e.org/'}, authoritative_namespaces: [ok]}\n"
            )
            .is_ok()
        );
    }

    /// A dot-prefixed name is how this build spells "still being written", and
    /// `Catalog::scan` walks `{root}/{dataset}/{version}` without knowing which
    /// directories are finished.
    #[test]
    fn dot_prefixed_names_are_refused() {
        assert!(".hidden".parse::<DatasetId>().is_err());
        assert!(".kgf-build-1".parse::<VersionLabel>().is_err());
        assert!("2026-06-01".parse::<VersionLabel>().is_ok());
        assert!(
            "a/b".parse::<DatasetId>().is_err(),
            "a slug is one path component"
        );
        assert!("..".parse::<DatasetId>().is_err());
        assert!("".parse::<DatasetId>().is_err());
    }

    #[test]
    fn an_output_path_must_agree_with_the_declared_identity() {
        let id: DatasetId = "dreamkg".parse().unwrap();
        let (dir, version) =
            resolve_output(Path::new("/root/dreamkg/2026-06-01"), &id, None).unwrap();
        assert_eq!(dir, Path::new("/root/dreamkg/2026-06-01"));
        assert_eq!(version.as_str(), "2026-06-01");

        let wrong_dataset = resolve_output(Path::new("/root/dream-kg/2026-06-01"), &id, None)
            .expect_err("a slug the manifest would disagree with must be refused");
        assert!(format!("{wrong_dataset:#}").contains("dataset.id"));

        let explicit: VersionLabel = "2026-07-01".parse().unwrap();
        let contradiction =
            resolve_output(Path::new("/root/dreamkg/2026-06-01"), &id, Some(&explicit))
                .expect_err("--version contradicting --out must be refused");
        assert!(format!("{contradiction:#}").contains("contradicts"));
    }

    #[test]
    fn a_blank_description_field_is_an_omission_not_a_fact() {
        let plan =
            resolve("schema: 1\ndataset: {id: a, iri: 'https://e.org/a', title: ''}\n").unwrap();
        assert!(plan.dataset.title.is_none());
    }

    #[test]
    fn contradictory_text_options_are_refused() {
        let error = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
             contents: {text: {index_all_datatypes: true, exclude_datatypes: ['http://e.org/d']}}\n",
        )
        .expect_err("indexing every datatype while excluding one must be refused");
        assert!(format!("{error:#}").contains("contradict"));
    }

    #[test]
    fn position_maps_are_parsed_sorted_and_deduplicated() {
        let plan = resolve(
            "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
             contents: {perm: {position_maps: [ops, pos]}}\n",
        )
        .unwrap();
        assert_eq!(
            plan.contents.perm.position_maps,
            vec![PositionMap::Pos, PositionMap::Ops]
        );
        assert!(
            resolve(
                "schema: 1\ndataset: {id: a, iri: 'https://e.org/a'}\n\
                 contents: {perm: {position_maps: [pos, pos]}}\n"
            )
            .is_err()
        );
    }
}
