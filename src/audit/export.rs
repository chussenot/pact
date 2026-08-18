//! `--export` and `--compare`: the summary and every check written down once,
//! and read back later to see what moved.
//!
//! One file because the two halves are one contract. `--compare` does not diff
//! the two documents structurally — it reads a fixed list of JSON pointers
//! ([`COMPARED`]) out of whatever `--export` wrote, so every pointer here is a
//! promise about [`ExportReport`]'s shape. Put them in separate files and the
//! next field rename breaks a comparison nobody re-runs until it matters.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::summary::{summary, Summary};
use super::{run_check, Check, CheckReport, Expect};

/// `pact audit --export` (pact-1l8.2): one self-contained snapshot bundling
/// the summary, every named check and `pact doctor`'s checks — the exact set
/// of things the arkanoid field audit (pact-juz) had to assemble by hand,
/// from a separate `pact doctor`, a separate `pact audit` per check, and a
/// raw grep of `.pact/events.jsonl`. Meant to be read directly by a human, or
/// handed to another agent session asking "how is pact actually being used,
/// and where does it fall down" — the same question that produced pact-juz.
#[derive(Debug, Serialize)]
pub struct ExportReport {
    pub summary: Summary,
    pub double_win: CheckReport,
    pub stale_holds: CheckReport,
    pub chain_integrity: CheckReport,
    pub commit_correlation: CheckReport,
    /// pact-mqw.3: successive holds that started from divergent content.
    pub merge_divergence: CheckReport,
    /// pact-mqw.4: holds whose note named a bead assigned to another agent.
    pub claim_lease_divergence: CheckReport,
    /// pact-1gv.3: agents that busy-retried a lease instead of backing off.
    pub retry_storm: CheckReport,
    /// pact-7kv + pact-1gv.7: contended paths nobody communicated about.
    pub silent_contention: CheckReport,
    /// Run with `--expect any`, so it reports the distribution rather than a
    /// verdict: a stored retrospective should not fail on an expectation
    /// nobody declared when it was written.
    pub topology: CheckReport,
    pub doctor: crate::doctor::DoctorReport,
    /// Messages their own recipient never marked read (pact-ler.3).
    ///
    /// Here rather than in `pact doctor`, and that placement is forced rather
    /// than preferred. Answering this needs a real `bd list`, and `bd` takes a
    /// `.beads/.write.lock` to serve one — while `doctor` is exposed over MCP
    /// as `pact_doctor`, which docs/mcp.md promises is strictly read-only and
    /// `tests/mcp.rs::every_tool_call_leaves_the_repository_byte_identical`
    /// enforces byte-for-byte. A doctor check would have quietly broken that
    /// guarantee for every MCP observer. `--export` is CLI-only (MCP exposes
    /// `audit::summary`, never this), deliberately run at the end of a run,
    /// and is exactly the retrospective artifact this question belongs in.
    ///
    /// Empty when there is no Beads CLI or no `.beads/` — "cannot ask" and
    /// "nothing pending" are both reported as nothing pending here, because
    /// the sibling `doctor` section of this same report already says loudly
    /// when the backend is missing.
    pub unacknowledged_messages: Vec<crate::msg::Message>,
    /// Short, human-readable highlights pulled out of the structured fields
    /// above, so a reader does not have to re-derive "is this worth looking
    /// at" from raw counts and thresholds. Empty means nothing here rose to
    /// that bar — not that nothing was checked; the structured fields are
    /// always the full data regardless of what lands here.
    pub observations: Vec<String>,
}

pub fn export(
    repo_root: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<ExportReport> {
    let summary_report = summary(repo_root, since, include_annotated)?;
    let double_win = run_check(repo_root, Check::DoubleWin, since, include_annotated)?;
    let stale_holds = run_check(repo_root, Check::StaleHolds, since, include_annotated)?;
    let chain_integrity = run_check(repo_root, Check::ChainIntegrity, since, include_annotated)?;
    let topology = run_check(
        repo_root,
        Check::Topology(Expect::Any),
        since,
        include_annotated,
    )?;
    let commit_correlation = run_check(
        repo_root,
        Check::CommitCorrelation,
        since,
        include_annotated,
    )?;
    let merge_divergence = run_check(repo_root, Check::MergeDivergence, since, include_annotated)?;
    let claim_lease_divergence = run_check(
        repo_root,
        Check::ClaimLeaseDivergence,
        since,
        include_annotated,
    )?;
    let retry_storm = run_check(repo_root, Check::RetryStorm, since, include_annotated)?;
    let silent_contention =
        run_check(repo_root, Check::SilentContention, since, include_annotated)?;
    let doctor = crate::doctor::checks(repo_root);

    // Best-effort by design: a repo with no `.beads/` or no backend on PATH
    // is not a repo with a messaging problem, and `doctor` above already
    // reports a missing CLI on its own line.
    // No `.beads` gate and no backend probe: messages are pact's own file now, so
    // "this repo has no issue tracker" says nothing about whether it has
    // unacknowledged mail. Still best-effort — an unreadable store is not a
    // messaging finding.
    let unacknowledged_messages = crate::msg::unacknowledged(repo_root).unwrap_or_default();

    let mut observations = Vec::new();
    if double_win.findings() > 0 {
        observations.push(format!(
            "{} double-win(s): two agents held one path at the same time — see pact-ehi.",
            double_win.findings()
        ));
    }
    if stale_holds.findings() > 0 {
        observations.push(format!(
            "{} stale hold(s): ran past their own recorded TTL with no renew.",
            stale_holds.findings()
        ));
    }
    if chain_integrity.findings() > 0 {
        observations.push(format!(
            "{} chain-integrity break(s): a chain-tracked line does not match its recorded hash.",
            chain_integrity.findings()
        ));
    }
    match &commit_correlation.git_unavailable {
        Some(reason) => observations.push(format!("commit-correlation could not run: {reason}")),
        None => {
            if !commit_correlation.concurrent_writes.is_empty() {
                observations.push(format!(
                    "{} concurrent write(s): real commits landed from both sides of an \
                     overlapping hold.",
                    commit_correlation.concurrent_writes.len()
                ));
            }
            if !commit_correlation.uncovered_commits.is_empty() {
                observations.push(format!(
                    "{} commit(s) landed on a leased path with no hold covering them.",
                    commit_correlation.uncovered_commits.len()
                ));
            }
            // `commits_attributed`/`commits_unattributed` are deliberately NOT
            // observations: they are scope, and `observations` is the findings
            // list. A clean history must not manufacture an entry there. The
            // counts ride in the report itself and in `render_check`'s scope line.
            if !commit_correlation.cross_held_commits.is_empty() {
                observations.push(format!(
                    "{} commit(s) landed on a path a DIFFERENT agent held, per their Pact-Agent \
                     trailer.",
                    commit_correlation.cross_held_commits.len()
                ));
            }
        }
    }
    if merge_divergence.findings() > 0 {
        observations.push(format!(
            "{} hold(s) started from a copy the previous holder never produced: an edit made \
             against a stale worktree, which git often merges with no conflict marker.",
            merge_divergence.findings()
        ));
    }
    // No observation for `claim_unavailable`, unlike commit-correlation's
    // `git_unavailable`: `doctor` already reports a missing Beads CLI on its own
    // line in this same list, and the reason a check could not run is scope rather
    // than a finding. `observations` is the findings list — a repo with no backend
    // must not read as a repo with a coordination problem.
    if silent_contention.findings() > 0 {
        observations.push(format!(
            "{} contended path(s) where the holder released without a message or a watch \
             delivery, leaving the refused agent to find out by asking again.",
            silent_contention.findings()
        ));
    }
    if retry_storm.findings() > 0 {
        let wasted: usize = retry_storm.retry_storms.iter().map(|r| r.refusals).sum();
        observations.push(format!(
            "{} retry storm(s), {wasted} refusal(s) spent hammering a held lease instead of \
             backing off or subscribing.",
            retry_storm.findings()
        ));
    }
    if claim_lease_divergence.findings() > 0 {
        observations.push(format!(
            "{} hold(s) named a bead their holder does not own: the bead claim and the file \
             lease are separate locks that do not consult each other.",
            claim_lease_divergence.findings()
        ));
    }
    if !unacknowledged_messages.is_empty() {
        // Named, not counted, and "nobody read it" kept distinct from "read
        // by somebody who was not the addressee" — the second is the common
        // field shape (`--to-owner-of` means a message about a path follows
        // the path, so the agent who picks that path up is often the reader),
        // and collapsing them would read as "nobody looked".
        observations.push(format!(
            "{} message(s) never read by their recipient: {}. `pact msg sent` shows these as \
             undelivered to whoever sent them.",
            unacknowledged_messages.len(),
            unacknowledged_messages
                .iter()
                .map(|m| {
                    if m.read_by.is_empty() {
                        format!("{} (to {}, nobody has read it)", m.id, m.to)
                    } else {
                        format!(
                            "{} (to {}, read only by {})",
                            m.id,
                            m.to,
                            m.read_by.join("/")
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for c in &doctor.checks {
        if !c.ok || c.warn {
            observations.push(format!(
                "doctor: {} {} — {}",
                if c.ok { "warn" } else { "FAIL" },
                c.name,
                c.detail
            ));
        }
    }

    Ok(ExportReport {
        summary: summary_report,
        double_win,
        stale_holds,
        chain_integrity,
        commit_correlation,
        merge_divergence,
        claim_lease_divergence,
        retry_storm,
        silent_contention,
        topology,
        doctor,
        unacknowledged_messages,
        observations,
    })
}

/// One number that moved between two exported reports.
#[derive(Debug, Clone, Serialize)]
pub struct Movement {
    pub field: &'static str,
    pub baseline: i64,
    pub current: i64,
    pub delta: i64,
}

/// What changed between a baseline export and this repository now
/// (pact-okz.2).
///
/// Reports movement and **never a verdict**. "Contention fell from 8 agents on
/// one path to 3" is a fact; whether that was the module tree, the wave
/// schedule or luck is not something the log can know. Scoring a run good or
/// bad would need weights nobody derived from data — the failure docs/audit.md
/// already records — so the judgement stays with the reader and the exit code
/// stays 0.
#[derive(Debug, Serialize)]
pub struct Comparison {
    pub baseline: String,
    /// The protocol era each side ran under, when both recorded one and they
    /// differ (pact-okz.1). Listed FIRST in the rendering because it is the
    /// interpretive key: two runs under different protocols are not a
    /// controlled comparison, and reading them as one is exactly the mistake
    /// that made 223 messages look like evidence agents message voluntarily.
    pub protocol_shift: Option<(String, String)>,
    pub movements: Vec<Movement>,
    /// Unchanged fields, counted rather than listed — a report that prints
    /// forty "0" rows buries the three that moved.
    pub unchanged: usize,
    /// Fields the baseline does not carry at all, because it was written by an
    /// older pact. Named, never rendered as a delta from zero: "uncovered
    /// commits went from 0 to 19" would be a fabricated finding when the
    /// baseline simply predates the check.
    pub not_comparable: Vec<&'static str>,
}

/// How a comparable field is read out of an export document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extract {
    /// A number at the pointer. Absent means the baseline predates it.
    Number,
    /// An array at the pointer; its length is the value. Absent means the
    /// baseline predates it; empty means it found nothing.
    Len,
    /// A key in a map that only carries what actually occurred, such as
    /// `by_kind`. **Absent means zero, not unrecorded** — `by_kind` lists
    /// only the kinds a run produced, so a missing `refused` is a run with no
    /// contention, and calling that "the baseline is too old" would tell a
    /// reader to go and upgrade when the honest answer is "nothing was
    /// refused". Only the containing map going missing is "not comparable".
    SparseCount,
}

/// Every field compared, as (label, JSON pointer, how to read it).
///
/// A fixed list rather than a structural diff of the two documents: a
/// structural diff would report movement in timestamps, agent names and paths,
/// which is noise, and would silently start comparing any field added later
/// without anyone deciding it was comparable.
const COMPARED: &[(&str, &str, Extract)] = &[
    ("events", "/summary/events", Extract::Number),
    ("agents", "/summary/agents", Extract::Len),
    (
        "completed holds",
        "/summary/hold_secs/completed",
        Extract::Number,
    ),
    (
        "hold median (s)",
        "/summary/hold_secs/median_secs",
        Extract::Number,
    ),
    (
        "hold p90 (s)",
        "/summary/hold_secs/p90_secs",
        Extract::Number,
    ),
    (
        "hold max (s)",
        "/summary/hold_secs/max_secs",
        Extract::Number,
    ),
    ("open holds", "/summary/open_holds", Extract::Number),
    ("refusals", "/summary/contention/refusals", Extract::Number),
    (
        // The RATIO is what carries meaning, but --compare deals in integers, so
        // both terms go in the table and a reader divides. Inventing a scaled
        // integer field just to make the ratio comparable would put a derived
        // number in the report that nothing else uses.
        "claims (acquire+takeover)",
        "/summary/contention/claims",
        Extract::Number,
    ),
    (
        "abandoned pairs",
        "/summary/contention/abandoned_pairs",
        Extract::Number,
    ),
    ("steals (forced)", "/summary/steals", Extract::Number),
    ("reclaims (expired)", "/summary/reclaims", Extract::Number),
    (
        "refusals (contention)",
        "/summary/by_kind/refused",
        Extract::SparseCount,
    ),
    ("watches active", "/summary/watches_active", Extract::Number),
    (
        "diffs delivered",
        "/summary/diffs_delivered",
        Extract::Number,
    ),
    (
        "deliveries failed",
        "/summary/deliveries_failed",
        Extract::Number,
    ),
    ("double-wins", "/double_win/double_wins", Extract::Len),
    ("stale holds", "/stale_holds/stale_holds", Extract::Len),
    (
        "chain breaks",
        "/chain_integrity/chain_breaks",
        Extract::Len,
    ),
    (
        "concurrent writes",
        "/commit_correlation/concurrent_writes",
        Extract::Len,
    ),
    (
        "uncovered commits",
        "/commit_correlation/uncovered_commits",
        Extract::Len,
    ),
    (
        "cross-held commits",
        "/commit_correlation/cross_held_commits",
        Extract::Len,
    ),
    (
        "holds with no commit",
        "/commit_correlation/holds_with_no_commit",
        Extract::Len,
    ),
    (
        "merge divergences",
        "/merge_divergence/merge_divergences",
        Extract::Len,
    ),
    (
        "claim/lease divergences",
        "/claim_lease_divergence/claim_divergences",
        Extract::Len,
    ),
    ("retry storms", "/retry_storm/retry_storms", Extract::Len),
    (
        "silent contentions",
        "/silent_contention/silent_contentions",
        Extract::Len,
    ),
    (
        "refusals with a channel",
        "/silent_contention/refusals_with_a_channel",
        Extract::Number,
    ),
    (
        "unacknowledged messages",
        "/unacknowledged_messages",
        Extract::Len,
    ),
];

fn extract(doc: &serde_json::Value, pointer: &str, how: Extract) -> Option<i64> {
    match how {
        Extract::Number => doc.pointer(pointer)?.as_i64(),
        // A missing array and an empty one are different: the first means the
        // baseline predates the field, the second means it found nothing.
        Extract::Len => doc.pointer(pointer)?.as_array().map(|a| a.len() as i64),
        Extract::SparseCount => {
            let (parent, _) = pointer.rsplit_once('/')?;
            // The map itself must exist for absence of the key to mean zero.
            doc.pointer(parent)?.as_object()?;
            Some(doc.pointer(pointer).and_then(|v| v.as_i64()).unwrap_or(0))
        }
    }
}

/// Compare `baseline` (a previously written `--export` document) against this
/// repository's current state.
pub fn compare(
    repo_root: &std::path::Path,
    baseline_path: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<Comparison> {
    let text = std::fs::read_to_string(baseline_path)
        .with_context(|| format!("reading baseline {}", baseline_path.display()))?;
    let baseline: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing baseline {}", baseline_path.display()))?;
    let current = serde_json::to_value(export(repo_root, since, include_annotated)?)?;

    let mut movements = Vec::new();
    let mut not_comparable = Vec::new();
    let mut unchanged = 0usize;
    for (field, pointer, how) in COMPARED {
        let now = extract(&current, pointer, *how).unwrap_or(0);
        match extract(&baseline, pointer, *how) {
            None => not_comparable.push(*field),
            Some(was) if was == now => unchanged += 1,
            Some(was) => movements.push(Movement {
                field,
                baseline: was,
                current: now,
                delta: now - was,
            }),
        }
    }

    // The dominant era on each side. A window spanning a protocol change is
    // already called out by the summary itself; here the question is only
    // whether the two runs are comparable at all.
    let era = |doc: &serde_json::Value| -> Option<String> {
        let map = doc.pointer("/summary/by_protocol")?.as_object()?;
        map.iter()
            .filter(|(k, _)| k.as_str() != "unknown")
            .max_by_key(|(_, v)| v.as_i64().unwrap_or(0))
            .map(|(k, _)| k.clone())
    };
    let protocol_shift = match (era(&baseline), era(&current)) {
        (Some(a), Some(b)) if a != b => Some((a, b)),
        _ => None,
    };

    Ok(Comparison {
        baseline: baseline_path.display().to_string(),
        protocol_shift,
        movements,
        unchanged,
        not_comparable,
    })
}

pub fn render_comparison(c: &Comparison) -> String {
    let mut out = vec![format!("compared against {}", c.baseline)];
    if let Some((was, now)) = &c.protocol_shift {
        out.push(String::new());
        out.push(format!(
            "PROTOCOL CHANGED between these runs: {was} -> {now}.\n\
             They are not a controlled comparison — anything below may be the \n\
             protocol rather than the fleet."
        ));
    }
    if c.movements.is_empty() {
        out.push(String::new());
        out.push(format!(
            "nothing moved ({} field(s) identical)",
            c.unchanged
        ));
    } else {
        out.push(String::new());
        out.push(format!(
            "{:<26} {:>10} {:>10} {:>10}",
            "FIELD", "BASELINE", "NOW", "DELTA"
        ));
        for m in &c.movements {
            out.push(format!(
                "{:<26} {:>10} {:>10} {:>+10}",
                m.field, m.baseline, m.current, m.delta
            ));
        }
        out.push(String::new());
        out.push(format!("{} field(s) unchanged", c.unchanged));
    }
    if !c.not_comparable.is_empty() {
        out.push(String::new());
        out.push(format!(
            "not comparable — the baseline predates {}: {}",
            if c.not_comparable.len() == 1 {
                "this field"
            } else {
                "these fields"
            },
            c.not_comparable.join(", ")
        ));
    }
    out.push(String::new());
    out.push(
        "Movement, not a verdict: which direction is GOOD depends on what you changed \n\
         and why, which the log cannot know."
            .to_string(),
    );
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::fixtures::*;
    use std::collections::BTreeMap;

    fn write_baseline(dir: &std::path::Path, report: &ExportReport) -> std::path::PathBuf {
        let p = dir.join("baseline.json");
        std::fs::write(&p, serde_json::to_string(report).unwrap()).unwrap();
        p
    }

    #[test]
    fn comparing_a_report_to_itself_reports_no_movement() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let base = write_baseline(tmp.path(), &export(tmp.path(), None, false).unwrap());
        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert!(c.movements.is_empty(), "{:?}", c.movements);
        assert!(c.not_comparable.is_empty(), "{:?}", c.not_comparable);
        assert!(c.unchanged > 0);
        assert!(render_comparison(&c).contains("nothing moved"));
    }

    #[test]
    fn a_run_that_moved_names_each_field_and_by_how_much() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let base = write_baseline(tmp.path(), &export(tmp.path(), None, false).unwrap());

        // A second, longer hold by a second agent.
        let log = tmp.path().join(".pact").join("events.jsonl");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push('\n');
        text.push_str(&ev("2026-08-01T11:00:00Z", "b", "acquired", "b.rs"));
        text.push('\n');
        text.push_str(&ev("2026-08-01T11:30:00Z", "b", "released", "b.rs"));
        std::fs::write(&log, text).unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        let moved: BTreeMap<&str, i64> = c.movements.iter().map(|m| (m.field, m.delta)).collect();
        assert_eq!(moved.get("events"), Some(&2), "{moved:?}");
        assert_eq!(moved.get("agents"), Some(&1), "{moved:?}");
        assert_eq!(moved.get("completed holds"), Some(&1), "{moved:?}");
        assert!(moved.contains_key("hold max (s)"), "{moved:?}");

        let text = render_comparison(&c);
        assert!(text.contains("events"), "{text}");
        // Never a verdict.
        assert!(text.contains("Movement, not a verdict"), "{text}");
    }

    /// The acceptance case that keeps this honest: an older pact's export has
    /// no `unacknowledged_messages` at all, and reporting "0 -> 3" would be a
    /// fabricated finding rather than a measurement.
    #[test]
    fn a_baseline_missing_a_field_is_not_comparable_rather_than_a_delta_from_zero() {
        let tmp = with_log(&[&ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs")]);
        let mut doc = serde_json::to_value(export(tmp.path(), None, false).unwrap()).unwrap();
        // Simulate an older export: drop two whole sections.
        doc.as_object_mut()
            .unwrap()
            .remove("unacknowledged_messages");
        doc.as_object_mut().unwrap().remove("commit_correlation");
        let base = tmp.path().join("old.json");
        std::fs::write(&base, serde_json::to_string(&doc).unwrap()).unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert!(
            c.not_comparable.contains(&"unacknowledged messages"),
            "{:?}",
            c.not_comparable
        );
        assert!(
            c.not_comparable.contains(&"uncovered commits"),
            "{:?}",
            c.not_comparable
        );
        assert!(
            !c.movements.iter().any(|m| m.field == "uncovered commits"),
            "a missing baseline field must not produce a delta: {:?}",
            c.movements
        );
        assert!(render_comparison(&c).contains("baseline predates"));
    }

    /// `by_kind` lists only the kinds that occurred, so an absent `refused`
    /// means a run with no contention — NOT a baseline too old to say. Getting
    /// this backwards tells a reader to upgrade when the honest answer is
    /// "nothing was refused".
    #[test]
    fn an_absent_sparse_count_reads_as_zero_not_as_unrecorded() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        let base = write_baseline(tmp.path(), &export(tmp.path(), None, false).unwrap());
        assert!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&base).unwrap())
                .unwrap()
                .pointer("/summary/by_kind/refused")
                .is_none(),
            "fixture must genuinely lack the key"
        );

        let log = tmp.path().join(".pact").join("events.jsonl");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push('\n');
        text.push_str(&ev("2026-08-01T11:00:00Z", "b", "refused", "a.rs"));
        std::fs::write(&log, text).unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert!(
            !c.not_comparable.contains(&"refusals (contention)"),
            "absent means zero here: {:?}",
            c.not_comparable
        );
        let m = c
            .movements
            .iter()
            .find(|m| m.field == "refusals (contention)")
            .expect("it moved from 0 to 1");
        assert_eq!((m.baseline, m.current, m.delta), (0, 1, 1));
    }

    /// pact-okz.1 feeding pact-okz.2: two runs under different protocol
    /// revisions are not a controlled comparison, and the report has to say so
    /// before anything else — reading them as one is the mistake that made 223
    /// messages look like evidence agents message voluntarily.
    #[test]
    fn a_protocol_shift_between_the_two_runs_is_called_out_first() {
        let tmp = with_log(&[&ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs")]);
        let mut doc = serde_json::to_value(export(tmp.path(), None, false).unwrap()).unwrap();
        doc.pointer_mut("/summary")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "by_protocol".to_string(),
                serde_json::json!({ "aaaaaaaa": 1 }),
            );
        let base = tmp.path().join("old-protocol.json");
        std::fs::write(&base, serde_json::to_string(&doc).unwrap()).unwrap();

        // Give the current side a different era.
        let log = tmp.path().join(".pact").join("events.jsonl");
        std::fs::write(
            &log,
            r#"{"at":"2026-08-01T10:00:00Z","agent":"a","kind":"acquired","path":"a.rs","protocol_hash":"bbbbbbbb"}"#,
        )
        .unwrap();

        let c = compare(tmp.path(), &base, None, false).unwrap();
        assert_eq!(
            c.protocol_shift,
            Some(("aaaaaaaa".to_string(), "bbbbbbbb".to_string()))
        );
        let text = render_comparison(&c);
        assert!(text.contains("PROTOCOL CHANGED"), "{text}");
        assert!(text.contains("not a controlled comparison"), "{text}");
    }

    #[test]
    fn export_combines_the_summary_every_check_and_doctor() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        let report = export(tmp.path(), None, false).unwrap();
        assert_eq!(report.summary.events, 2);
        assert_eq!(report.double_win.check, "double-win");
        assert_eq!(report.stale_holds.check, "stale-holds");
        assert_eq!(report.chain_integrity.check, "chain-integrity");
        assert_eq!(report.commit_correlation.check, "commit-correlation");
        assert!(
            !report.doctor.checks.is_empty(),
            "doctor's checks must ride along, not just its healthy flag"
        );

        // The whole point is a file another agent session can read directly —
        // pin that it actually round-trips through serde as one JSON object,
        // not just that the Rust struct is well-formed.
        let json = serde_json::to_value(&report).unwrap();
        for key in [
            "summary",
            "double_win",
            "stale_holds",
            "chain_integrity",
            "commit_correlation",
            "doctor",
            "observations",
        ] {
            assert!(json.get(key).is_some(), "missing {key} in exported JSON");
        }
    }

    /// The `observations` list is what saves a reader from re-deriving
    /// "worth a look" from raw counts — a stale hold must actually show up
    /// there, in a form that names what it is.
    #[test]
    fn export_observations_name_a_real_stale_hold() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "slow", "acquired", "src/slow.rs"),
            &ev("2026-08-01T12:00:00Z", "slow", "released", "src/slow.rs"),
        ]);
        let report = export(tmp.path(), None, false).unwrap();
        assert_eq!(report.stale_holds.findings(), 1);
        assert!(
            report.observations.iter().any(|o| o.contains("stale hold")),
            "{:?}",
            report.observations
        );
    }

    /// A clean lease/commit history must not manufacture an audit-side
    /// observation when nothing rose to that bar — only `doctor`'s own
    /// checks (a separate concern, covered by its own test suite) may still
    /// contribute to the list for this fixture.
    #[test]
    fn export_adds_no_audit_observation_when_the_history_is_clean() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:00:30+00:00");

        let report = export(tmp.path(), None, false).unwrap();
        assert_eq!(report.double_win.findings(), 0);
        assert_eq!(report.stale_holds.findings(), 0);
        assert_eq!(report.chain_integrity.findings(), 0);
        assert_eq!(report.commit_correlation.findings(), 0);
        assert!(
            report.observations.iter().all(|o| o.starts_with("doctor:")),
            "a clean history must not add any non-doctor observation: {:?}",
            report.observations
        );
    }
}
