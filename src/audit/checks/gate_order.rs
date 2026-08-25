//! `--check gate-order` — did the fleet honour the ordering it declared?
//!
//! # The only check whose subject is a declaration
//!
//! Every other check here measures behaviour against a rule pact holds: a lease
//! held past its own TTL, two agents on one path, a chain hash that does not
//! verify. This one measures a fleet's **own stated ordering** against what it
//! then did, and pact has no opinion about the ordering at all. A gate is
//! something the plan said; this reports whether the plan and the ledger agree.
//!
//! # Why it is a report and not a refusal
//!
//! Runtime pact never declines an acquire because a gate has not closed, and that
//! is the load-bearing decision rather than an unimplemented feature. Enforcement
//! would make pact a scheduler — something that decides what may run — and pact's
//! whole shape is the opposite: advisory leases, honoured because they are
//! visible, not because they are enforced.
//!
//! The field record is the argument. Agents route around enforcement and respect
//! what is measured: they skipped renewal (one in 153 events) and messaging (four
//! messages between 28 agents) because neither was measured, and they honoured
//! leases they could trivially have stolen because every steal is recorded with
//! their name on it. A gate that refuses an acquire gets a `--steal`, or a lease
//! taken on a path the gate does not cover, or the manifest edited. A gate that is
//! *audited* gets honoured or gets explained, and both are outcomes worth having.
//!
//! # A violation is a finding about the PLAN as much as the agent
//!
//! This is why the output names the chain rather than the culprit. An agent that
//! started wave 2 before the wave-1 test gate closed may have been wrong — or the
//! gate may have been declared over work that never depended on it, in which case
//! the plan asked for a wait that bought nothing and the agent's judgement was
//! better than the manifest's. The check cannot tell those apart and does not try;
//! it reports who, for which bead, how long before which gate, and leaves the
//! reading to somebody who knows what the gate was for.

use serde::Serialize;

use crate::audit::CheckReport;
use crate::events::Event;

/// One bead that started before a gate it was declared to wait for.
#[derive(Debug, Clone, Serialize)]
pub struct GateViolation {
    /// The bead that started early, and the wave the plan put it in.
    pub bead: String,
    pub wave: i64,
    /// The gate it should have waited for, and its wave.
    pub gate: String,
    pub gate_wave: i64,
    /// Who started it, and how — a lease acquire, or a bd claim.
    pub agent: String,
    pub via: String,
    /// When the work started, RFC3339.
    pub started_at: String,
    /// When the gate closed. `None` when it never did — the louder case, since a
    /// gate that is still open means the whole wave behind it ran unguarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_closed_at: Option<String>,
    /// Seconds between the start and the close. `None` where the gate never
    /// closed, for the same reason: there is no interval to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub early_by_secs: Option<i64>,
    /// The line in `.pact/events.jsonl` this was seen at, where the evidence is a
    /// lease. `None` for a bd claim, which lives in the interactions export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

/// Every start that preceded a gate it was declared to wait for.
pub(in crate::audit) fn detect(
    repo_root: &std::path::Path,
    events: &[(usize, Event)],
    report: &mut CheckReport,
) {
    let Some(snapshot) = crate::plan::snapshot(repo_root) else {
        report.gates_unavailable = Some(format!(
            "no dependency graph at {} — run `pact plan lint <manifest>` first",
            crate::plan::SNAPSHOT_PATH
        ));
        return;
    };
    if snapshot.gates.is_empty() {
        // The pre-gate-era case, and it must not read as a pass. A plan that
        // declared no gates has not been checked against anything, which is a
        // different statement from "it obeyed every gate it declared" — the same
        // distinction `--check topology` draws for a log written before invocation
        // context existed.
        report.gates_unavailable =
            Some("no gates declared in this plan — nothing to order against".to_string());
        return;
    }

    let closed = crate::beads::closed_at(repo_root);
    let claims = crate::beads::assignee_changes(repo_root);

    // Each gate, its wave, and when it closed. A gate the export has never seen
    // closed is kept with `None`, deliberately: that is the loudest case, because
    // everything behind it ran with nothing verified.
    let gates: Vec<(&str, i64, Option<&str>)> = snapshot
        .gates
        .iter()
        .filter_map(|g| {
            let wave = *snapshot.waves.get(g)?;
            Some((g.as_str(), wave, closed.get(g).map(String::as_str)))
        })
        .collect();

    // Every recorded START of a bead, from both places one can be seen. A lease
    // acquired FOR a bead is the stronger evidence — it is pact's own row, and
    // `Event::bead` is a structured field rather than a substring of a note — but
    // a claim is a start too, and a fleet that claims before it leases would
    // otherwise be invisible here.
    let mut starts: Vec<(String, String, String, &str, Option<usize>)> = Vec::new();
    for (line, e) in events {
        if !crate::audit::model::opens(&e.kind) {
            continue;
        }
        if let Some(bead) = &e.bead {
            starts.push((
                bead.clone(),
                e.agent.clone(),
                e.at.clone(),
                "lease",
                Some(*line),
            ));
        }
    }
    for (at, issue, who) in &claims {
        if who.trim().is_empty() {
            continue;
        }
        starts.push((issue.clone(), who.clone(), at.clone(), "claim", None));
    }

    for (bead, agent, started_at, via, line) in starts {
        let Some(&wave) = snapshot.waves.get(&bead) else {
            continue;
        };
        let Some(start) = parse(&started_at) else {
            continue;
        };
        for (gate, gate_wave, gate_closed) in &gates {
            // Only gates in a STRICTLY earlier wave, and never the gate itself: a
            // gate starting in its own wave is the normal case, and a gate is not
            // required to wait for gates beside it.
            if *gate_wave >= wave || *gate == bead {
                continue;
            }
            let early_by = match gate_closed {
                Some(at) => match parse(at) {
                    // Closed before this started: the plan was honoured, which is
                    // the common case and produces nothing.
                    Some(closed_at) if closed_at <= start => continue,
                    Some(closed_at) => Some((closed_at - start).num_seconds()),
                    None => None,
                },
                None => None,
            };
            report.gate_violations.push(GateViolation {
                bead: bead.clone(),
                wave,
                gate: (*gate).to_string(),
                gate_wave: *gate_wave,
                agent: agent.clone(),
                via: via.to_string(),
                started_at: started_at.clone(),
                gate_closed_at: gate_closed.map(str::to_string),
                early_by_secs: early_by,
                line,
            });
        }
    }

    // Worst first: how far ahead of the gate somebody was is the ordering a reader
    // wants, and a gate that never closed sorts above any interval because there
    // is no amount of earliness worse than "the gate is still open".
    report
        .gate_violations
        .sort_by_key(|v| (v.early_by_secs.is_some(), -v.early_by_secs.unwrap_or(0)));
}

fn parse(at: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// The scope line, stated even when clean.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) -> bool {
    if let Some(reason) = &r.gates_unavailable {
        // A reason, and a stop. Everything below would be a clean bill of health
        // this check has not earned — the same rule `claim-lease-divergence`
        // follows when the sidecar is missing.
        out.push(format!("  {reason} — gate-order could not run"));
        return true;
    }
    false
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "every bead started after the gates its wave was declared to wait for".to_string()
}

/// Every violation, and how to read one.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for v in &r.gate_violations {
        let when = match (&v.gate_closed_at, v.early_by_secs) {
            (Some(_), Some(secs)) => format!("{} before it closed", crate::audit::secs(secs)),
            _ => "and that gate has never closed".to_string(),
        };
        out.push(format!(
            "  {} (wave {}) started by {} via {} at {} — gate {} (wave {}) {}{}",
            v.bead,
            v.wave,
            v.agent,
            v.via,
            v.started_at,
            v.gate,
            v.gate_wave,
            when,
            v.line.map(|l| format!(" (line {l})")).unwrap_or_default()
        ));
    }
    if !r.gate_violations.is_empty() {
        out.push(String::new());
        out.push(
            "A gate is something the PLAN declared, not a rule pact enforces — no acquire was \n\
             ever refused on these grounds, and none will be. So read these as a question about \n\
             the plan as much as about the agents: work that ran early and turned out fine means \n\
             the gate was declared over something that did not depend on it, and the manifest \n\
             asked for a wait that bought nothing. Work that ran early and broke means the gate \n\
             was right and nobody could see it. This check cannot tell those apart; somebody who \n\
             knows what the gate was for can."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, run_check_strict, Check};

    fn plan(tmp: &tempfile::TempDir, json: &str) {
        std::fs::create_dir_all(tmp.path().join(".pact")).unwrap();
        std::fs::write(tmp.path().join(".pact/plan.json"), json).unwrap();
    }

    fn closed(tmp: &tempfile::TempDir, rows: &[(&str, &str)]) {
        std::fs::create_dir_all(tmp.path().join(".beads")).unwrap();
        let body: String = rows
            .iter()
            .map(|(id, at)| {
                format!(
                    r#"{{"id":"1","issue_id":"{id}","kind":"field_change","actor":"t","created_at":"{at}","extra":{{"field":"status","new_value":"closed"}}}}
"#
                )
            })
            .collect();
        std::fs::write(tmp.path().join(".beads/interactions.jsonl"), body).unwrap();
    }

    const GRAPH: &str = r#"{"at":"2026-08-25T00:00:00Z","edges":{"g-tst":[],"m-imp":[]},
        "waves":{"g-tst":0,"m-imp":1},"gates":["g-tst"]}"#;

    fn ev_bead(at: &str, agent: &str, path: &str, bead: &str) -> String {
        format!(
            r#"{{"at":"{at}","agent":"{agent}","kind":"acquired","path":"{path}","bead":"{bead}"}}"#
        )
    }

    /// The clean case: the gate closed before the next wave started.
    #[test]
    fn work_that_waited_for_its_gate_is_not_a_finding() {
        let tmp = with_log(&[&ev_bead("2026-08-25T11:00:00Z", "patient", "b.rs", "m-imp")]);
        plan(&tmp, GRAPH);
        closed(&tmp, &[("g-tst", "2026-08-25T10:00:00Z")]);

        let r = run_check(tmp.path(), Check::GateOrder, None, false).unwrap();
        assert!(r.gate_violations.is_empty(), "{:?}", r.gate_violations);
        assert!(render_check(&r).contains("every bead started after"));
    }

    /// One start an hour before the gate closed, reported with the whole chain.
    ///
    /// The chain is the point. A bare count would say a rule was broken; naming
    /// who, via what, for which bead and how far ahead is what lets somebody
    /// decide whether the AGENT was wrong or the PLAN was — which is the reading
    /// this check exists to leave room for.
    #[test]
    fn a_start_before_the_gate_closed_is_reported_with_its_forensics() {
        let tmp = with_log(&[&ev_bead("2026-08-25T09:00:00Z", "eager", "b.rs", "m-imp")]);
        plan(&tmp, GRAPH);
        closed(&tmp, &[("g-tst", "2026-08-25T10:00:00Z")]);

        let r = run_check(tmp.path(), Check::GateOrder, None, false).unwrap();
        assert_eq!(r.gate_violations.len(), 1, "{:?}", r.gate_violations);
        let v = &r.gate_violations[0];
        assert_eq!(v.bead, "m-imp");
        assert_eq!(v.gate, "g-tst");
        assert_eq!(v.agent, "eager");
        assert_eq!(v.via, "lease");
        assert_eq!(v.early_by_secs, Some(3600), "an hour early");

        let out = render_check(&r);
        assert!(out.contains("eager") && out.contains("g-tst"), "{out}");
        // And the reading it must not foreclose.
        assert!(
            out.contains("not a rule pact enforces"),
            "the output has to say pact refused nothing: {out}"
        );
    }

    /// A gate that never closed is the loud case: everything behind it ran
    /// unguarded, and there is no interval to report because there is no close.
    #[test]
    fn a_gate_that_never_closed_reports_no_interval() {
        let tmp = with_log(&[&ev_bead("2026-08-25T09:00:00Z", "eager", "b.rs", "m-imp")]);
        plan(&tmp, GRAPH);
        closed(&tmp, &[]);

        let r = run_check(tmp.path(), Check::GateOrder, None, false).unwrap();
        assert_eq!(r.gate_violations.len(), 1);
        assert_eq!(r.gate_violations[0].gate_closed_at, None);
        assert_eq!(r.gate_violations[0].early_by_secs, None);
        assert!(render_check(&r).contains("never closed"));
    }

    /// `--strict` moves the EXIT CODE and nothing else.
    ///
    /// Both halves matter. A violation must not fail a build by default, because a
    /// gate is a declaration a fleet made about itself rather than a rule pact
    /// holds — and failing on it would teach fleets to stop declaring gates, which
    /// costs the measurement to buy an enforcement pact is deliberately not doing.
    /// But it must be REPORTED either way, or the default is indistinguishable
    /// from not checking.
    #[test]
    fn strict_changes_the_exit_code_and_not_the_report() {
        let tmp = with_log(&[&ev_bead("2026-08-25T09:00:00Z", "eager", "b.rs", "m-imp")]);
        plan(&tmp, GRAPH);
        closed(&tmp, &[("g-tst", "2026-08-25T10:00:00Z")]);

        let lax = run_check(tmp.path(), Check::GateOrder, None, false).unwrap();
        let strict = run_check_strict(tmp.path(), Check::GateOrder, None, false, true).unwrap();

        assert_eq!(
            lax.findings(),
            0,
            "a smell must not fail a build by default"
        );
        assert_eq!(
            strict.findings(),
            1,
            "--strict is somebody's decision to make"
        );
        assert_eq!(
            lax.gate_violations.len(),
            strict.gate_violations.len(),
            "the report is the same either way"
        );
        assert!(
            render_check(&lax).contains("eager"),
            "reported when lax too"
        );
    }

    /// No plan, and a plan with no gates, both say what is missing rather than
    /// passing — the distinction `--check topology` draws for an unstamped log.
    #[test]
    fn a_missing_or_gateless_plan_says_so_instead_of_passing() {
        let bare = with_log(&[&ev_bead("2026-08-25T09:00:00Z", "a", "b.rs", "m-imp")]);
        let r = run_check(bare.path(), Check::GateOrder, None, false).unwrap();
        assert!(r.gates_unavailable.is_some());
        assert!(render_check(&r).contains("could not run"));
        assert!(!render_check(&r).contains("every bead started after"));

        let gateless = with_log(&[&ev_bead("2026-08-25T09:00:00Z", "a", "b.rs", "m-imp")]);
        plan(
            &gateless,
            r#"{"at":"2026-08-25T00:00:00Z","edges":{"m-imp":[]},"waves":{"m-imp":1},"gates":[]}"#,
        );
        let r = run_check(gateless.path(), Check::GateOrder, None, false).unwrap();
        assert!(r.gates_unavailable.as_deref().unwrap().contains("no gates"));
    }

    /// A snapshot written before gates existed carries no `waves` or `gates` key
    /// at all, and must parse rather than failing the check.
    #[test]
    fn a_pre_gate_snapshot_degrades_instead_of_erroring() {
        let tmp = with_log(&[&ev_bead("2026-08-25T09:00:00Z", "a", "b.rs", "m-imp")]);
        plan(
            &tmp,
            r#"{"at":"2026-08-25T00:00:00Z","edges":{"m-imp":[]}}"#,
        );

        let r = run_check(tmp.path(), Check::GateOrder, None, false).unwrap();
        assert!(
            r.gates_unavailable.as_deref().unwrap().contains("no gates"),
            "an old snapshot is a plan with no gates, not a broken one"
        );
    }
}
