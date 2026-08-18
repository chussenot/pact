//! One pass over the log, and the three things that decide what it contains:
//! the `--since` window, the annotations that retract lines, and the run context
//! the events themselves declared.
//!
//! One file because the three are not independent. The order they are applied in
//! is the correctness argument — annotations before the window, context before
//! both — and each of those orderings exists because the other order produced a
//! wrong report. [`load`] is where that order is written down; splitting the
//! window from the exclusions would leave the reason for it in neither file.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::model::parse_at;
use crate::events::Event;
use crate::identity;

/// The one event kind that is not history: a correction pointing at lines that
/// are.
///
/// `.pact/events.jsonl` is append-only, and that is load-bearing rather than
/// incidental — it is committed, and the guard-file bead (pact-ehi) treats it as
/// the evidence base for a real decision. So a wrong entry is never edited or
/// deleted; it is *annotated*, by appending a record that names the lines and says
/// why. The original stays readable, the correction is attributable, and anyone
/// can disagree with an annotation by reading what it covers.
///
/// Older pact binaries need no change to cope: `kind` is a `String`, so an
/// annotation parses as an unknown kind, opens no hold window and closes none.
/// They simply do not apply the exclusion — which is the safe direction, because
/// it over-reports rather than hiding events.
///
/// The first one exists because on 2026-07-31 hand-run expiry and atomicity
/// experiments in this repository's root wrote six synthetic events: agents
/// `victim`, `ghost` and `grabber` on paths `shared.rs`, `ghost.rs` and `new.rs`,
/// none of which have ever existed here.
pub const ANNOTATION_KIND: &str = "annotation";

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

/// What one pass over the log produced.
pub(in crate::audit) struct Loaded {
    pub(in crate::audit) events: Vec<(usize, Event)>,
    pub(in crate::audit) unparseable: usize,
    /// Events dropped because an annotation covers their line.
    pub(in crate::audit) excluded: usize,
    /// The annotations themselves, so a report can say what was excluded and why
    /// rather than only how many.
    pub(in crate::audit) annotations: Vec<Annotation>,
    /// The constraints the run operated under: the last value set for each key.
    ///
    /// **Deliberately not filtered by `--since`.** A policy declared at run start
    /// is the policy in force three hours later, and dropping it because it falls
    /// outside the window would leave a report showing behaviour with the
    /// constraints stripped off — precisely the reading this records exist to
    /// prevent (see [`crate::events::CONTEXT_KIND`]).
    pub(in crate::audit) context: std::collections::BTreeMap<String, String>,
}

/// A correction: which lines are not real history, and who says so.
#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub line: usize,
    pub at: String,
    pub actor: Option<String>,
    pub note: Option<String>,
    pub covers_lines: Vec<usize>,
    /// `false` when `actor` is `Some` but fails [`identity::validate`]'s
    /// `[a-z0-9][a-z0-9-]{1,31}` format check. `true` when `actor` is absent —
    /// unattributed is a different, already-surfaced condition ("unknown" in
    /// the rendered report), not a malformed one.
    ///
    /// pact has no command that writes an annotation itself — every one today
    /// is a hand-typed JSONL line — so there is no write-time gate to put this
    /// check behind. Flagging it here, where the line is read back, is the
    /// only reachable point: rejecting the line outright would make a single
    /// bad `actor` field silently swallow the correction it was meant to
    /// record, which is worse than trusting a forgeable field already was.
    pub actor_valid: bool,
}

/// Read the log, drop annotated lines unless asked not to, and narrow by `--since`.
///
/// The exclusion happens BEFORE `--since`, deliberately: an annotation and the
/// lines it covers are usually days apart, so filtering by time first would drop
/// the correction and silently re-admit the events it corrects.
pub(in crate::audit) fn load(
    repo_root: &std::path::Path,
    since: Option<DateTime<Utc>>,
    include_annotated: bool,
) -> Result<Loaded> {
    let (all, unparseable) = crate::events::numbered(repo_root)?;

    let mut annotations = Vec::new();
    let mut covered: BTreeSet<usize> = BTreeSet::new();
    for (line, e) in &all {
        if e.kind != ANNOTATION_KIND {
            continue;
        }
        let covers = e.covers_lines.clone().unwrap_or_default();
        covered.extend(covers.iter().copied());
        annotations.push(Annotation {
            line: *line,
            at: e.at.clone(),
            actor: e.actor.clone(),
            note: e.detail.clone(),
            covers_lines: covers,
            actor_valid: e
                .actor
                .as_deref()
                .is_none_or(|a| identity::validate(a).is_ok()),
        });
    }

    // Computed from `all`, before the window and the annotation filter below:
    // see `Loaded::context` for why a policy outside `--since` is still the
    // policy that was in force.
    let context = crate::events::active_context(&all);

    let mut excluded = 0;
    let events: Vec<(usize, Event)> = all
        .into_iter()
        .filter(|(line, e)| {
            // Annotation rows are never history themselves, whatever
            // `--include-annotated` says: counting them as events would inflate
            // every total with records that describe the log rather than the fleet.
            if e.kind == ANNOTATION_KIND {
                return false;
            }
            // A row describing the run is not a thing the fleet did. Counting
            // context as behaviour would inflate every total with the operator's
            // own declarations — the same reason annotations are dropped above.
            if e.kind == crate::events::CONTEXT_KIND {
                return false;
            }
            if !include_annotated && covered.contains(line) {
                excluded += 1;
                return false;
            }
            true
        })
        .filter(|(_, e)| match since {
            None => true,
            Some(cut) => parse_at(&e.at).map(|t| t >= cut).unwrap_or(false),
        })
        .collect();

    Ok(Loaded {
        context,
        events,
        unparseable,
        excluded,
        annotations,
    })
}
