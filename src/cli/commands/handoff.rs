//! `pact handoff` — what you learned, left where the next agent will look.
//!
//! # The problem, from the field
//!
//! In an orchestratorless run nobody is holding the shape of the work. An agent
//! finishes a bead, closes it, and exits — and everything it learned exits with
//! it. The agent that picks up the dependent bead an hour later starts from the
//! bead description, the diff, and whatever it can infer. Knowledge transferred
//! through prose in a commit message, or through luck.
//!
//! Messaging did not solve this, and could not, for a reason that has nothing to
//! do with messaging: **`pact msg send` needs a recipient, and the recipient does
//! not exist yet.** The agent who will inherit this work has not been spawned. So
//! a handoff is addressed to the WORK — `bead:<dependent-id>` — which outlives
//! everyone, the way `--to-owner-of` addresses a path that outlives its holder.
//!
//! # Inheritance, not ceremony
//!
//! It never blocks, never gates a close, and nothing waits on it. A bead with
//! nothing worth saying sends nothing at all, and that is a legitimate outcome
//! rather than a lapse — `pact audit --check handoff-coverage` reports where it
//! happened and calls it a smell, not a failure.
//!
//! That restraint is deliberate and measured. The protocol's own history is a
//! list of ceremonies agents did not perform: one renewal in 153 events, 4
//! messages between 28 agents across three runs. Anything that costs a turn and
//! produces nothing the agent was asked for does not get done. A handoff survives
//! only if it is cheap, optional, and obviously in the sender's interest — which
//! it is, because the agent it saves is usually a later instance of the same
//! fleet working on the same thing.

use anyhow::{bail, Result};
use std::path::Path;

use crate::cli::Confidence;
use crate::{identity, msg, output, plan, repo};

/// One handoff, as `--json` reports it.
#[derive(serde::Serialize)]
struct Handed {
    /// The bead that was finished.
    bead: String,
    /// The dependent this went to, and the thread it landed on.
    to_bead: String,
    thread: String,
    id: String,
}

/// `pact handoff <bead> --confidence <tier> --findings <text|@file>`.
pub(in crate::cli) fn run_handoff(
    cwd: &Path,
    agent_flag: Option<&str>,
    json: bool,
    bead: &str,
    confidence: Confidence,
    findings: &str,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    let agent = identity::resolve_agent(agent_flag)?;

    // REFUSED rather than guessed, and this is the one hard error in the command.
    // The alternative to a snapshot is inferring edges — from bead ids in lease
    // notes, from the event log, from names that look related — and every one of
    // those would deliver somebody's findings to the wrong bead. A handoff that
    // arrives in the wrong inheritance is worse than one that never arrives: the
    // second is a gap, the first is misinformation with a confidence tier on it.
    let Some(snapshot) = plan::snapshot(&root) else {
        bail!(
            "no dependency graph at {} — run `pact plan lint <manifest>` first.\n\n\
             `handoff` routes along the edges that manifest declares, and it will not \
             guess them: a handoff delivered to the wrong bead is worse than one never \
             sent, because it arrives carrying a confidence tier.",
            plan::SNAPSHOT_PATH
        );
    };

    let body = read_findings(findings)?;
    let dependents = snapshot.dependents(bead);
    if dependents.is_empty() {
        // Exit 0, and say why. "Nothing depends on this" is a fact about the plan,
        // not a failure of the command, and an agent that just closed a leaf bead
        // has done nothing wrong. A non-zero exit here would teach fleets that
        // `handoff` is a step that can fail, which is exactly the reading that
        // makes an optional thing get skipped.
        output::emit(json, &Vec::<Handed>::new(), |_: &Vec<Handed>| {
            format!(
                "nothing in the plan depends on {bead} — no handoff sent. \
                 (If that is wrong, the manifest `pact plan lint` last accepted \
                 does not say so.)"
            )
        });
        return Ok(());
    }

    let subject = format!("handoff from {bead} ({} confidence)", confidence.label());
    let mut sent = Vec::new();
    for dependent in dependents {
        let thread = msg::bead_thread(dependent);
        // The tier is rendered INTO the body as well as into the subject, because
        // a subject is what a listing shows and the body is what gets read. A
        // finding separated from how much to trust it is a finding somebody will
        // act on at full weight.
        let full = format!(
            "Inherited from {bead}, closed by {agent}.\nConfidence: {} — {}\n\n{body}",
            confidence.label(),
            match confidence {
                Confidence::High => "verified: tested, reproduced, or read from the source",
                Confidence::Medium => "consistent with what was seen, not independently confirmed",
                Confidence::Low => "a lead worth checking, and worth doubting",
            }
        );
        let record = msg::post_to_thread(&root, &agent, &thread, bead, &subject, &full)?;
        sent.push(Handed {
            bead: bead.to_string(),
            to_bead: dependent.to_string(),
            thread,
            id: record.id,
        });
    }

    output::emit(json, &sent, |s: &Vec<Handed>| {
        format!(
            "handed off {bead} to {} dependent(s): {}\nthey read it with `pact msg thread {}`",
            s.len(),
            s.iter()
                .map(|h| h.to_bead.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            s.first().map(|h| h.thread.as_str()).unwrap_or_default()
        )
    });
    Ok(())
}

/// `--findings @path` reads a file; anything else is the text itself.
///
/// The same affordance `msg send --body-file` exists for, and for the same
/// measured reason: quotes, backslashes and aligned tables do not survive a
/// shell, and a handoff is precisely the kind of content that has all three.
fn read_findings(arg: &str) -> Result<String> {
    let Some(path) = arg.strip_prefix('@') else {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            bail!("--findings is empty; a handoff with nothing in it is not a handoff");
        }
        return Ok(trimmed.to_string());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading findings from {path}: {e}"))?;
    if text.trim().is_empty() {
        bail!("{path} is empty; a handoff with nothing in it is not a handoff");
    }
    Ok(text.trim_end().to_string())
}
