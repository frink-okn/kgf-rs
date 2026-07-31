//! The `kgf` binary: `kgf serve` today, `kgf build` once `kgf-build` exists.
//!
//! This crate stays thin. It is separate from `kgf-server` because it will
//! eventually drive both the server and the build pipeline, and reaching
//! `kgf-build` from inside the server crate would point the dependency
//! backwards.

fn main() -> anyhow::Result<()> {
    todo!("parse subcommands; `serve` calls kgf_server::serve")
}
