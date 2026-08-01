//! The KGF read layer: bundles on disk, answers in id space.
//!
//! This crate is the implementation of **KGF doc 20**. It opens a published
//! bundle, memory-maps its artifacts, and answers triple patterns at the bounds
//! doc 03 §3.5 promises. It contains no HTTP, no async, and no locks on the read
//! path; the server holds an [`Arc<Store>`](std::sync::Arc) per request and calls
//! synchronous methods from a blocking pool.
//!
//! # The three things worth knowing before reading further
//!
//! **Open has bounded, size-independent I/O.** Opening maps files, parses
//! headers, and reads a fixed number of rank-directory sentinels. It never scans
//! payloads, rebuilds indexes, hashes whole files, or materializes structures
//! proportional to bundle size (doc 20 §20.3). An open-but-idle bundle therefore
//! costs address space plus a small fixed metadata working set, which is what
//! makes lazy multi-tenant serving work.
//!
//! **Id-space in, id-space through, strings at the edges.** Every operation
//! resolves terms to ids once at the boundary, runs entirely over ids, and
//! materializes strings only while serializing. Term caches belong to the
//! server, not here.
//!
//! **One implementation per operation.** There is no fallback path for a missing
//! or superseded index (doc 20 §20.8). A bundle without `data.hdt.perm` is
//! refused at open; `.hdt.index.v1-1` is never read; `data.hdt.graphs` and
//! `data.hdt.graphs.idx` must occur together. What looks like a fallback in [`pattern`] —
//! `s ? o` probing whichever endpoint is smaller — is one algorithm making a
//! cost decision, and both routes emit in the same order and resume from the
//! same cursor.
//!
//! # Status
//!
//! The mapped query core is implemented: immutable store opening, dictionary and
//! permutation traversal, all eight triple patterns, and the lazy multi-tenant
//! catalog. Composed optional-sidecar operations and the HTTP layer are later
//! milestones in their owning modules/crates.

#![deny(unsafe_code)]
#![warn(missing_docs)]

// The only module permitted to map memory. Keeping the `unsafe` surface to one
// audited file is a doc 20 §20.9 obligation, not a style preference.
#[allow(unsafe_code)]
pub mod map;

pub mod catalog;
pub mod dict;
pub mod error;
pub mod hdt;
pub mod manifest;
pub mod pattern;
pub mod perm;
pub mod rank;
pub mod store;

#[cfg(test)]
mod testing;

pub use catalog::Catalog;
pub use error::{Error, Result};
pub use manifest::{BundleFacts, Capability, Manifest};
pub use map::{PublishedBundle, PublishedRoot};
pub use store::{OpenOptions, Store};

/// A term identifier in one of HDT's role-scoped id spaces.
///
/// Ids are 1-based and scoped by [`Role`]: the same integer means a different
/// term as a subject than as an object, except within the shared section, where
/// subject and object ids coincide by construction. Nothing outside [`dict`]
/// should reason about that overlap — ask the dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermId(pub u64);

/// Which of HDT's identifier spaces a [`TermId`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Subjects: the shared section followed by subject-only terms.
    Subject,
    /// Predicates: their own space.
    Predicate,
    /// Objects: the shared section followed by object-only terms.
    Object,
}

/// A triple in id space, the unit everything below the serialization edge deals in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdTriple {
    /// Subject id, in [`Role::Subject`]'s space.
    pub subject: u64,
    /// Predicate id, in [`Role::Predicate`]'s space.
    pub predicate: u64,
    /// Object id, in [`Role::Object`]'s space.
    pub object: u64,
}
