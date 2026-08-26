//! Offline bundle producers.

pub mod bundle;
pub mod stats;

use anyhow::Result;

/// Arguments for `kgf build`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Artifact family to build.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Assemble and publish a complete bundle from one config.
    Bundle(bundle::Args),

    /// Build the complete Tier-1 description/statistics artifact set.
    Stats(stats::Args),
}

/// Run `kgf build`.
pub fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Bundle(args) => bundle::run(args),
        Command::Stats(args) => stats::run(args),
    }
}
