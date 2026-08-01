//! The `kgf` binary: `kgf serve` and `kgf manifest` today, `kgf build` once
//! `kgf-build` exists.
//!
//! This crate stays thin. It is separate from `kgf-server` because it will
//! eventually drive both the server and the build pipeline, and reaching
//! `kgf-build` from inside the server crate would point the dependency
//! backwards.

#![deny(unsafe_code)]

mod manifest;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Knowledge Graph Fragments: a bounded-cost query interface over RDF bundles.
#[derive(Debug, Parser)]
#[command(name = "kgf", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Parsed once per process and immediately consumed, so the size spread between
// a two-field `serve` and a description-carrying `manifest` costs nothing.
// Boxing is not the alternative here in any case: clap's derive needs the
// variant's payload to implement `Args`, which `Box<_>` does not.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Serve bundles over the KGF HTTP API.
    Serve(ServeArgs),

    /// Write a bundle's manifest.json from its artifacts.
    ///
    /// A stand-in for the manifest step of `kgf build`, for bundles assembled
    /// by hand with `hdtc create --perm`.
    Manifest(manifest::Args),
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Directory of bundles, laid out as {root}/{dataset}/{version}.
    #[arg(long)]
    bundle_root: PathBuf,

    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve(_args) => {
            todo!("run kgf_server::serve on an async runtime once doc 03's routes exist")
        }
        Command::Manifest(args) => manifest::run(args),
    }
}
