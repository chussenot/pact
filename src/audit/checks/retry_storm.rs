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
    /// When the storm started and when it ended, RFC3339, and the lines its
    /// refusals sit on in `.pact/events.jsonl` (pact-x16.5).
    ///
    /// **A storm is a span, and this was the only audit finding with no time on
    /// it at all** — which made it the only one nothing downstream could join to
    /// anything. That is a sharp irony rather than a small gap: this is the one
    /// check that is ABOUT agent behaviour over time. `recount testify` classifies
    /// these as passed-through and exits 0, which is right behaviour on wrong
    /// data. Answering "why did tailrace's storm spin?" meant finding the refusal
    /// events by hand in the log and then the transcript by hand after that.
    ///
    /// Named `first_refusal_at`/`last_refusal_at` rather than `at`/`ended_at`
    /// because consumers find the moment inside a finding BY NAME, matching the
    /// `*_at` convention every other finding in this document already uses.
    ///
    /// `None` only when no refusal in the storm carried a parsable timestamp,
    /// which no pact has ever written — absent rather than defaulted, like every
    /// other optional field here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_refusal_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refusal_at: Option<String>,
    /// The 1-based lines of this storm's refusals in `.pact/events.jsonl`, so a
    /// reader can go straight to them instead of grepping for an (agent, path)
    /// pair across a log where the same pair may contend more than once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<usize>,
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
    // The line number rides along with the event: a storm's whole value to a
    // reader is being able to go and look at it, and an (agent, path) pair is not
    // a unique key over a log where the same pair may contend more than once.
    let mut by_pair: BTreeMap<(&str, &str), Vec<(usize, &Event)>> = BTreeMap::new();
    for (line, e) in events {
        if e.kind != "refused" || is_injector(&e.agent) {
            continue;
        }
        report.refusals_seen += 1;
        if let Some(path) = e.path.as_deref() {
            by_pair
                .entry((e.agent.as_str(), path))
                .or_default()
                .push((*line, e));
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
                let (a, b) = (parse_at(&w[0].1.at)?, parse_at(&w[1].1.at)?);
                Some((b - a).num_seconds())
            })
            .collect();
        gaps.sort_unstable();
        let mut remaining: Vec<i64> = rows
            .iter()
            .filter_map(|(_, e)| e.holder_remaining_secs)
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
            // Both ends, because a storm is a span. The rows arrive in log order,
            // which is append order, so first and last are the ends of it.
            first_refusal_at: rows.first().map(|(_, e)| e.at.clone()),
            last_refusal_at: rows.last().map(|(_, e)| e.at.clone()),
            lines: rows.iter().map(|(line, _)| *line).collect(),
        });
    }
    // Worst first: the loudest offender is what a reader wants at the top.
    report
        .retry_storms
        .sort_by_key(|r| std::cmp::Reverse(r.refusals));
}

/// The scope line, stated even when clean.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) {
    // Nothing to scope when the check could not run: a "0 refusal(s) …" line
    // above a "could not run" line is two ways of saying the same absence, and the
    // first one looks like a measurement.
    if r.refusals_seen == 0 {
        return;
    }
    // Scope before verdict, clean or not: the impatience half of this check
    // cannot speak to a refusal whose holder-remaining was never recorded.
    out.push(format!(
        "  {} refusal(s) carry no holder-remaining, so only their COUNT could be \
         judged (logs written before pact recorded it are entirely in this state)",
        r.refusals_without_remaining
    ));
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean(r: &CheckReport) -> String {
    // Vacuous is not clean. A storm is a pattern in refusals; with no refusals
    // there was nothing to hammer, and "no agent hammered a lease it was refused"
    // reads as a fleet that behaved well under contention rather than as a fleet
    // that never met any.
    if r.refusals_seen == 0 {
        return "could not run: no refusals in this log, so no retry behaviour to \
                measure. This check reads what an agent does after it is told no."
            .to_string();
    }
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
    /// A log with no refusals in it must say the check COULD NOT RUN, not that no
    /// agent hammered anything (pact-k1n.4).
    ///
    /// The modmill proving-ground run read this line as a pass on a log with 0
    /// refusals end to end. It is not a pass — a fleet that never contended for a
    /// path has not demonstrated good behaviour under contention, it has just not
    /// been asked. `claim-lease-divergence` and `gate-order` already say "could not
    /// run" in their own vacuous cases; this makes the third one consistent.
    #[test]
    fn no_refusals_reads_as_could_not_run_rather_than_clean() {
        let tmp = with_log(&[
            &ev(
                "2026-08-11T08:00:00Z",
                "agent-06",
                "acquired",
                "src/eval.rs",
            ),
            &ev(
                "2026-08-11T08:10:00Z",
                "agent-06",
                "released",
                "src/eval.rs",
            ),
        ]);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        assert_eq!(r.refusals_seen, 0, "the fixture has no refusals");

        let text = render_check(&r);
        assert!(text.contains("could not run"), "{text}");
        assert!(
            !text.contains("no agent hammered"),
            "a vacuous scan must not claim the fleet behaved well: {text}"
        );
        // And no "0 refusal(s) carry no holder-remaining" line either — a zero
        // rendered as a measurement is the same lie in smaller type.
        assert!(!text.contains("0 refusal(s)"), "{text}");
    }

    /// The other half: with refusals present and none stormy, the clean sentence is
    /// still the right answer. The fix must not turn every quiet run into "could
    /// not run".
    #[test]
    fn refusals_present_and_well_spaced_still_read_as_clean() {
        let tmp = with_log(&[
            &ev(
                "2026-08-11T08:00:00Z",
                "agent-06",
                "acquired",
                "src/eval.rs",
            ),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "agent-02",
                "src/eval.rs",
                "agent-06",
                355,
            ),
        ]);
        let r = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        assert_eq!(r.refusals_seen, 1);
        let text = render_check(&r);
        assert!(text.contains("no agent hammered"), "{text}");
        assert!(!text.contains("could not run"), "{text}");
    }
}

#[cfg(test)]
mod span_tests {
    use crate::audit::fixtures::*;
    use crate::audit::{run_check, Check};

    /// pact-x16.5: a storm is a span, and it must be findable.
    ///
    /// This was the only audit finding carrying no time at all — which made it the
    /// only one nothing downstream could join to anything, on the one check that
    /// is ABOUT behaviour over time. `recount testify` classified these as
    /// passed-through: right behaviour on wrong data. Answering "why did this
    /// storm spin?" meant locating the refusals by hand in the log and the
    /// transcript by hand after that.
    ///
    /// Asserted on the SERIALIZED shape, because the consumer is `--json` and a
    /// by-name lookup for `*_at` is how recount finds the moment inside a finding.
    #[test]
    fn a_storm_carries_both_ends_and_the_lines_to_look_at() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..6 {
            lines.push(ev_refused(
                &format!("2026-08-15T21:{:02}:00Z", 5 + i),
                "tailrace",
                "m.rs",
                "spillway",
                2600,
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = with_log(&refs);

        let report = run_check(tmp.path(), Check::RetryStorm, None, false).unwrap();
        let storm = report
            .retry_storms
            .first()
            .expect("six refusals is a storm");

        assert_eq!(
            storm.first_refusal_at.as_deref(),
            Some("2026-08-15T21:05:00Z")
        );
        assert_eq!(
            storm.last_refusal_at.as_deref(),
            Some("2026-08-15T21:10:00Z")
        );
        assert_eq!(
            storm.lines.len(),
            6,
            "every refusal's line, because an (agent, path) pair is not a unique \
             key over a log where the same pair may contend twice"
        );

        let json = serde_json::to_value(storm).unwrap();
        assert!(
            json.as_object().unwrap().keys().any(|k| k.ends_with("_at")),
            "recount finds the moment inside a finding BY NAME: {json}"
        );
    }
}
