//! `pact completion <shell>` — the completion script, on stdout.
//!
//! Owns the guarantee that completions cannot drift from the parser: they are
//! generated from `Cli::command()`, the same tree clap parses arguments with,
//! so a new flag is completed the moment it is accepted. See [`run_completion`]
//! for why that is worth a command instead of five checked-in scripts.

use anyhow::Result;
use clap::CommandFactory;

use crate::cli::Cli;
use crate::output;

/// `pact completion <shell>`: the completion script, on stdout.
///
/// Generated from `Cli::command()` — the same tree clap parses arguments with
/// — rather than hand-written per shell. That is the whole reason this exists
/// as a command instead of five checked-in scripts: pact has 23 commands and
/// 23 long flags, and a checked-in script drifts silently the moment one is
/// added. `scripts/check-docs.sh` exists because exactly that happened to the
/// docs; completions have the same failure mode and this is the version of
/// that fix which needs no CI to enforce it.
///
/// Through `output::line`, not `println!`, like every other surface: a closed
/// pipe (`pact completion bash | head -1`) must not panic after the work is
/// done. `clap_complete` writes into a `Vec<u8>` first so nothing reaches the
/// real stdout until the whole script exists.
pub(in crate::cli) fn run_completion(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, name, &mut buf);
    // Lossy rather than strict: a completion script is text by construction,
    // and refusing to print one because a description held an unexpected byte
    // would be a worse outcome than printing that byte as U+FFFD.
    output::line(String::from_utf8_lossy(&buf).trim_end());
    Ok(())
}
