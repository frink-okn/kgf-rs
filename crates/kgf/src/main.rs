//! The `kgf` binary: `kgf serve` and `kgf manifest` today, `kgf build` once
//! `kgf-build` exists.
//!
//! Argument parsing and logging setup only; every command's body is in the
//! library beside this (`kgf::manifest`, `kgf::serve`).

#![deny(unsafe_code)]

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
    Serve(kgf::serve::Args),

    /// Write a bundle's manifest.json from its artifacts.
    ///
    /// A stand-in for the manifest step of `kgf build`, for bundles assembled
    /// by hand with `hdtc create --perm`.
    Manifest(kgf::manifest::Args),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve(args) => {
            install_logging();
            kgf::serve::run(args)
        }
        // Deliberately unlogged: a CLI that describes a bundle reports through
        // its exit status and its own output, and a `tracing` line on stderr
        // would be noise in a build script.
        Command::Manifest(args) => kgf::manifest::run(args),
    }
}

/// Structured logs on stderr, filtered by `RUST_LOG` (default `info`).
fn install_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
