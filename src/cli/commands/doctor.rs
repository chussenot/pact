use anyhow::Result;
use std::path::Path;

use crate::cli::refuse_if_a_target_is_leased;
use crate::{agents_md, doctor, output, repo};

pub(in crate::cli) fn run_doctor(
    cwd: &Path,
    json: bool,
    fix: bool,
    agent_flag: Option<&str>,
) -> Result<i32> {
    // Without a repo root none of the other checks mean anything, so this one
    // is a hard prerequisite rather than a soft check: propagate its exit
    // code (4) straight through instead of folding it into the report.
    let root = repo::find_repo_root(cwd)?;
    if fix {
        return run_doctor_fix(&root, json, agent_flag);
    }
    let report = doctor::checks(&root);

    output::emit(json, &report, |r| {
        let mut lines: Vec<String> = r
            .checks
            .iter()
            .map(|c| {
                let glyph = match (c.ok, c.warn) {
                    (false, _) => "✗",
                    (true, true) => "!",
                    (true, false) => "✓",
                };
                format!("{glyph} {}: {}", c.name, c.detail)
            })
            .collect();
        lines.push(String::new());
        lines.push(doctor::summary(r));
        lines.join("\n")
    });

    // Handed back to `main` rather than exited here: `std::process::exit`
    // skips every destructor, so the failing run — the only one anybody
    // troubleshoots — used to export no trace at all. The code itself is
    // unchanged; exit codes are API.
    Ok(if report.healthy { 0 } else { 1 })
}

/// `pact doctor --fix`.
///
/// The lease guard runs FIRST and over every candidate at once, exactly as
/// `init` does. Checking per-file would let a refusal land after AGENTS.md was
/// already rewritten, and a half-applied repair is worse than none — the same
/// all-or-nothing `acquire_many` promises. There is no `--force` here on
/// purpose: `pact init --force` is already that command, and a second spelling
/// of "write through somebody's live claim" is not a feature worth having twice.
pub(in crate::cli) fn run_doctor_fix(
    root: &Path,
    json: bool,
    agent_flag: Option<&str>,
) -> Result<i32> {
    let mut candidates = vec![
        root.join("AGENTS.md"),
        root.join("CLAUDE.md"),
        root.join(".gitignore"),
        root.join(".gitattributes"),
    ];
    candidates.extend(agents_md::managed_instruction_files(root));
    refuse_if_a_target_is_leased(root, &candidates, agent_flag, false)?;

    let report = doctor::fix(root);

    output::emit(json, &report, |r| {
        let mut lines = Vec::new();
        if r.repairs.is_empty() {
            lines.push("nothing to repair".to_string());
        } else {
            for repair in &r.repairs {
                let glyph = if repair.changed { "fixed" } else { "     " };
                lines.push(format!("{glyph} {}: {}", repair.check, repair.detail));
            }
        }

        // Only the ones that are actually unhappy. Listing all five on a green
        // repo would bury the repairs under a wall of "not attempted" for
        // checks nobody asked about.
        let unhappy: Vec<&doctor::Refusal> = r
            .refused
            .iter()
            .filter(|refusal| {
                r.after
                    .checks
                    .iter()
                    .any(|c| c.name == refusal.check && (!c.ok || c.warn))
            })
            .collect();
        if !unhappy.is_empty() {
            lines.push(String::new());
            lines.push("not fixed, deliberately:".to_string());
            for refusal in unhappy {
                lines.push(format!("  {}: {}", refusal.check, refusal.why));
            }
        }

        lines.push(String::new());
        lines.push(doctor::summary(&r.after));
        lines.join("\n")
    });

    Ok(if report.after.healthy { 0 } else { 1 })
}
