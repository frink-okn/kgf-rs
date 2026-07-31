//! Store errors.
//!
//! The distinction that matters here is between *this bundle is not servable*
//! and *this request cannot be answered*. Doc 20 §20.8 makes the first case
//! loud: a bundle missing a required artifact is refused at open with a message
//! naming what to build, because there is no degraded mode to fall into.

use std::path::PathBuf;

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong opening or reading a bundle.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required artifact is absent. There is no fallback: the bundle is not
    /// servable until it is built (doc 04 §4.1, doc 20 §20.8).
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

    /// A cursor was issued against different data or a different request.
    #[error("stale cursor")]
    StaleCursor,

    /// Anything the OS refused.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Anything hdtc's format layer refused.
    #[error(transparent)]
    Format(#[from] anyhow::Error),
}
