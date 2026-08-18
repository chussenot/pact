//! `--check double-win`: two agents holding one path at the same time.
//!
//! **The detection is not here, and that is deliberate.** An overlap is only
//! visible while walking a path's open hold windows in time order, so it is
//! produced by `model::reconstruct` — the same pass that produces the holds
//! every other check reads. Splitting it out would mean walking the log twice
//! and keeping two copies of the re-entrant-acquire and takeover argument in
//! step with each other. What lives here is what `--check double-win` says
//! about what that pass found, and the fixture tests that pin it.
//!
//! [`DoubleWin`] and `HoldingAgent` are in `model` beside `reconstruct` for the
//! same reason.

use crate::audit::CheckReport;

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "no overlapping hold windows — no two agents ever held one path at once".to_string()
}

/// Every overlap found, and the bead the report exists to feed.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for d in &r.double_wins {
        out.push(String::new());
        out.push(format!("DOUBLE-WIN on {}", d.path));
        out.push(format!(
            "  {} {} at {} (line {})",
            d.incoming_agent, d.incoming_kind, d.incoming_at, d.incoming_line
        ));
        for h in &d.already_holding {
            out.push(format!(
                "  while {} had held it since {} (line {})",
                h.agent, h.since, h.since_line
            ));
        }
    }
    if !r.double_wins.is_empty() {
        out.push(String::new());
        // The whole reason this check exists, said where the reader is looking.
        out.push(
            "This is the trigger condition for the guard-file backlog item (pact-ehi), which\n\
             says to implement the guard file if and only if a double-win appears in a real\n\
             events log. Attach this output to that bead: it is the evidence the bead is\n\
             waiting for, and the reason not to implement on suspicion."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, summary, Check};

    #[test]
    fn a_clean_history_has_no_findings() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "agent-a", "released", "src/a.rs"),
            &ev("2026-08-01T10:06:00Z", "agent-b", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:07:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "sequential holds are not a double-win");

        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(s.events, 4);
        assert_eq!(s.agents, ["agent-a", "agent-b"]);
        assert_eq!(s.open_holds, 0);
        let h = s.hold_secs.expect("two completed holds");
        assert_eq!(h.completed, 2);
        assert_eq!(h.max_secs, 300);
        assert_eq!(s.top_contended[0].distinct_agents, 2);
    }

    #[test]
    fn one_double_win_is_found_with_both_agents_and_lines() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            // No release in between: agent-b takes a path agent-a still holds.
            &ev("2026-08-01T10:01:00Z", "agent-b", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:02:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        let d = &r.double_wins[0];
        assert_eq!(d.path, "src/a.rs");
        assert_eq!(d.incoming_agent, "agent-b");
        assert_eq!(d.incoming_line, 2, "the line number is the event id");
        assert_eq!(d.already_holding.len(), 1);
        assert_eq!(d.already_holding[0].agent, "agent-a");
        assert_eq!(d.already_holding[0].since_line, 1);

        // The rendering must point at the bead whose trigger condition this is.
        let text = render_check(&r);
        assert!(text.contains("pact-ehi"), "{text}");
        assert!(
            text.contains("agent-a") && text.contains("agent-b"),
            "{text}"
        );
    }

    /// A reclaim is `expired` (the dead holder) then `stolen` (the new one), and
    /// it must NOT read as a double-win — otherwise every routine takeover in
    /// every log is a false finding and the check is useless.
    #[test]
    fn a_routine_reclaim_after_expiry_is_not_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:30:00Z", "agent-a", "expired", "src/a.rs"),
            &ev("2026-08-01T10:30:01Z", "agent-b", "stolen", "src/a.rs"),
            &ev("2026-08-01T10:31:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "expired closes the old window first");
    }

    /// The crucible regression (pact-mqw.1). A holder is SIGKILLed, so it never
    /// releases; a successor steals the path before the TTL lapses, so no
    /// `expired` fires for it either. The `displaced` row is the ONLY thing that
    /// ever closes that window.
    ///
    /// Without it, the dead agent stayed open for the rest of the log and every
    /// later acquire of the path was reported against it. On the real crucible
    /// log that was nine findings, eight naming one killed agent, and not one of
    /// them a concurrent hold. The steal itself is still reported — see the test
    /// below, that part is deliberate — but it must not leak past the steal.
    #[test]
    fn a_killed_holders_window_does_not_leak_past_the_steal() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "victim", "acquired", "src/a.rs"),
            // no release, no expiry: the process was killed
            &ev("2026-08-01T10:05:00Z", "heir", "stolen", "src/a.rs"),
            &ev("2026-08-01T10:05:01Z", "victim", "displaced", "src/a.rs"),
            &ev("2026-08-01T10:09:00Z", "heir", "released", "src/a.rs"),
            // Three later, entirely uncontended acquires of the same path.
            &ev("2026-08-01T10:20:00Z", "agent-c", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:21:00Z", "agent-c", "released", "src/a.rs"),
            &ev("2026-08-01T10:30:00Z", "agent-d", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:31:00Z", "agent-d", "released", "src/a.rs"),
            &ev("2026-08-01T10:40:00Z", "agent-e", "acquired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "only the steal itself, never the clean acquires after it: {:?}",
            r.double_wins
        );
        assert_eq!(r.double_wins[0].incoming_agent, "heir");
    }

    /// `displaced` closes the victim's window even when the log never shows the
    /// steal opening one for the thief — a truncated or bounded log that starts
    /// mid-takeover must not leave the victim open forever either.
    #[test]
    fn a_displaced_row_closes_its_own_holder_not_the_event_author() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "victim", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:01Z", "victim", "displaced", "src/a.rs"),
            &ev("2026-08-01T10:20:00Z", "later", "acquired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.double_wins);
    }

    /// A `--steal` over a LIVE lease is a real overlap at the instant it happens
    /// and must be reported. Deliberate, and the reason the `displaced` row that
    /// closes the victim's window is logged AFTER the `stolen` row rather than
    /// before: closing first would silently retire this detection.
    #[test]
    fn stealing_a_live_lease_is_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "agent-b", "stolen", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.double_wins[0].incoming_kind, "stolen");
    }

    /// pact re-acquires its own lease to refresh it, deliberately. That is one
    /// window, not two, and not a finding.
    #[test]
    fn a_reentrant_acquire_by_the_same_agent_is_not_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:10Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:20Z", "agent-a", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        let s = summary(tmp.path(), None, false).unwrap();
        assert_eq!(
            s.hold_secs.unwrap().completed,
            1,
            "a refresh extends one hold rather than opening a second"
        );
    }
}
