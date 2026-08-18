//! `--check claim-lease-divergence` (pact-mqw.4): did a hold's note name a bead
//! that belongs to somebody else?

use serde::Serialize;

use crate::audit::model::opens;
use crate::audit::CheckReport;
use crate::events::Event;

/// One hold whose note named a bead assigned to a different agent.
///
/// **The caveat, stated first because it bounds the claim.** `assignee` is the
/// LAST assignee `.beads/interactions.jsonl` recorded, not the assignee at acquire
/// time — the log records the note, not the bead's state when it was written. So a
/// hold that legitimately handed its bead on afterwards shows up here too. This
/// answers "whose bead did this hold's note name, and who was it last assigned
/// to", which is the retrospective question; the live question is answered at
/// acquire time, where the assignee really is current.
///
/// The second caveat is [`claim_divergences`]': beads never *re*assigned have no
/// row in the export and cannot appear here at all.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimDivergence {
    pub path: String,
    pub agent: String,
    pub bead: String,
    pub assignee: String,
    pub acquired_at: String,
    pub line: usize,
}

/// Cross-check each hold's note against the assignees recorded in
/// `.beads/interactions.jsonl`.
///
/// Widens audit's usual "`.pact/` and nothing else" scope for the second time, the
/// same way [`Check::CommitCorrelation`] widened it for git — and under the same
/// rule: the invariant that section protects is "never touch the Beads DB
/// directly", which this obeys. Since pact-as5.6 it spawns no subprocess at all:
/// [`crate::beads::interaction_assignees`] reads the committed, git-tracked
/// export, so this check runs with no `bd` on `PATH`.
///
/// **It is deliberately less sensitive than the `bd show` version it replaced.**
/// The export records CHANGES, so a bead assigned at creation and never
/// reassigned has no assignee row and resolves to nothing — it finds fewer
/// divergences than a live query would, never more. And bd only writes the export
/// when `audit.enabled` is on, which it is not by default, so on most repositories
/// this check reports "no beads data" and passes; `pact doctor`'s `Beads audit
/// sidecar` check names that and how to switch it on.
///
/// Since pact-as5.5 this is also the ONLY place pact asks the question. The live
/// cross-check that used to run at `lease acquire` time is gone: it spawned `bd
/// show` on the hot path, and measured against this repository's whole event log
/// it would have warned zero times when moved to this offline source (100 acquire
/// notes named a bead, 8 resolved, all 8 to their own acquirer). See
/// [`crate::beads::interaction_assignees`] and `docs/audit.md`.
pub(in crate::audit) fn claim_divergences(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    report: &mut CheckReport,
) {
    let assignees = crate::beads::interaction_assignees(repo_root);
    if assignees.is_empty() {
        // Absent, empty, unparseable, or no assignee change ever recorded: all
        // one answer, "nothing to check against", and all of them PASS.
        report.claim_unavailable =
            Some("no assignee history in .beads/interactions.jsonl".to_string());
        return;
    }
    // Walked over the OPENING EVENTS rather than over reconstructed `Hold`s,
    // because the note lives on the event's `detail` and a Hold does not carry it.
    // Same set either way — every hold has exactly one open — with no need to widen
    // a serialized shape three other checks render.
    for (line, e) in events.iter().filter(|(_, e)| opens(&e.kind)) {
        let Some(path) = e.path.as_deref() else {
            continue;
        };
        let Some(note) = e.detail.as_deref() else {
            report.holds_naming_no_bead += 1;
            continue;
        };
        let Some(bead) = crate::beads::bead_id_in(note) else {
            report.holds_naming_no_bead += 1;
            continue;
        };
        let Some(assignee) = assignees.get(bead) else {
            continue;
        };
        if assignee == &e.agent {
            continue;
        }
        report.claim_divergences.push(ClaimDivergence {
            path: path.to_string(),
            agent: e.agent.clone(),
            bead: bead.to_string(),
            assignee: assignee.clone(),
            acquired_at: e.at.clone(),
            line: *line,
        });
    }
}

/// The scope lines, and whether the caller should stop there.
///
/// `true` means this check could not run at all, and everything below the line
/// it just printed would be a clean bill of health it has not earned.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) -> bool {
    if let Some(reason) = &r.claim_unavailable {
        // pact-83r.6 / finding 6. "Could not run" on its own sends a reader to bd's
        // `bd audit --help`, which tells them to run `bd config set audit.enabled
        // true` — and bd 1.2.1 answers THAT with `"audit.enabled" is not a
        // recognized config key` before honouring it anyway. Measured end to end:
        // the key works, only bd's config-key allowlist disagrees. A reader who is
        // not told that reasonably concludes the remediation failed and goes hunting
        // for one that does not exist, which is how this check stayed unrun in the
        // field. So the fix is named here, warning and all — `pact audit` never
        // spawns bd, so it cannot check the outcome, only state it accurately.
        out.push(format!(
            "  no beads data ({reason}) — claim-lease-divergence could not run. bd's \
             audit sidecar is not recording: turn it on with `BD_AUDIT_ENABLED=1` in \
             the environment your agents run bd in, or `bd config set audit.enabled \
             true` to persist it — bd 1.2.1 warns that key is unrecognised and then \
             honours it, so the warning is not a failure. bd records from that point, \
             not retroactively, so this stays empty for work already done"
        ));
        return true;
    }
    out.push(format!(
        "  {} hold(s) named no bead in their note, so there was nothing to cross-check",
        r.holds_naming_no_bead
    ));
    false
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "every hold whose note named a bead was held by that bead's own assignee".to_string()
}

/// Every divergence found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for c in &r.claim_divergences {
        out.push(String::new());
        out.push(format!(
            "CLAIM/LEASE DIVERGENCE on {}: held by {} for {}, last assigned to {} (line {}, \
             acquired {})",
            c.path, c.agent, c.bead, c.assignee, c.line, c.acquired_at
        ));
    }
    if !r.claim_divergences.is_empty() {
        out.push(String::new());
        out.push(
            "A fleet on this protocol has TWO mutual-exclusion mechanisms answering two halves \n\
             of one question — `bd update --claim` for who owns the work, `pact lease acquire` \n\
             for who may edit the files — and pact grants the second without consulting the \n\
             first. Assignees above are CURRENT, not as of the acquire, so a bead legitimately \n\
             handed on later appears here too; see docs/audit.md."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    /// pact-as5.6: the assignee is reconstructed by replaying `field=assignee` rows
    /// from the committed export, with no `bd` subprocess anywhere. Three things at
    /// once, because they are one replay: the last row wins, a `field=status` row is
    /// ignored, and a final empty `new_value` means unassigned (so no divergence).
    #[test]
    fn claim_divergence_reads_the_assignee_out_of_interactions_jsonl() {
        let tmp = with_log(&[
            &ev_note(
                "2026-08-11T09:00:00Z",
                "agent-b",
                "src/printer.rs",
                "pact-abc: rewriting the printer",
            ),
            &ev_note(
                "2026-08-11T09:05:00Z",
                "agent-b",
                "src/token.rs",
                "pact-xyz: nobody owns this one",
            ),
        ]);
        with_interactions(
            &tmp,
            &[
                // Ignored: not an assignee change.
                r#"{"id":"int-0","kind":"field_change","created_at":"2026-08-11T08:00:00Z","actor":"someone","issue_id":"pact-abc","extra":{"field":"status","new_value":"in_progress","old_value":"open"}}"#,
                // Reassigned: the LAST row wins, so agent-a owns pact-abc.
                &assignee_row("2026-08-11T08:01:00Z", "pact-abc", "agent-b"),
                &assignee_row("2026-08-11T08:02:00Z", "pact-abc", "agent-a"),
                // Unassigned at the end: resolves to nothing, so no divergence.
                &assignee_row("2026-08-11T08:03:00Z", "pact-xyz", "agent-a"),
                &assignee_row("2026-08-11T08:04:00Z", "pact-xyz", ""),
            ],
        );
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert_eq!(r.claim_unavailable, None);
        assert_eq!(r.findings(), 1, "{:?}", r.claim_divergences);
        let c = &r.claim_divergences[0];
        assert_eq!(c.path, "src/printer.rs");
        assert_eq!(c.agent, "agent-b");
        assert_eq!(c.bead, "pact-abc");
        assert_eq!(c.assignee, "agent-a");
        assert!(
            render_check(&r).contains("CLAIM/LEASE DIVERGENCE on src/printer.rs"),
            "{}",
            render_check(&r)
        );
    }

    /// One malformed line is skipped, not fatal — the rest of the export still
    /// answers. An export is appended to live and can be cut mid-write, the same
    /// hazard `.pact/events.jsonl` has.
    #[test]
    fn a_malformed_interactions_line_is_skipped_and_the_rest_still_resolves() {
        let tmp = with_log(&[&ev_note(
            "2026-08-11T09:00:00Z",
            "agent-b",
            "src/printer.rs",
            "pact-abc: rewriting the printer",
        )]);
        with_interactions(
            &tmp,
            &[
                "{not json at all",
                &assignee_row("2026-08-11T08:02:00Z", "pact-abc", "agent-a"),
                r#"{"kind":"field_change","created_at":"2026-08-11T08:03:00Z""#, // truncated
            ],
        );
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert_eq!(r.claim_unavailable, None);
        assert_eq!(r.findings(), 1, "{:?}", r.claim_divergences);
        assert_eq!(r.claim_divergences[0].assignee, "agent-a");
    }

    /// A wholly unparseable export is "no beads data" and PASSES. Never an error,
    /// never a finding — the same contract `git_unavailable` has, because a check
    /// that cannot run must not read as a clean one.
    #[test]
    fn a_wholly_malformed_interactions_file_reports_no_beads_data_and_passes() {
        let tmp = with_log(&[&ev_note(
            "2026-08-11T09:00:00Z",
            "agent-b",
            "src/printer.rs",
            "pact-abc: rewriting the printer",
        )]);
        with_interactions(&tmp, &["garbage", "", "still not json"]);
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert!(r.claim_divergences.is_empty());
        assert_eq!(r.findings(), 0);
        let reason = r.claim_unavailable.as_deref().unwrap_or_default();
        assert!(reason.contains(".beads/interactions.jsonl"), "{reason}");
        assert!(
            render_check(&r).contains("no beads data"),
            "{}",
            render_check(&r)
        );
    }

    /// No `.beads/` at all — the common case for a repo that never adopted bd, and
    /// the one that proves the check needs no backend.
    #[test]
    fn an_absent_interactions_file_reports_no_beads_data_and_passes() {
        let tmp = with_log(&[&ev_note(
            "2026-08-11T09:00:00Z",
            "agent-b",
            "src/printer.rs",
            "pact-abc: rewriting the printer",
        )]);
        let r = run_check(tmp.path(), Check::ClaimLeaseDivergence, None, false).unwrap();
        assert!(r.claim_divergences.is_empty());
        assert_eq!(r.findings(), 0);
        assert!(r.claim_unavailable.is_some());
    }
}
