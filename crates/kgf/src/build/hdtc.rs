//! Invoking hdtc, with the settings every step shares.
//!
//! One invoker rather than one per producer. The build's own steps and the
//! description producer's both spend a bundle's configured memory limit, and a
//! second call site that quietly skipped it would stay invisible until a large
//! graph hit it.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, ensure};

/// One hdtc invocation.
pub(crate) struct Step {
    /// What this step is called in logs and error messages.
    pub(crate) name: &'static str,
    /// The full argument list, less the settings [`Runner`] adds to every step.
    pub(crate) args: Vec<OsString>,
    /// A private temporary directory, for the steps that sort to disk.
    pub(crate) temp: Option<&'static str>,
}

/// Invokes hdtc with the settings every step shares.
pub(crate) struct Runner<'a> {
    pub(crate) hdtc: &'a Path,
    pub(crate) work: &'a Path,
    pub(crate) memory_limit: Option<String>,
}

impl Runner<'_> {
    /// Run one hdtc step.
    ///
    /// `temp` names a private temporary directory for steps that sort to disk.
    /// It is per invocation and never shared: doc 18 §18.4 records a build in
    /// which concurrent `hdtc` processes sharing one temp directory produced key
    /// sets that were structurally perfect — correct checksums, correct source
    /// digest, ascending keys — and held another graph's keys.
    pub(crate) fn run(&self, step: &Step) -> Result<()> {
        if let Some(temp) = step.temp {
            let dir = self.work.join(temp);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let argv = self.hdtc_argv(step);
        let command = render(&argv);

        tracing::info!(step = step.name, %command, "running hdtc");
        let started = Instant::now();
        let status = Command::new(self.hdtc)
            .args(&argv[1..])
            .status()
            .with_context(|| format!("running hdtc for {}: {command}", step.name))?;
        ensure!(
            status.success(),
            "hdtc {} failed with {status}: {command}",
            step.name
        );
        tracing::info!(step = step.name, elapsed = ?started.elapsed(), "hdtc step complete");
        Ok(())
    }

    /// The full argument vector: the program, the step, and the settings every
    /// step shares.
    ///
    /// Shared with the rehearsal so `--dry-run` prints the command that would
    /// actually run rather than a second construction of it that can drift.
    pub(crate) fn hdtc_argv(&self, step: &Step) -> Vec<OsString> {
        let mut argv = vec![self.hdtc.as_os_str().to_owned()];
        argv.extend(step.args.iter().cloned());
        if let Some(temp) = step.temp {
            argv.push(OsString::from("--temp-dir"));
            argv.push(self.work.join(temp).into_os_string());
        }
        if let Some(limit) = &self.memory_limit {
            argv.push(OsString::from("--memory-limit"));
            argv.push(OsString::from(limit));
        }
        argv.push(OsString::from("--quiet"));
        argv
    }
}

/// One invocation, spelled the way a person would retype it.
pub(super) fn render(argv: &[OsString]) -> String {
    argv.iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote(argument: &OsStr) -> String {
    let text = argument.to_string_lossy();
    if text.is_empty() || text.contains(|c: char| c.is_whitespace() || "'\"\\$`".contains(c)) {
        format!("'{}'", text.replace('\'', r"'\''"))
    } else {
        text.into_owned()
    }
}
