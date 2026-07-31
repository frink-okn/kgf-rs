//! Shared test support.
//!
//! Only the *oracles* in each module's tests must avoid sharing code with the
//! implementation they check. Sharing between test modules is fine, and worth
//! doing: every module still to be written wants a deterministic generator and
//! a way to read a bit out of a byte slice.

/// SplitMix64 — a deterministic generator, so a failure reproduces.
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next value.
    pub fn next(&mut self) -> u64 {
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
///
/// [`Mapping::open`](crate::map::Mapping::open) is `unsafe` because the caller
/// must guarantee the file does not change while mapped. Tests satisfy that by
/// writing a fixture into a `tempdir` and leaving it alone, so the obligation is
/// discharged once here rather than at every call site — keeping the crate's
/// `unsafe` surface to `map` plus this one test-only wrapper.
#[allow(unsafe_code)]
pub fn map_fixture(path: &std::path::Path) -> crate::map::Mapping {
    // SAFETY: the caller has just written this file into a temporary directory
    // and does not modify or truncate it for the mapping's lifetime.
    unsafe { crate::map::Mapping::open(path) }.expect("map fixture")
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
        let dir = tempfile::tempdir().expect("temp dir");
        let input = dir.path().join("input.nt");
        std::fs::write(&input, source).expect("write fixture source");

        let hdtc = hdtc_binary();
        let output = std::process::Command::new(&hdtc)
            .args([
                "create",
                input.to_str().unwrap(),
                "-o",
                dir.path().join(HDT).to_str().unwrap(),
                "--temp-dir",
                dir.path().join("work").to_str().unwrap(),
                "--memory-limit",
                "64M",
                "--perm",
            ])
            .output()
            .unwrap_or_else(|e| panic!("run {}: {e}", hdtc.display()));
        assert!(
            output.status.success(),
            "hdtc create failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        Self { dir }
    }

    /// Path of `data.hdt`.
    pub fn hdt_path(&self) -> std::path::PathBuf {
        self.dir.path().join(HDT)
    }

    /// Path of `data.hdt.perm`.
    pub fn perm_path(&self) -> std::path::PathBuf {
        self.dir.path().join(PERM)
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

const HDT: &str = crate::store::artifact::HDT;
const PERM: &str = crate::store::artifact::PERM;

/// Locate the `hdtc` binary: `$KGF_HDTC` if set, else the sibling checkout's
/// build. hdtc is a path dependency of this crate, so its checkout is where
/// `CLAUDE.md` says the three siblings are.
fn hdtc_binary() -> std::path::PathBuf {
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
