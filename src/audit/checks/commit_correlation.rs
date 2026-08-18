//! `--check commit-correlation` (pact-1l8.1): does real git history back up what
//! the lease log claims?
//!
//! The one check whose findings depend on something other than
//! `.pact/events.jsonl` — see the module doc on the parent for why reading `git`
//! directly is a deliberate, narrow widening rather than a break of audit's
//! Beads-store invariant.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::audit::model::{parse_at, Hold};
use crate::audit::CheckReport;
use crate::events::Event;

/// `Check::CommitCorrelation`: a closed hold with no commit landing anywhere
/// inside its own window.
///
/// Informational, never a finding that fails the check — see
/// `CheckReport::findings`'s doc comment. A read-only lease (research,
/// reviewing, waiting on something) closes exactly like this, and treating
/// every one as a defect would train readers to ignore the check.
#[derive(Debug, Clone, Serialize)]
pub struct UncommittedHold {
    pub path: String,
    pub agent: String,
    pub opened_at: String,
    pub closed_at: Option<String>,
}

/// `Check::CommitCorrelation`: one commit that landed while a path was held.
#[derive(Debug, Clone, Serialize)]
pub struct CommitTouch {
    pub hash: String,
    pub author: String,
    pub at: String,
}

/// `Check::CommitCorrelation`: two holds of the same path with overlapping
/// windows where real commits — not just the lease events — actually landed
/// during the overlap.
///
/// Stronger evidence than `DoubleWin`, which only proves the *lease* events
/// overlapped. This proves work was actually written more than once during
/// the disputed period. Deliberately does not try to attribute which commit
/// belongs to which hold by matching commit author against agent name — that
/// correlation is exactly as unreliable as the one `doctor::checks`'s "Beads
/// actor attribution" check exists to flag (a shared checkout collapses every
/// agent's commits to one git identity) — so this reports every commit found
/// in the overlap and lets a human attribute them.
#[derive(Debug, Clone, Serialize)]
pub struct ConcurrentWrite {
    pub path: String,
    pub first_agent: String,
    pub first_opened_at: String,
    pub first_closed_at: String,
    pub second_agent: String,
    pub second_opened_at: String,
    pub second_closed_at: String,
    pub overlap_start: String,
    pub overlap_end: String,
    /// Always 2 or more — that is what makes this "concurrent" rather than
    /// merely "both holds happened to eventually touch the file".
    pub commits_in_overlap: Vec<CommitTouch>,
}

/// `Check::CommitCorrelation`: a commit touching a path with no hold covering
/// its author date at all — work done with no lease, the thing the whole
/// protocol exists to prevent. Scoped to paths that were leased at *some*
/// point in the window audited; a path nobody has ever leased is a different
/// question ("is this file even under pact's protocol") that this check does
/// not try to answer, since most of a real repository is never leased at any
/// given moment and flagging all of it would be pure noise.
#[derive(Debug, Clone, Serialize)]
pub struct UncoveredCommit {
    pub path: String,
    pub hash: String,
    pub author: String,
    pub at: String,
}

/// `Check::CommitCorrelation`: a commit that fell inside a hold, but a hold held
/// by a DIFFERENT agent than the one that made the commit (pact-mqw.10).
///
/// The class that had to exist because "covered" was answering the wrong question.
/// `uncovered` asks whether ANYONE held the path at that moment; it never asked
/// whether the COMMITTER did. In the crucible run one agent was deliberately told
/// the protocol did not apply to it, authored zero of 346 coordination events, and
/// committed freely — and its **worst** commit was invisible. `6fa4542` touched
/// five files in one unleased shot, and at that instant every one of those paths
/// was under an active lease held by a compliant peer, so the commit passed.
///
/// The perverse consequence: the rogue's most damaging commit was hidden
/// *specifically because* its peers were compliant. The better the rest of the
/// fleet behaves, the better a rogue hides. Cost of that miss: merging the branch
/// conflicted on all five files and could not be resolved by taking either side,
/// because the rogue had branched before three peers' AST variants landed.
/// Detection lag ~13 minutes, and the detector was a human running `git merge`.
///
/// This is a finding, and a louder one than `uncovered`: committing where nobody
/// held the path risks your own work, committing where somebody ELSE held it
/// corrupts theirs.
#[derive(Debug, Clone, Serialize)]
pub struct CrossHeldCommit {
    pub path: String,
    pub hash: String,
    /// The `Pact-Agent` trailer — who actually made this commit.
    pub committer_agent: String,
    /// Who held the path across its author date.
    pub holder: String,
    pub at: String,
    pub hold_opened_at: String,
}

/// Run the correlation, unless the run's own recorded policy makes it vacuous.
pub(in crate::audit) fn detect(
    repo_root: &std::path::Path,
    context: &BTreeMap<String, String>,
    events: &[(usize, Event)],
    holds: &[Hold],
    report: &mut CheckReport,
) {
    match context.get("commit-policy").map(String::as_str) {
        // A policy that forbade committing makes the correlation vacuous, and
        // a vacuous check must say so rather than return an empty finding
        // list that reads as a clean bill of health.
        Some(policy @ ("none" | "orchestrator-only")) => {
            report.commit_policy_skipped = Some(policy.to_string());
        }
        _ => correlate_commits(repo_root, events, holds, report),
    }
}

/// The body of `Check::CommitCorrelation`, split out because it is the one
/// check whose findings depend on something other than `.pact/events.jsonl`
/// — see the module doc comment on why that is a deliberate, narrow widening
/// rather than a break of audit's Beads-store invariant.
/// One hold's window plus WHO held it — the second half of which `covered` used to
/// throw away, and the whole of pact-mqw.10's fix.
struct HoldWindow {
    agent: String,
    open: DateTime<Utc>,
    /// `None` while the log shows the lease still held.
    close: Option<DateTime<Utc>>,
}

fn correlate_commits(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    holds: &[Hold],
    report: &mut CheckReport,
) {
    let earliest = events.first().and_then(|(_, e)| parse_at(&e.at));
    let commits = match crate::git_history::commits_since(repo_root, earliest) {
        Ok(c) => c,
        Err(e) => {
            report.git_unavailable = Some(format!("{e:#}"));
            return;
        }
    };

    for c in &commits {
        if c.pact_agent.is_some() {
            report.commits_attributed += 1;
        } else {
            report.commits_unattributed += 1;
        }
    }

    let mut by_path: BTreeMap<&str, Vec<&crate::git_history::Commit>> = BTreeMap::new();
    for c in &commits {
        for p in &c.paths {
            by_path.entry(p.as_str()).or_default().push(c);
        }
    }
    for v in by_path.values_mut() {
        v.sort_by_key(|c| c.at);
    }

    // Closed holds only: an open hold has not finished, so judging whether it
    // ever produced a commit would be premature.
    for h in holds {
        let (Some(open), Some(close)) = (
            parse_at(&h.opened_at),
            h.closed_at.as_deref().and_then(parse_at),
        ) else {
            continue;
        };
        // THE EXACT ANSWER, when the log carries it (pact-b73.6). With a head on both
        // boundaries the hold brackets a real commit range, so "did this lease land
        // anything on this path" is a lookup rather than an inference from wall-clock
        // time. Only `git log open..close` can distinguish the agent's own commits
        // from a peer's that merely landed inside the same minutes.
        //
        // The fallback is not optional and never will be: every log written before
        // pact stamped `head` has none, and a recorded hash can stop resolving when a
        // worktree branch is deleted and gc'd, force-pushed, or read in a shallow
        // clone. `commits_in_range` returns `None` for all of those, and the counters
        // above make the choice visible instead of quietly reporting fewer findings.
        let ranged = match (h.open_head.as_deref(), h.close_head.as_deref()) {
            (Some(from), Some(to)) => crate::git_history::commits_in_range(repo_root, from, to),
            _ => None,
        };
        let has_commit = match &ranged {
            Some(paths) => {
                report.correlated_by_head += 1;
                paths.contains(&h.path)
            }
            None => {
                report.correlated_by_time += 1;
                by_path
                    .get(h.path.as_str())
                    .is_some_and(|v| v.iter().any(|c| c.at >= open && c.at <= close))
            }
        };
        if !has_commit {
            report.holds_with_no_commit.push(UncommittedHold {
                path: h.path.clone(),
                agent: h.agent.clone(),
                opened_at: h.opened_at.clone(),
                closed_at: h.closed_at.clone(),
            });
        }
    }

    let mut by_hold_path: BTreeMap<&str, Vec<&Hold>> = BTreeMap::new();
    for h in holds {
        if h.closed_at.is_some() {
            by_hold_path.entry(h.path.as_str()).or_default().push(h);
        }
    }
    for (path, hs) in &by_hold_path {
        for i in 0..hs.len() {
            for j in (i + 1)..hs.len() {
                let (a, b) = (hs[i], hs[j]);
                let (Some(a_open), Some(a_close), Some(b_open), Some(b_close)) = (
                    parse_at(&a.opened_at),
                    a.closed_at.as_deref().and_then(parse_at),
                    parse_at(&b.opened_at),
                    b.closed_at.as_deref().and_then(parse_at),
                ) else {
                    continue;
                };
                let overlap_start = a_open.max(b_open);
                let overlap_end = a_close.min(b_close);
                if overlap_start > overlap_end {
                    continue;
                }
                let commits_in_overlap: Vec<CommitTouch> = by_path
                    .get(path)
                    .map(|v| {
                        v.iter()
                            .filter(|c| c.at >= overlap_start && c.at <= overlap_end)
                            .map(|c| CommitTouch {
                                hash: c.hash.clone(),
                                author: c.author.clone(),
                                at: c.at.to_rfc3339(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Two or more, not one: a single commit in the overlap only
                // proves one hold's tenant wrote once, which the lease system
                // already allows for its own holder. Two is the shape that
                // needs a real write from more than one side.
                if commits_in_overlap.len() >= 2 {
                    report.concurrent_writes.push(ConcurrentWrite {
                        path: path.to_string(),
                        first_agent: a.agent.clone(),
                        first_opened_at: a.opened_at.clone(),
                        first_closed_at: a.closed_at.clone().unwrap_or_default(),
                        second_agent: b.agent.clone(),
                        second_opened_at: b.opened_at.clone(),
                        second_closed_at: b.closed_at.clone().unwrap_or_default(),
                        overlap_start: overlap_start.to_rfc3339(),
                        overlap_end: overlap_end.to_rfc3339(),
                        commits_in_overlap,
                    });
                }
            }
        }
    }

    // Scoped to paths leased at some point in this window — see
    // `UncoveredCommit`'s doc comment for why the rest of the tree is out of
    // scope.
    let leased_paths: BTreeSet<&str> = holds.iter().map(|h| h.path.as_str()).collect();
    for path in &leased_paths {
        let Some(touches) = by_path.get(path) else {
            continue;
        };
        let windows: Vec<HoldWindow> = holds
            .iter()
            .filter(|h| h.path == *path)
            .filter_map(|h| {
                parse_at(&h.opened_at).map(|o| HoldWindow {
                    agent: h.agent.clone(),
                    open: o,
                    close: h.closed_at.as_deref().and_then(parse_at),
                })
            })
            .collect();
        for c in touches {
            // Every hold spanning this commit, with WHO held it — the second half
            // is what `covered` used to throw away.
            let covering: Vec<&HoldWindow> = windows
                .iter()
                .filter(|w| w.open <= c.at && w.close.is_none_or(|cl| cl >= c.at))
                .collect();
            if covering.is_empty() {
                report.uncovered_commits.push(UncoveredCommit {
                    path: path.to_string(),
                    hash: c.hash.clone(),
                    author: c.author.clone(),
                    at: c.at.to_rfc3339(),
                });
                continue;
            }
            // Attribution, or the honest absence of it. Without a trailer this
            // stays exactly as permissive as it was — the alternative would be to
            // guess from `author`, which every fleet so far collapses to one git
            // identity for every agent.
            let Some(committer) = c.pact_agent.as_deref() else {
                continue;
            };
            if covering.iter().any(|w| w.agent == committer) {
                continue;
            }
            // Held by someone, and not by whoever committed. The FIRST covering
            // hold is named rather than all of them: more than one covering hold is
            // itself a double-win, which its own check reports.
            let first = covering[0];
            report.cross_held_commits.push(CrossHeldCommit {
                path: path.to_string(),
                hash: c.hash.clone(),
                committer_agent: committer.to_string(),
                holder: first.agent.clone(),
                at: c.at.to_rfc3339(),
                hold_opened_at: first.open.to_rfc3339(),
            });
        }
    }
    report.uncovered_commits.sort_by(|a, b| a.at.cmp(&b.at));
    report.cross_held_commits.sort_by(|a, b| a.at.cmp(&b.at));
}

/// The scope lines, and whether the caller should stop there.
///
/// `true` means this check could not run — a policy that forbade committing, or
/// a git history that could not be read — and everything below would read as a
/// clean bill of health it has not earned.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) -> bool {
    // Before the git check: a policy that forbade committing makes the
    // question vacuous whether or not git is readable, and answering
    // "could not run" would blame the wrong thing.
    if let Some(policy) = &r.commit_policy_skipped {
        out.push(format!(
            "  commit policy: {policy} — correlation not evaluated"
        ));
        out.push(
            "  no agent in this run was permitted to commit, so holds without commits are \
             the policy working, not a finding"
                .to_string(),
        );
        return true;
    }
    if let Some(reason) = &r.git_unavailable {
        out.push(format!(
            "  git history unavailable ({reason}) — commit-correlation could not run"
        ));
        return true;
    }
    // Stated clean or not: with no trailers this check can only ask "did ANYONE
    // hold the path", and a reader has to know that is the question it answered.
    out.push(format!(
        "  {} commit(s) carry a Pact-Agent trailer, {} do not{}",
        r.commits_attributed,
        r.commits_unattributed,
        if r.commits_attributed == 0 && r.commits_unattributed > 0 {
            " — with none attributed, a commit counts as covered when ANY agent held the \
             path, so an agent working without a lease is invisible whenever a compliant peer \
             holds it (see docs/audit.md)"
        } else {
            ""
        }
    ));
    false
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "no concurrent write landed, no commit fell outside every hold's window, and \
     every attributed commit was made by an agent that held the path"
        .to_string()
}

/// Every finding: concurrent writes, uncovered commits, cross-held commits.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for w in &r.concurrent_writes {
        out.push(String::new());
        out.push(format!("CONCURRENT WRITE on {}", w.path));
        out.push(format!(
            "  {} held {} -> {}",
            w.first_agent, w.first_opened_at, w.first_closed_at
        ));
        out.push(format!(
            "  {} held {} -> {}",
            w.second_agent, w.second_opened_at, w.second_closed_at
        ));
        out.push(format!(
            "  {} commit(s) landed in the overlap ({} -> {}):",
            w.commits_in_overlap.len(),
            w.overlap_start,
            w.overlap_end
        ));
        for c in &w.commits_in_overlap {
            out.push(format!(
                "    {} by {} at {}",
                &c.hash[..c.hash.len().min(12)],
                c.author,
                c.at
            ));
        }
    }
    if !r.concurrent_writes.is_empty() {
        out.push(String::new());
        out.push(
            "This is stronger evidence than --check double-win: real commits landed from more\n\
             than one side during the overlap, not just overlapping lease events."
                .to_string(),
        );
    }

    for u in &r.uncovered_commits {
        out.push(String::new());
        out.push(format!(
            "UNCOVERED COMMIT on {}: {} by {} at {} — no hold on this path covered that moment",
            u.path,
            &u.hash[..u.hash.len().min(12)],
            u.author,
            u.at
        ));
    }
    if !r.uncovered_commits.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} commit(s) touched a leased path outside every recorded hold for it — work done \
             with no lease, which the protocol exists to prevent.",
            r.uncovered_commits.len()
        ));
    }

    for x in &r.cross_held_commits {
        out.push(String::new());
        out.push(format!(
            "CROSS-HELD COMMIT on {}: {} by {} at {} — but {} held that path from {}",
            x.path,
            &x.hash[..x.hash.len().min(12)],
            x.committer_agent,
            x.at,
            x.holder,
            x.hold_opened_at
        ));
    }
    if !r.cross_held_commits.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} commit(s) landed on a path a DIFFERENT agent held. Louder than an uncovered \n\
             commit, not quieter: committing where nobody held the path risks your own work, \n\
             committing where somebody else held it corrupts theirs — and a peer's in-flight \n\
             edits are the thing a lease exists to protect.",
            r.cross_held_commits.len()
        ));
    }
}

/// How the answer was reached, and the holds that produced no commit at all.
///
/// Printed after every other check's findings, and separately from [`findings`],
/// because it is informational rather than a finding — a clean run still reaches
/// it (see `render_check`'s early return for the one check that does not stop at
/// zero findings).
pub(in crate::audit) fn correlation_footer(r: &CheckReport, out: &mut Vec<String>) {
    if r.correlated_by_head + r.correlated_by_time > 0 {
        out.push(String::new());
        out.push(match (r.correlated_by_head, r.correlated_by_time) {
            (n, 0) => format!(
                "{n} hold(s) correlated by their recorded HEAD range — an exact commit set \
                 per hold, not a timestamp window"
            ),
            (0, n) => format!(
                "{n} hold(s) correlated by TIMESTAMP WINDOW: no usable HEAD range. Logs \
                 written before pact stamped `head` have none, and a recorded hash stops \
                 resolving after a branch is deleted, force-pushed or read in a shallow \
                 clone. A window can attribute a peer's commit to your hold."
            ),
            (h, t) => format!(
                "{h} hold(s) correlated by their recorded HEAD range (exact), {t} by \
                 timestamp window (inferred — no head recorded, or the hash no longer \
                 resolves)"
            ),
        });
    }
    if !r.holds_with_no_commit.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} hold(s) closed with no commit landing in their window (informational, not \
             counted as a finding — a read-only lease produces no commit):",
            r.holds_with_no_commit.len()
        ));
        for h in &r.holds_with_no_commit {
            out.push(format!(
                "  {:<40} {:<16} {} -> {}",
                h.path,
                h.agent,
                h.opened_at,
                h.closed_at.as_deref().unwrap_or("(open)")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    /// pact-mqw.10: "covered" was answering the wrong question.
    ///
    /// It asked whether ANYONE held the path across a commit, never whether the
    /// COMMITTER did. In the crucible run one agent was told the protocol did not
    /// apply to it; its worst commit touched five files in one unleased shot, and
    /// every one of those paths was under an active lease held by a compliant peer,
    /// so the commit passed. **The rogue's most damaging commit was invisible
    /// specifically because its peers were compliant.**
    #[test]
    fn a_commit_inside_another_agents_hold_is_its_own_louder_class() {
        let tmp = with_git_log(&[
            // agent-a holds src/ast.rs across the whole window, compliantly.
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/ast.rs"),
            &ev("2026-08-01T10:10:00Z", "agent-a", "released", "src/ast.rs"),
        ]);
        // The rogue commits INSIDE agent-a's hold, and says so in its trailer.
        git_commit_as(
            tmp.path(),
            "src/ast.rs",
            "2026-08-01T10:05:00+00:00",
            "rogue",
        );

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert!(r.git_unavailable.is_none(), "{:?}", r.git_unavailable);
        assert_eq!(
            r.uncovered_commits.len(),
            0,
            "a hold DID span it — that is exactly why the old check passed it"
        );
        assert_eq!(r.cross_held_commits.len(), 1, "{:?}", r.cross_held_commits);
        let x = &r.cross_held_commits[0];
        assert_eq!(x.path, "src/ast.rs");
        assert_eq!(x.committer_agent, "rogue");
        assert_eq!(x.holder, "agent-a");
        assert_eq!(r.findings(), 1, "and it must be a finding, not a note");
        let text = render_check(&r);
        assert!(text.contains("CROSS-HELD COMMIT on src/ast.rs"), "{text}");
        assert!(
            text.contains("1 commit(s) carry a Pact-Agent trailer"),
            "{text}"
        );
        // Louder than uncovered, and the render must say why.
        assert!(text.contains("corrupts theirs"), "{text}");
    }

    /// The holder's OWN commit inside its own hold is the compliant case and must
    /// stay silent — the whole point of the protocol working.
    #[test]
    fn a_commit_inside_your_own_hold_is_not_a_finding() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:10:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit_as(tmp.path(), "a.rs", "2026-08-01T10:05:00+00:00", "agent-a");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{r:?}");
        assert_eq!(r.commits_attributed, 1);
        assert_eq!(r.commits_unattributed, 0);
    }

    /// Degrading, not failing. Every log and every commit that exists today
    /// predates the trailer, so an unattributed commit must behave exactly as it
    /// did before this class existed — and the report must SAY that is the question
    /// it answered, or a clean result reads as a stronger claim than it is.
    #[test]
    fn an_unattributed_commit_keeps_the_old_permissive_behaviour_and_says_so() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:10:00Z", "agent-a", "released", "a.rs"),
        ]);
        // No trailer: this could be the rogue or it could be agent-a, and pact
        // cannot tell. It must not guess from the git author, which every fleet
        // collapses to one identity.
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:05:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.cross_held_commits.len(), 0);
        assert_eq!(r.findings(), 0);
        assert_eq!((r.commits_attributed, r.commits_unattributed), (0, 1));
        let text = render_check(&r);
        assert!(
            text.contains("0 commit(s) carry a Pact-Agent trailer, 1 do not"),
            "{text}"
        );
        assert!(
            text.contains("invisible whenever a compliant peer holds it"),
            "an unattributed run must state the weaker question it answered: {text}"
        );
    }

    /// A mixed history: one attributed cross-held commit, one unattributed commit
    /// in the same hold. Exactly one finding, and both counted.
    #[test]
    fn a_mixed_history_reports_only_what_it_can_attribute() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "m.rs"),
            &ev("2026-08-01T10:30:00Z", "agent-a", "released", "m.rs"),
        ]);
        git_commit(tmp.path(), "m.rs", "2026-08-01T10:05:00+00:00");
        git_commit_as(tmp.path(), "m.rs", "2026-08-01T10:10:00+00:00", "rogue");
        git_commit_as(tmp.path(), "m.rs", "2026-08-01T10:15:00+00:00", "agent-a");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!((r.commits_attributed, r.commits_unattributed), (2, 1));
        assert_eq!(r.cross_held_commits.len(), 1, "{:?}", r.cross_held_commits);
        assert_eq!(r.cross_held_commits[0].committer_agent, "rogue");
    }

    /// pact-b73.6, the exact answer: with a head on both boundaries the hold brackets a
    /// real commit range, so the correlation is a lookup rather than an inference from
    /// wall-clock time.
    ///
    /// The timestamps here are deliberately WRONG — the commit's clock puts it far
    /// outside the hold's window — so a pass can only come from the range being used.
    /// That is the whole point: on a busy fleet the window is what misattributes.
    #[test]
    fn a_hold_is_correlated_by_its_recorded_head_range_not_by_time() {
        let tmp = with_git_log(&[]);
        git_commit(tmp.path(), "seed.rs", "2026-08-01T09:00:00+00:00");
        let before = head_of(tmp.path());
        git_commit(tmp.path(), "a.rs", "2026-08-01T23:59:00+00:00");
        let after = head_of(tmp.path());

        let log = tmp.path().join(".pact/events.jsonl");
        std::fs::write(
            &log,
            format!(
                "{}\n{}\n",
                ev_head(
                    "2026-08-01T10:00:00Z",
                    "agent-a",
                    "acquired",
                    "a.rs",
                    &before
                ),
                ev_head(
                    "2026-08-01T10:02:00Z",
                    "agent-a",
                    "released",
                    "a.rs",
                    &after
                ),
            ),
        )
        .unwrap();

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.correlated_by_head, 1, "the range must be used");
        assert_eq!(r.correlated_by_time, 0);
        assert!(
            r.holds_with_no_commit.is_empty(),
            "the commit is inside the RANGE even though it is outside the window: {:?}",
            r.holds_with_no_commit
        );
    }

    /// And the range must be able to say NO: a hold whose range contains commits, none
    /// of them touching the held path, still reports the hold as uncommitted.
    #[test]
    fn a_head_range_that_touches_other_paths_does_not_credit_the_held_one() {
        let tmp = with_git_log(&[]);
        git_commit(tmp.path(), "seed.rs", "2026-08-01T09:00:00+00:00");
        let before = head_of(tmp.path());
        git_commit(tmp.path(), "elsewhere.rs", "2026-08-01T10:01:00+00:00");
        let after = head_of(tmp.path());

        std::fs::write(
            tmp.path().join(".pact/events.jsonl"),
            format!(
                "{}\n{}\n",
                ev_head(
                    "2026-08-01T10:00:00Z",
                    "agent-a",
                    "acquired",
                    "a.rs",
                    &before
                ),
                ev_head(
                    "2026-08-01T10:02:00Z",
                    "agent-a",
                    "released",
                    "a.rs",
                    &after
                ),
            ),
        )
        .unwrap();

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.correlated_by_head, 1);
        assert_eq!(r.holds_with_no_commit.len(), 1, "a.rs was never committed");
    }

    /// The fallback that is not optional: every log written before pact stamped `head`
    /// has none, and the check must degrade to the timestamp window and SAY it did
    /// rather than silently reporting fewer findings.
    #[test]
    fn a_hold_with_no_recorded_head_falls_back_to_the_timestamp_window_and_says_so() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.correlated_by_head, 0);
        assert_eq!(r.correlated_by_time, 1);
        assert!(r.holds_with_no_commit.is_empty(), "the window still works");
        let text = render_check(&r);
        assert!(
            text.contains("TIMESTAMP WINDOW"),
            "the reader must be told which path was taken:\n{text}"
        );
    }

    /// A recorded hash that no longer resolves — a deleted and gc'd worktree branch, a
    /// force-push, a shallow clone — must degrade to the window rather than report
    /// nothing. Reporting nothing would read as "this hold landed no commit", which is
    /// a false finding rather than a missing one.
    #[test]
    fn a_head_that_no_longer_resolves_degrades_to_the_window_rather_than_reporting_nothing() {
        let tmp = with_git_log(&[
            &ev_head(
                "2026-08-01T10:00:00Z",
                "agent-a",
                "acquired",
                "a.rs",
                "dead1ee",
            ),
            &ev_head(
                "2026-08-01T10:02:00Z",
                "agent-a",
                "released",
                "a.rs",
                "dead2ee",
            ),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(
            r.correlated_by_head, 0,
            "an unresolvable range is not a usable one"
        );
        assert_eq!(r.correlated_by_time, 1);
        assert!(
            r.holds_with_no_commit.is_empty(),
            "the fallback found the commit the range could not: {:?}",
            r.holds_with_no_commit
        );
    }

    /// Both eras in one log, which is what an upgrading repository actually looks like.
    #[test]
    fn a_mixed_log_correlates_each_hold_by_whatever_it_recorded() {
        let tmp = with_git_log(&[]);
        git_commit(tmp.path(), "seed.rs", "2026-08-01T09:00:00+00:00");
        let before = head_of(tmp.path());
        git_commit(tmp.path(), "new.rs", "2026-08-01T10:01:00+00:00");
        let after = head_of(tmp.path());
        git_commit(tmp.path(), "old.rs", "2026-08-01T11:01:00+00:00");

        std::fs::write(
            tmp.path().join(".pact/events.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                ev_head(
                    "2026-08-01T10:00:00Z",
                    "agent-a",
                    "acquired",
                    "new.rs",
                    &before
                ),
                ev_head(
                    "2026-08-01T10:02:00Z",
                    "agent-a",
                    "released",
                    "new.rs",
                    &after
                ),
                ev("2026-08-01T11:00:00Z", "agent-b", "acquired", "old.rs"),
                ev("2026-08-01T11:02:00Z", "agent-b", "released", "old.rs"),
            ),
        )
        .unwrap();

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!((r.correlated_by_head, r.correlated_by_time), (1, 1));
        assert!(
            r.holds_with_no_commit.is_empty(),
            "each hold found its commit by its own route: {:?}",
            r.holds_with_no_commit
        );
        let text = render_check(&r);
        assert!(
            text.contains("1 hold(s) correlated by their recorded HEAD range"),
            "{text}"
        );
    }

    #[test]
    fn a_hold_with_a_commit_inside_its_window_is_not_reported() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert!(r.git_unavailable.is_none(), "{:?}", r.git_unavailable);
        assert_eq!(r.findings(), 0);
        assert!(
            r.holds_with_no_commit.is_empty(),
            "a commit landed inside the window: {:?}",
            r.holds_with_no_commit
        );
    }

    /// Informational only — read-only work (research, review, a lease taken
    /// and then released with nothing to show for it) is a legitimate
    /// outcome, not a defect, so this must never move the exit code.
    #[test]
    fn a_hold_with_no_commit_at_all_is_reported_but_is_not_a_finding() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        // Not one line of history exists for a.rs at all — the "read-only
        // lease" shape, distinct from a commit landing outside the window
        // (that is `uncovered_commits`'s job, covered separately below).
        git_commit(tmp.path(), "unrelated.rs", "2026-08-01T10:01:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "an uncommitted hold must not fail the check"
        );
        assert_eq!(r.holds_with_no_commit.len(), 1);
        assert_eq!(r.holds_with_no_commit[0].path, "a.rs");
        assert_eq!(r.holds_with_no_commit[0].agent, "agent-a");
        let text = render_check(&r);
        assert!(text.contains("informational"), "{text}");
        assert!(text.contains("a.rs"), "{text}");
    }

    /// The finding this check exists for: not just overlapping lease
    /// events (already `--check double-win`'s job) but real commits landing
    /// from both sides during the overlap.
    #[test]
    fn two_holds_with_commits_landing_in_their_overlap_are_flagged() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            // agent-b's acquire overlaps agent-a's still-open hold.
            &ev("2026-08-01T10:01:00Z", "agent-b", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
            &ev("2026-08-01T10:03:00Z", "agent-b", "released", "a.rs"),
        ]);
        // Both commits fall inside the overlap window [10:01, 10:02].
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:10+00:00");
        // A second, distinct commit needs a real change to land in history —
        // git commits a no-op tree change once, so touch a second file to
        // force a second commit at the timestamp that matters.
        std::fs::write(tmp.path().join(".marker"), "2").unwrap();
        let run = |args: &[&str], at: &str| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "tester")
                .env("GIT_AUTHOR_EMAIL", "tester@example.com")
                .env("GIT_AUTHOR_DATE", at)
                .env("GIT_COMMITTER_NAME", "tester")
                .env("GIT_COMMITTER_EMAIL", "tester@example.com")
                .env("GIT_COMMITTER_DATE", at)
                .status()
                .unwrap()
        };
        let at = "2026-08-01T10:01:20+00:00";
        std::fs::write(tmp.path().join("a.rs"), "second write").unwrap();
        assert!(run(&["add", "a.rs"], at).success());
        assert!(run(&["commit", "--quiet", "-m", "second touch"], at).success());

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 1, "the concurrent write is a real finding");
        assert_eq!(r.concurrent_writes.len(), 1);
        let w = &r.concurrent_writes[0];
        assert_eq!(w.path, "a.rs");
        assert_eq!(w.commits_in_overlap.len(), 2);
        assert!([&w.first_agent, &w.second_agent].contains(&&"agent-a".to_string()));
        assert!([&w.first_agent, &w.second_agent].contains(&&"agent-b".to_string()));
        let text = render_check(&r);
        assert!(text.contains("CONCURRENT WRITE"), "{text}");
        assert!(text.contains("double-win"), "{text}");
    }

    /// The commit-correlation counterpart to "did anyone hold this path" —
    /// did anyone hold it AT THE TIME this commit landed. A commit outside
    /// every recorded window for a path pact does otherwise coordinate is
    /// work done with no lease at all.
    #[test]
    fn a_commit_outside_every_hold_for_a_leased_path_is_uncovered() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        // Outside the [10:00, 10:02] window, with nothing else leasing it.
        git_commit(tmp.path(), "a.rs", "2026-08-01T12:00:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.uncovered_commits.len(), 1);
        assert_eq!(r.uncovered_commits[0].path, "a.rs");
        let text = render_check(&r);
        assert!(text.contains("UNCOVERED COMMIT"), "{text}");
    }

    /// A path nobody ever leased is a different question this check does not
    /// try to answer — most of a real repository is never leased at any
    /// given moment, and flagging all of it would be pure noise.
    #[test]
    fn a_commit_to_a_never_leased_path_is_never_flagged() {
        let tmp = with_git_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        git_commit(tmp.path(), "a.rs", "2026-08-01T10:01:00+00:00");
        // README.md is never leased by anyone in this log.
        git_commit(tmp.path(), "README.md", "2026-08-01T13:00:00+00:00");

        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        assert!(r.uncovered_commits.is_empty());
    }

    /// `with_log`'s `.git` is a bare directory, not a real repository — the
    /// shape this check must degrade cleanly against rather than panicking
    /// or reporting a false, blanket set of findings.
    #[test]
    fn a_repository_git_cannot_actually_read_degrades_without_crashing() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-a", "released", "a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::CommitCorrelation, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "no git history to correlate against must never itself be a finding"
        );
    }
}
