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
    /// It is per invocation and never shared: concurrent `hdtc` processes using
    /// one temp directory have produced key
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

/// One argument, safe to paste into a shell.
///
/// An allowlist, not a denylist. `rehearse` calls its output "a script a person
/// can follow", and an operator pastes it — so a denylist that misses `;`, `&`,
/// `|`, `#`, `*` or `~` does not merely look wrong, it runs something else:
/// `&` backgrounds the command, `#` truncates the line, `*` globs against
/// whatever directory they happen to be in. A dataset IRI with `&` in its query
/// string is enough to trigger it. Only the characters that are inert in every
/// shell context go through bare.
fn quote(argument: &OsStr) -> String {
    let text = argument.to_string_lossy();
    let inert = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !text.is_empty() && text.chars().all(inert) {
        return text.into_owned();
    }
    // Single quotes make every other character literal; the only thing that
    // cannot appear inside them is a single quote, which is closed, escaped,
    // and reopened.
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rehearsal is printed so an operator can retype it. An argument that
    /// changes meaning when pasted makes the whole output worse than useless,
    /// because it no longer describes what `Runner::run` actually executed.
    #[test]
    fn every_shell_metacharacter_is_quoted() {
        for hostile in [
            "https://e.org/a?x=1&y=2",
            "a;rm -rf /",
            "a|b",
            "a>b",
            "glob*",
            "trailing#comment",
            "~/relative",
            "(sub)",
            "back`tick`",
            "dollar$sign",
            "with space",
            "",
        ] {
            let quoted = quote(OsStr::new(hostile));
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "{hostile:?} rendered unquoted as {quoted}"
            );
        }
    }

    /// And ordinary arguments stay readable, or the output is unusable noise.
    #[test]
    fn inert_arguments_are_left_bare() {
        for plain in [
            "hdtc",
            "--max-literal-bytes",
            "4096",
            "/var/bundles/dreamkg/2026-06-01/data.hdt",
            "subjects-only,objects-only,shared",
            "elias-fano",
        ] {
            assert_eq!(quote(OsStr::new(plain)), plain);
        }
    }

    #[test]
    fn a_single_quote_is_closed_escaped_and_reopened() {
        assert_eq!(quote(OsStr::new("it's")), r"'it'\''s'");
    }
}
