//! `--check stale-holds`: holds that ran past their own recorded TTL with no
//! renew.
//!
//! Judged against the TTL the opening event RECORDED, never against today's
//! compiled default — see `LEGACY_DEFAULT_TTL_SECS` for what re-judging old
//! history by a moved default would do to the verdicts.

use crate::audit::model::{Hold, LEGACY_DEFAULT_TTL_SECS};
use crate::audit::{secs, CheckReport};
use crate::lease::{ttl_as_i64, DEFAULT_TTL_SECS};

/// Every hold that outlived its own recorded TTL without a single renew.
pub(in crate::audit) fn detect(holds: Vec<Hold>, report: &mut CheckReport) {
    // No single threshold any more: each hold is judged against its own
    // recorded TTL. `ttl_secs` on the report stays as the CURRENT default,
    // for context only, and the per-hold value is what decided each row.
    report.ttl_secs = Some(DEFAULT_TTL_SECS);
    // Over TTL AND never renewed. The protocol says a long task must not
    // outlive its lease and to renew if it does, so a long hold that
    // renewed is an agent following instructions — reporting it would
    // train people to ignore this check. A hold that lapsed into
    // `expired` is included whatever its length: that is the same smell,
    // already realised.
    report.stale_holds = holds
        .into_iter()
        .filter(|h| {
            let over = h.held_secs.unwrap_or(0) > ttl_as_i64(h.ttl_secs);
            let lapsed = h.closed_by.as_deref() == Some("expired");
            (over || lapsed) && h.renewals == 0
        })
        .collect();
    // Longest first: the worst offender is what a reader wants at the top.
    // `Reverse` rather than a flipped comparator, which clippy is right to
    // object to — the key form cannot get the operands the wrong way round.
    report
        .stale_holds
        .sort_by_key(|h| std::cmp::Reverse(h.held_secs));
}

/// What this check prints when it found nothing.
///
/// The catch-all arm in `render_check`'s match, and named there rather than
/// left implicit: a new check landing on that arm inherited this message once.
pub(in crate::audit) fn clean() -> String {
    "no holds ran past their own recorded TTL without a renew".to_string()
}

/// Every stale hold found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for h in &r.stale_holds {
        let ended = match h.closed_by.as_deref() {
            Some(k) => format!("ended by {k}"),
            None => "still open".to_string(),
        };
        out.push(format!(
            "  {:<40} {:<16} held {:>8} vs ttl {:>7}{}, {} (line {})",
            h.path,
            h.agent,
            h.held_secs.map(secs).unwrap_or_else(|| "?".to_string()),
            secs(ttl_as_i64(h.ttl_secs)),
            if h.ttl_assumed { "*" } else { " " },
            ended,
            h.opened_line
        ));
        // A hold that outran its TTL is two different smells, and this is the only
        // thing that separates them (pact-88z): a worker overrunning a TTL it
        // should have renewed, versus a lock nobody is behind. Appended rather
        // than folded into the row, because it is present-tense state beside a
        // historical finding and the reader has to be able to tell which is which.
        //
        // **Expired holds are qualified too, and that is pact-k1n.3.** This used to
        // fire only on `closed_by == None`, on the reasoning that an expired hold
        // is already over so there is nothing live to say about it. The modmill
        // proving-ground run is the counter-example: its one real stale hold had
        // been reclaimed by the TTL, so `closed_by` was `expired`, so the qualifier
        // skipped precisely the hold the check existed to explain. The only shape
        // that occurred in the field was the one shape not covered.
        //
        // For an ended hold the question is a DIFFERENT one, not the same one asked
        // later: not "is somebody behind this lock" — there is no lock — but **was
        // this agent seen after the lease lapsed**. An agent seen since overran a
        // TTL and kept working, which is a protocol lapse by a healthy agent; an
        // agent not seen since died holding the path, which is an incident. Same
        // record, opposite readings, and `run_check` computes which by comparing
        // the activity stamp against the hold's own `closed_at`.
        //
        // That comparison is what makes this safe, and it is worth being explicit
        // because the objection to qualifying closed holds was a good one: the
        // activity record is a single timestamp, not a history, so classifying it
        // present-tense says something about today rather than about a hold that
        // ended last Tuesday. "Seen since X" is the one question a single timestamp
        // answers exactly — it needs an ordering, not a history.
        //
        // `released` and `stolen` are still skipped, and deliberately: the first is
        // an agent that acted, the second is a peer that took over. Neither leaves
        // the "is anybody there" question open.
        let qualify = matches!(h.closed_by.as_deref(), None | Some("expired"));
        if let Some(state) = qualify.then(|| r.liveness.get(&h.agent)).flatten() {
            out.push(format!("  {:<40} holder is {}", "", state));
        }
    }
    if !r.stale_holds.is_empty() {
        out.push(String::new());
        // Deliberately no "distinct incidents" count. One `lease acquire` naming
        // three paths writes three event rows whose timestamps differ by
        // microseconds, so grouping them would need a tolerance window — and a
        // number that depends on an arbitrary tolerance is worse than no number.
        // Holds sharing an agent and a duration are almost certainly one acquire;
        // the reader can see that from the rows.
        let assumed = r.stale_holds.iter().filter(|h| h.ttl_assumed).count();
        if assumed > 0 {
            out.push(format!(
                "  * {assumed} hold(s) predate pact recording a TTL per event; judged against the \
                 {}s default of that era, not today's.",
                LEGACY_DEFAULT_TTL_SECS
            ));
        }
        out.push(String::new());
        out.push(format!(
            "{} hold(s) ran past their OWN recorded TTL without a single renew. Rows sharing an agent and a\n\
             duration are one `lease acquire` that named several paths. The protocol says a long\n\
             task must not outlive its lease, and `pact lease renew` refreshes it — a lapsed lease\n\
             is reclaimable by anyone, so each of these is a window where a peer could have taken\n\
             a path its holder still believed it owned. (The current default is {}.)",
            r.stale_holds.len(),
            secs(ttl_as_i64(r.ttl_secs.unwrap_or(DEFAULT_TTL_SECS)))
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    #[test]
    fn stale_holds_reports_long_unrenewed_holds_only() {
        let tmp = with_log(&[
            // 2 hours, never renewed: the smell.
            &ev("2026-08-01T10:00:00Z", "slow", "acquired", "src/slow.rs"),
            &ev("2026-08-01T12:00:00Z", "slow", "released", "src/slow.rs"),
            // 2 hours, renewed: following the protocol, not a finding.
            &ev("2026-08-01T10:00:00Z", "good", "acquired", "src/good.rs"),
            &ev("2026-08-01T10:30:00Z", "good", "renewed", "src/good.rs"),
            &ev("2026-08-01T12:00:00Z", "good", "released", "src/good.rs"),
            // Short and unrenewed: fine.
            &ev("2026-08-01T10:00:00Z", "quick", "acquired", "src/quick.rs"),
            &ev("2026-08-01T10:00:30Z", "quick", "released", "src/quick.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1, "only the long unrenewed hold");
        assert_eq!(r.stale_holds[0].agent, "slow");
        assert_eq!(r.stale_holds[0].held_secs, Some(7200));
        assert_eq!(r.ttl_secs, Some(DEFAULT_TTL_SECS));
        assert!(render_check(&r).contains("without a single renew"));
    }

    /// The assertion that makes raising the default safe. Each hold is judged
    /// against the TTL IT recorded, so a hold taken under a short TTL stays a
    /// finding no matter what the binary is compiled with — and one taken under a
    /// long TTL is not a finding even though it ran longer.
    #[test]
    fn a_hold_is_judged_against_its_own_recorded_ttl_not_the_compiled_default() {
        let tmp = with_log(&[
            // 30 min under a 10 min TTL: over its own, and would ALSO be over a
            // 900s default — so this row alone would not prove anything.
            &ev_ttl("2026-08-01T10:00:00Z", "short", "acquired", "a.rs", 600),
            &ev_ttl("2026-08-01T10:30:00Z", "short", "released", "a.rs", 600),
            // 30 min under a 2 hour TTL: LONGER than the old 900s default, and
            // still not a finding, because its own TTL covered it. Under a
            // hardcoded threshold this would be reported.
            &ev_ttl("2026-08-01T11:00:00Z", "generous", "acquired", "b.rs", 7200),
            &ev_ttl("2026-08-01T11:30:00Z", "generous", "released", "b.rs", 7200),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1, "only the hold that outran its own TTL");
        assert_eq!(r.stale_holds[0].agent, "short");
        assert_eq!(r.stale_holds[0].ttl_secs, 600);
        assert!(!r.stale_holds[0].ttl_assumed);
    }

    /// pact-m7j.9.10: a bare `ttl_secs as i64` bit-reinterprets `u64::MAX` as
    /// `-1`, so a "hold forever" lease's TTL read back negative and every
    /// hold — however short — compared as `over` it. `Check::StaleHolds` has
    /// its own independent cast from `lease.rs`'s, so this pins it separately.
    #[test]
    fn a_u64_max_ttl_is_never_reported_as_stale() {
        let tmp = with_log(&[
            &ev_ttl(
                "2026-08-01T10:00:00Z",
                "forever",
                "acquired",
                "a.rs",
                u64::MAX,
            ),
            &ev_ttl(
                "2026-08-01T10:00:10Z",
                "forever",
                "released",
                "a.rs",
                u64::MAX,
            ),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "a u64::MAX ttl must never read back as already over its own TTL"
        );
    }

    /// Events written before pact recorded a TTL are judged against the default of
    /// THEIR era, not today's. Without this, raising the default would silently
    /// clear every historical finding — 22 of them in this repository — with
    /// nothing having changed about the holds.
    #[test]
    fn holds_with_no_recorded_ttl_use_the_legacy_default() {
        let tmp = with_log(&[
            // 20 minutes, no ttl_secs: over the 900s of its era, under a 2700s
            // present-day default.
            &ev("2026-08-01T10:00:00Z", "historic", "acquired", "a.rs"),
            &ev("2026-08-01T10:20:00Z", "historic", "released", "a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "a pre-recording hold must still be judged against 900s"
        );
        let h = &r.stale_holds[0];
        assert_eq!(h.ttl_secs, LEGACY_DEFAULT_TTL_SECS);
        assert!(
            h.ttl_assumed,
            "and the report must say the TTL was inferred"
        );
        let text = render_check(&r);
        assert!(text.contains("predate pact recording a TTL"), "{text}");
    }

    /// A renew that grants a different TTL moves the window's threshold with it.
    #[test]
    fn a_renew_updates_the_ttl_the_hold_is_judged_against() {
        let tmp = with_log(&[
            &ev_ttl("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", 600),
            // Renewed onto a much longer TTL, so the 30-minute hold is covered.
            &ev_ttl("2026-08-01T10:05:00Z", "a", "renewed", "a.rs", 7200),
            &ev_ttl("2026-08-01T10:30:00Z", "a", "released", "a.rs", 7200),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        // Renewals also disqualify it, which is the pre-existing rule; the point
        // here is that the recorded TTL followed the renew.
        assert_eq!(r.findings(), 0);
    }

    /// pact-m7j.1.3: `acquire_many`'s rollback logs a "restored" event when it
    /// undoes a refresh (lease.rs, pact-m7j.1.2). That "renewed" it undoes must
    /// stop counting toward the hold's renewal total — otherwise the phantom
    /// renewal exempts a hold from `stale-holds` even after it later genuinely
    /// lapses, which is exactly the case this check exists to catch.
    #[test]
    fn a_restored_hold_still_counts_as_never_renewed() {
        let tmp = with_log(&[
            // Pre-batch acquire under a 600s ttl.
            &ev_ttl("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", 600),
            // A batch acquire refreshes it onto a much longer ttl...
            &ev_ttl("2026-08-01T10:01:00Z", "a", "renewed", "a.rs", 7200),
            // ...then fails on a later path and rolls the refresh back.
            &ev_ttl("2026-08-01T10:02:00Z", "a", "restored", "a.rs", 600),
            // No further activity: the restored 600s ttl lapses for real.
            &ev_ttl("2026-08-01T11:00:00Z", "a", "expired", "a.rs", 600),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "the retracted renewal must not exempt a hold that really lapsed"
        );
        assert_eq!(r.stale_holds[0].renewals, 0, "the renewal was undone");
        assert_eq!(
            r.stale_holds[0].ttl_secs, 600,
            "judged against the restored ttl, not the batch's"
        );
        assert_eq!(r.stale_holds[0].closed_by.as_deref(), Some("expired"));
    }

    #[test]
    fn a_lapsed_lease_is_a_stale_hold_even_if_short() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "gone", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:05Z", "gone", "expired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.stale_holds[0].closed_by.as_deref(), Some("expired"));
    }

    /// A hold the TTL reclaimed is qualified by whether its agent was seen SINCE
    /// (pact-k1n.3) — the shape the modmill proving-ground run actually produced,
    /// and the one this check used to skip.
    ///
    /// Both directions, because they are opposite verdicts on identical rows: an
    /// agent seen after the lapse overran a TTL and kept working; an agent not seen
    /// since died holding the path.
    #[test]
    fn an_expired_hold_says_whether_its_agent_was_seen_since() {
        for (label, active_at, expected) in [
            (
                "came back",
                "2026-08-01T12:00:00Z",
                "it outlived its own lease",
            ),
            ("never did", "2026-08-01T09:00:00Z", "never came back"),
        ] {
            let tmp = with_log(&[
                &ev_ttl("2026-08-01T10:00:00Z", "w3", "acquired", "a.rs", 600),
                &ev_ttl("2026-08-01T11:00:00Z", "w3", "expired", "a.rs", 600),
            ]);
            // The activity record the fleet would have written by simply running
            // pact commands — before the lapse in one case, after it in the other.
            let dir = tmp.path().join(".pact/activity");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("w3"), active_at).unwrap();

            let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
            assert_eq!(
                r.findings(),
                1,
                "{label}: the hold itself is still the finding"
            );
            assert_eq!(r.stale_holds[0].closed_by.as_deref(), Some("expired"));

            let text = render_check(&r);
            assert!(
                text.contains(expected),
                "{label}: expected {expected:?} in:\n{text}"
            );
        }
    }

    /// No activity record is NOT evidence of death. Every repository is in that
    /// state until an agent runs a pact new enough to write one, and a fleet that
    /// ran on an older pact must not be reported as a fleet of corpses.
    #[test]
    fn an_expired_hold_with_no_activity_record_is_left_unqualified() {
        let tmp = with_log(&[
            &ev_ttl("2026-08-01T10:00:00Z", "w3", "acquired", "a.rs", 600),
            &ev_ttl("2026-08-01T11:00:00Z", "w3", "expired", "a.rs", 600),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None, false).unwrap();
        assert_eq!(r.findings(), 1);
        assert!(
            r.liveness.is_empty(),
            "no record means no claim: {:?}",
            r.liveness
        );
        let text = render_check(&r);
        assert!(!text.contains("never came back"), "{text}");
        assert!(!text.contains("outlived"), "{text}");
    }
}
