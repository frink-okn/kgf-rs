//! Offline bundle producers.

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
    /// Build the complete Tier-1 description/statistics artifact set.
    Stats(stats::Args),
}

/// Run `kgf build`.
pub fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Stats(args) => stats::run(args),
    }
}
