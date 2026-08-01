//! Subprocess adapter over the Beads CLI. v0.1.0 targets bd-only; br
//! (beads-rust) compatibility is still a later phase, but this adapter already
//! stays on the shared subprocess + JSON surface.
//! Never reads/writes the Beads database or JSONL directly; always shells out.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::output::exit_with;

const TESTED_BD_MIN: (u64, u64, u64) = (1, 1, 0);
const TESTED_BD_MAX_EXCLUSIVE: (u64, u64, u64) = (1, 2, 0);

pub struct BeadsCli {
    // pub(crate) so tui.rs's tests can construct one directly without a real
    // `bd` on PATH, instead of adding a test-only constructor for one field.
    pub(crate) binary: &'static str,
}

impl BeadsCli {
    /// Locate `bd` on PATH. Exit code 3 if not found.
    pub fn locate() -> Result<Self> {
        which("bd")
            .map(|_| BeadsCli { binary: "bd" })
            .ok_or_else(|| {
                exit_with(
                    3,
                    "bd (beads) not found on PATH — install it: https://github.com/gastownhall/beads",
                )
            })
    }

    pub fn binary(&self) -> &'static str {
        self.binary
    }

    /// `bd --version` output, trimmed, for `pact doctor`.
    pub fn version(&self, repo_root: &Path) -> Result<String> {
        Ok(self.run(repo_root, &["--version"])?.trim().to_string())
    }

    /// Run `bd <args>` in `repo_root`, capturing stdout; stderr is surfaced on failure.
    pub fn run(&self, repo_root: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new(self.binary)
            .args(args)
            .current_dir(repo_root)
            .output()
            .with_context(|| format!("spawning {} {:?}", self.binary, args))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "{} {:?} failed ({}): {}",
                self.binary,
                args,
                output.status,
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Warning text for versions outside pact's tested bd range.
pub fn version_compat_warning(version_output: &str) -> Option<String> {
    let parsed = parse_triplet(version_output)?;
    if parsed >= TESTED_BD_MIN && parsed < TESTED_BD_MAX_EXCLUSIVE {
        None
    } else {
        Some(format!(
            "outside tested range {}.{}.{} <= version < {}.{}.{}",
            TESTED_BD_MIN.0,
            TESTED_BD_MIN.1,
            TESTED_BD_MIN.2,
            TESTED_BD_MAX_EXCLUSIVE.0,
            TESTED_BD_MAX_EXCLUSIVE.1,
            TESTED_BD_MAX_EXCLUSIVE.2
        ))
    }
}

fn parse_triplet(s: &str) -> Option<(u64, u64, u64)> {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|p| !p.is_empty())
        .find_map(|part| {
            let mut it = part.split('.');
            let major = it.next()?.parse().ok()?;
            let minor = it.next()?.parse().ok()?;
            let patch = it.next()?.parse().ok()?;
            if it.next().is_some() {
                return None;
            }
            Some((major, minor, patch))
        })
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_a_real_binary_on_path() {
        // `sh` is a safe cross-platform stand-in; asserts the PATH-walking
        // logic itself works without depending on `bd` being installed here.
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn detects_versions_outside_the_tested_range() {
        assert_eq!(version_compat_warning("bd version 1.1.0"), None);
        assert_eq!(version_compat_warning("bd 1.1.9"), None);
        assert!(version_compat_warning("bd version 1.2.0")
            .unwrap()
            .contains("outside tested range"));
        assert!(version_compat_warning("bd version 0.9.0")
            .unwrap()
            .contains("outside tested range"));
    }

    #[test]
    fn ignores_unparseable_versions() {
        assert_eq!(version_compat_warning("beads unknown"), None);
    }
}
