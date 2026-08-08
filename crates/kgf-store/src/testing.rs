//! Shared test support.
//!
//! Only the *oracles* in each module's tests must avoid sharing code with the
//! implementation they check. Sharing between test modules is fine, and worth
//! doing: every module still to be written wants a deterministic generator and
//! a way to read a bit out of a byte slice.
//!
//! # What the `testing` feature exports
//!
//! Soundness is a property of a crate's *public* API, so the feature exports
//! only what a fixture-driven test in another crate needs: build a golden
//! bundle, and map or search its bytes. Everything that takes a caller-supplied
//! path is crate-private.
//!
//! That line is not fussiness. A safe function taking `&Path` and returning a
//! [`Mapping`](crate::map::Mapping) or a publication capability lets safe code
//! outside this crate map a file it can still truncate — precisely the
//! obligation [`PublishedBundle::new`](crate::map::PublishedBundle::new) and
//! [`Mapping::open`](crate::map::Mapping::open) are `unsafe` to record, handed
//! back for free. Inside the crate the same wrappers are sound, because `map`'s
//! soundness argument covers this crate's own code and its tests are audited
//! alongside it; publishing them widened that audit boundary to every future
//! caller.
//!
//! [`Fixture`] stays safe under the same rule: it owns the temporary directory,
//! and with the path accessors crate-private an external caller has no way to
//! reach the bytes it has mapped. When an external test needs a `Store` rather
//! than a `Mapping`, the fixture should grow a method handing out a
//! `PublishedBundle` for the directory it owns — never the directory.

/// SplitMix64 — a deterministic generator, so a failure reproduces.
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Whether bit `index` of `bytes` is set, LSB-first within each byte.
///
/// The one-line definition of the encoding every oracle in the crate walks.
pub fn bit(bytes: &[u8], index: u64) -> bool {
    bytes[(index / 8) as usize] >> (index % 8) & 1 == 1
}

/// Map a file a test has just written and will not touch again.
pub(crate) fn map_fixture(path: &std::path::Path) -> crate::map::Mapping {
    crate::map::map_fixture(path)
}

/// Assert the publication invariant for a test bundle that is never modified.
#[cfg(test)]
pub(crate) fn published_bundle(path: &std::path::Path) -> crate::map::PublishedBundle {
    crate::map::PublishedBundle::for_test(path)
}

/// Assert the publication invariant for a test catalog root.
#[cfg(test)]
pub(crate) fn published_root(path: &std::path::Path) -> crate::map::PublishedRoot {
    crate::map::PublishedRoot::for_test(path)
}

/// The golden fixture graph: small enough to reason about, wide enough to
/// exercise the structures (doc 20 §20.9).
///
/// Chosen so that every case the dictionary and the permutations distinguish is
/// present: terms that are both subject and object (`alice`, `bob` — the shared
/// section), subject-only (`_:b1`), object-only (every literal and `Thing`),
/// subjects with several predicates, one (subject, predicate) pair with several
/// objects (`alice label`), a language-tagged and a typed literal, and a
/// predicate used by more than one subject. Terms are deliberately unsorted
/// here; the dictionary sorts them.
pub const TINY_NT: &str = concat!(
    "<http://example.org/alice> <http://example.org/name> \"Alice\" .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .\n",
    "<http://example.org/alice> <http://example.org/label> \"Alice\"@en .\n",
    "<http://example.org/alice> <http://example.org/label> \"Alicia\"@es .\n",
    "<http://example.org/alice> <http://example.org/age> ",
    "\"30\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" .\n",
    "<http://example.org/bob> <http://example.org/knows> <http://example.org/alice> .\n",
    "_:b1 <http://example.org/type> <http://example.org/Thing> .\n",
);

/// A minimal quad fixture, for the cases that need the optional graph pair.
///
/// Two graphs with one shared triple, so the sidecar has something to
/// distinguish rather than a single layer covering everything.
pub const TINY_NQ: &str = concat!(
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> ",
    "<http://example.org/g1> .\n",
    "<http://example.org/alice> <http://example.org/knows> <http://example.org/bob> ",
    "<http://example.org/g2> .\n",
    "<http://example.org/bob> <http://example.org/name> \"Bob\" ",
    "<http://example.org/g2> .\n",
);

/// The golden fixture's triples in role-scoped id space.
///
/// Dictionary lookup is independent of BitmapTriples traversal, so this is an
/// oracle for traversal and pattern tests without copying hdtc's reader.
pub fn tiny_id_triples(dictionary: &crate::dict::Dictionary<'_>) -> Vec<crate::IdTriple> {
    use crate::{IdTriple, Role};

    let id = |role, term: &[u8]| {
        dictionary
            .locate(role, term)
            .expect("read fixture dictionary")
            .unwrap_or_else(|| panic!("fixture term is absent: {}", String::from_utf8_lossy(term)))
            .0
    };
    let subject = |term| id(Role::Subject, term);
    let predicate = |term| id(Role::Predicate, term);
    let object = |term| id(Role::Object, term);

    let alice = b"http://example.org/alice";
    let bob = b"http://example.org/bob";
    vec![
        IdTriple {
            subject: subject(alice),
            predicate: predicate(b"http://example.org/name"),
            object: object(b"\"Alice\""),
        },
        IdTriple {
            subject: subject(alice),
            predicate: predicate(b"http://example.org/knows"),
            object: object(bob),
        },
        IdTriple {
            subject: subject(alice),
            predicate: predicate(b"http://example.org/label"),
            object: object(b"\"Alice\"@en"),
        },
        IdTriple {
            subject: subject(alice),
            predicate: predicate(b"http://example.org/label"),
            object: object(b"\"Alicia\"@es"),
        },
        IdTriple {
            subject: subject(alice),
            predicate: predicate(b"http://example.org/age"),
            object: object(b"\"30\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
        },
        IdTriple {
            subject: subject(bob),
            predicate: predicate(b"http://example.org/name"),
            object: object(b"\"Bob\""),
        },
        IdTriple {
            subject: subject(bob),
            predicate: predicate(b"http://example.org/knows"),
            object: object(alice),
        },
        IdTriple {
            subject: subject(b"_:b1"),
            predicate: predicate(b"http://example.org/type"),
            object: object(b"http://example.org/Thing"),
        },
    ]
}

/// A bundle built by hdtc into a temporary directory.
///
/// Doc 20 §20.9's golden bundle: the fixture RDF is checked in and hdtc builds
/// the artifacts, so the bytes under test are the bytes a real bundle has.
/// Nothing here writes a byte of any format — that is the point, since a test
/// fixture written by hand would be this crate's own guess at the format rather
/// than the producer's output.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// Build a bundle from N-Triples source.
    ///
    /// Runs the `hdtc` binary from the sibling checkout, with `--perm` because
    /// `data.hdt.perm` is a required artifact (doc 04 §4.1) and hdtc does not
    /// emit it by default. Panics with the command to run if the binary is not
    /// built: a fixture that silently skipped would leave every differential
    /// test in this crate passing vacuously.
    pub fn build(source: &str) -> Self {
        Self::build_with(source, false)
    }

    /// Build a quad bundle with its graph sidecar and graph index.
    pub fn build_quads(source: &str) -> Self {
        Self::build_with(source, true)
    }

    /// Add a full-text index over this bundle's literals.
    ///
    /// A second `hdtc` invocation rather than a flag on the first, because that
    /// is how a bundle acquires one: `hdtc text` runs over a built HDT, and its
    /// default output path is the bundle's `data.hdt.text` (doc 04 §4.1). A
    /// separate step is also what lets a test have the same graph with and
    /// without `search`, which is what the capability gate needs.
    #[must_use]
    pub fn with_text(self) -> Self {
        let hdtc = hdtc_binary();
        let output = std::process::Command::new(&hdtc)
            .arg("text")
            .arg(self.hdt_path())
            .output()
            .unwrap_or_else(|error| panic!("run {} text: {error}", hdtc.display()));
        assert!(
            output.status.success(),
            "hdtc text failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            self.dir.path().join(TEXT).is_dir(),
            "hdtc text must write {TEXT} beside the HDT"
        );
        self
    }

    fn build_with(source: &str, graphs: bool) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir
            .path()
            .join(if graphs { "input.nq" } else { "input.nt" });
        std::fs::write(&input, source).expect("write fixture source");

        let hdtc = hdtc_binary();
        let mut command = std::process::Command::new(&hdtc);
        command.args([
            "create",
            input.to_str().unwrap(),
            "-o",
            dir.path().join(HDT).to_str().unwrap(),
            "--temp-dir",
            dir.path().join("work").to_str().unwrap(),
            "--memory-limit",
            "64M",
            "--perm",
        ]);
        if graphs {
            command.args(["--mode", "quads", "--graphs-index"]);
        }
        let output = command
            .output()
            .unwrap_or_else(|e| panic!("run {}: {e}", hdtc.display()));
        assert!(
            output.status.success(),
            "hdtc create failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::write(dir.path().join(MANIFEST), b"{}\n").expect("write fixture manifest");

        Self { dir }
    }

    /// Root of this complete minimal bundle.
    #[cfg(test)]
    pub(crate) fn bundle_path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Copy the fixture's required and present optional artifacts into a bundle.
    ///
    /// Public, unlike the path accessors beside it, and safely so: this *writes*
    /// to a directory the caller already owns rather than handing out a path to
    /// bytes this fixture has mapped. An out-of-crate test that needs a catalog
    /// — a whole `{root}/{dataset}/{version}` tree — has no other way to build
    /// one from a golden bundle.
    pub fn copy_bundle_to(&self, destination: &std::path::Path) {
        std::fs::create_dir_all(destination).expect("create fixture bundle directory");
        for name in [MANIFEST, HDT, PERM, GRAPHS, GRAPHS_IDX] {
            if !self.dir.path().join(name).exists() {
                continue;
            }
            std::fs::copy(self.dir.path().join(name), destination.join(name))
                .unwrap_or_else(|error| panic!("copy fixture artifact {name}: {error}"));
        }
        // The one artifact that is a directory (doc 04 §4.1).
        let text = self.dir.path().join(TEXT);
        if text.is_dir() {
            copy_dir(&text, &destination.join(TEXT));
        }
    }

    /// Path of `data.hdt`.
    pub(crate) fn hdt_path(&self) -> std::path::PathBuf {
        self.dir.path().join(HDT)
    }

    /// Path of `data.hdt.perm`.
    pub(crate) fn perm_path(&self) -> std::path::PathBuf {
        self.dir.path().join(PERM)
    }

    /// Publish the complete physical description set around caller-supplied
    /// TSV bytes. Manifest metadata stays with the test that exercises it.
    #[cfg(test)]
    pub(crate) fn add_description_artifacts(&self, schema_nodes: &[u8], class_relations: &[u8]) {
        let bundle = self.bundle_path();
        std::fs::create_dir_all(bundle.join("stats")).expect("create fixture stats directory");
        std::fs::copy(
            self.hdt_path(),
            bundle.join(crate::store::artifact::VOID_HDT),
        )
        .expect("copy fixture VoID HDT");
        std::fs::copy(
            self.perm_path(),
            bundle.join(crate::store::artifact::VOID_PERM),
        )
        .expect("copy fixture VoID permutations");
        for (name, bytes) in [
            (crate::store::artifact::SCHEMA_NODES, schema_nodes),
            (crate::store::artifact::CLASS_RELATIONS, class_relations),
            (
                crate::store::artifact::NAMESPACES,
                NAMESPACES_JSON.as_bytes(),
            ),
            (
                crate::store::artifact::SUMMARY_JSON,
                SUMMARY_JSON.as_bytes(),
            ),
            (
                crate::store::artifact::SUMMARY_MD,
                b"# Summary\n".as_slice(),
            ),
        ] {
            std::fs::write(bundle.join(name), bytes)
                .unwrap_or_else(|error| panic!("write fixture artifact {name}: {error}"));
        }
    }

    /// Map `data.hdt`.
    pub fn map_hdt(&self) -> crate::map::Mapping {
        map_fixture(&self.hdt_path())
    }

    /// Map `data.hdt.perm`.
    pub fn map_perm(&self) -> crate::map::Mapping {
        map_fixture(&self.perm_path())
    }

    /// Run hdtc's independent search path and return its non-empty output rows.
    pub fn search(&self, query: &str) -> Vec<Vec<u8>> {
        let hdtc = hdtc_binary();
        let output = std::process::Command::new(&hdtc)
            .arg("search")
            .arg(self.hdt_path())
            .arg("--query")
            .arg(query)
            .output()
            .unwrap_or_else(|error| panic!("run {} search: {error}", hdtc.display()));
        assert!(
            output.status.success(),
            "hdtc search failed for {query}:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    }
}

/// Exact selector-index header from doc 04 §4.2.
#[cfg(test)]
pub(crate) const SCHEMA_NODES_HEADER: &[u8] =
    b"view\tkind\tclass\tpredicate\tdatatype\tsubject_id\n";

/// Exact class-relation header from doc 04 §4.2.
#[cfg(test)]
pub(crate) const CLASS_RELATIONS_HEADER: &[u8] =
    b"view\tsubject_class\tpredicate\tobject_class\ttriples\n";

#[cfg(test)]
pub(crate) const NAMESPACES_JSON: &str = concat!(
    "{\n",
    "  \"prefix_table\": {\"source\": \"fixture\", ",
    "\"version\": \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"},\n",
    "  \"roles\": {\n",
    "    \"subject\": {\"distinct_iris\": 0, \"matched\": 0, \"residual\": 0},\n",
    "    \"predicate\": {\"distinct_iris\": 0, \"matched\": 0, \"residual\": 0},\n",
    "    \"object\": {\"distinct_iris\": 0, \"matched\": 0, \"residual\": 0}\n",
    "  },\n",
    "  \"namespaces\": []\n",
    "}\n",
);

#[cfg(test)]
pub(crate) const SUMMARY_JSON: &str = "{\n  \"title\": \"Fixture summary\"\n}\n";

/// Copy a directory artifact, recursively.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create fixture artifact directory");
    for entry in std::fs::read_dir(from).expect("read fixture artifact directory") {
        let entry = entry.expect("read fixture directory entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture artifact file");
        }
    }
}

const HDT: &str = crate::store::artifact::HDT;
const GRAPHS: &str = crate::store::artifact::GRAPHS;
const GRAPHS_IDX: &str = crate::store::artifact::GRAPHS_IDX;
const MANIFEST: &str = crate::store::artifact::MANIFEST;
const PERM: &str = crate::store::artifact::PERM;
const TEXT: &str = crate::store::artifact::TEXT;

/// Locate the `hdtc` binary: `$KGF_HDTC` if set, else the sibling checkout's
/// build. hdtc is a path dependency of this crate, so its checkout is where
/// `CLAUDE.md` says the three siblings are.
///
/// Public because tests in other crates build bundles too, and a second copy of
/// this search is a second thing to fix when the layout moves.
pub fn hdtc_binary() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("KGF_HDTC") {
        return std::path::PathBuf::from(path);
    }
    let hdtc = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../hdtc")
        .canonicalize()
        .expect("../hdtc sibling checkout");
    for profile in ["release", "debug"] {
        let candidate = hdtc.join("target").join(profile).join("hdtc");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no hdtc binary under {}; build it with \
         `cargo build --release --manifest-path {}/Cargo.toml`, \
         or point $KGF_HDTC at one",
        hdtc.join("target").display(),
        hdtc.display()
    );
}
