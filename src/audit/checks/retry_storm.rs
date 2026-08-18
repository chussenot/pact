//! `--check retry-storm` (pact-1gv.3): which agents busy-retried a lease instead
//! of backing off. The only check about what the FLEET wasted rather than what
//! pact got wrong.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::audit::model::{is_injector, opens, parse_at};
use crate::audit::{secs, CheckReport};
use crate::events::Event;

/// One (agent, path) that was refused over and over.
///
/// The crucible shape: agent-02 refused `src/eval.rs` 33 times, and the retry
/// spacing was not adaptive-with-jitter but **15 seconds flat** — 27 of 32 gaps
/// exactly 15, against a median advertised remaining hold of 355 seconds. It
/// retried roughly 24x more often than the number in its own refusal message told
/// it to, and the holder's note, quoted back in every single one, said "LONG BEAD,
/// will renew".
///
/// Nothing here broke a rule, which is why the report says the fleet wasted work
/// rather than that pact was violated.
#[derive(Debug, Clone, Serialize)]
pub struct RetryStorm {
    pub agent: String,
    pub path: String,
    pub refusals: usize,
    /// Seconds between consecutive refusals: the smallest, and the middle one.
    pub min_gap_secs: Option<i64>,
    pub median_gap_secs: Option<i64>,
    /// Median of the holder's remaining lease across those refusals — the number a
    /// rational wait would be keyed to. `None` for logs written before
    /// `holder_remaining_secs` was recorded (pact-1gv.1).
    pub median_holder_remaining_secs: Option<i64>,
    /// Did this agent ever end up holding the path?
    pub ever_claimed: bool,
}

/// How many refusals of one path by one agent stop being contention and start being
/// a poll loop.
///
/// Five, and the number barely matters: the crucible storms were 33, 20, 14, 13 and
/// 8, while ordinary resolved contention in the same log sat at 1. There is a wide
/// empty band between the two, so any threshold in it gives the same answer — which
/// is the argument for having one at all rather than tuning it.
const RETRY_STORM_REFUSALS: usize = 5;

/// A retry is "impatient" when it comes back in less than this fraction of what the
/// holder said it had left.
///
/// A quarter, chosen to be generous rather than tight: an agent that waits a third
/// of the remaining hold is doing something defensible, and the shape this exists
/// to catch is not marginal. agent-02 waited 15 seconds against a median 355 —
/// about a twenty-fourth.
const RETRY_IMPATIENCE_RATIO: i64 = 4;

/// Group refusals by (agent, path) and report the ones that hammered.
///
/// Two independent shapes, either of which flags:
///
/// - **volume** — more than [`RETRY_STORM_REFUSALS`] refusals of one path by one
///   agent. Works on any log, including every one written before the holder's
///   remaining lease was recorded.
/// - **impatience** — median spacing far below the holder's advertised remaining
///   lease. This is the shape that names the actual mistake, and it needs
///   `holder_remaining_secs` (pact-1gv.1): before that field, the number was in
///   English inside `detail`, and a check that regexes its own prose breaks the
///   next time somebody improves the wording.
///
/// Non-agent identities are excluded. `chaos-ghost` is `scripts/chaos.sh` planting
/// a stale lease, and in the crucible log its single failed acquire would otherwise
/// be reported as a badly-behaved peer — the fault injector's deliberate skip
/// counted against the fleet.
pub(in crate::audit) fn retry_storms(events: &[(usize, Event)], report: &mut CheckReport) {
    let mut by_pair: BTreeMap<(&str, &str), Vec<&Event>> = BTreeMap::new();
    for (_, e) in events {
        if e.kind != "refused" || is_injector(&e.agent) {
            continue;
        }
        if let Some(path) = e.path.as_deref() {
            by_pair.entry((e.agent.as_str(), path)).or_default().push(e);
        }
        if e.kind == "refused" && e.holder_remaining_secs.is_none() {
            report.refusals_without_remaining += 1;
        }
    }
    let claimed: BTreeSet<(&str, &str)> = events
        .iter()
        .filter(|(_, e)| opens(&e.kind))
        .filter_map(|(_, e)| e.path.as_deref().map(|p| (e.agent.as_str(), p)))
        .collect();

    for ((agent, path), rows) in by_pair {
        let mut gaps: Vec<i64> = rows
            .windows(2)
            .filter_map(|w| {
                let (a, b) = (parse_at(&w[0].at)?, parse_at(&w[1].at)?);
                Some((b - a).num_seconds())
            })
            .collect();
        gaps.sort_unstable();
        let mut remaining: Vec<i64> = rows
            .iter()
            .filter_map(|e| e.holder_remaining_secs)
            .collect();
        remaining.sort_unstable();
        let median = |v: &[i64]| v.get(v.len() / 2).copied();
        let median_gap = median(&gaps);
        let median_remaining = median(&remaining);

        let by_volume = rows.len() > RETRY_STORM_REFUSALS;
        // Only when both numbers exist. A missing remaining-lease is scope, not
        // evidence of patience.
        let by_impatience = match (median_gap, median_remaining) {
            (Some(gap), Some(rem)) => rem > 0 && gap * RETRY_IMPATIENCE_RATIO < rem,
            _ => false,
        };
        if !by_volume && !by_impatience {
            continue;
        }
        report.retry_storms.push(RetryStorm {
            agent: agent.to_string(),
            path: path.to_string(),
            refusals: rows.len(),
            min_gap_secs: gaps.first().copied(),
            median_gap_secs: median_gap,
            median_holder_remaining_secs: median_remaining,
            ever_claimed: claimed.contains(&(agent, path)),
        });
    }
    // Worst first: the loudest offender is what a reader wants at the top.
    report
        .retry_storms
        .sort_by_key(|r| std::cmp::Reverse(r.refusals));
}

/// The scope line, stated even when clean.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) {
    // Scope before verdict, clean or not: the impatience half of this check
    // cannot speak to a refusal whose holder-remaining was never recorded.
    out.push(format!(
        "  {} refusal(s) carry no holder-remaining, so only their COUNT could be \
         judged (logs written before pact recorded it are entirely in this state)",
        r.refusals_without_remaining
    ));
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "no agent hammered a lease it was refused — every retry was either rare or \
     spaced against what the holder advertised"
        .to_string()
}

/// Every storm found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for st in &r.retry_storms {
        out.push(String::new());
        out.push(format!(
            "RETRY STORM: {} refused {} {} time(s){}{}",
            st.agent,
            st.path,
            st.refusals,
            match (st.median_gap_secs, st.median_holder_remaining_secs) {
                (Some(g), Some(rem)) => format!(
                    ", retrying every ~{} against a median {} of holder lease left",
                    secs(g),
                    secs(rem)
                ),
                (Some(g), None) => format!(", retrying every ~{}", secs(g)),
                _ => String::new(),
            },
            if st.ever_claimed {
                String::new()
            } else {
                " — and NEVER got the path".to_string()
            }
        ));
    }
    if !r.retry_storms.is_empty() {
        out.push(String::new());
        out.push(
            "Nothing here broke a rule: a refused agent is entitled to ask again. It is work \n\
             the fleet spent for nothing. The refusal message states how long the holder has \n\
             left; an agent that waits on that order of magnitude — or better, subscribes with \n\
             `pact watch add <path>` and picks up other ready work — spends none of it. One \n\
             fleet retried every 15 seconds, 33 times, against a median 355 seconds of \n\
             remaining hold whose note said it would renew."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    /// pact-1gv.3, the crucible shape reproduced: agent-02 refused src/eval.rs 33
    /// times at 15-second intervals against a median 355s of holder lease left.
    #[test]
    fn retry_storm_names_the_agent_that_hammered_a_held_lease() {
        let mut lines: Vec<String> = vec![ev(
            "2026-08-11T08:00:00Z",
            "agent-06",
            "acquired",
            "src/eval.rs",
        )];
        // 33 refusals, 15 seconds apart, holder always with minutes to spare.
        for i in 0..33 {
            lines.push(ev_refused(
                &format!("2026-08-11T08:{:02}:{:02}Z", (i * 15) / 60, (i * 15) % 60),
                "agent-02",
                "src/eval.rs",
                "agent-06",
                355,
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);

        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.retry_storms);
        let st = &r.retry_storms[0];
        assert_eq!(
            (st.agent.as_str(), st.path.as_str()),
            ("agent-02", "src/eval.rs")
        );
        assert_eq!(st.refusals, 33);
        assert_eq!(st.median_gap_secs, Some(15));
        assert_eq!(st.median_holder_remaining_secs, Some(355));
        assert!(!st.ever_claimed, "it never got the path: {st:?}");
        assert_eq!(r.refusals_without_remaining, 0);

        let text = render_check(&r);
        assert!(
            text.contains("RETRY STORM: agent-02 refused src/eval.rs 33 time(s)"),
            "{text}"
        );
        assert!(text.contains("NEVER got the path"), "{text}");
        // The verdict must say the fleet wasted work, not that pact was violated —
        // asking again is allowed.
        assert!(text.contains("Nothing here broke a rule"), "{text}");
    }

    /// Contention that RESOLVED must not be reported. An agent refused once or twice
    /// and then granted the lease is the protocol working, and flagging it is how a
    /// check trains people to ignore it.
    #[test]
    fn retry_storm_ignores_a_refusal_that_resolved_at_a_sane_interval() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "p.rs"),
            // Two polite retries: each waits most of the advertised remaining.
            &ev_refused("2026-08-11T08:00:10Z", "waiter", "p.rs", "holder", 120),
            &ev_refused("2026-08-11T08:02:10Z", "waiter", "p.rs", "holder", 10),
            &ev("2026-08-11T08:02:30Z", "holder", "released", "p.rs"),
            &ev("2026-08-11T08:02:31Z", "waiter", "acquired", "p.rs"),
        ]);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.retry_storms);
        assert!(
            render_check(&r).contains("no agent hammered a lease"),
            "{}",
            render_check(&r)
        );
    }

    /// The impatience half flags a SHORT storm the volume half would miss — three
    /// retries is not a lot, but three retries at 5s against 600s of remaining hold
    /// is the same mistake.
    #[test]
    fn retry_storm_flags_impatience_even_below_the_volume_threshold() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "p.rs"),
            &ev_refused("2026-08-11T08:00:05Z", "waiter", "p.rs", "holder", 600),
            &ev_refused("2026-08-11T08:00:10Z", "waiter", "p.rs", "holder", 595),
            &ev_refused("2026-08-11T08:00:15Z", "waiter", "p.rs", "holder", 590),
        ]);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.retry_storms);
        assert_eq!(r.retry_storms[0].median_gap_secs, Some(5));
    }

    /// `chaos-ghost` is scripts/chaos.sh planting a stale lease. Its failed acquire
    /// is a rail firing correctly, and counting it would credit the fleet with waste
    /// the fault injector caused deliberately.
    #[test]
    fn retry_storm_never_reports_the_fault_injector_as_a_bad_peer() {
        let mut lines = vec![ev(
            "2026-08-11T08:00:00Z",
            "agent-05",
            "acquired",
            "src/printer.rs",
        )];
        for i in 0..9 {
            lines.push(ev_refused(
                &format!("2026-08-11T08:00:{:02}Z", i * 5),
                "chaos-ghost",
                "src/printer.rs",
                "agent-05",
                600,
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.retry_storms);
    }

    /// A log written before `holder_remaining_secs` existed can still be judged on
    /// COUNT, and the report must say which half it could run. Silence here would let
    /// a clean result read as a stronger claim than it is.
    #[test]
    fn retry_storm_still_counts_on_a_log_that_predates_the_holder_fields() {
        let mut lines = vec![ev("2026-08-11T08:00:00Z", "holder", "acquired", "p.rs")];
        for i in 0..7 {
            lines.push(ev(
                &format!("2026-08-11T08:00:{:02}Z", i * 5),
                "waiter",
                "refused",
                "p.rs",
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(
            r.findings(),
            1,
            "volume alone must still flag: {:?}",
            r.retry_storms
        );
        assert_eq!(r.retry_storms[0].median_holder_remaining_secs, None);
        assert_eq!(r.refusals_without_remaining, 7);
        assert!(
            render_check(&r).contains("7 refusal(s) carry no holder-remaining"),
            "{}",
            render_check(&r)
        );
    }
}
