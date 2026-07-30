//! Subprocess adapter over the Beads CLI. bd-only for v0.1.0 — br
//! (beads-rust) compatibility is a deliberate later phase, not this one.
//! Never reads/writes the Beads database or JSONL directly; always shells out.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::output::exit_with;

pub struct BeadsCli {
    binary: &'static str,
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
}
