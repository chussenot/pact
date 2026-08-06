//! Offline analysis of pact's own coordination history.
//!
//! ## Why this exists
//!
//! pact records every acquire, renew, release, steal and expiry in
//! `.pact/events.jsonl`, and until now nothing read it back except `pact log`,
//! which prints the tail. Questions a fleet actually raises — was one path
//! contended by six agents, did anyone hold a lease for an hour without
//! renewing, did two agents ever hold one path at once — needed a human with
//! `jq` and a hypothesis.
//!
//! The last of those is the one that matters, because it has a written trigger
//! condition attached. The guard-file backlog item (**pact-ehi**) says: implement
//! the guard file *if and only if* a double-win appears in a real events log.
//! That is a falsifiable claim with no detector, which makes it a claim nobody
//! can act on. [`Check::DoubleWin`] is the detector, and the two point at each
//! other — the bead names this command, this command's help names the bead.
//!
//! ## Scope, which is narrow on purpose
//!
//! Audit reads **`.pact/` and nothing else**. It never opens `.beads/`, a Dolt
//! directory, a SQLite file or a JSONL export, because "pact never touches the
//! Beads store directly, only the CLI" is an invariant the whole messaging design
//! rests on, and an analytics command is exactly where it would be convenient to
//! break it. Beads-side questions live in `scripts/beads-retro.sh`, which is
//! best-effort, jq-based, and says so in its header.
//!
//! No new dependencies: line-by-line `serde_json` over the log, tolerant of
//! unknown event kinds (a `kind` is a `String`, so a future one parses) and of a
//! truncated final line (an append-only log gets cut mid-write, which is expected
//! rather than corrupt — the count is reported).
//!
//! ## Exit codes
//!
//! `0` clean, `1` findings — the documented contract, reused rather than
//! extended. An audit finding is not a usage error, so it must not be 5, and it
//! is not a lease conflict, so it must not be 2.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::events::Event;
use crate::lease::DEFAULT_TTL_SECS;

/// How many contended paths and agents a summary lists before it stops being a
/// summary. The full data is in the log; this is the part a human reads.
const TOP_N: usize = 10;

/// Which named check to run. Absent means the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    DoubleWin,
    StaleHolds,
}

impl Check {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "double-win" => Ok(Check::DoubleWin),
            "stale-holds" => Ok(Check::StaleHolds),
            other => Err(anyhow::anyhow!(
                "unknown check \"{other}\"; expected double-win or stale-holds"
            )),
        }
    }
}

/// One completed or still-open hold of one path by one agent.
#[derive(Debug, Clone, Serialize)]
pub struct Hold {
    pub path: String,
    pub agent: String,
    /// Line number in `.pact/events.jsonl` of the acquire/steal that opened it.
    pub opened_line: usize,
    pub opened_at: String,
    /// `None` while the log shows the lease still held.
    pub closed_line: Option<usize>,
    pub closed_at: Option<String>,
    /// `released`, `force-released` or `expired`.
    pub closed_by: Option<String>,
    /// How many `renewed` events fell inside this window. A long hold that
    /// renewed is following the protocol; one that did not is the smell.
    pub renewals: usize,
    pub held_secs: Option<i64>,
}

/// Two agents holding one path at the same time.
#[derive(Debug, Clone, Serialize)]
pub struct DoubleWin {
    pub path: String,
    /// The acquire/steal that should not have succeeded.
    pub incoming_agent: String,
    pub incoming_kind: String,
    pub incoming_line: usize,
    pub incoming_at: String,
    /// Whoever the log says was already holding it, and since when.
    pub already_holding: Vec<HoldingAgent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HoldingAgent {
    pub agent: String,
    pub since: String,
    pub since_line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contended {
    pub path: String,
    pub holds: usize,
    pub distinct_agents: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentActivity {
    pub agent: String,
    pub events: usize,
    pub holds: usize,
    pub steals: usize,
    pub held_secs_total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HoldStats {
    pub completed: usize,
    pub median_secs: i64,
    pub p90_secs: i64,
    pub max_secs: i64,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub events: usize,
    /// Lines the parser could not read. A torn final line is normal for an
    /// append-only log; a large number here means something else is wrong.
    pub unparseable_lines: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub agents: Vec<String>,
    pub first_event_at: Option<String>,
    pub last_event_at: Option<String>,
    pub steals: usize,
    pub open_holds: usize,
    pub hold_secs: Option<HoldStats>,
    pub top_contended: Vec<Contended>,
    pub per_agent: Vec<AgentActivity>,
}

/// The report for a named check: findings plus enough context to judge them.
#[derive(Debug, Serialize)]
pub struct CheckReport {
    pub check: &'static str,
    pub events_scanned: usize,
    pub unparseable_lines: usize,
    pub double_wins: Vec<DoubleWin>,
    pub stale_holds: Vec<Hold>,
    /// The threshold `stale-holds` used, so a finding can be judged without
    /// reading the source.
    pub ttl_secs: Option<u64>,
}

impl CheckReport {
    pub fn findings(&self) -> usize {
        self.double_wins.len() + self.stale_holds.len()
    }
}

/// `--since`: an RFC3339 instant, or a duration back from now.
///
/// Both spellings because both are what people reach for: an exact instant when
/// correlating with something else, and "the last day" when triaging. Durations
/// are `<n><unit>` with unit in `smhdw` — deliberately not a general parser,
/// because `--since 3` meaning three of something unstated is a bug waiting.
pub fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .with_context(|| format!("--since {s}: expected RFC3339 or a duration like 7d"))?,
    );
    let n: i64 = num
        .parse()
        .with_context(|| format!("--since {s}: {num} is not a number"))?;
    let d = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        other => {
            return Err(anyhow::anyhow!(
                "--since {s}: unknown unit \"{other}\"; use s, m, h, d or w"
            ))
        }
    };
    Ok(Utc::now() - d)
}

fn parse_at(at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Does this kind open a hold window?
///
/// `stolen` counts, and that is not a detail: a takeover is a hold. Treating it
/// as anything else would make every reclaim invisible to `stale-holds` and would
/// hide exactly the double-wins a `--steal` could cause.
fn opens(kind: &str) -> bool {
    matches!(kind, "acquired" | "stolen")
}

/// Does this kind close one?
///
/// `expired` closes the **previous** holder's window: `lease.rs` logs it under
/// `existing.agent`, not under the agent taking over. That is what makes a
/// routine reclaim — `expired` then `stolen` — read correctly instead of looking
/// like two agents holding at once.
fn closes(kind: &str) -> bool {
    matches!(kind, "released" | "force-released" | "expired")
}

/// Reconstruct every hold window, and notice where two overlap.
///
/// One pass per path over time-ordered events, tracking who is currently open.
/// An acquire arriving while somebody *else* is open is the double-win; an
/// acquire while the *same* agent is open is a re-entrant refresh, which pact
/// does deliberately and which must not be reported.
fn reconstruct(events: &[(usize, Event)]) -> (Vec<Hold>, Vec<DoubleWin>) {
    let mut by_path: BTreeMap<&str, Vec<(usize, &Event)>> = BTreeMap::new();
    for (line, e) in events {
        if let Some(p) = e.path.as_deref() {
            by_path.entry(p).or_default().push((*line, e));
        }
    }

    let mut holds = Vec::new();
    let mut doubles = Vec::new();

    for (path, mut rows) in by_path {
        // Stable by time, then by line, so two events in the same millisecond
        // keep the order the log recorded them in.
        rows.sort_by(|a, b| {
            parse_at(&a.1.at)
                .cmp(&parse_at(&b.1.at))
                .then(a.0.cmp(&b.0))
        });

        // agent -> (opened_line, opened_at, renewals)
        let mut open: BTreeMap<String, (usize, String, usize)> = BTreeMap::new();

        for (line, e) in rows {
            if opens(&e.kind) {
                let others: Vec<HoldingAgent> = open
                    .iter()
                    .filter(|(a, _)| *a != &e.agent)
                    .map(|(a, (l, at, _))| HoldingAgent {
                        agent: a.clone(),
                        since: at.clone(),
                        since_line: *l,
                    })
                    .collect();
                if !others.is_empty() {
                    doubles.push(DoubleWin {
                        path: path.to_string(),
                        incoming_agent: e.agent.clone(),
                        incoming_kind: e.kind.clone(),
                        incoming_line: line,
                        incoming_at: e.at.clone(),
                        already_holding: others,
                    });
                }
                // A re-entrant acquire by the same agent refreshes rather than
                // opening a second window, so the original open time is kept.
                open.entry(e.agent.clone())
                    .or_insert((line, e.at.clone(), 0));
            } else if e.kind == "renewed" {
                if let Some(slot) = open.get_mut(&e.agent) {
                    slot.2 += 1;
                }
            } else if closes(&e.kind) {
                if let Some((oline, oat, renewals)) = open.remove(&e.agent) {
                    let held = parse_at(&oat)
                        .zip(parse_at(&e.at))
                        .map(|(a, b)| (b - a).num_seconds());
                    holds.push(Hold {
                        path: path.to_string(),
                        agent: e.agent.clone(),
                        opened_line: oline,
                        opened_at: oat,
                        closed_line: Some(line),
                        closed_at: Some(e.at.clone()),
                        closed_by: Some(e.kind.clone()),
                        renewals,
                        held_secs: held,
                    });
                }
            }
        }

        // Whatever is still open at the end of the log: a live lease, or an agent
        // that exited without releasing. Reported with no close, never guessed at.
        for (agent, (oline, oat, renewals)) in open {
            holds.push(Hold {
                path: path.to_string(),
                agent,
                opened_line: oline,
                opened_at: oat,
                closed_line: None,
                closed_at: None,
                closed_by: None,
                renewals,
                held_secs: None,
            });
        }
    }

    (holds, doubles)
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Read the log, optionally narrowed to events at or after `since`.
fn load(
    repo_root: &std::path::Path,
    since: Option<DateTime<Utc>>,
) -> Result<(Vec<(usize, Event)>, usize)> {
    let (all, skipped) = crate::events::numbered(repo_root)?;
    let kept = match since {
        None => all,
        Some(cut) => all
            .into_iter()
            .filter(|(_, e)| parse_at(&e.at).map(|t| t >= cut).unwrap_or(false))
            .collect(),
    };
    Ok((kept, skipped))
}

pub fn summary(repo_root: &std::path::Path, since: Option<DateTime<Utc>>) -> Result<Summary> {
    let (events, unparseable) = load(repo_root, since)?;
    let (holds, _) = reconstruct(&events);

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut agents: BTreeSet<String> = BTreeSet::new();
    let mut per: BTreeMap<String, AgentActivity> = BTreeMap::new();
    for (_, e) in &events {
        *by_kind.entry(e.kind.clone()).or_insert(0) += 1;
        agents.insert(e.agent.clone());
        let a = per.entry(e.agent.clone()).or_insert_with(|| AgentActivity {
            agent: e.agent.clone(),
            events: 0,
            holds: 0,
            steals: 0,
            held_secs_total: 0,
        });
        a.events += 1;
        if e.kind == "stolen" {
            a.steals += 1;
        }
    }
    for h in &holds {
        if let Some(a) = per.get_mut(&h.agent) {
            a.holds += 1;
            a.held_secs_total += h.held_secs.unwrap_or(0);
        }
    }

    let mut per_agent: Vec<AgentActivity> = per.into_values().collect();
    per_agent.sort_by(|a, b| b.events.cmp(&a.events).then(a.agent.cmp(&b.agent)));
    per_agent.truncate(TOP_N);

    // Contention is distinct agents first, then hold count: a path one agent took
    // forty times is busy, a path four agents took once each is contended, and
    // the second is the one worth a human's attention.
    let mut per_path: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
    for h in &holds {
        let e = per_path.entry(&h.path).or_default();
        e.0 += 1;
        e.1.insert(&h.agent);
    }
    let mut top_contended: Vec<Contended> = per_path
        .into_iter()
        .map(|(p, (n, set))| Contended {
            path: p.to_string(),
            holds: n,
            distinct_agents: set.len(),
        })
        .collect();
    top_contended.sort_by(|a, b| {
        b.distinct_agents
            .cmp(&a.distinct_agents)
            .then(b.holds.cmp(&a.holds))
            .then(a.path.cmp(&b.path))
    });
    top_contended.truncate(TOP_N);

    let mut durations: Vec<i64> = holds.iter().filter_map(|h| h.held_secs).collect();
    durations.sort_unstable();
    let hold_secs = (!durations.is_empty()).then(|| HoldStats {
        completed: durations.len(),
        median_secs: percentile(&durations, 0.5),
        p90_secs: percentile(&durations, 0.9),
        max_secs: *durations.last().unwrap_or(&0),
    });

    Ok(Summary {
        events: events.len(),
        unparseable_lines: unparseable,
        steals: by_kind.get("stolen").copied().unwrap_or(0),
        by_kind,
        agents: agents.into_iter().collect(),
        first_event_at: events.first().map(|(_, e)| e.at.clone()),
        last_event_at: events.last().map(|(_, e)| e.at.clone()),
        open_holds: holds.iter().filter(|h| h.closed_at.is_none()).count(),
        hold_secs,
        top_contended,
        per_agent,
    })
}

pub fn run_check(
    repo_root: &std::path::Path,
    check: Check,
    since: Option<DateTime<Utc>>,
) -> Result<CheckReport> {
    let (events, unparseable) = load(repo_root, since)?;
    let (holds, doubles) = reconstruct(&events);

    let mut report = CheckReport {
        check: match check {
            Check::DoubleWin => "double-win",
            Check::StaleHolds => "stale-holds",
        },
        events_scanned: events.len(),
        unparseable_lines: unparseable,
        double_wins: Vec::new(),
        stale_holds: Vec::new(),
        ttl_secs: None,
    };

    match check {
        Check::DoubleWin => report.double_wins = doubles,
        Check::StaleHolds => {
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
                    let over = h.held_secs.unwrap_or(0) > DEFAULT_TTL_SECS as i64;
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
    }
    Ok(report)
}

// --------------------------------------------------------------- rendering

fn secs(n: i64) -> String {
    crate::lease::human_secs(n)
}

pub fn render_summary(s: &Summary) -> String {
    if s.events == 0 {
        return format!(
            "no coordination history yet{}\n\n\
             .pact/events.jsonl is written by the lease commands; run one and it appears. \
             If this repository HAS been used, the log may predate events-log preservation \
             — see docs/audit.md.",
            if s.unparseable_lines > 0 {
                format!(" ({} unreadable line(s))", s.unparseable_lines)
            } else {
                String::new()
            }
        );
    }

    let mut out = Vec::new();
    out.push(format!(
        "{} events from {} agent(s)",
        s.events,
        s.agents.len()
    ));
    if let (Some(a), Some(b)) = (&s.first_event_at, &s.last_event_at) {
        out.push(format!("  span   {a}  ->  {b}"));
    }
    if s.unparseable_lines > 0 {
        out.push(format!(
            "  note   {} unreadable line(s) — a torn final line is normal for an append-only log",
            s.unparseable_lines
        ));
    }

    let kinds: Vec<String> = s.by_kind.iter().map(|(k, n)| format!("{k} {n}")).collect();
    out.push(format!("  kinds  {}", kinds.join(", ")));
    if s.open_holds > 0 {
        out.push(format!("  open   {} lease(s) still held", s.open_holds));
    }

    if let Some(h) = &s.hold_secs {
        out.push(String::new());
        out.push(format!(
            "hold time over {} completed hold(s): median {}, p90 {}, max {}",
            h.completed,
            secs(h.median_secs),
            secs(h.p90_secs),
            secs(h.max_secs)
        ));
    }

    if !s.top_contended.is_empty() {
        out.push(String::new());
        out.push("most contended paths".to_string());
        for c in &s.top_contended {
            out.push(format!(
                "  {:<44} {} hold(s) by {} agent(s)",
                c.path, c.holds, c.distinct_agents
            ));
        }
    }

    if !s.per_agent.is_empty() {
        out.push(String::new());
        out.push("busiest agents".to_string());
        for a in &s.per_agent {
            out.push(format!(
                "  {:<24} {} event(s), {} hold(s){}, {} held",
                a.agent,
                a.events,
                a.holds,
                if a.steals > 0 {
                    format!(", {} steal(s)", a.steals)
                } else {
                    String::new()
                },
                secs(a.held_secs_total)
            ));
        }
    }

    out.join("\n")
}

pub fn render_check(r: &CheckReport) -> String {
    let mut out = vec![format!(
        "{}: scanned {} event(s)",
        r.check, r.events_scanned
    )];
    if r.unparseable_lines > 0 {
        out.push(format!("  {} unreadable line(s)", r.unparseable_lines));
    }

    if r.findings() == 0 {
        out.push(match r.check {
            "double-win" => {
                "no overlapping hold windows — no two agents ever held one path at once".to_string()
            }
            _ => format!(
                "no holds past {} without a renew",
                secs(r.ttl_secs.unwrap_or(DEFAULT_TTL_SECS) as i64)
            ),
        });
        return out.join("\n");
    }

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

    for h in &r.stale_holds {
        let ended = match h.closed_by.as_deref() {
            Some(k) => format!("ended by {k}"),
            None => "still open".to_string(),
        };
        out.push(format!(
            "  {:<40} {:<16} held {:>8}, {} (line {})",
            h.path,
            h.agent,
            h.held_secs.map(secs).unwrap_or_else(|| "?".to_string()),
            ended,
            h.opened_line
        ));
    }
    if !r.stale_holds.is_empty() {
        out.push(String::new());
        // Deliberately no "distinct incidents" count. One `lease acquire` naming
        // three paths writes three event rows whose timestamps differ by
        // microseconds, so grouping them would need a tolerance window — and a
        // number that depends on an arbitrary tolerance is worse than no number.
        // Holds sharing an agent and a duration are almost certainly one acquire;
        // the reader can see that from the rows.
        out.push(format!(
            "{} hold(s) ran past the {} TTL without a single renew. Rows sharing an agent and a\n\
             duration are one `lease acquire` that named several paths. The protocol says a long\n\
             task must not outlive its lease, and `pact lease renew` refreshes it — a lapsed lease\n\
             is reclaimable by anyone, so each of these is a window where a peer could have taken\n\
             a path its holder still believed it owned.",
            r.stale_holds.len(),
            secs(r.ttl_secs.unwrap_or(DEFAULT_TTL_SECS) as i64)
        ));
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a log and audit it. Takes raw lines so a test can plant a truncated
    /// one, an unknown kind, or outright junk.
    fn with_log(lines: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let pact = tmp.path().join(".pact");
        std::fs::create_dir_all(&pact).unwrap();
        std::fs::write(pact.join("events.jsonl"), lines.join("\n")).unwrap();
        tmp
    }

    fn ev(at: &str, agent: &str, kind: &str, path: &str) -> String {
        format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}"}}"#)
    }

    #[test]
    fn an_empty_log_is_not_an_error() {
        let tmp = with_log(&[]);
        let s = summary(tmp.path(), None).unwrap();
        assert_eq!(s.events, 0);
        assert!(render_summary(&s).contains("no coordination history"));

        let r = run_check(tmp.path(), Check::DoubleWin, None).unwrap();
        assert_eq!(r.findings(), 0, "an empty log has no findings, not one");
    }

    /// No log at all — a repo that has never used pact. Must read as empty rather
    /// than failing, and must NOT create the file: audit is a question.
    #[test]
    fn a_missing_log_reads_as_empty_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert_eq!(summary(tmp.path(), None).unwrap().events, 0);
        assert!(
            !tmp.path().join(".pact").exists(),
            "auditing must not create .pact/"
        );
    }

    #[test]
    fn a_clean_history_has_no_findings() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "agent-a", "released", "src/a.rs"),
            &ev("2026-08-01T10:06:00Z", "agent-b", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:07:00Z", "agent-b", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None).unwrap();
        assert_eq!(r.findings(), 0, "sequential holds are not a double-win");

        let s = summary(tmp.path(), None).unwrap();
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
        let r = run_check(tmp.path(), Check::DoubleWin, None).unwrap();
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
        let r = run_check(tmp.path(), Check::DoubleWin, None).unwrap();
        assert_eq!(r.findings(), 0, "expired closes the old window first");
    }

    /// A `--steal` over a LIVE lease has no `expired` before it, so it is a real
    /// overlap and must be reported. That asymmetry is the whole reason `expired`
    /// rows exist.
    #[test]
    fn stealing_a_live_lease_is_a_double_win() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "agent-b", "stolen", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::DoubleWin, None).unwrap();
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
        let r = run_check(tmp.path(), Check::DoubleWin, None).unwrap();
        assert_eq!(r.findings(), 0);
        let s = summary(tmp.path(), None).unwrap();
        assert_eq!(
            s.hold_secs.unwrap().completed,
            1,
            "a refresh extends one hold rather than opening a second"
        );
    }

    /// An append-only log gets cut mid-write. The torn line must be counted and
    /// skipped, and everything before it must still be analysed.
    #[test]
    fn a_truncated_final_line_is_counted_not_fatal() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "src/a.rs"),
            r#"{"at":"2026-08-01T10:02:00Z","agent":"agent-b","kind":"acq"#,
        ]);
        let s = summary(tmp.path(), None).unwrap();
        assert_eq!(s.events, 2, "the two whole events still count");
        assert_eq!(s.unparseable_lines, 1);
        assert!(render_summary(&s).contains("unreadable"));
        // And a check still runs rather than refusing on a torn tail.
        assert_eq!(
            run_check(tmp.path(), Check::DoubleWin, None)
                .unwrap()
                .unparseable_lines,
            1
        );
    }

    /// A kind this version has never heard of must pass through. The log is
    /// append-only and older binaries read newer logs.
    #[test]
    fn an_unknown_kind_is_counted_and_ignored_by_the_checks() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:30Z", "agent-a", "teleported", "src/a.rs"),
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "src/a.rs"),
        ]);
        let s = summary(tmp.path(), None).unwrap();
        assert_eq!(s.by_kind.get("teleported"), Some(&1));
        assert_eq!(s.unparseable_lines, 0, "unknown is not unparseable");
        // It neither opens nor closes a window.
        assert_eq!(s.hold_secs.unwrap().completed, 1);
        assert_eq!(
            run_check(tmp.path(), Check::DoubleWin, None)
                .unwrap()
                .findings(),
            0
        );
    }

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
        let r = run_check(tmp.path(), Check::StaleHolds, None).unwrap();
        assert_eq!(r.findings(), 1, "only the long unrenewed hold");
        assert_eq!(r.stale_holds[0].agent, "slow");
        assert_eq!(r.stale_holds[0].held_secs, Some(7200));
        assert_eq!(r.ttl_secs, Some(DEFAULT_TTL_SECS));
        assert!(render_check(&r).contains("without a single renew"));
    }

    /// A lease that lapsed is the same smell already realised, whatever its
    /// duration.
    #[test]
    fn a_lapsed_lease_is_a_stale_hold_even_if_short() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "gone", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:00:05Z", "gone", "expired", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::StaleHolds, None).unwrap();
        assert_eq!(r.findings(), 1);
        assert_eq!(r.stale_holds[0].closed_by.as_deref(), Some("expired"));
    }

    #[test]
    fn an_unreleased_lease_shows_as_open_rather_than_guessed_at() {
        let tmp = with_log(&[&ev(
            "2026-08-01T10:00:00Z",
            "agent-a",
            "acquired",
            "src/a.rs",
        )]);
        let s = summary(tmp.path(), None).unwrap();
        assert_eq!(s.open_holds, 1);
        assert!(
            s.hold_secs.is_none(),
            "an open hold has no duration to average"
        );
    }

    #[test]
    fn since_accepts_both_spellings_and_filters() {
        assert!(parse_since("2026-08-01T00:00:00Z").is_ok());
        for d in ["30s", "15m", "6h", "7d", "2w"] {
            assert!(parse_since(d).is_ok(), "{d}");
        }
        for bad in ["", "7", "7y", "yesterday", "d7"] {
            assert!(parse_since(bad).is_err(), "{bad} should not parse");
        }

        let tmp = with_log(&[
            &ev("2020-01-01T00:00:00Z", "old", "acquired", "src/a.rs"),
            &ev("2020-01-01T00:01:00Z", "old", "released", "src/a.rs"),
        ]);
        let cut = parse_since("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            summary(tmp.path(), Some(cut)).unwrap().events,
            0,
            "--since must exclude older events"
        );
        assert_eq!(summary(tmp.path(), None).unwrap().events, 2);
    }

    /// Junk that is not JSON at all, in the middle rather than at the end. Still
    /// counted, still skipped, and the surrounding events still analysed.
    #[test]
    fn junk_lines_anywhere_do_not_derail_the_scan() {
        let tmp = with_log(&[
            "this is not json",
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            "",
            "{}",
            &ev("2026-08-01T10:01:00Z", "agent-a", "released", "src/a.rs"),
        ]);
        let s = summary(tmp.path(), None).unwrap();
        assert_eq!(s.events, 2);
        // `{}` fails to deserialize (no required fields) and the blank line is
        // skipped without counting.
        assert_eq!(s.unparseable_lines, 2);
    }
}
