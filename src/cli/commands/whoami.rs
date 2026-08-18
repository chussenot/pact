use anyhow::Result;
use std::path::Path;

use crate::cli::util::table;
use crate::{beads, identity, output, repo};

/// Everything pact resolved about its own environment. Every field is optional
/// on purpose: `whoami` is what you run *because* something is broken, so it
/// must never be the thing that fails (pact-rnc.12). Problems are collected and
/// reported, not raised.
#[derive(serde::Serialize)]
struct Whoami {
    /// The resolved identity — `None` when unset or invalid; see `problems`.
    agent: Option<String>,
    /// Where the identity came from: `--agent`, `PACT_AGENT`, or `unset`.
    agent_source: &'static str,
    pact_binary: Option<String>,
    repo_root: Option<String>,
    pact_dir: Option<String>,
    bd_binary: Option<String>,
    bd_version: Option<String>,
    problems: Vec<String>,
}

/// `pact-juz.2`: `pact`'s own `--actor=<agent>` attribution (docs/messaging.md)
/// only covers bd calls pact itself makes — `lease`/`msg`'s own mutations. It
/// never reaches `bd ready`/`bd update --claim`/`bd close`, which AGENTS.md's
/// managed Quick Reference tells every agent to run directly. Confirmed on a
/// real 15-agent build: every one of 16 `.beads/interactions.jsonl` entries
/// attributed to the operator's own `git user.name`, none to any of the 16
/// distinct `agent-*` identities `.pact/events.jsonl` correctly tracked for
/// the same run — because none of those direct `bd` calls carried `--actor`
/// or had `BEADS_ACTOR` set, so bd fell through to its own next precedence
/// tier. `--actor` > `$BEADS_ACTOR` > `git user.name` > `$USER` is bd's own
/// documented order (docs/messaging.md); this is the copy-pasteable fix for
/// the middle tier, not a new mechanism pact invented.
fn beads_actor_hint(agent: Option<&str>, repo_root: Option<&Path>) -> Option<String> {
    let agent = agent?;
    // Gated on .beads/ existing, same reasoning as the messaging-check noise
    // fix: a repo that never opted into Beads task tracking should not be
    // told to configure an env var for a system it does not use.
    if !repo_root.is_some_and(|r| r.join(".beads").exists()) {
        return None;
    }
    Some(format!("export BEADS_ACTOR={agent}"))
}

pub(in crate::cli) fn run_whoami(cwd: &Path, agent_flag: Option<&str>, json: bool) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();

    let agent_source = match agent_flag {
        Some(_) => "--agent",
        None if std::env::var_os("PACT_AGENT").is_some() => "PACT_AGENT",
        None => "unset",
    };
    // resolve_agent's error already quotes the offending value, so a rejected
    // PACT_AGENT shows up as a problem rather than silently reading as "none".
    let agent = match identity::resolve_agent(agent_flag) {
        Ok(a) => Some(a),
        Err(e) => {
            problems.push(format!("{e:#}"));
            None
        }
    };

    let root = match repo::find_repo_root(cwd) {
        Ok(r) => Some(r),
        Err(e) => {
            problems.push(format!("{e:#}"));
            None
        }
    };
    // Deliberately does not create it: whoami is a read-only question. Resolved
    // rather than joined, so a linked worktree reports the SHARED directory it
    // will really use — "where is my state" answered with a path pact does not
    // write to would be worse than not answering.
    let pact_dir = root.as_ref().map(|r| repo::pact_dir_path(r));

    let (bd_binary, bd_version) = match beads::BeadsCli::locate() {
        Ok(bd) => {
            let version = match root.as_deref() {
                Some(r) => {
                    let (version, found) = bd_health(&bd, r);
                    problems.extend(found);
                    version
                }
                None => None,
            };
            (Some(bd.binary().to_string()), version)
        }
        Err(e) => {
            problems.push(format!("{e:#}"));
            (None, None)
        }
    };

    // Computed before `agent`/`root` move into `info`, and deliberately not a
    // Whoami field: it's a ready-to-run shell line, not new data — anything
    // scripting against --json already has `agent` and can build it itself.
    let actor_hint = beads_actor_hint(agent.as_deref(), root.as_deref());

    let info = Whoami {
        agent,
        agent_source,
        pact_binary: std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
        repo_root: root.as_ref().map(|r| r.display().to_string()),
        pact_dir: pact_dir.as_ref().map(|p| p.display().to_string()),
        bd_binary,
        bd_version,
        problems,
    };

    let dir_missing = pact_dir.map(|p| !p.exists()).unwrap_or(false);
    output::emit(json, &info, move |i: &Whoami| {
        let mut rows = vec![vec![
            "agent".to_string(),
            match &i.agent {
                Some(a) => format!("{a}  (from {})", i.agent_source),
                None => "(none)".to_string(),
            },
        ]];
        rows.push(vec![
            "pact".to_string(),
            i.pact_binary.clone().unwrap_or_else(|| "?".to_string()),
        ]);
        rows.push(vec![
            "repo root".to_string(),
            i.repo_root.clone().unwrap_or_else(|| "(none)".to_string()),
        ]);
        rows.push(vec![
            "pact dir".to_string(),
            match (&i.pact_dir, dir_missing) {
                (Some(p), true) => format!("{p}  (not created yet)"),
                (Some(p), false) => p.clone(),
                (None, _) => "(none)".to_string(),
            },
        ]);
        rows.push(vec![
            "bd".to_string(),
            match (&i.bd_binary, &i.bd_version) {
                (Some(b), Some(v)) => format!("{b}  ({v})"),
                (Some(b), None) => b.clone(),
                (None, _) => "(not found)".to_string(),
            },
        ]);
        let mut out = table(&rows);
        if let Some(hint) = &actor_hint {
            out.push_str(&format!(
                "\n\n{hint}  — so bd commands you run directly (bd update --claim, \
                 bd close, ...) attribute to you, not whoever's git identity owns this checkout"
            ));
        }
        for p in &i.problems {
            out.push_str(&format!("\n! {p}"));
        }
        out
    });
    Ok(())
}

/// bd's version plus whatever is wrong with it, for `whoami` to report.
///
/// Just `--version` since 0.9.0 (pact-as5.5). It used to also run `bd list
/// --json` and report "bd cannot read this repo's beads database, so `pact msg`
/// and the message half of `pact agents` will fail" (pact-rnc.12) — a sentence
/// that is now simply false: messages live in `.pact/messages.jsonl` and no pact
/// command asks bd anything. A diagnostic that lies about the thing you ran it to
/// diagnose is the exact defect that probe was introduced to fix, so it goes
/// rather than gets reworded, and `pact whoami` spawns one subprocess instead of
/// two. Whether the agents' own `bd` commands work is a question `bd` answers.
fn bd_health(bd: &beads::BeadsCli, root: &Path) -> (Option<String>, Vec<String>) {
    match bd.version(root) {
        Ok(v) => (Some(v), Vec::new()),
        Err(e) => (None, vec![format!("bd found but not runnable: {e:#}")]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Changed by pact-as5.5, deliberately, replacing
    /// `bd_health_probes_the_workspace_not_just_the_binary`.** That test asserted
    /// the opposite: `bd_health` also ran `bd list --json` and reported "bd cannot
    /// read this repo's beads database, so `pact msg` … will fail" (pact-rnc.12),
    /// with `git` standing in for a bd that answers `--version` and nothing else.
    /// The claim it pinned is now false — messages are pact's own file and no pact
    /// command asks bd anything — so what must be pinned instead is that `whoami`
    /// reports the version and invents no problem out of a workspace it no longer
    /// cares about. `git` still stands in: it answers `--version`, and could not
    /// answer a bd query if one were ever put back.
    #[test]
    fn bd_health_reports_the_version_and_no_longer_probes_the_workspace() {
        let root = std::env::current_dir().unwrap();
        let (version, problems) = bd_health(&beads::BeadsCli { binary: "git" }, &root);
        assert!(
            version.is_some(),
            "--version answered, so bd looks installed"
        );
        assert!(problems.is_empty(), "{problems:?}");
    }
}
