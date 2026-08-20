//! `--check topology` (pact-ler.2/.5): did this run use the topology it was
//! supposed to?
//!
//! The expectation itself — [`Expect`] and its `NAMES` — stays in the parent
//! module with the registry, because clap renders `--expect` from it and the
//! round-trip tests that guard the two lists have to sit together.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::audit::{CheckReport, Expect};
use crate::events::Event;

/// One invocation point that contradicted `--expect`.
#[derive(Debug, Clone, Serialize)]
pub struct TopologyMismatch {
    pub invoked_from: String,
    pub events: usize,
}

// No timestamp here, and that is the one deliberate exception to the rule
// pact-x16.5 established — every finding that names an AGENT carries the moment
// the named behaviour happened, so it can be joined to a transcript. This finding
// names a LOCATION and counts a whole run's events at it; its span is the run's
// span, and a first/last pair would say nothing a reader could act on. Every
// other finding type was checked: Hold, DoubleWin, SilentContention,
// MergeDivergence, ClaimDivergence, UncommittedHold and ConcurrentWrite all
// already carried theirs, and retry-storm was the only real gap.

/// Count every event by where pact was invoked from, and report the points that
/// contradict the expectation.
pub(in crate::audit) fn detect(
    context: &BTreeMap<String, String>,
    events: &[(usize, Event)],
    expect: &Expect,
    report: &mut CheckReport,
) {
    let resolved;
    let expect = match expect {
        Expect::FromContext => {
            resolved = match context.get("topology-expectation") {
                Some(declared) => Expect::parse(declared, &[]).unwrap_or(Expect::Any),
                None => Expect::Any,
            };
            &resolved
        }
        other => other,
    };
    report.expected_topology = Some(expect.label());
    let allowed = expect.allowed_from_main();
    let mut by_point: BTreeMap<String, usize> = BTreeMap::new();
    for (_, e) in events {
        match e.invoked_from.as_deref() {
            // A declared main-checkout identity is excused, and only from `main`
            // — naming an agent does not license it to act from anywhere else
            // (pact-83r.3 / finding 5b).
            Some("main") if allowed.contains(&e.agent) => {
                report.topology_allowed_from_main += 1;
            }
            Some(from) => *by_point.entry(from.to_string()).or_insert(0) += 1,
            None => report.topology_unstamped += 1,
        }
    }
    report.topology_mismatches = by_point
        .into_iter()
        .filter(|(from, _)| !expect.satisfied_by(from))
        .map(|(invoked_from, events)| TopologyMismatch {
            invoked_from,
            events,
        })
        .collect();
}

/// The scope lines, stated before any verdict.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) {
    // Stated before any verdict, and stated even when clean: a reader has
    // to know how much of the log this check could speak to at all before
    // believing what it says about it.
    out.push(format!(
        "  expected {}; {} event(s) carry no invocation context (written before pact \
         recorded it)",
        r.expected_topology.unwrap_or("any"),
        r.topology_unstamped
    ));
    // In the header for the same reason the line above is: an exception has to be
    // visible on a PASS, or a reader cannot tell "the fleet ran where it was asked"
    // from "the exception list was wide enough to cover where it did not"
    // (pact-83r.3 / finding 5b).
    if r.topology_allowed_from_main > 0 {
        out.push(format!(
            "  {} event(s) excused from the main checkout by --allow-main",
            r.topology_allowed_from_main
        ));
    }
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean(r: &CheckReport) -> String {
    format!(
        "every context-stamped event matches --expect {}",
        r.expected_topology.unwrap_or("any")
    )
}

/// Every mismatch found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for m in &r.topology_mismatches {
        out.push(String::new());
        out.push(format!(
            "TOPOLOGY MISMATCH: {} event(s) invoked from {:?}, which --expect {} does not allow",
            m.events,
            m.invoked_from,
            r.expected_topology.unwrap_or("any")
        ));
    }
    if !r.topology_mismatches.is_empty() {
        out.push(String::new());
        out.push(
            "The run did not use the topology it was asked to. Under an orchestrated-wave fleet \n\
             this usually means agents edited in their worktrees but ran pact from the main \n\
             checkout, so the lease/edit binding rests on convention — see \n\
             docs/fleet-patterns.md."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check, Expect};

    #[test]
    fn topology_expectations_are_all_or_nothing() {
        let tmp = with_log(&[
            &ev_from("2026-08-01T10:00:00Z", "a", "acquired", "a.rs", "main"),
            &ev_from("2026-08-01T10:01:00Z", "b", "acquired", "b.rs", "wt-b"),
        ]);
        // A mixed run satisfies neither expectation — deliberately, because
        // any "mostly" rule needs a cutoff nobody derived from data.
        let worktrees = run_check(
            tmp.path(),
            Check::Topology(Expect::Worktrees {
                allow_main: Vec::new(),
            }),
            None,
            false,
        )
        .unwrap();
        assert_eq!(worktrees.findings(), 1);
        assert_eq!(worktrees.topology_mismatches[0].invoked_from, "main");

        let main = run_check(tmp.path(), Check::Topology(Expect::Main), None, false).unwrap();
        assert_eq!(main.findings(), 1);
        assert_eq!(main.topology_mismatches[0].invoked_from, "wt-b");

        // `any` is the "just show me" mode and can never fail.
        let any = run_check(tmp.path(), Check::Topology(Expect::Any), None, false).unwrap();
        assert_eq!(any.findings(), 0);
        assert!(
            render_check(&any).contains("expected any"),
            "{}",
            render_check(&any)
        );
    }

    /// `outside` is not a worktree: it means pact ran somewhere that is not
    /// under this repository at all, which is precisely the value that says
    /// the lease/edit binding cannot be assumed.
    #[test]
    fn outside_never_satisfies_expect_worktrees() {
        let tmp = with_log(&[&ev_from(
            "2026-08-01T10:00:00Z",
            "a",
            "acquired",
            "a.rs",
            "outside",
        )]);
        let r = run_check(
            tmp.path(),
            Check::Topology(Expect::Worktrees {
                allow_main: Vec::new(),
            }),
            None,
            false,
        )
        .unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.topology_mismatches);
    }

    /// The convention every existing repository depends on: a log written
    /// before pact recorded invocation context reports "no data" and exits
    /// clean, whatever was expected. Flagging it would have failed every repo
    /// on the day this shipped.
    #[test]
    fn a_pre_stamping_log_never_fails_a_topology_expectation() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "a", "acquired", "a.rs"),
            &ev("2026-08-01T10:01:00Z", "a", "released", "a.rs"),
        ]);
        for expect in [
            Expect::Worktrees {
                allow_main: Vec::new(),
            },
            Expect::Main,
            Expect::Any,
        ] {
            let label = format!("{expect:?}");
            let r = run_check(tmp.path(), Check::Topology(expect), None, false).unwrap();
            assert_eq!(
                r.findings(),
                0,
                "{label} must not fail on a pre-stamping log"
            );
            assert_eq!(r.topology_unstamped, 2);
        }
    }

    /// FINDING 5b: `--expect worktrees` could not pass for any real fleet, because in the
    /// topology pact documents somebody must sit in the main checkout — it is where the
    /// coordination logs are committed from. Run 5 failed with 19 offending events, not one
    /// of which was an agent working in the wrong place.
    #[test]
    fn allow_main_excuses_a_declared_orchestrator_and_nobody_else() {
        let tmp = with_log(&[
            &ev_from(
                "2026-08-01T10:00:00Z",
                "agent-a",
                "acquired",
                "a.rs",
                "wt-a",
            ),
            &ev_from(
                "2026-08-01T10:01:00Z",
                "orchestrator",
                "acquired",
                "b.rs",
                "main",
            ),
        ]);

        // Without the exception, the orchestrator's own protocol-following lease fails it.
        let bare = Check::parse("topology", Some("worktrees"), &[]).unwrap();
        let r = run_check(tmp.path(), bare, None, false).unwrap();
        assert_eq!(r.findings(), 1, "{:?}", r.topology_mismatches);

        // With it, the run passes and the exception is COUNTED — an exception nobody can
        // see the size of stops being read as one.
        let allowed =
            Check::parse("topology", Some("worktrees"), &["orchestrator".to_string()]).unwrap();
        let r = run_check(tmp.path(), allowed, None, false).unwrap();
        assert_eq!(r.findings(), 0, "{:?}", r.topology_mismatches);
        assert_eq!(r.topology_allowed_from_main, 1);
        assert!(render_check(&r).contains("excused from the main checkout"));
    }

    /// Naming an identity excuses it from `main` ONLY. It is not a licence to act from
    /// anywhere, and it does not excuse anyone else.
    #[test]
    fn allow_main_does_not_excuse_another_agent_or_another_location() {
        let tmp = with_log(&[
            &ev_from("2026-08-01T10:00:00Z", "stray", "acquired", "a.rs", "main"),
            &ev_from(
                "2026-08-01T10:01:00Z",
                "orchestrator",
                "acquired",
                "b.rs",
                "outside",
            ),
        ]);
        let check =
            Check::parse("topology", Some("worktrees"), &["orchestrator".to_string()]).unwrap();
        let r = run_check(tmp.path(), check, None, false).unwrap();
        assert_eq!(
            r.findings(),
            2,
            "an unlisted agent from main and a listed one from outside both fail: {:?}",
            r.topology_mismatches
        );
        assert_eq!(r.topology_allowed_from_main, 0);
    }
}
