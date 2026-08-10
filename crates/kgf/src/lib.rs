//! The `kgf` tool's commands, as a library.
//!
//! The binary is a thin `clap` shell over this; the commands live here so they
//! can be driven from an integration test without a subprocess. That matters
//! for [`serve`] in particular, whose test needs a listener, a catalog and a
//! [`PublishedRoot`](kgf_store::map::PublishedRoot) — the last of which only
//! this crate may produce.
//!
//! This crate is separate from `kgf-server` because it will eventually drive
//! both the server and the build pipeline, and reaching `kgf-build` from inside
//! the server crate would point the dependency backwards.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod build;
pub mod manifest;
pub mod serve;
