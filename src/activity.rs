//! When each agent last ran *any* pact command — liveness as a by-product of
//! participation rather than a thing anyone has to remember to do.
//!
//! # Why this is not a protocol step
//!
//! The measurement that shapes this whole module: **agents skip ceremony.**
//! Across the field runs there is **one renewal in 153 events**. A protocol that
//! asks an agent to announce it is still alive gets the same compliance a
//! protocol that asks it to renew gets, which is approximately none — and the
//! agents are not being lazy, they are being sensible, because a heartbeat is
//! work that produces nothing they were asked for.
//!
//! So the signal has to cost zero extra commands. It is written by the identity
//! resolution every invocation already performs, from [`crate::main`]'s single
//! call site, before the subcommand runs. An agent that reads its inbox has
//! announced itself by reading its inbox.
//!
//! # What was already there, and what this adds
//!
//! pact has had a liveness signal since pact-mqw.6: `LeaseEntry::suspect` and
//! `holder_silent_secs` come from [`crate::events::actors`], the most recent
//! event each agent wrote. That is a real signal and it is **event-shaped** —
//! every mutation feeds it. Acquire, renew, release, steal, a refusal, a watch
//! delivery, a context row: all of them say "this agent is here".
//!
//! What leaves no trace at all is the read-only half. `pact msg inbox`,
//! `pact lease ls`, `pact log`, `pact agents`, `pact audit`, `pact whoami` —
//! none writes an event, so an agent doing them is indistinguishable from an
//! agent that has stopped. That is precisely the pact-g50 residual: the worker
//! making one deep change to one file, emitting nothing between acquire and
//! release, which `sweep --suspect` had to be taught not to reclaim. Reading is
//! the most common thing such an agent does, and it was the one thing that did
//! not count.
//!
//! # An mtime is evidence of USAGE, not of progress
//!
//! Stated here because every consumer of this module inherits it, and because it
//! is the honest limit of the whole idea: this says an agent ran a pact command,
//! and nothing whatever about whether the agent is making progress. **A spinning
//! agent is alive AND stuck.** An agent retrying a refused lease every fifteen
//! seconds looks maximally healthy here and is the exact pathology
//! `pact audit --check retry-storm` exists to catch. The two signals answer
//! different questions and neither substitutes for the other.
//!
//! # What it costs, measured
//!
//! Against `benches/lease.rs`, which is the budget this path answers to:
//!
//! | | |
//! |---|---|
//! | `activity/touch` — the write | **14.7 µs** |
//! | `activity/absent` — no `.pact/`, the read-only-command path | **2.5 µs** |
//! | `events/append/by_kind/notified` — an append with no subprocess | 549 µs |
//! | `events/append/by_kind/acquired` — an append that spawns `git rev-parse` | 3.19 ms |
//!
//! So the record costs **2.7% of the cheapest thing pact already does per event**
//! and 0.5% of a hold boundary. That is what makes "every invocation" affordable,
//! and it is why the mechanism is one file keyed by agent: touching the agent's
//! own LOCK files instead — the other candidate — needs a `read_dir` of
//! `.pact/leases/` plus a parse per lock to find out which are its own, which is
//! a scan on the path every single invocation takes, to record the same fact.
//!
//! The pair is in the bench so that trade cannot be quietly reversed later: a
//! per-lease record would show up there as the regression it is.
//!
//! # Machine-local, and absent is a value
//!
//! `.pact/activity/` is runtime state, gitignored with `.pact/leases/` — a
//! record of who ran a command on THIS machine, which says nothing about anyone
//! else's. A log that travels must not carry it.
//!
//! Every consumer degrades to "no data" rather than to a guess, because a
//! repository whose fleet ran on an older pact has no records at all and must
//! not be reported as a fleet of dead agents.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// Where the per-agent records live, relative to the state directory.
const DIR: &str = "activity";

/// The record for one agent.
///
/// **Per agent, not per lease**, and the choice is not close. A per-lease record
/// would have to know which locks this agent holds, which is a `read_dir` plus a
/// parse of every lock file — a scan, on a path every single pact invocation
/// takes. The agent's own name is already in hand, so a per-agent path is
/// derived from it with no I/O at all beyond the write itself.
///
/// The staleness that buys: the record says when the agent last ran *a* command,
/// not when it last touched *this* path. An agent holding four leases and
/// working hard on one of them looks equally alive on all four. That is the
/// right answer for the question every consumer here asks — "is anybody home" is
/// about the agent, not the file — and the wrong answer for "is this particular
/// hold making progress", which is what `--check commit-correlation` and the
/// commit rung of the sweep ladder are for.
fn record_path(state_dir: &Path, agent: &str) -> PathBuf {
    // The agent name IS the filename. `identity::validate` already constrains it
    // to `[a-z0-9][a-z0-9-]{1,31}`, so there is nothing here to encode, no
    // separator to collide on, and no `..` to traverse with — the same reason
    // `.pact/read/<agent>.json` spells it this way.
    state_dir.join(DIR).join(agent)
}

/// Record that `agent` is here, now.
///
/// Called from exactly one place — the identity resolution `main` already
/// performs for every invocation — so no subcommand has to remember, and a
/// subcommand added tomorrow is covered by having been run at all.
///
/// **Skipped when `.pact/` does not exist, and that is what keeps "a question
/// must not mutate" true.** What that invariant protects is a read path creating
/// state: `repo::pact_dir_path` deliberately does not create `.pact/`, and
/// `lease::peek` deliberately does not collect. Writing a record inside a
/// `.pact/` that is already there changes no answer any command was asked for —
/// it is a side-note about the asking. Writing one into a repository that has
/// never run `pact init` would materialise the state directory as a side effect
/// of a question, which is the thing the invariant forbids.
///
/// Infallible by signature. Every error is swallowed: a read-only checkout, a
/// full disk, a permission the agent does not have. Liveness is an optional
/// nicety and no pact command may fail because it could not be recorded — the
/// same rule `events::append` follows for the log, which is far more important
/// than this.
pub fn touch(state_dir: &Path, agent: &str) {
    if !state_dir.is_dir() {
        return;
    }
    let dir = state_dir.join(DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // The timestamp as CONTENT, not as the file's mtime.
    //
    // Same cost — one `O_CREAT|O_TRUNC` open dominates, and ~30 bytes of payload
    // is below the noise floor of the stamp-cost benchmark that guards this path
    // — and it buys three things an mtime does not. It is readable (`cat` answers
    // the question), it survives a copy or an archive that resets mtimes, and it
    // does not make pact's liveness answer depend on a filesystem's mtime
    // granularity, which on some filesystems is coarse enough to matter at the
    // seconds these consumers reason in.
    let _ = std::fs::write(record_path(state_dir, agent), Utc::now().to_rfc3339());
}

/// When `agent` last ran a pact command, if this machine has ever seen it.
///
/// `None` means no record, which is the state every repository is in until the
/// first invocation of a pact that writes them. Consumers must render that as
/// "no data" rather than as "dead" — a fleet that ran on an older pact is not a
/// fleet of corpses.
pub fn last_active(state_dir: &Path, agent: &str) -> Option<DateTime<Utc>> {
    let raw = std::fs::read_to_string(record_path(state_dir, agent)).ok()?;
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Seconds since `agent` last ran a pact command.
///
/// Clamped at zero: a record from the future is a clock that moved, not an agent
/// that will act later, and every consumer here reasons about "how long ago"
/// where a negative answer is meaningless. `lease::is_expired` takes the same
/// position on the same hazard.
pub fn idle_secs(state_dir: &Path, agent: &str, now: DateTime<Utc>) -> Option<i64> {
    last_active(state_dir, agent).map(|at| (now - at).num_seconds().max(0))
}

/// Every agent this machine has a record for, and when it was last seen.
///
/// One `read_dir` of a directory holding one small file per agent — which is why
/// the TUI can afford it on its refresh where it cannot afford a log scan. A
/// fleet is tens of agents, so this is tens of files; the event log it replaces
/// for this purpose is thousands of lines, parsed as JSON, every refresh.
///
/// Unreadable entries are skipped rather than reported: this is a convenience
/// index over records that are themselves best-effort, and one truncated file
/// must not cost the caller the other forty.
pub fn all(state_dir: &Path) -> Vec<(String, DateTime<Utc>)> {
    let Ok(dir) = std::fs::read_dir(state_dir.join(DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<(String, DateTime<Utc>)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let at = last_active(state_dir, &name)?;
            Some((name, at))
        })
        .collect();
    out.sort();
    out
}

/// How long an agent may go without running a pact command before it stops
/// counting as active.
///
/// **Half the default lease TTL, derived rather than picked**, so this and
/// `LeaseEntry::suspect` draw the line in the same place. `suspect` uses half of
/// each lease's OWN ttl, which is the better rule where a lease is in hand; this
/// is the agent-level answer, and an agent may hold several leases with different
/// TTLs or none at all, so it needs one number. Deriving it from the same
/// constant is what stops the TUI calling an agent ACTIVE while `lease ls` calls
/// its hold SUSPECT.
///
/// The TTL default is itself calibrated rather than chosen — 2700s from `pact
/// audit` over 147 real events — so this inherits that calibration instead of
/// adding a second unmeasured number.
pub const FRESH_SECS: i64 = crate::lease::DEFAULT_TTL_SECS as i64 / 2;

/// What an operator needs to know about one agent at a glance.
///
/// The ordering is the severity ordering, so `sort` puts the dead first — which
/// is why the panel was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Liveness {
    /// Past its TTL on everything it holds. Its locks are reclaimable by anyone.
    Dead,
    /// Quiet past the window and STILL HOLDING. The shape worth acting on: work
    /// is parked behind a lease nobody is behind.
    Stale,
    /// Quiet past the window, holding nothing. Finished, or never started —
    /// either way it is blocking no one.
    Idle,
    /// Ran a pact command inside the window.
    Active,
    /// No record on this machine. NOT a verdict: every repository is in this
    /// state until an agent runs a pact new enough to write one, and a fleet
    /// that ran on an older pact is not a fleet of corpses.
    NoData,
}

impl Liveness {
    /// Classify one agent from its record and what it is holding.
    ///
    /// `holds` is how many live leases it has; `all_expired` whether every one of
    /// them is past TTL+grace. Passed in rather than read here, because the
    /// callers already have the lease list in hand and this must not become a
    /// second scan on a refresh path.
    pub fn of(idle_secs: Option<i64>, holds: usize, all_expired: bool) -> Self {
        let Some(idle) = idle_secs else {
            return Self::NoData;
        };
        if holds > 0 && all_expired {
            return Self::Dead;
        }
        if idle < FRESH_SECS {
            return Self::Active;
        }
        if holds > 0 {
            Self::Stale
        } else {
            Self::Idle
        }
    }

    /// The word an operator reads. `NoData` says what is missing rather than
    /// naming a state the agent is in.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dead => "DEAD",
            Self::Stale => "STALE",
            Self::Idle => "IDLE",
            Self::Active => "ACTIVE",
            Self::NoData => "no data",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("x")).unwrap();
        tmp
    }

    #[test]
    fn a_touch_is_readable_back_as_a_time() {
        let tmp = state();
        let dir = tmp.path().join("x");
        assert_eq!(last_active(&dir, "agent-a"), None, "nothing recorded yet");

        touch(&dir, "agent-a");
        let at = last_active(&dir, "agent-a").expect("a record");
        assert!(
            (Utc::now() - at).num_seconds().abs() < 5,
            "the record must be now, not an epoch: {at}"
        );
        assert_eq!(idle_secs(&dir, "agent-a", Utc::now()), Some(0));
    }

    /// The carve-out that keeps "a question must not mutate" true.
    ///
    /// Every read-only command touches this, so if it created the state
    /// directory then `pact lease ls` in a repository that has never run
    /// `pact init` would leave `.pact/` behind — which is exactly what
    /// `repo::pact_dir_path` refuses to do and why it exists.
    #[test]
    fn no_state_dir_means_no_record_and_nothing_created() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("never-initialised");

        touch(&absent, "agent-a");

        assert!(!absent.exists(), "a question created state: {absent:?}");
        assert_eq!(last_active(&absent, "agent-a"), None);
    }

    /// Absence is "no data", never "dead" — the distinction every consumer
    /// inherits, and the one that decides whether a pre-liveness repository
    /// reads as a stopped fleet.
    #[test]
    fn an_agent_with_no_record_reads_as_unknown_not_as_idle_forever() {
        let tmp = state();
        let dir = tmp.path().join("x");
        touch(&dir, "agent-a");

        assert_eq!(idle_secs(&dir, "agent-b", Utc::now()), None);
        assert_eq!(
            all(&dir).len(),
            1,
            "only agents actually seen on this machine"
        );
    }

    /// A record from the future is a clock that moved, not a negative age.
    #[test]
    fn a_record_ahead_of_now_clamps_to_zero() {
        let tmp = state();
        let dir = tmp.path().join("x");
        touch(&dir, "agent-a");

        let past = Utc::now() - chrono::Duration::seconds(600);
        assert_eq!(idle_secs(&dir, "agent-a", past), Some(0));
    }

    #[test]
    fn all_lists_every_agent_seen_here_sorted() {
        let tmp = state();
        let dir = tmp.path().join("x");
        for a in ["gudgeon", "bedstone", "pitwheel"] {
            touch(&dir, a);
        }
        let names: Vec<String> = all(&dir).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["bedstone", "gudgeon", "pitwheel"]);
    }

    /// A truncated or hand-mangled record costs its own agent and nobody else.
    #[test]
    fn an_unreadable_record_does_not_hide_the_others() {
        let tmp = state();
        let dir = tmp.path().join("x");
        touch(&dir, "good");
        std::fs::write(dir.join(DIR).join("bad"), "not a timestamp").unwrap();

        assert_eq!(last_active(&dir, "bad"), None);
        let names: Vec<String> = all(&dir).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["good"]);
    }
}
