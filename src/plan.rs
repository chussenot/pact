//! `pact plan lint` — check a wave plan BEFORE spawning a fleet.
//!
//! ## Why this exists, and why it is not a lease feature
//!
//! Five field runs (arkanoid, megablast, crucible, grimcast, quern) point the same
//! way: contention is decided at PLANNING time, not at acquire time. quern was
//! deliberately built to contend — 37 agents, deep coupling, three declared hot
//! files — and produced **1 refusal in 64 claims**, its first only as the run wound
//! down. Across all five runs the only sustained contention ever observed was the
//! crucible's, and that one was engineered on purpose.
//!
//! One refusal is not zero, and the lease handled it correctly. The claim is about
//! the RATE: 37 agents on a deliberately coupled codebase produced one, and that
//! number is set by the plan rather than by the arbitration.
//!
//! A lease resolves a collision after two agents have already been sent at one file.
//! A plan that never puts them there costs nothing to arrange and cannot be
//! contended. That work was happening entirely in an orchestrator's head with
//! nothing to check it, which is what this lints.
//!
//! ## Why a manifest, and not the bead graph
//!
//! pact does not read the Beads store. Since 0.9.0 bd is the agents' task tracker,
//! read only through its committed `interactions.jsonl` export, and reaching into it
//! to reconstruct a plan would put a backend back on a pact command's path — the
//! exact dependency that release removed.
//!
//! The plan is not invented here either. Orchestrators already write one: 9 of 24
//! beads in the quern run carry a structured `files:` / `group:` / `slug:` convention
//! inside their descriptions. So the manifest is an EXPORT of a plan that exists,
//! `bd list --json` is the export mechanism, and docs/plan.md shows the transform.
//! One shell pipeline, no second copy to keep in step by hand.
//!
//! ## Deliberately out of scope
//!
//! Reading `.beads`. Inferring which files a bead touches from its prose. Scheduling
//! waves. This lints a plan; it does not make one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A path appearing in this many entries or more is worth reporting across waves.
///
/// Three, not two: two entries touching one file is ordinary sequencing (write it,
/// then test it). Three or more is a file the plan keeps coming back to, which is
/// where a frozen interface or an owner would pay off.
const HOT_FILE_ENTRIES: usize = 3;

/// One planned unit of work.
///
/// `id` is opaque to pact and does NOT have to be a bead id — that keeps the charter
/// clean and lets an orchestrator lint a plan before any beads exist.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Which wave this runs in. Absent is a warning, not an error: a
    /// partially-planned manifest is the normal intermediate state.
    #[serde(default)]
    pub wave: Option<i64>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// What the lint found. `error` decides the exit code; everything else is reported.
#[derive(Debug, Serialize)]
pub struct Finding {
    pub kind: String,
    pub error: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub entries: usize,
    pub waves: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.error).count()
    }

    pub fn warnings(&self) -> usize {
        self.findings.iter().filter(|f| !f.error).count()
    }
}

/// Parse a manifest that is either a JSON array or one object per line.
///
/// Both, because an orchestrator writing this from a shell loop produces JSONL and
/// one writing it from a script produces an array, and refusing either would just
/// mean a `jq` incantation in the docs.
///
/// **Parse-tolerance is deliberately NOT wanted here**, unlike
/// `beads::interaction_assignees`, which skips a malformed line and carries on. That
/// file is somebody else's export and a partial answer beats no answer. This one is
/// the plan for a fleet a human is about to spawn: a line pact cannot read is a line
/// the human must fix first, so it is an error naming the line number.
pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).context("parsing the manifest as a JSON array");
    }
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Entry = serde_json::from_str(line)
            .with_context(|| format!("parsing manifest line {}", i + 1))?;
        out.push(entry);
    }
    Ok(out)
}

/// Lint `entries`, resolving every path the way `lease acquire` would.
///
/// `repo_root` exists only so paths normalize identically to a real lease — one file
/// must be one path however the manifest spelled it, or the overlap check and the
/// lease it is protecting disagree about what they are talking about.
pub fn lint(repo_root: &Path, entries: &[Entry]) -> Report {
    let mut findings = Vec::new();

    // Normalize, then DEDUPE within the entry. Without the dedupe, one entry listing
    // the same file twice — a copy-paste in a hand-written manifest, or two spellings
    // of one path that normalization collapses — was reported as
    // "x.rs is claimed by 2 entries (a, a)": a false error, naming one entry twice,
    // that would block a run for nothing. An entry cannot contend with itself.
    //
    // Order-preserving, because the first spelling is the one the author wrote and any
    // message quoting the path should quote that.
    let mut dupes: Vec<Finding> = Vec::new();
    let normalized: Vec<(&Entry, Vec<String>)> = entries
        .iter()
        .map(|e| {
            let mut seen = BTreeSet::new();
            let mut files = Vec::new();
            let mut repeated = BTreeSet::new();
            for f in &e.files {
                let norm = crate::lease::normalize_path(repo_root, f);
                if seen.insert(norm.clone()) {
                    files.push(norm);
                } else {
                    repeated.insert(norm);
                }
            }
            // Reported, not swallowed: a repeat is usually a copy-paste, and once it
            // stops being an error nothing else would ever mention it.
            for path in repeated {
                dupes.push(Finding {
                    kind: "duplicate-file-in-entry".into(),
                    error: false,
                    detail: format!(
                        "{} lists {path} more than once — counted once; an entry cannot \
                         contend with itself",
                        e.id
                    ),
                });
            }
            (e, files)
        })
        .collect();
    findings.extend(dupes);

    // Duplicate ids first: every check below keys on id, so a duplicate would make
    // the rest of the report ambiguous rather than wrong.
    let mut seen_ids: BTreeMap<&str, usize> = BTreeMap::new();
    for (e, _) in &normalized {
        *seen_ids.entry(e.id.as_str()).or_default() += 1;
    }
    for (id, n) in seen_ids.iter().filter(|(_, n)| **n > 1) {
        findings.push(Finding {
            kind: "duplicate-id".into(),
            error: true,
            detail: format!("{n} entries share the id {id}"),
        });
    }

    // THE MEGABLAST RULE. Two entries in one wave naming one path is the contention a
    // planner removes for free, and it is the only finding here that is worth
    // stopping a run for.
    let mut per_wave: BTreeMap<i64, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    for (e, files) in &normalized {
        let Some(w) = e.wave else { continue };
        let slot = per_wave.entry(w).or_default();
        for f in files {
            slot.entry(f.as_str()).or_default().push(e.id.as_str());
        }
    }
    for (wave, paths) in &per_wave {
        for (path, owners) in paths.iter().filter(|(_, o)| o.len() > 1) {
            findings.push(Finding {
                kind: "intra-wave-overlap".into(),
                error: true,
                detail: format!(
                    "wave {wave}: {path} is claimed by {} entries ({}) — they will \
                     contend, and one of them can move to another wave for free",
                    owners.len(),
                    owners.join(", ")
                ),
            });
        }
    }

    // Cycles. Reported as the cycle, not as "a cycle exists": a reader who has to
    // find it themselves in a 40-entry plan is not much better off.
    let ids: BTreeSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let deps: BTreeMap<&str, Vec<&str>> = entries
        .iter()
        .map(|e| {
            (
                e.id.as_str(),
                e.depends_on.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    for cycle in cycles(&deps) {
        findings.push(Finding {
            kind: "dependency-cycle".into(),
            error: true,
            detail: format!("dependency cycle: {}", cycle.join(" -> ")),
        });
    }
    for e in entries {
        for d in &e.depends_on {
            if !ids.contains(d.as_str()) {
                findings.push(Finding {
                    kind: "unknown-dependency".into(),
                    error: true,
                    detail: format!("{} depends on {d}, which is not in this manifest", e.id),
                });
            }
        }
    }

    // A dependency that runs no earlier than its dependent is a plan that says one
    // thing and schedules another.
    let wave_of: BTreeMap<&str, i64> = entries
        .iter()
        .filter_map(|e| e.wave.map(|w| (e.id.as_str(), w)))
        .collect();
    for e in entries {
        let Some(mine) = wave_of.get(e.id.as_str()) else {
            continue;
        };
        for d in &e.depends_on {
            if let Some(theirs) = wave_of.get(d.as_str()) {
                if theirs >= mine {
                    findings.push(Finding {
                        kind: "dependency-not-earlier".into(),
                        error: true,
                        detail: format!(
                            "{} is in wave {mine} but depends on {d} in wave {theirs} — a \
                             dependency must finish first",
                            e.id
                        ),
                    });
                }
            }
        }
    }

    // Warnings: both directions of "the plan and the work do not line up".
    for (e, files) in &normalized {
        if files.is_empty() {
            findings.push(Finding {
                kind: "entry-claims-no-files".into(),
                error: false,
                detail: format!(
                    "{} names no files, so nothing here can check what it will touch",
                    e.id
                ),
            });
        }
        if e.wave.is_none() {
            findings.push(Finding {
                kind: "entry-has-no-wave".into(),
                error: false,
                detail: format!("{} has no wave, so it is not checked for overlap", e.id),
            });
        }
    }

    // Hot files across waves: informational, and the one output that suggests an
    // interface freeze rather than a reshuffle.
    let mut per_path: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (e, files) in &normalized {
        for f in files {
            per_path
                .entry(f.as_str())
                .or_default()
                .insert(e.id.as_str());
        }
    }
    for (path, owners) in per_path.iter().filter(|(_, o)| o.len() >= HOT_FILE_ENTRIES) {
        findings.push(Finding {
            kind: "hot-file".into(),
            error: false,
            detail: format!(
                "{path} appears in {} entries — the plan keeps returning to it; \
                 freezing its interface early is cheaper than sequencing around it",
                owners.len()
            ),
        });
    }

    Report {
        entries: entries.len(),
        waves: per_wave.len(),
        findings,
    }
}

/// Every dependency cycle, each as the ids on it.
///
/// Iterative DFS with an explicit stack rather than recursion: a hand-written or
/// generated manifest is untrusted input, and a deep chain must not overflow the
/// stack of the tool that was supposed to be checking it.
fn cycles<'a>(deps: &BTreeMap<&'a str, Vec<&'a str>>) -> Vec<Vec<&'a str>> {
    let mut found: Vec<Vec<&str>> = Vec::new();
    let mut done: BTreeSet<&str> = BTreeSet::new();

    for start in deps.keys() {
        if done.contains(start) {
            continue;
        }
        // (node, how many of its edges we have walked)
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut on_path: Vec<&str> = vec![start];
        while let Some((node, edge)) = stack.last_mut() {
            let edges = deps.get(node).map(Vec::as_slice).unwrap_or_default();
            if *edge >= edges.len() {
                done.insert(node);
                on_path.pop();
                stack.pop();
                continue;
            }
            let next = edges[*edge];
            *edge += 1;
            if let Some(at) = on_path.iter().position(|n| *n == next) {
                let mut cycle: Vec<&str> = on_path[at..].to_vec();
                cycle.push(next);
                // One cycle reported once, however many nodes it is entered from.
                let mut key = cycle.clone();
                key.pop();
                key.sort_unstable();
                if !found.iter().any(|c| {
                    let mut k = c.clone();
                    k.pop();
                    k.sort_unstable();
                    k == key
                }) {
                    found.push(cycle);
                }
                continue;
            }
            if !done.contains(next) && deps.contains_key(next) {
                on_path.push(next);
                stack.push((next, 0));
            }
        }
    }
    found
}

pub fn render(r: &Report) -> String {
    let mut out = Vec::new();
    out.push(format!(
        "{} entr{} across {} wave(s)",
        r.entries,
        if r.entries == 1 { "y" } else { "ies" },
        r.waves
    ));
    if r.entries == 0 {
        out.push(String::new());
        out.push(
            "the manifest is empty — nothing to check. If that is a surprise, the export \
             that produced it found no entries."
                .into(),
        );
        return out.join("\n");
    }
    if r.findings.is_empty() {
        out.push(String::new());
        out.push("plan is clean: no two entries in one wave touch the same file".into());
        return out.join("\n");
    }
    let (errs, warns): (Vec<&Finding>, Vec<&Finding>) = r.findings.iter().partition(|f| f.error);
    if !errs.is_empty() {
        out.push(String::new());
        out.push(format!("{} error(s) — fix before spawning", errs.len()));
        // Errors are listed individually however many there are: each one names a
        // specific pair of entries a human has to move, and a count would not be
        // actionable.
        for f in &errs {
            out.push(format!("  ✗ {}", f.detail));
        }
    }
    if !warns.is_empty() {
        out.push(String::new());
        out.push(format!("{} warning(s)", r.warnings()));
        out.extend(coalesce(&warns));
    }
    out.join("\n")
}

/// How many findings of one kind get listed before they collapse into a count.
///
/// Warnings here are mostly SYSTEMIC: a manifest exported before waves were assigned
/// produces "has no wave" once per entry, which on a real 43-entry plan was 43 of 54
/// warnings and buried the two that were specific. That is one fact about the
/// manifest, not 43 facts.
///
/// The same lesson `msg::split_notices` learned from a fleet whose inbox was 41
/// status pings: a report nobody can read is a report nobody reads. Errors are never
/// coalesced — see [`render`].
const LIST_BEFORE_COLLAPSE: usize = 3;

/// Group warnings by kind, listing a few and counting the rest.
fn coalesce(warns: &[&Finding]) -> Vec<String> {
    let mut by_kind: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
    for f in warns {
        by_kind.entry(f.kind.as_str()).or_default().push(f);
    }
    let mut out = Vec::new();
    for (kind, group) in by_kind {
        if group.len() <= LIST_BEFORE_COLLAPSE {
            out.extend(group.iter().map(|f| format!("  ! {}", f.detail)));
            continue;
        }
        // The shared clause, taken from the first member so the wording lives in one
        // place — the finding itself — rather than being restated per kind here.
        let shared = group[0]
            .detail
            .split_once(' ')
            .map(|(_, rest)| rest)
            .unwrap_or(&group[0].detail);
        let first: Vec<&str> = group
            .iter()
            .take(LIST_BEFORE_COLLAPSE)
            .map(|f| f.detail.split(' ').next().unwrap_or(""))
            .collect();
        out.push(format!(
            "  ! {} entries [{}]: {} ({}, and {} more)",
            group.len(),
            kind,
            shared,
            first.join(", "),
            group.len() - LIST_BEFORE_COLLAPSE
        ));
    }
    out
}

/// Read, parse and lint. Returns the report; the caller decides the exit code.
pub fn run(repo_root: &Path, manifest: &Path) -> Result<Report> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let entries = parse(&text)?;
    Ok(lint(repo_root, &entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        tmp
    }

    fn entry(id: &str, wave: Option<i64>, files: &[&str], deps: &[&str]) -> Entry {
        Entry {
            id: id.into(),
            wave,
            files: files.iter().map(|s| (*s).into()).collect(),
            depends_on: deps.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn kinds(r: &Report) -> Vec<&str> {
        r.findings.iter().map(|f| f.kind.as_str()).collect()
    }

    #[test]
    fn a_clean_plan_has_nothing_to_say() {
        let tmp = root();
        let entries = [
            entry("a", Some(1), &["src/a.rs"], &[]),
            entry("b", Some(1), &["src/b.rs"], &[]),
            entry("c", Some(2), &["src/c.rs"], &["a"]),
        ];
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 0, "{:?}", r.findings);
        assert_eq!(r.warnings(), 0, "{:?}", r.findings);
        assert_eq!(r.waves, 2);
        assert!(render(&r).contains("plan is clean"));
    }

    /// The megablast rule, and the reason this command exists.
    #[test]
    fn two_entries_on_one_file_in_one_wave_is_an_error() {
        let tmp = root();
        let entries = [
            entry("writer", Some(1), &["src/shared.rs"], &[]),
            entry("tester", Some(1), &["src/shared.rs"], &[]),
        ];
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 1, "{:?}", r.findings);
        let d = &r.findings[0].detail;
        assert!(d.contains("src/shared.rs"), "{d}");
        assert!(d.contains("writer") && d.contains("tester"), "{d}");
    }

    /// The same file in DIFFERENT waves is ordinary sequencing, not a problem.
    #[test]
    fn the_same_file_in_two_waves_is_fine() {
        let tmp = root();
        let entries = [
            entry("write", Some(1), &["src/a.rs"], &[]),
            entry("test", Some(2), &["src/a.rs"], &["write"]),
        ];
        assert_eq!(lint(tmp.path(), &entries).errors(), 0);
    }

    /// One path however the manifest spelled it, so the check and a real lease agree.
    #[test]
    fn overlap_is_detected_across_different_spellings_of_one_path() {
        let tmp = root();
        let entries = [
            entry("a", Some(1), &["src/a.rs"], &[]),
            entry("b", Some(1), &["./src/../src/a.rs"], &[]),
        ];
        let r = lint(tmp.path(), &entries);
        assert!(
            kinds(&r).contains(&"intra-wave-overlap"),
            "normalization must make these one path: {:?}",
            r.findings
        );
    }

    /// A false error that would have blocked a run: one entry listing a path twice
    /// was reported as "claimed by 2 entries (a, a)". An entry cannot contend with
    /// itself.
    #[test]
    fn a_file_listed_twice_in_one_entry_is_not_contention() {
        let tmp = root();
        let entries = [entry("a", Some(1), &["x.rs", "x.rs"], &[])];
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 0, "{:?}", r.findings);
        assert!(
            kinds(&r).contains(&"duplicate-file-in-entry"),
            "counted once, but still worth mentioning: {:?}",
            r.findings
        );
    }

    /// Two SPELLINGS of one path inside one entry collapse the same way, since
    /// normalization runs before the dedupe.
    #[test]
    fn two_spellings_of_one_path_in_one_entry_collapse_quietly() {
        let tmp = root();
        let entries = [entry("a", Some(1), &["x.rs", "./x.rs"], &[])];
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 0, "{:?}", r.findings);
    }

    /// And the dedupe must not hide REAL contention between two entries.
    #[test]
    fn deduping_within_an_entry_still_catches_overlap_between_entries() {
        let tmp = root();
        let entries = [
            entry("a", Some(1), &["x.rs", "x.rs"], &[]),
            entry("b", Some(1), &["x.rs"], &[]),
        ];
        let r = lint(tmp.path(), &entries);
        let overlap: Vec<&Finding> = r
            .findings
            .iter()
            .filter(|f| f.kind == "intra-wave-overlap")
            .collect();
        assert_eq!(overlap.len(), 1, "{:?}", r.findings);
        assert!(
            overlap[0].detail.contains("(a, b)"),
            "{}",
            overlap[0].detail
        );
    }

    #[test]
    fn an_empty_manifest_says_so_rather_than_calling_itself_clean() {
        let tmp = root();
        let r = lint(tmp.path(), &[]);
        let text = render(&r);
        assert!(text.contains("empty"), "{text}");
        assert!(!text.contains("plan is clean"), "{text}");
    }

    /// Unknown fields are IGNORED, so an orchestrator can carry its own metadata in
    /// the same file without pact having to know about it.
    #[test]
    fn unknown_fields_are_ignored_rather_than_rejected() {
        let parsed =
            parse(r#"{"id":"a","wave":1,"files":["x.rs"],"owner":"nobody","slug":"whatever"}"#)
                .expect("unknown fields must not be an error");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "a");
    }

    #[test]
    fn a_dependency_cycle_is_named_not_merely_announced() {
        let tmp = root();
        let entries = [
            entry("a", None, &["x.rs"], &["b"]),
            entry("b", None, &["y.rs"], &["c"]),
            entry("c", None, &["z.rs"], &["a"]),
        ];
        let r = lint(tmp.path(), &entries);
        let cyc: Vec<&Finding> = r
            .findings
            .iter()
            .filter(|f| f.kind == "dependency-cycle")
            .collect();
        assert_eq!(cyc.len(), 1, "one cycle, reported once: {:?}", r.findings);
        for id in ["a", "b", "c"] {
            assert!(cyc[0].detail.contains(id), "{}", cyc[0].detail);
        }
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let tmp = root();
        let entries = [entry("a", None, &["x.rs"], &["a"])];
        assert!(kinds(&lint(tmp.path(), &entries)).contains(&"dependency-cycle"));
    }

    /// A long chain must not overflow the stack of the tool checking it.
    #[test]
    fn a_deep_chain_is_linted_without_recursing() {
        let tmp = root();
        let ids: Vec<String> = (0..5000).map(|i| format!("n{i}")).collect();
        let entries: Vec<Entry> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| Entry {
                id: id.clone(),
                wave: None,
                files: vec![format!("f{i}.rs")],
                depends_on: ids.get(i + 1).cloned().into_iter().collect(),
            })
            .collect();
        let r = lint(tmp.path(), &entries);
        assert!(
            !kinds(&r).contains(&"dependency-cycle"),
            "a chain is not a cycle"
        );
    }

    #[test]
    fn a_dependency_scheduled_no_earlier_is_an_error() {
        let tmp = root();
        let entries = [
            entry("late", Some(1), &["a.rs"], &["early"]),
            entry("early", Some(1), &["b.rs"], &[]),
        ];
        let r = lint(tmp.path(), &entries);
        assert!(
            kinds(&r).contains(&"dependency-not-earlier"),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn an_unknown_dependency_is_an_error() {
        let tmp = root();
        let entries = [entry("a", Some(1), &["a.rs"], &["ghost"])];
        assert!(kinds(&lint(tmp.path(), &entries)).contains(&"unknown-dependency"));
    }

    #[test]
    fn a_duplicate_id_is_an_error_because_every_other_check_keys_on_it() {
        let tmp = root();
        let entries = [
            entry("a", Some(1), &["x.rs"], &[]),
            entry("a", Some(2), &["y.rs"], &[]),
        ];
        assert!(kinds(&lint(tmp.path(), &entries)).contains(&"duplicate-id"));
    }

    #[test]
    fn a_file_less_or_wave_less_entry_warns_but_does_not_fail() {
        let tmp = root();
        let entries = [entry("a", None, &[], &[])];
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 0);
        let k = kinds(&r);
        assert!(k.contains(&"entry-claims-no-files"), "{k:?}");
        assert!(k.contains(&"entry-has-no-wave"), "{k:?}");
    }

    #[test]
    fn a_path_the_plan_keeps_returning_to_is_reported_but_not_an_error() {
        let tmp = root();
        let entries = [
            entry("a", Some(1), &["src/api.rs"], &[]),
            entry("b", Some(2), &["src/api.rs"], &[]),
            entry("c", Some(3), &["src/api.rs"], &[]),
        ];
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 0);
        let hot: Vec<&Finding> = r.findings.iter().filter(|f| f.kind == "hot-file").collect();
        assert_eq!(hot.len(), 1);
        assert!(hot[0].detail.contains("3 entries"), "{}", hot[0].detail);
    }

    /// A systemic warning is one fact about the manifest, not one per entry. A real
    /// 43-entry export produced 43 "has no wave" warnings out of 54 and buried the
    /// two that were specific.
    #[test]
    fn a_systemic_warning_collapses_instead_of_flooding_the_report() {
        let tmp = root();
        let entries: Vec<Entry> = (0..20)
            .map(|i| entry(&format!("e{i}"), None, &["shared.rs"], &[]))
            .collect();
        let r = lint(tmp.path(), &entries);
        assert_eq!(
            r.errors(),
            0,
            "no waves means no overlap check, so no errors"
        );
        let text = render(&r);
        let listed = text
            .lines()
            .filter(|l| l.trim_start().starts_with('!'))
            .count();
        assert!(
            listed <= 4,
            "20 identical warnings must not print 20 lines:\n{text}"
        );
        assert!(text.contains("20 entries"), "{text}");
        assert!(text.contains("and 17 more"), "{text}");
    }

    /// Errors are never collapsed: each names a specific pair a human has to move.
    #[test]
    fn errors_are_always_listed_individually() {
        let tmp = root();
        let mut entries = Vec::new();
        for i in 0..8 {
            entries.push(entry(
                &format!("a{i}"),
                Some(1),
                &[&format!("f{i}.rs")],
                &[],
            ));
            entries.push(entry(
                &format!("b{i}"),
                Some(1),
                &[&format!("f{i}.rs")],
                &[],
            ));
        }
        let r = lint(tmp.path(), &entries);
        assert_eq!(r.errors(), 8);
        let text = render(&r);
        assert_eq!(
            text.lines()
                .filter(|l| l.trim_start().starts_with('✗'))
                .count(),
            8,
            "every overlap must be named:\n{text}"
        );
    }

    #[test]
    fn both_a_json_array_and_jsonl_parse_to_the_same_plan() {
        let arr = r#"[{"id":"a","wave":1,"files":["x.rs"]},{"id":"b","wave":2,"files":["y.rs"]}]"#;
        let jsonl = "{\"id\":\"a\",\"wave\":1,\"files\":[\"x.rs\"]}\n\
                     {\"id\":\"b\",\"wave\":2,\"files\":[\"y.rs\"]}\n";
        let a = parse(arr).expect("array");
        let b = parse(jsonl).expect("jsonl");
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        assert_eq!(a[0].id, b[0].id);
        assert_eq!(a[1].wave, b[1].wave);
    }

    /// Blank lines are skipped; a shell loop writing this will leave them.
    #[test]
    fn blank_lines_in_jsonl_are_skipped() {
        let jsonl = "\n{\"id\":\"a\",\"files\":[]}\n\n";
        assert_eq!(parse(jsonl).expect("jsonl").len(), 1);
    }

    /// The opposite policy from `interaction_assignees`, and on purpose.
    #[test]
    fn a_malformed_line_is_an_error_naming_the_line() {
        let jsonl = "{\"id\":\"a\",\"files\":[]}\nnot json at all\n";
        let err = parse(jsonl).expect_err("must not be tolerated");
        let text = format!("{err:#}");
        assert!(text.contains("line 2"), "must name the line: {text}");
    }

    #[test]
    fn an_entry_missing_its_id_is_an_error() {
        assert!(parse("{\"wave\":1,\"files\":[]}\n").is_err());
    }
}
