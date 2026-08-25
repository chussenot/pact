//! `--check silent-contention` (pact-7kv + pact-1gv.7): was a contended path
//! ever communicated about, by anybody, before its holder let go?

use serde::Serialize;

use crate::audit::model::{is_injector, parse_at, Hold};
use crate::audit::CheckReport;
use crate::events::Event;

/// A path somebody was refused, whose holder then released it without a word.
///
/// **The threshold objection that deferred pact-7kv dissolves once the boundary is
/// the HOLD.** The check was parked because "communicated in the same window" needed
/// an arbitrary cutoff, and it does not: a refusal happens while some agent holds the
/// path, `reconstruct` already computes that hold exactly, and the question becomes
/// "between the refusal and that holder's release, did anything communicate about
/// this path". No cutoff, nothing to tune — the same all-or-nothing move that
/// unblocked pact-ler.5.
///
/// Three things count as communication, and the third is the one this run forced:
///
/// 1. a `notified` delivery for the path — the holder's release told a subscriber;
/// 2. a message tagged with the path (the `message_id` on a `notified`, or a
///    `--to-owner-of` send, both of which land as events);
/// 3. **the refused agent already held a covering watch at the moment of the
///    refusal** — it had arranged to be told, which is using the channel that works.
///
/// (3) is counted but deliberately does NOT net out of the contention numbers.
/// 24 of the crucible run's 124 refusals came from an agent that was already
/// subscribed, and those same agents then polled 13, 6 and 3 more times. Crediting
/// the subscription while the agent busy-retried would score the run as communicating
/// well at exactly the moment it wasted the most work — so `refusals_with_a_channel`
/// is reported alongside, and `--check retry-storm` still says what the agent did
/// with the channel it had.
#[derive(Debug, Clone, Serialize)]
pub struct SilentContention {
    pub path: String,
    /// Who was refused, and when.
    pub refused_agent: String,
    pub refused_at: String,
    pub line: usize,
    /// Who was holding it, and when they let go. `None` when the log never shows the
    /// hold closing — an open hold has not had its chance to communicate yet, so
    /// those are skipped rather than reported.
    pub holder: String,
    pub holder_released_at: String,
    /// Was the refused agent subscribed to the path at the time? Reported per
    /// finding, because "nobody said anything AND they had no channel either" is a
    /// different situation from "nobody said anything but they were subscribed".
    pub refused_agent_had_a_channel: bool,
}

/// Refusals whose holder released without anything communicating about the path.
pub(in crate::audit) fn silent_contentions(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    holds: &[Hold],
    report: &mut CheckReport,
) {
    // Point-in-time, not the live registry: a subscription retired afterwards must
    // not rewrite whether the agent had a channel at the refusal (pact-1gv.7).
    let (watch_records, _) = crate::watch::records(repo_root).unwrap_or_default();

    for (line, e) in events.iter().filter(|(_, e)| e.kind == "refused") {
        let Some(path) = e.path.as_deref() else {
            continue;
        };
        if is_injector(&e.agent) {
            continue;
        }
        report.refusals_seen += 1;
        let had_channel = crate::watch::was_subscribed_at(&watch_records, &e.agent, path, &e.at);
        if had_channel {
            report.refusals_with_a_channel += 1;
        }
        let Some(refused_at) = parse_at(&e.at) else {
            continue;
        };
        // The hold this refusal collided with: same path, spanning the refusal, held
        // by somebody else. That hold's close is the deadline to communicate by.
        let Some(hold) = holds.iter().find(|h| {
            h.path == path
                && h.agent != e.agent
                && parse_at(&h.opened_at).is_some_and(|o| o <= refused_at)
                && h.closed_at
                    .as_deref()
                    .and_then(parse_at)
                    .is_some_and(|c| c >= refused_at)
        }) else {
            // No closed hold covering it. Either the log does not show the hold
            // closing — it has not had its chance yet — or the refusal collided with
            // something reconstruct could not pair. Neither is a finding.
            continue;
        };
        let Some(released_at) = hold.closed_at.as_deref().and_then(parse_at) else {
            continue;
        };
        // Did anything at all communicate about this path in that window?
        let communicated = events.iter().any(|(_, c)| {
            c.path.as_deref() == Some(path)
                && (c.kind == "notified" || c.message_id.is_some())
                && parse_at(&c.at).is_some_and(|t| t >= refused_at && t <= released_at)
        });
        if communicated || had_channel {
            continue;
        }
        report.silent_contentions.push(SilentContention {
            path: path.to_string(),
            refused_agent: e.agent.clone(),
            refused_at: e.at.clone(),
            line: *line,
            holder: hold.agent.clone(),
            holder_released_at: hold.closed_at.clone().unwrap_or_default(),
            refused_agent_had_a_channel: had_channel,
        });
    }
}

/// The scope line, stated even when clean.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) {
    // Nothing to scope when the check could not run: a "0 refusal(s) …" line
    // above a "could not run" line is two ways of saying the same absence, and the
    // first one looks like a measurement.
    if r.refusals_seen == 0 {
        return;
    }
    // Stated clean or not. A run where every refused agent was already
    // subscribed has NO findings here, and that fact is the interesting one —
    // silence would read as "nothing to see" rather than "the channel was used".
    out.push(format!(
        "  {} refusal(s) came from an agent already subscribed to the path (channel in \
         place); see --check retry-storm for what they did with it",
        r.refusals_with_a_channel
    ));
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean(r: &CheckReport) -> String {
    // Vacuous is not clean. This check reasons only about refusals; with none in
    // the log it examined nothing, and the clean sentence below would be claiming
    // a fleet communicated well about contention that never happened.
    if r.refusals_seen == 0 {
        return "could not run: no refusals in this log, so no contention to be \
                silent about. This check reads a fleet's behaviour when a path was \
                actually wanted by two agents at once."
            .to_string();
    }
    "every contended path was communicated about before its holder let go — by a \
     watch delivery, a message, or the refused agent's own subscription"
        .to_string()
}

/// Every silent contention found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for sc in &r.silent_contentions {
        out.push(String::new());
        out.push(format!(
            "SILENT CONTENTION on {}: {} was refused at {} (line {}), and {} released it at {} \
             without a message or a watch delivery about it",
            sc.path, sc.refused_agent, sc.refused_at, sc.line, sc.holder, sc.holder_released_at
        ));
    }
    if !r.silent_contentions.is_empty() {
        out.push(String::new());
        out.push(
            "Somebody wanted a path, was told no, and learned nothing when it came free. The \n\
             cheapest fix is not a message: `pact watch add <path>` makes the next release tell \n\
             them automatically, and delivery rides `lease release`, which agents perform \n\
             reliably. Four fleet runs produced 0 voluntary agent-to-agent messages and 64 watch \n\
             deliveries — see docs/watch.md."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    /// pact-7kv: somebody wanted a path, was told no, and learned nothing when it
    /// came free.
    ///
    /// The threshold objection that deferred this bead dissolves once the boundary is
    /// the HOLD: no cutoff to tune, because the holder's own release is the deadline.
    #[test]
    fn silent_contention_flags_a_holder_that_released_without_a_word() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/token.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/token.rs",
                "holder",
                540,
            ),
            &ev("2026-08-11T08:05:00Z", "holder", "released", "src/token.rs"),
        ]);
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.silent_contentions);
        let sc = &r.silent_contentions[0];
        assert_eq!(sc.path, "src/token.rs");
        assert_eq!(sc.refused_agent, "waiter");
        assert_eq!(sc.holder, "holder");
        assert!(!sc.refused_agent_had_a_channel);

        let text = render_check(&r);
        assert!(text.contains("SILENT CONTENTION on src/token.rs"), "{text}");
        // The remedy must be the channel that works, not a plea to send messages.
        assert!(text.contains("pact watch add"), "{text}");
    }

    /// A watch delivery about the path, inside the hold, IS communication — that is
    /// spec item 4 of pact-8qu, which asked that notified deliveries count.
    #[test]
    fn a_watch_delivery_inside_the_hold_counts_as_communication() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/token.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/token.rs",
                "holder",
                540,
            ),
            &ev("2026-08-11T08:04:00Z", "holder", "notified", "src/token.rs"),
            &ev("2026-08-11T08:05:00Z", "holder", "released", "src/token.rs"),
        ]);
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.silent_contentions);
    }

    /// pact-1gv.7: the refused agent's OWN subscription counts as using the channel —
    /// and is counted separately rather than netted out, because the same agents then
    /// polled anyway. Crediting the subscription while it busy-retries would score the
    /// run as communicating well at the moment it wasted the most work.
    #[test]
    fn a_refused_agents_own_subscription_counts_and_is_reported_separately() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/ast.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/ast.rs",
                "holder",
                540,
            ),
            &ev("2026-08-11T08:05:00Z", "holder", "released", "src/ast.rs"),
        ]);
        with_watches(
            &tmp,
            &[&watch_rec(
                "2026-08-11T07:59:00Z",
                "waiter",
                "watch",
                "src/ast.rs",
            )],
        );
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "the channel was in place: {:?}",
            r.silent_contentions
        );
        assert_eq!(r.refusals_with_a_channel, 1);
        // Stated even when clean — a run where every refused agent was subscribed has
        // no findings, and THAT is the interesting fact.
        assert!(
            render_check(&r).contains("1 refusal(s) came from an agent already subscribed"),
            "{}",
            render_check(&r)
        );
    }

    /// Point-in-time, not the live registry. A subscription added AFTER the refusal
    /// must not retroactively excuse it, and one retired before the refusal must not
    /// count either — otherwise a later `watch rm` rewrites history.
    #[test]
    fn a_subscription_only_counts_if_it_was_in_force_at_the_refusal() {
        let base = [
            ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/ast.rs"),
            ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/ast.rs",
                "holder",
                540,
            ),
            ev("2026-08-11T08:05:00Z", "holder", "released", "src/ast.rs"),
        ];
        let refs: Vec<&str> = base.iter().map(String::as_str).collect();

        // Subscribed only AFTER the refusal: does not count.
        let late = with_log(&refs);
        with_watches(
            &late,
            &[&watch_rec(
                "2026-08-11T08:02:00Z",
                "waiter",
                "watch",
                "src/ast.rs",
            )],
        );
        let r = run_check(late.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(
            r.refusals_with_a_channel, 0,
            "a later subscription is not a channel then"
        );
        assert_eq!(r.findings(), 1);

        // Subscribed, then unsubscribed BEFORE the refusal: does not count.
        let gone = with_log(&refs);
        with_watches(
            &gone,
            &[
                &watch_rec("2026-08-11T07:50:00Z", "waiter", "watch", "src/ast.rs"),
                &watch_rec("2026-08-11T07:55:00Z", "waiter", "unwatch", "src/ast.rs"),
            ],
        );
        let r = run_check(gone.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(
            r.refusals_with_a_channel, 0,
            "a retired subscription is not a channel"
        );
        assert_eq!(r.findings(), 1);

        // And a re-add after the unwatch, still before the refusal, counts again.
        let back = with_log(&refs);
        with_watches(
            &back,
            &[
                &watch_rec("2026-08-11T07:50:00Z", "waiter", "watch", "src/ast.rs"),
                &watch_rec("2026-08-11T07:55:00Z", "waiter", "unwatch", "src/ast.rs"),
                &watch_rec("2026-08-11T07:58:00Z", "waiter", "watch", "src/ast.rs"),
            ],
        );
        let r = run_check(back.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.refusals_with_a_channel, 1);
        assert_eq!(r.findings(), 0);
    }

    /// An OPEN hold has not had its chance to communicate yet. Reporting it would
    /// flag a fleet mid-run for something it may be about to do.
    #[test]
    fn silent_contention_ignores_a_hold_the_log_never_shows_closing() {
        let tmp = with_log(&[
            &ev("2026-08-11T08:00:00Z", "holder", "acquired", "src/ast.rs"),
            &ev_refused(
                "2026-08-11T08:01:00Z",
                "waiter",
                "src/ast.rs",
                "holder",
                540,
            ),
        ]);
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.silent_contentions);
    }

    /// No refusals means no contention to have been silent about, and the check
    /// must say so rather than crediting the fleet with communicating well
    /// (pact-k1n.4).
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
        let r = run_check(tmp.path(), Check::SilentContention, None, false).unwrap();
        assert_eq!(r.findings(), 0);
        assert_eq!(r.refusals_seen, 0);

        let text = render_check(&r);
        assert!(text.contains("could not run"), "{text}");
        assert!(
            !text.contains("every contended path was communicated about"),
            "a run with no contention has not communicated well about it: {text}"
        );
        assert!(!text.contains("0 refusal(s)"), "{text}");
    }
}
