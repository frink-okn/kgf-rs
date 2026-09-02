//! Store errors.
//!
//! The distinction that matters here is between *this bundle is not servable*
//! and *this request cannot be answered*. The first case
//! loud: a bundle missing a required artifact is refused at open with a message
//! naming what to build, because there is no degraded mode to fall into.

use std::path::PathBuf;
use std::sync::Arc;

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong opening or reading a bundle.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required artifact is absent. There is no fallback: the bundle is not
    /// servable until it is built.
    #[error("bundle {bundle} is missing required artifact {artifact}; build it with `{remedy}`")]
    MissingRequiredArtifact {
        /// The bundle directory.
        bundle: PathBuf,
        /// The artifact's file name.
        artifact: String,
        /// The command that produces it.
        remedy: String,
    },

    /// A sidecar is present but does not belong to this HDT — its recorded
    /// suffix length, triple count, or digest disagrees.
    #[error("{artifact} does not bind to {hdt}: {detail}")]
    ArtifactBindingMismatch {
        /// The sidecar that failed to bind.
        artifact: PathBuf,
        /// The HDT it should have bound to.
        hdt: PathBuf,
        /// Which check failed.
        detail: String,
    },

    /// A file's contents are not what its header says they are.
    #[error("malformed {artifact}: {detail}")]
    Malformed {
        /// The offending file.
        artifact: PathBuf,
        /// What was wrong.
        detail: String,
    },

    /// A mapped region does not match the shape its descriptor claims.
    ///
    /// Carries no path: this is raised deep in [`crate::map`], where only the
    /// region and its declared shape are known. Callers that do know which file
    /// a region came from should wrap it in [`Error::Malformed`].
    #[error("region does not match its descriptor: {0}")]
    Region(String),

    /// A role-scoped dictionary id is zero or beyond that role's id space.
    #[error("{role:?} term id {id} is out of range (valid ids: 1..={maximum})")]
    TermIdOutOfRange {
        /// The id space the caller addressed.
        role: crate::Role,
        /// The rejected id.
        id: u64,
        /// The largest valid id in that role.
        maximum: u64,
    },

    /// A decoded continuation position falls outside its enumeration.
    /// Position zero is the initial page; nonzero resumptions must name a row
    /// inside the immutable enumeration.
    #[error("resume position {position} is outside an enumeration of {length} rows")]
    ResumePositionOutOfRange {
        /// Rejected zero-based position.
        position: u64,
        /// Number of rows in the addressed enumeration.
        length: u64,
    },

    /// A catalog lookup named no scanned dataset/version directory.
    #[error("unknown bundle {dataset}/{version}")]
    UnknownBundle {
        /// Dataset identifier from the lookup.
        dataset: String,
        /// Version identifier from the lookup.
        version: String,
    },

    /// A dataset or version directory name cannot be represented by the API.
    #[error("bundle path is not valid UTF-8: {path}")]
    NonUtf8BundlePath {
        /// The directory whose final component is not UTF-8.
        path: PathBuf,
    },

    /// A lazily opened catalog entry failed. Deterministic failures retain the
    /// same `source` until eviction; transient I/O failures may be retried.
    #[error("opening bundle {bundle}: {source}")]
    BundleOpen {
        /// The scanned bundle directory.
        bundle: PathBuf,
        /// The classified store-open failure for this version.
        #[source]
        source: Arc<Error>,
    },

    /// A manifest is not JSON or does not have the required schema.
    #[error("manifest {path} is not a readable bundle manifest: {detail}")]
    ManifestSyntax {
        /// The manifest file.
        path: PathBuf,
        /// What the parser objected to.
        detail: String,
    },

    /// A manifest declares a schema version this build does not read.
    ///
    /// Refused rather than parsed best-effort: `formats.manifest` exists so a
    /// reader can tell "fields I don't know about" (fine, and ignored) from
    /// "fields whose meaning changed" (not fine, and undetectable field by
    /// field).
    #[error(
        "manifest {path} declares manifest format {found}, but this build reads format {supported}"
    )]
    UnsupportedManifestFormat {
        /// The manifest file.
        path: PathBuf,
        /// The version it declares.
        found: String,
        /// The version this build understands.
        supported: String,
    },

    /// A manifest no longer describes the artifacts beside it.
    #[error(
        "manifest {path} records {field} = {recorded}, but the artifacts contain {actual}; \
         regenerate it with `{remedy}`"
    )]
    ManifestDisagreement {
        /// The manifest file.
        path: PathBuf,
        /// The field that disagrees, in dotted form.
        field: String,
        /// What the manifest claims.
        recorded: u64,
        /// What the artifacts actually contain.
        actual: u64,
        /// The command that rewrites the manifest.
        remedy: String,
    },

    /// Anything the OS refused.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Anything hdtc's format layer refused.
    #[error(transparent)]
    Format(#[from] anyhow::Error),
}

impl Error {
    /// Whether this failure could resolve on its own, without the bundle
    /// changing.
    ///
    /// A published version is immutable, so a failure about the
    /// *bytes* — a truncated sidecar, a header that disagrees with its own file
    /// — can never heal; a caller that caches failures should keep it until the
    /// entry is evicted. A failure about this process's ability to read at all
    /// — descriptor pressure, a mount that briefly went away — says nothing
    /// about the bundle and may be retried.
    ///
    /// The distinction cannot be drawn from the error's *type*. hdtc's format
    /// layer detects truncation with `read_exact`, so an [`Error::Format`]
    /// chain routinely carries an `io::Error` for a condition that is purely
    /// about content. It is the [`std::io::ErrorKind`] that separates the two.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Io(source) => !describes_content(source),
            Error::Format(source) => source
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                .is_some_and(|source| !describes_content(source)),
            _ => false,
        }
    }
}

/// Whether an OS error describes what a file contains rather than whether it
/// could be read at all.
///
/// Running off the end of a file its own header said was longer, or refusing
/// bytes that cannot mean what they must, is a corrupt artifact — not a busy
/// machine.
fn describes_content(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "test")
    }

    #[test]
    fn truncation_is_deterministic_however_it_is_wrapped() {
        // The case the catalog gets wrong if it classifies by type: a short
        // artifact fails inside hdtc's `read_exact`, so the anyhow chain
        // carries an `io::Error` for a condition an immutable bundle cannot
        // recover from.
        assert!(!Error::Io(io(std::io::ErrorKind::UnexpectedEof)).is_transient());
        assert!(!Error::Io(io(std::io::ErrorKind::InvalidData)).is_transient());

        let wrapped = anyhow::Error::new(io(std::io::ErrorKind::UnexpectedEof))
            .context("reading the section directory")
            .context("opening permutation index data.hdt.perm");
        assert!(!Error::Format(wrapped).is_transient());
    }

    #[test]
    fn process_conditions_are_retryable_however_they_are_wrapped() {
        // Descriptor pressure reaches stable Rust as `Other`/`Uncategorized`
        // rather than a named kind, so the rule is "not about content" rather
        // than an allow-list of exhaustion kinds.
        assert!(Error::Io(std::io::Error::other("descriptor pressure")).is_transient());
        assert!(Error::Io(io(std::io::ErrorKind::PermissionDenied)).is_transient());

        let wrapped = anyhow::Error::new(std::io::Error::other("descriptor pressure"))
            .context("opening graph index data.hdt.graphs.idx");
        assert!(Error::Format(wrapped).is_transient());
    }

    #[test]
    fn structural_refusals_carry_no_io_cause_and_are_deterministic() {
        assert!(
            !Error::Format(anyhow::anyhow!("permutation-index file-size mismatch")).is_transient()
        );
        assert!(
            !Error::Malformed {
                artifact: PathBuf::from("data.hdt"),
                detail: "sections end at 10 but the file is 12 bytes".to_owned(),
            }
            .is_transient()
        );
        assert!(
            !Error::MissingRequiredArtifact {
                bundle: PathBuf::from("bundle"),
                artifact: "data.hdt.perm".to_owned(),
                remedy: "hdtc perm data.hdt".to_owned(),
            }
            .is_transient()
        );
    }
}
