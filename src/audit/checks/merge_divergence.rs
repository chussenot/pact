//! `--check merge-divergence` (pact-mqw.3): did an agent start editing from a
//! copy the previous holder never produced?

use std::collections::BTreeMap;

use serde::Serialize;

use crate::audit::model::{closes, opens};
use crate::audit::CheckReport;
use crate::events::Event;

/// One path whose next holder started from content the previous holder never
/// produced — the signature of an edit made against a stale copy.
///
/// Both agents were compliant. That is what makes this worth a check of its own:
/// nothing in the lease log looks wrong, because nothing in the lease protocol WAS
/// wrong. The divergence lives between a release on one branch and an acquire on
/// another, in a window no lease covers.
#[derive(Debug, Clone, Serialize)]
pub struct MergeDivergence {
    pub path: String,
    /// Who released, and what they left.
    pub released_by: String,
    pub released_at: String,
    pub released_hash: String,
    /// Who acquired next, and what their copy actually contained.
    pub acquired_by: String,
    pub acquired_at: String,
    pub acquired_hash: String,
    /// 1-based line of the acquire in `.pact/events.jsonl`, which is the only id
    /// an event has (see `events::numbered`).
    pub line: usize,
}

/// Pair each close that recorded a content hash with the next open of the same
/// path that recorded one, and report the pairs that disagree.
///
/// Only *adjacent* pairs, and only in that order: a hash that differs two holds
/// later says nothing, because the intervening holder was entitled to change the
/// file. The claim being made is narrow on purpose — "the copy this agent started
/// from is not the copy the last agent finished with" — and anything wider would
/// flag ordinary sequential work.
///
/// Renewals are neither, and are skipped: a renewal is the same hold continuing,
/// so treating it as a new open would compare an agent against itself.
pub(in crate::audit) fn merge_divergences(
    events: &[(usize, Event)],
) -> (Vec<MergeDivergence>, usize) {
    // Per path: the last close's (agent, at, hash), waiting for the next open.
    let mut pending: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    let mut out = Vec::new();
    let mut unhashed = 0usize;
    for (line, e) in events {
        let Some(path) = e.path.as_deref() else {
            continue;
        };
        if closes(&e.kind) {
            match &e.content_hash {
                Some(h) => {
                    pending.insert(path.to_string(), (e.agent.clone(), e.at.clone(), h.clone()));
                }
                // A close with no hash cannot anchor the next comparison, and must
                // also not leave a STALE anchor behind for it — that would compare
                // an acquire against a release two holds back.
                None => {
                    pending.remove(path);
                    unhashed += 1;
                }
            }
            continue;
        }
        if !opens(&e.kind) {
            continue;
        }
        // Consumed either way: one close anchors exactly one comparison, so a
        // third and fourth acquire of an unchanged path do not each report it.
        let Some((released_by, released_at, released_hash)) = pending.remove(path) else {
            continue;
        };
        let Some(acquired_hash) = e.content_hash.clone() else {
            continue;
        };
        if acquired_hash == released_hash {
            continue;
        }
        out.push(MergeDivergence {
            path: path.to_string(),
            released_by,
            released_at,
            released_hash,
            acquired_by: e.agent.clone(),
            acquired_at: e.at.clone(),
            acquired_hash,
            line: *line,
        });
    }
    (out, unhashed)
}

/// The scope line, stated even when clean.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) {
    // Same reasoning as topology's and chain-integrity's scope lines, and
    // stated even when clean: a close with no content hash gives the acquire
    // after it nothing to compare against, so a reader has to know how much
    // of the log this check could speak to before believing what it says.
    out.push(format!(
        "  {} close(s) recorded no content hash, so the acquire after each had nothing to \
         compare against (logs written before pact stamped releases are entirely in this \
         state)",
        r.divergence_unhashed
    ));
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "every hold started from the content the previous holder left — no edit was made \
     against a stale copy"
        .to_string()
}

/// Every divergence found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for d in &r.merge_divergences {
        out.push(String::new());
        out.push(format!(
            "MERGE DIVERGENCE on {}: {} released it at {} leaving {}, then {} acquired it at {} \
             holding {} instead (line {})",
            d.path,
            d.released_by,
            d.released_at,
            &d.released_hash[..d.released_hash.len().min(12)],
            d.acquired_by,
            d.acquired_at,
            &d.acquired_hash[..d.acquired_hash.len().min(12)],
            d.line
        ));
    }
    if !r.merge_divergences.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} hold(s) started from a copy the previous holder never produced. Both agents were \n\
             compliant — a lease is exclusive in TIME, not across worktrees, so the conflict was \n\
             deferred to a merge no lease covered. git often merges such edits with NO conflict \n\
             marker when they are textually non-adjacent, which is the shape an additive change \n\
             to a shared enum or match statement has. Check those paths for duplicated arms, \n\
             duplicated definitions, or a peer's change silently reverted — see docs/leases.md.",
            r.merge_divergences.len()
        ));
    }
    // How the answer was reached, whenever this check ran at all. A reader must be able
    // to see that a hold was correlated EXACTLY rather than inferred from wall-clock
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    #[test]
    fn merge_divergence_flags_an_acquire_from_a_copy_the_releaser_never_left() {
        let tmp = with_log(&[
            // Compliant hold: agent-a takes it at aaa, leaves it as bbb.
            &ev_hash(
                "2026-08-11T09:00:00Z",
                "agent-a",
                "acquired",
                "src/printer.rs",
                "aaa",
            ),
            &ev_hash(
                "2026-08-11T09:10:00Z",
                "agent-a",
                "released",
                "src/printer.rs",
                "bbb",
            ),
            // agent-b acquires on a branch that never contained agent-a's change.
            &ev_hash(
                "2026-08-11T09:11:00Z",
                "agent-b",
                "acquired",
                "src/printer.rs",
                "aaa",
            ),
            &ev_hash(
                "2026-08-11T09:20:00Z",
                "agent-b",
                "released",
                "src/printer.rs",
                "ccc",
            ),
            // And a path whose successor DID start from what was left: silent.
            &ev_hash(
                "2026-08-11T09:00:00Z",
                "agent-c",
                "acquired",
                "src/ok.rs",
                "111",
            ),
            &ev_hash(
                "2026-08-11T09:05:00Z",
                "agent-c",
                "released",
                "src/ok.rs",
                "222",
            ),
            &ev_hash(
                "2026-08-11T09:06:00Z",
                "agent-d",
                "acquired",
                "src/ok.rs",
                "222",
            ),
        ]);
        let r = run_check(tmp.path(), Check::MergeDivergence, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.merge_divergences);
        let d = &r.merge_divergences[0];
        assert_eq!(d.path, "src/printer.rs");
        assert_eq!(
            (d.released_by.as_str(), d.released_hash.as_str()),
            ("agent-a", "bbb")
        );
        assert_eq!(
            (d.acquired_by.as_str(), d.acquired_hash.as_str()),
            ("agent-b", "aaa")
        );
        // The renderer must name both sides and say what to look for, or the
        // finding is unactionable.
        let text = render_check(&r);
        assert!(
            text.contains("MERGE DIVERGENCE on src/printer.rs"),
            "{text}"
        );
        assert!(text.contains("NO conflict"), "{text}");
    }

    /// The claim is narrow on purpose: only ADJACENT close/open pairs. A hash that
    /// differs two holds later says nothing, because the intervening holder was
    /// entitled to change the file — and a close anchors exactly one comparison, so
    /// repeated acquires of an unchanged path do not each report it.
    #[test]
    fn merge_divergence_compares_only_adjacent_holds() {
        let tmp = with_log(&[
            &ev_hash("2026-08-11T09:00:00Z", "a", "acquired", "p.rs", "h1"),
            &ev_hash("2026-08-11T09:01:00Z", "a", "released", "p.rs", "h2"),
            // Started from h2: fine. Legitimately changed it to h3.
            &ev_hash("2026-08-11T09:02:00Z", "b", "acquired", "p.rs", "h2"),
            &ev_hash("2026-08-11T09:03:00Z", "b", "released", "p.rs", "h3"),
            // Started from h3: fine, even though it differs from h1 and h2.
            &ev_hash("2026-08-11T09:04:00Z", "c", "acquired", "p.rs", "h3"),
        ]);
        let r = run_check(tmp.path(), Check::MergeDivergence, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.merge_divergences);
    }

    /// Every log written before pact stamped releases has no hash to compare
    /// against. That must be reported as SCOPE, never as a finding — flagging it
    /// would fail every existing repository the moment the check shipped, which is
    /// the same discipline `chain_untracked` and `topology_unstamped` follow.
    ///
    /// And an unhashed close must not leave a stale anchor behind: comparing the
    /// next acquire against a release two holds back would invent a finding.
    #[test]
    fn merge_divergence_treats_an_unhashed_close_as_scope_not_a_finding() {
        let tmp = with_log(&[
            &ev_hash("2026-08-11T09:00:00Z", "a", "acquired", "p.rs", "h1"),
            // A pre-stamping release.
            &ev("2026-08-11T09:01:00Z", "a", "released", "p.rs"),
            // Would have "diverged" from h1 if the walk fell back to the acquire.
            &ev_hash("2026-08-11T09:02:00Z", "b", "acquired", "p.rs", "h9"),
        ]);
        let r = run_check(tmp.path(), Check::MergeDivergence, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.merge_divergences);
        assert_eq!(r.divergence_unhashed, 1);
        assert!(
            render_check(&r).contains("1 close(s) recorded no content hash"),
            "the scope must be stated even when the check is clean: {}",
            render_check(&r)
        );
        // And the clean message must be this check's own, not the one the
        // catch-all arm used to hand every new check.
        assert!(
            render_check(&r).contains("no edit was made against a stale copy"),
            "{}",
            render_check(&r)
        );
    }
}
