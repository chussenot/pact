//! Who is working in this repo? Derived, never stored: identities are read
//! back out of the two places pact already writes them — lease lock files
//! (`.pact/leases/*.lock`) and message beads. There is no agent registry to
//! keep in sync, which is the point: an agent exists exactly as long as it has
//! left a trace.
//!
//! Also the lookup half of recipient validation: `is_known` + `suggest` let
//! `msg send` warn about a recipient nobody has ever been.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::beads::BeadsCli;
use crate::{identity, lease, msg};

#[derive(Debug, Serialize)]
pub struct AgentInfo {
    pub name: String,
    pub last_seen: String, // RFC3339, most recent evidence of activity
    pub leases_held: usize,
    /// Lease events this agent produced, from `.pact/events.jsonl`. Survives
    /// release, which `leases_held` deliberately does not: one says "is working
    /// on something now", the other says "has ever worked here".
    pub lease_events: usize,
    pub messages_sent: usize,
    pub messages_received: usize,
    /// Does `name` pass `identity::validate`? `check_recipient` only enforces
    /// the grammar on the literal `--to` argument at send time — a `from`/`to`
    /// string read back from bd/br is never re-checked, so anything with write
    /// access to the shared store (a human running `bd create`, another tool,
    /// a looser pact build) can plant a name no real `pact` process could ever
    /// hold (pact-m7j.6.3). `false` here is what lets `is_known`/`suggest` and
    /// the `pact agents` table flag such an entry instead of rendering it
    /// indistinguishably from a real identity.
    pub name_valid: bool,
}

/// The operator's mailbox, reserved by the protocol block itself
/// (`pact msg send --to <peer-or-human>`). A human reads pact's output; they do
/// not run commands as `human`, so "never acted" is normal here and must not
/// read as a typo.
pub const HUMAN: &str = "human";

impl AgentInfo {
    /// Does anyone actually answer to this name? Holding a lease or sending a
    /// message means something ran pact under it. Merely *receiving* proves only
    /// that somebody typed the name — which is exactly what a typo looks like,
    /// and counting it is how one typo'd send used to certify itself forever
    /// (pact-rnc.5).
    pub fn answers(&self) -> bool {
        self.leases_held > 0
            || self.lease_events > 0
            || self.messages_sent > 0
            || self.name == HUMAN
    }

    /// Too long gone to be what somebody meant to type.
    ///
    /// This is deliberately NOT part of [`answers`]. `answers` asks "is this a
    /// real identity", and `is_known` uses it to decide whether to warn about a
    /// recipient — an agent that finished its work and exited is still real, and
    /// warning about it is the bug pact-6sx fixed. Staleness only narrows what
    /// is offered as a CORRECTION, which is a different question: not "does
    /// this name exist" but "is this plausibly the name you meant".
    ///
    /// The distinction matters because the two failures pull in opposite
    /// directions. Suggesting a ghost is how a typo spreads — one bad send
    /// registers a misspelling that then gets recommended to the next agent —
    /// and pact-4tj measured the cost of following such a suggestion: of 30
    /// messages sent to agents that had already exited, 0 were ever read.
    ///
    /// An unparsable timestamp is NOT treated as stale. The roster is derived
    /// from stamps pact wrote itself, so a stamp that will not parse is more
    /// likely clock skew on a live machine than a genuinely ancient agent, and
    /// hiding a live peer is the worse of the two mistakes.
    fn is_stale_for_suggestion(&self, now: DateTime<Utc>) -> bool {
        // `human` is a mailbox, not a process; it never "acts" and must never
        // age out of the suggestions.
        if self.name == HUMAN {
            return false;
        }
        match parse_ts(&self.last_seen) {
            Some(seen) => (now - seen).num_seconds() > SUGGESTION_HORIZON_SECS,
            None => false,
        }
    }
}

/// Identities seen holding leases or in message traffic, most-recent first.
/// `cli` is None when bd is unavailable: lease-derived agents only, never an
/// error, so `pact agents` still works without bd the way `pact lease` does.
///
/// "bd unavailable" means *cannot answer*, not just *not installed*: a bd that
/// is on PATH but has no reachable database is the common case in a fresh repo,
/// and it used to take the whole command down with it (pact-rnc.6). The lease
/// half is on disk and readable, so it is still reported.
///
/// Reads leases with `lease::peek`, not `lease::list`: asking who is active
/// must not change the answer (pact-rnc.19).
pub fn list(cli: Option<&BeadsCli>, repo_root: &Path) -> Result<Vec<AgentInfo>> {
    let mut seen: BTreeMap<String, AgentInfo> = BTreeMap::new();

    for entry in lease::peek(repo_root, true)? {
        observe(&mut seen, &entry.lease.agent, &entry.lease.acquired_at).leases_held += 1;
    }

    // History, not just live locks. Releasing a lease deletes its lock file, so
    // an agent that finished cleanly used to disappear from this roster while
    // `pact agents --for <path>` — reading the same event log — still named it.
    // Two sources of truth for "who is an agent here", and the one behind the
    // unknown-recipient warning was the one that forgets (pact-6sx).
    match crate::events::actors(repo_root) {
        Ok(actors) => {
            for (agent, at, count) in actors {
                observe(&mut seen, &agent, &at).lease_events += count;
            }
        }
        // Same rule as the message half below: the lease answer is still true.
        Err(e) => crate::output::warn(&format!("warning: lease history unavailable: {e:#}")),
    }

    if let Some(cli) = cli {
        match msg::all_messages(cli, repo_root) {
            Ok(messages) => {
                for m in messages {
                    observe(&mut seen, &m.from, &m.created_at).messages_sent += 1;
                    observe(&mut seen, &m.to, &m.created_at).messages_received += 1;
                }
            }
            // Loud but not fatal: the caller asked who is working here, and the
            // lease answer is still true.
            Err(e) => crate::output::warn(&format!("warning: message history unavailable: {e:#}")),
        }
    }

    let mut agents: Vec<AgentInfo> = seen.into_values().collect();
    sort_by_recency(&mut agents);
    Ok(agents)
}

/// Most-recently-active first; name breaks ties so output is deterministic.
fn sort_by_recency(agents: &mut [AgentInfo]) {
    agents.sort_by(|a, b| {
        parse_ts(&b.last_seen)
            .cmp(&parse_ts(&a.last_seen))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Is `name` an identity somebody answers to? Being addressed does not count:
/// counting recipients made every typo self-certifying — one send to `tuidev`
/// registered `tuidev`, and every later send to it, from any agent, was silent
/// (pact-rnc.5). `human` is known before it has any traffic at all.
///
/// `name_valid` is checked too: a name that fails `identity::validate` cannot
/// be what any `pact` process actually ran as, however much lease or message
/// traffic bd shows for it (pact-m7j.6.3).
pub fn is_known(agents: &[AgentInfo], name: &str) -> bool {
    name == HUMAN
        || agents
            .iter()
            .any(|a| a.name == name && a.answers() && a.name_valid)
}

/// Up to 3 plausible corrections for a possibly-typo'd name, best tier first:
/// same name in a different case, then prefix, then one typo away, then
/// substring. Deliberately no fuzzy-match crate — a warning does not need
/// ranked scores, it needs to catch `tuidev` for `tui-dev`.
///
/// Only names somebody answers to are offered: suggesting a ghost is how the
/// typo spreads — the incident had pact answer a send to the *correct* `alice`
/// with "did you mean alic?", the typo from the previous send.
pub fn suggest(agents: &[AgentInfo], name: &str) -> Vec<String> {
    suggest_at(agents, name, Utc::now())
}

/// How long ago an agent can have acted and still be offered as a correction.
///
/// A day: long enough to span a working session and any fleet run pact has
/// seen, short enough that a name from last month is not presented as the
/// thing you meant to type.
const SUGGESTION_HORIZON_SECS: i64 = 24 * 3600;

/// The body of [`suggest`] with the clock injected, so a test can age an agent
/// without sleeping.
fn suggest_at(agents: &[AgentInfo], name: &str, now: DateTime<Utc>) -> Vec<String> {
    let needle = name.to_lowercase();
    let mut out: Vec<String> = Vec::new();

    // `agents` is already most-recent-first, so each tier stays in that order.
    let tiers: [fn(&str, &str) -> bool; 4] = [
        |c, n| c == n,
        |c, n| c.starts_with(n) || n.starts_with(c),
        |c, n| edit_distance_is_1(c, n),
        |c, n| c.contains(n) || n.contains(c),
    ];

    for matches in tiers {
        for agent in agents
            .iter()
            // Grammar-invalid names are excluded too: a forged identity is not
            // a plausible correction for a typo (pact-m7j.6.3).
            .filter(|a| a.answers() && a.name_valid && !a.is_stale_for_suggestion(now))
        {
            if agent.name == name || out.contains(&agent.name) {
                continue; // never suggest the queried name itself
            }
            if matches(&agent.name.to_lowercase(), &needle) {
                out.push(agent.name.clone());
                if out.len() == 3 {
                    return out;
                }
            }
        }
    }
    out
}

fn observe<'a>(
    seen: &'a mut BTreeMap<String, AgentInfo>,
    name: &str,
    at: &str,
) -> &'a mut AgentInfo {
    // bd reports no creator for some beads, and an unassigned message has no
    // recipient; "" is not an identity.
    let key = if name.is_empty() { "(unknown)" } else { name };
    // Checked once, at the single point every source (lease files, event log,
    // bd/br message traffic) funnels into the roster — the same grammar
    // `check_recipient` enforces on `--to` at send time, applied here on read
    // (pact-m7j.6.3). "(unknown)" itself never matches it, which is fine: it
    // is a placeholder, not a claimed identity, and was never going to be
    // offered as one.
    let name_valid = identity::validate(key).is_ok();
    let info = seen.entry(key.to_string()).or_insert_with(|| AgentInfo {
        name: key.to_string(),
        last_seen: at.to_string(),
        leases_held: 0,
        lease_events: 0,
        messages_sent: 0,
        messages_received: 0,
        name_valid,
    });
    if parse_ts(at) > parse_ts(&info.last_seen) {
        info.last_seen = at.to_string();
    }
    info
}

/// Timestamps reach us from two writers: pact's own `to_rfc3339()` (`+00:00`)
/// and bd's (`Z`). Same instant, different bytes, so compare parsed values —
/// string ordering would call `...+00:00` older than `...Z`. Unparsable sorts
/// oldest rather than blowing up: this is an advisory listing.
fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// True when `a` and `b` are exactly one insertion, deletion, or substitution
/// apart. Cheaper and shorter than a full Levenshtein matrix, and 1 is the
/// only distance we act on.
fn edit_distance_is_1(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (long, short) = if a.len() >= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let same_len = long.len() == short.len();

    let mut i = 0;
    let mut j = 0;
    let mut diffs = 0;
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        diffs += 1;
        if diffs > 1 {
            return false;
        }
        i += 1; // skip the extra char in the longer string...
        if same_len {
            j += 1; // ...unless it's a substitution, where both advance.
        }
    }
    // A trailing extra char in `long` is the one allowed edit; identical
    // strings are distance 0, not 1.
    diffs + (long.len() - i) == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent that has acted — the only kind `is_known`/`suggest` count.
    /// The clock these fixtures live at. Pinned, not `Utc::now()`: the
    /// fixtures carry explicit `last_seen` dates, so the test asserts what
    /// `suggest` does relative to them rather than drifting stale as the real
    /// clock moves past the suggestion horizon.
    fn fixture_now() -> DateTime<Utc> {
        "2026-07-30T13:00:00Z".parse().expect("fixture clock")
    }

    fn agent(name: &str, last_seen: &str) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            last_seen: last_seen.into(),
            leases_held: 1,
            lease_events: 0,
            messages_sent: 0,
            messages_received: 0,
            name_valid: true,
        }
    }

    /// Addressed, never acted: the shape of a typo'd `--to`.
    fn ghost(name: &str, last_seen: &str) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            last_seen: last_seen.into(),
            leases_held: 0,
            lease_events: 0,
            messages_sent: 0,
            messages_received: 2,
            name_valid: true,
        }
    }

    fn find<'a>(agents: &'a [AgentInfo], name: &str) -> &'a AgentInfo {
        agents
            .iter()
            .find(|a| a.name == name)
            .expect("agent missing")
    }

    /// The union: one entry per name no matter how many sources saw it, and
    /// `last_seen` is the latest sighting from any of them.
    #[test]
    fn observe_unions_names_and_keeps_latest_timestamp() {
        let mut seen = BTreeMap::new();
        observe(&mut seen, "tui-dev", "2026-07-30T09:00:00Z").leases_held += 1;
        observe(&mut seen, "tui-dev", "2026-07-30T11:00:00+00:00").messages_sent += 1;
        observe(&mut seen, "tui-dev", "2026-07-30T10:00:00Z").messages_received += 1;
        observe(&mut seen, "human", "2026-07-30T08:00:00Z").messages_received += 1;

        assert_eq!(seen.len(), 2, "same name from 3 sources is one agent");
        let agents: Vec<AgentInfo> = seen.into_values().collect();
        let tui = find(&agents, "tui-dev");
        assert_eq!(tui.last_seen, "2026-07-30T11:00:00+00:00");
        assert_eq!(
            (tui.leases_held, tui.messages_sent, tui.messages_received),
            (1, 1, 1)
        );
        assert_eq!(find(&agents, "human").messages_received, 1);
    }

    /// Without bd we still answer "who is working here" from lease files alone.
    #[test]
    fn list_works_without_bd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        lease::acquire(root, "tui-dev", "src/tui.rs", 900, false, None).unwrap();
        lease::acquire(root, "msg-fix", "src/msg.rs", 900, false, None).unwrap();

        let agents = list(None, root).unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(find(&agents, "tui-dev").leases_held, 1);
        assert_eq!(find(&agents, "msg-fix").messages_sent, 0);
        assert!(is_known(&agents, "tui-dev"));
        assert!(!is_known(&agents, "tui-de"), "is_known is exact");
    }

    /// pact-rnc.6: bd on PATH but unable to answer must degrade to the lease
    /// half, not take the command down. A binary that does not exist stands in
    /// for "no beads database" — both are `cli.run` returning Err.
    #[test]
    fn list_degrades_when_bd_cannot_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        lease::acquire(root, "tui-dev", "src/tui.rs", 900, false, None).unwrap();

        let broken = BeadsCli {
            binary: "pact-definitely-not-bd",
        };
        let agents = list(Some(&broken), root).expect("a broken bd must not fail the listing");
        assert_eq!(agents.len(), 1);
        assert_eq!(find(&agents, "tui-dev").leases_held, 1);
    }

    /// pact-rnc.5: one typo'd send used to register the typo forever, so every
    /// later send to it was silent — fleet-wide, for every sender.
    #[test]
    fn a_pure_recipient_is_never_known() {
        let mut seen = BTreeMap::new();
        observe(&mut seen, "animator", "2026-07-30T09:00:00Z").messages_sent += 1;
        observe(&mut seen, "tuidev", "2026-07-30T09:00:00Z").messages_received += 1;
        observe(&mut seen, "tui-dev", "2026-07-30T09:00:00Z").leases_held += 1;
        observe(&mut seen, HUMAN, "2026-07-30T09:00:00Z").messages_received += 1;
        let agents: Vec<AgentInfo> = seen.into_values().collect();

        assert!(is_known(&agents, "tui-dev"), "holds a lease");
        assert!(is_known(&agents, "animator"), "has sent mail");
        assert!(
            !is_known(&agents, "tuidev"),
            "addressed once, never ran pact: still a ghost"
        );
        assert!(!find(&agents, "tuidev").answers());
        // The operator never runs pact, and the protocol says to message them.
        assert!(is_known(&agents, HUMAN));
        assert!(is_known(&[], HUMAN), "known before any traffic at all");
        // Ghosts are still listed, so `pact agents` can show them as ghosts.
        assert_eq!(agents.len(), 4);
    }

    /// pact-m7j.6.3: `observe` is the one point where `from`/`to` strings read
    /// back from bd/br enter the roster, and until now nothing there matched
    /// `check_recipient`'s grammar check on the literal `--to` at send time.
    /// A name planted straight into the shared store — by a human running `bd
    /// create`, another tool, or a looser pact build — used to fold in and
    /// render exactly like a real identity.
    #[test]
    fn a_grammar_invalid_name_is_flagged_not_treated_as_a_real_agent() {
        let mut seen = BTreeMap::new();
        // Uppercase and an underscore are both outside [a-z0-9][a-z0-9-]{1,31}.
        observe(&mut seen, "Attacker_Admin", "2026-07-30T09:00:00Z").messages_sent += 1;
        observe(&mut seen, "tui-dev", "2026-07-30T09:00:00Z").leases_held += 1;
        let agents: Vec<AgentInfo> = seen.into_values().collect();

        let bad = find(&agents, "Attacker_Admin");
        assert!(!bad.name_valid, "a grammar-invalid name must be flagged");
        assert!(
            !is_known(&agents, "Attacker_Admin"),
            "no traffic volume makes a forged identity a real one"
        );
        assert!(
            suggest_at(&agents, "attacker_adnin", fixture_now()).is_empty(),
            "must never be offered as a did-you-mean correction either"
        );
        assert!(
            find(&agents, "tui-dev").name_valid,
            "a real name is unaffected"
        );
        // Flagged, not dropped: an operator can still see the forged traffic.
        assert_eq!(agents.len(), 2);
    }

    /// The horizon itself: a name that has not acted in a day is not what you
    /// meant to type, even though it is a real agent that really did work here.
    ///
    /// `is_known` must NOT change with it — an exited agent is still a real
    /// recipient, and warning about one is the bug pact-6sx fixed. The two
    /// questions are different: "does this name exist" and "is this plausibly
    /// the name you meant".
    #[test]
    fn an_agent_gone_a_long_time_is_not_offered_but_is_still_known() {
        let agents = vec![agent("alice-prime", "2026-01-01T09:00:00Z")];
        let now = fixture_now();

        assert!(
            suggest_at(&agents, "alice-prim", now).is_empty(),
            "six months gone is not a correction"
        );
        assert!(
            is_known(&agents, "alice-prime"),
            "but it is still a real agent, and msg send must not warn about it"
        );

        // Just inside the horizon, the same name is offered again.
        let recent = vec![agent("alice-prime", "2026-07-30T09:00:00Z")];
        assert_eq!(suggest_at(&recent, "alice-prim", now), ["alice-prime"]);
    }

    /// A stamp that will not parse must not hide a live peer: the roster is
    /// derived from stamps pact wrote itself, so garbage is likelier to be clock
    /// skew than genuine age.
    #[test]
    fn an_unparsable_last_seen_is_not_treated_as_stale() {
        let agents = vec![agent("alice-prime", "not-a-timestamp")];
        assert_eq!(
            suggest_at(&agents, "alice-prim", fixture_now()),
            ["alice-prime"]
        );
    }

    /// Offering the previous send's typo as the correction for the right name is
    /// how the typo spreads, so a ghost is never a suggestion.
    #[test]
    fn ghosts_are_not_offered_as_corrections() {
        let agents = vec![
            ghost("alic", "2026-07-30T09:00:00Z"),
            agent("bob", "2026-07-30T09:00:00Z"),
        ];
        assert!(
            suggest_at(&agents, "alice", fixture_now()).is_empty(),
            "alic is a ghost"
        );
        assert_eq!(suggest_at(&agents, "bo", fixture_now()), ["bob"]);
    }

    #[test]
    fn list_sorts_most_recent_first() {
        let agents = vec![
            agent("old", "2026-07-30T08:00:00Z"),
            agent("newest", "2026-07-30T12:00:00Z"),
            agent("middle", "2026-07-30T10:00:00+00:00"),
        ];
        let mut sorted = agents;
        sort_by_recency(&mut sorted);
        let names: Vec<&str> = sorted.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["newest", "middle", "old"]);
    }

    #[test]
    fn suggest_catches_a_plausible_typo() {
        let agents = vec![
            agent("tui-dev", "2026-07-30T09:00:00Z"),
            agent("msg-fix", "2026-07-30T09:00:00Z"),
            agent("human", "2026-07-30T09:00:00Z"),
        ];
        // The real incident: one deleted character, message sent into the void.
        assert_eq!(suggest_at(&agents, "tuidev", fixture_now()), ["tui-dev"]);
        assert_eq!(suggest_at(&agents, "TUI-DEV", fixture_now()), ["tui-dev"]);
        assert_eq!(suggest_at(&agents, "tui", fixture_now()), ["tui-dev"]);
        // "reviewer" resembles nobody who has ever run a pact command here.
        assert!(suggest_at(&agents, "reviewer", fixture_now()).is_empty());
    }

    #[test]
    fn suggest_never_returns_the_queried_name_and_caps_at_three() {
        let agents = vec![
            agent("dev", "2026-07-30T09:00:00Z"),
            agent("dev-a", "2026-07-30T09:00:00Z"),
            agent("dev-b", "2026-07-30T09:00:00Z"),
            agent("dev-c", "2026-07-30T09:00:00Z"),
        ];
        let out = suggest_at(&agents, "dev", fixture_now());
        assert_eq!(out.len(), 3);
        assert!(!out.contains(&"dev".to_string()));
    }

    #[test]
    fn edit_distance_is_1_only_for_one_edit() {
        assert!(edit_distance_is_1("tui-dev", "tuidev")); // deletion
        assert!(edit_distance_is_1("tuidev", "tui-dev")); // insertion
        assert!(edit_distance_is_1("cli-wire", "cli-wore")); // substitution
        assert!(edit_distance_is_1("agents", "agent")); // trailing deletion
        assert!(!edit_distance_is_1("agents", "agents")); // distance 0
        assert!(!edit_distance_is_1("agents", "agnets")); // transposition = 2
        assert!(!edit_distance_is_1("human", "reviewer"));
    }
}
