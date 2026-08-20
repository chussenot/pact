//! `pact lease`: the advisory claim, and everything printed about one.
//!
//! `acquire`, `renew`, `release`, `sweep` and `ls` share this file because they
//! share a subject — who holds what, and what pact says about it — and because
//! `acquire` alone reaches for four of the helpers below. The race argument
//! itself lives one layer down in `crate::lease`; this is the surface over it.

use anyhow::Result;
use std::path::Path;

use crate::cli::util::{age_of, one_line, table};
use crate::cli::{LeaseAction, USAGE_ERROR};
use crate::lease::human_secs;
use crate::{events, identity, lease, msg, output, repo};

/// The outcome of checking one leased path for pending messages about it.
///
/// pact-m7j.10.3/10.4: a plain `Vec<String>` of findings could not tell "we
/// checked and it's clean" apart from "we could not check at all" — both
/// rendered as zero lines, so a `bd` that was down, absent, or timed out
/// looked exactly like a healthy path with nothing to report. Worse, in a
/// batch acquire a genuine failure on one path rendered identically to a
/// genuine clean result on another, so the one visible finding made the whole
/// mechanism look like it had worked. This type keeps the three states apart
/// all the way to the terminal.
#[derive(Debug)]
enum MessageCheck {
    /// Checked, nothing unread. The common case, and printed as nothing —
    /// see `messages_about`'s own note on that.
    Clean,
    /// Checked, found something: the warning line to print.
    Found(String),
    /// Could not check: the warning line to print, distinct from both of the
    /// above. Must never affect `lease acquire`'s exit code — the lease
    /// already succeeded, and acquiring one must not start depending on the
    /// messaging backend, which `lease` has never needed.
    Failed(String),
}

/// Unread messages about the paths being leased, surfaced to whoever is taking
/// them over.
///
/// `--to-owner-of` addressed a file and then resolved it to an agent name, and
/// delivery stopped there. Over one fleet run, 30 of 44 agent-to-agent messages
/// went to agents who had already exited and none were ever read, while every
/// message to a live agent WAS read: addressing was never the failure,
/// deliverability was. Every one of the 30 was about a file, sent to the agent
/// who had just released it — so the moment someone leases that file is exactly
/// the moment the message becomes useful again (pact-4tj).
///
/// One [`MessageCheck`] per path, same order as `paths`.
///
/// **No `.beads/` gate, and no way left to fail.** This used to skip the check
/// entirely unless `.beads/` existed, because messages were beads: a repo that had
/// never run `bd init` could not have any, and surfacing "could not check for pending
/// messages" on every acquire forever would have been exactly the noise AGENTS.md's
/// own messaging discipline warns against. It also had to report a whole-call failure
/// when no backend could be located.
///
/// Messages are `.pact/messages.jsonl` now. A repository that has never seen the
/// issue tracker can still have mail waiting on a path, and there is no subprocess
/// left to be missing — so the gate is gone and [`MessageCheck::Failed`] is reachable
/// only from a genuinely unreadable store.
fn messages_about(root: &Path, paths: &[String], agent: &str) -> Vec<MessageCheck> {
    // No `.beads` gate any more: that guard existed because a repo with no Beads
    // store could hold no messages. Messages are pact's own file now, so a repo
    // that has never seen the issue tracker can still have mail waiting on a path.
    paths
        .iter()
        .map(|path| check_one_path(msg::about_path(root, path), path, agent))
        .collect()
}

/// One path's [`MessageCheck`], from its own `about_path` result — split out
/// of [`messages_about`] so this per-path resolution (an `Ok`/`Err` from ONE
/// path must never affect a SIBLING path's result, pact-m7j.10.4) is testable
/// directly against synthetic results, without spawning a Beads CLI for two
/// paths and needing a way to make exactly one of two identical-shaped
/// subprocess calls fail.
fn check_one_path(result: Result<Vec<msg::Message>>, path: &str, agent: &str) -> MessageCheck {
    match result {
        Ok(msgs) => {
            let mut notices = 0usize;
            let waiting: Vec<msg::Message> = msgs
                .into_iter()
                // Yours already, or already read by you: not news.
                .filter(|m| m.from != agent && !m.read_by.iter().any(|r| r == agent))
                // Watch notices are counted, not quoted. This line is the last
                // thing an agent reads before it starts editing, and in crucible
                // it said "32 unread message(s) about src/ast.rs, oldest from
                // agent-01 — 'src/ast.rs changed — released by agent-01'": a
                // number dominated by this file's own release fanout, quoting a
                // superseded diff as the thing to read first. The count of
                // authored messages is the actionable number; the notices are a
                // separate clause (pact-mqw.5/pact-mqw.7).
                .filter(|m| {
                    if m.notice {
                        notices += 1;
                    }
                    !m.notice
                })
                .collect();
            let tail = if notices > 0 {
                format!(
                    " ({notices} watch notice(s) on this path too — \
                     `pact msg inbox --watch-only`)"
                )
            } else {
                String::new()
            };
            match waiting.first() {
                Some(first) => MessageCheck::Found(format!(
                    "note: {} unread message(s) about {path}, oldest from {} — \"{}\". \
                     Read it before you edit: `pact msg read {}`{tail}",
                    waiting.len(),
                    first.from,
                    first.subject.as_deref().unwrap_or("(no subject)"),
                    first.id
                )),
                // Notices alone do not make an acquire noisy, but staying
                // completely silent about them would hide the diff of what the
                // last holder did to the file being claimed — which is the one
                // moment it is most worth knowing.
                None if notices > 0 => MessageCheck::Found(format!(
                    "note: no unread messages about {path}, but {notices} watch notice(s) \
                     — `pact msg inbox --watch-only` shows what changed under you"
                )),
                None => MessageCheck::Clean,
            }
        }
        Err(e) => MessageCheck::Failed(format!(
            "note: could not check for pending messages about {path}: {e:#}"
        )),
    }
}

/// Advisory lines for paths someone else worked on recently.
///
/// A lease says nothing about a path once it is released, so `acquire` on a
/// file another agent finished with three minutes ago printed the same
/// four-word success line it prints for an untouched file. That is how one
/// word-fix in a nine-agent run got routed to the same agent by three peers and
/// was then nearly applied a second time, with worse wording, by an agent whose
/// `acquire` told it nothing (pact-o38).
///
/// Advisory only: it never blocks and never changes the exit code. The point is
/// that you find out before you edit, not that pact decides for you.
fn prior_owners(root: &Path, paths: &[String], agent: &str) -> Vec<String> {
    paths
        .iter()
        .filter_map(|p| {
            // Normalized before the lookup, not after — pact-m7j.8.6: `p` is
            // this command's own raw CLI argument, and `events::owner_of` does
            // a plain string comparison against whatever `acquire` itself
            // logged (already normalized). Without this, `acquire foo.rs` from
            // a subdirectory found no prior owner for a file `acquire
            // src/foo.rs` from the root had just released. The suggested
            // `--to-owner-of` command below also needs the canonical spelling:
            // it will be typed back verbatim, quite possibly from a third CWD.
            let relative = lease::normalize_path(root, p);
            let owner = events::owner_of(root, &relative).ok().flatten()?;
            // Your own history is not news.
            if owner.agent == agent {
                return None;
            }
            let ago = age_of(&owner.at)
                .map(|s| format!("{} ago", human_secs(s)))
                .unwrap_or_else(|| owner.at.clone());
            let note = owner
                .detail
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| format!(" — their note: {d}"))
                .unwrap_or_default();
            Some(format!(
                "note: {relative} was last {} by {} ({ago}){note}. `pact log` has the history; \
                 `pact msg send --to-owner-of {relative}` reaches them.",
                owner.kind, owner.agent
            ))
        })
        .collect()
}

/// How long a swept hold's holder had been gone, in whichever terms the log
/// can actually support — the TTL it outlived, or the silence since its last
/// event. Never both, because saying "lapsed 3m ago, silent 40m" invites the
/// reader to work out which one the decision rested on.
fn describe_absence(e: &lease::Swept) -> String {
    match (e.past_ttl_secs, e.holder_silent_secs) {
        (Some(past), _) => format!("lapsed {} ago", human_secs(past)),
        (None, Some(silent)) => format!("silent {}", human_secs(silent)),
        (None, None) => "never seen in the event log".to_string(),
    }
}

pub(in crate::cli) fn run_lease(
    cwd: &Path,
    agent_flag: Option<&str>,
    json: bool,
    action: LeaseAction,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    match action {
        LeaseAction::Acquire {
            paths,
            ttl,
            steal,
            note,
            bead,
            wait,
        } => {
            let agent = identity::resolve_agent(agent_flag)?;
            // Parsed here rather than in a clap `value_parser` because a bare
            // small value must WARN and still succeed, and a parser that writes
            // to stderr would also fire while clap renders the default in
            // `--help`.
            // A bad `--ttl` stays exit 5: clap raised it as an invalid value
            // before the grammar moved out of clap, and the exit code is API.
            let ttl = lease::parse_ttl(&ttl)
                .map_err(|e| output::exit_with(USAGE_ERROR, e.to_string()))?;
            // Look up prior owners BEFORE acquiring: the acquire appends its own
            // event, which would make the caller the answer to its own question.
            let prior = prior_owners(&root, &paths, &agent);
            // No bead-claim cross-check here any more (pact-as5.5). It resolved
            // the bead in the note and asked `bd show` who owned it — one
            // subprocess on the hot path of the single most frequently run pact
            // command, to answer a question `pact audit --check
            // claim-lease-divergence` answers offline from the committed export.
            // Moving it to that export instead of deleting it was the other
            // option and measured worthless: over this repository's whole event
            // log, 100 acquire notes named a bead, 8 resolved through the export,
            // and all 8 were acquired by the agent it names — zero warnings, for
            // a file read per acquire. See beads::interaction_assignees.
            let note = with_bead(bead.as_deref(), note);
            let wait_secs = wait
                .as_deref()
                .map(lease::parse_ttl)
                .transpose()
                .map_err(|e| output::exit_with(USAGE_ERROR, e.to_string()))?;
            let mut outcomes = acquire_waiting(&root, &agent, &paths, ttl, steal, note, wait_secs)?;
            // One path renders and serializes exactly as it always did — a
            // script doing `lease acquire f --json | jq .lease.path` must not
            // start getting an array back because the command learned to batch.
            if outcomes.len() == 1 {
                let outcome = outcomes.pop().expect("len == 1");
                output::emit(json, &outcome, |o: &lease::AcquireOutcome| {
                    format!(
                        "{} lease on {} for {}",
                        acquire_verb(o),
                        o.lease.path,
                        o.lease.agent
                    )
                });
            } else {
                output::emit(json, &outcomes, |os: &Vec<lease::AcquireOutcome>| {
                    format!(
                        "took {} lease(s) for {agent}:\n{}",
                        os.len(),
                        os.iter()
                            .map(|o| format!("  {} {}", acquire_verb(o), o.lease.path))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                });
            }
            // After the success line, never before it: what happened first,
            // then what you should know.
            for line in prior {
                output::warn(&line);
            }
            for check in messages_about(&root, &paths, &agent) {
                match check {
                    MessageCheck::Clean => {}
                    MessageCheck::Found(line) | MessageCheck::Failed(line) => {
                        output::warn(&line);
                    }
                }
            }
            Ok(())
        }
        LeaseAction::Renew { path } => {
            let agent = identity::resolve_agent(agent_flag)?;
            let renewed = lease::renew(&root, &agent, &path)?;
            output::emit(json, &renewed, |l: &lease::LeaseInfo| {
                format!(
                    "renewed lease on {} for {} ({} ttl)",
                    l.path,
                    l.agent,
                    human_secs(l.ttl_secs as i64)
                )
            });
            Ok(())
        }
        LeaseAction::Release { path, force, all } => {
            let agent = identity::resolve_agent(agent_flag)?;
            if all {
                let released = lease::release_all(&root, &agent)?;
                // The scope, resolved once, so an empty result can say WHERE it looked
                // (finding 2). "held no leases" is a confident negative that an agent
                // acts on by exiting — it printed that while `lease ls` showed the
                // leases in the same second, and 45 minutes of TTL followed. A negative
                // an agent stakes its exit on has to name the store it searched.
                let scope = repo::RepoContext::resolve(&root);
                output::emit(json, &released, |paths: &Vec<String>| {
                    if paths.is_empty() {
                        format!(
                            "{agent} holds no leases in {} \n\
                             (searched every lease in this store, not just this \
                             directory — `pact lease ls` lists what is there)",
                            scope.state_dir.display()
                        )
                    } else {
                        format!(
                            "released {} lease(s):\n{}",
                            paths.len(),
                            paths
                                .iter()
                                .map(|p| format!("  {p}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    }
                });
                return Ok(());
            }
            // Not all-or-nothing, unlike `acquire`: a half-held set of leases
            // is useless, but a half-released one is strictly better than none,
            // and `release` is what an agent runs on its way out. So every path
            // is attempted and the first refusal decides the exit code rather
            // than aborting the rest (pact-mqw.7).
            let mut released: Vec<Released> = Vec::new();
            let mut refusal: Option<anyhow::Error> = None;
            for p in &path {
                match lease::release(&root, &agent, p, force) {
                    Ok(outcome) => {
                        if let Some(who) = outcome.displaced() {
                            // pact-rnc.11: overriding someone else's claim is
                            // loud in the `acquire --steal` direction; make it
                            // loud here too.
                            output::warn(&format!(
                                "warning: force-released {p} — destroyed {who}'s live claim; \
                                 they were not notified (`pact msg send --to {who}`)"
                            ));
                        }
                        released.push(Released::from(p.clone(), &outcome));
                    }
                    // Kept, not returned: the paths after this one still deserve
                    // their attempt. Re-raised below once every path has had it.
                    Err(e) => {
                        output::warn(&format!("warning: {e:#}"));
                        refusal = refusal.or(Some(e));
                    }
                }
            }
            // pact-rnc.25: the displaced holder used to exist only in stderr
            // prose, so `--json` callers — the scripted ones, the ones that most
            // need to go apologise — could not see whose claim they destroyed.
            //
            // One path stays an OBJECT and many are an ARRAY, matching
            // `acquire`'s pinned convention (pact-er0) rather than inventing a
            // second one.
            //
            // A refusal anywhere suppresses the payload and returns the error
            // instead, so `--json` stays exactly one document — the shape a
            // refused single-path release has always had. The refused paths are
            // named on stderr above and in the error itself; what did succeed is
            // visible in `pact log` and `lease ls`.
            if let Some(e) = refusal {
                return Err(e);
            }
            if path.len() == 1 {
                let one = released.remove(0);
                output::emit(json, &one, |r: &Released| r.line());
            } else {
                output::emit(json, &released, |rs: &Vec<Released>| {
                    rs.iter().map(Released::line).collect::<Vec<_>>().join("\n")
                });
            }
            Ok(())
        }
        LeaseAction::Sweep { path, suspect } => {
            let agent = identity::resolve_agent(agent_flag)?;
            let mode = if suspect {
                lease::Sweep::Suspect
            } else {
                lease::Sweep::Expired
            };
            let swept = lease::sweep(&root, &agent, mode, &path)?;
            output::emit(json, &swept, |s: &Vec<lease::Swept>| {
                let (taken, left): (Vec<_>, Vec<_>) = s.iter().partition(|e| e.reclaimed);
                if taken.is_empty() && left.is_empty() {
                    return "nothing to sweep — no lease here is held by an absent agent".into();
                }
                let mut out = Vec::new();
                for e in &taken {
                    out.push(format!(
                        "reclaimed {} from {} ({})",
                        e.path,
                        e.holder,
                        describe_absence(e)
                    ));
                }
                // Named, not merely counted: an agent that swept nothing needs
                // to know whether that is because nothing was abandoned or
                // because what it wanted is still somebody's.
                for e in &left {
                    out.push(format!(
                        "left {} alone — {} still looks alive ({})",
                        e.path,
                        e.holder,
                        describe_absence(e)
                    ));
                }
                if !taken.is_empty() {
                    out.push(String::new());
                    out.push(
                        "Recorded as `reclaimed`, not `stolen`: the audit can tell this \
                         from trampling a live peer."
                            .into(),
                    );
                }
                out.join("\n")
            });
            Ok(())
        }
        LeaseAction::Ls { all } => {
            let entries = lease::list(&root, all)?;
            // `--all` means "show me everything pact knows about paths", and
            // until now a released path vanished completely: `src/doctor.rs`
            // blocked two agents in sequence because nothing distinguished it
            // from a file nobody had ever opened (pact-o38).
            //
            // Human output only, deliberately. `lease ls --json` is an array of
            // LeaseEntry, a shape pact-er0 just pinned, and a released path has
            // no lock file to describe — synthesizing a LeaseEntry with an
            // invented ttl would be a lie in a typed field. `pact agents --for
            // <path>` is the scriptable answer.
            let released = if all {
                released_paths(&root, &entries)
            } else {
                Vec::new()
            };
            output::emit(json, &entries, |entries: &Vec<lease::LeaseEntry>| {
                let mut out = render_leases(entries);
                if !released.is_empty() {
                    out.push_str("\n\nrecently released (no lease held, last owner known):\n");
                    out.push_str(&released.join("\n"));
                }
                out
            });
            Ok(())
        }
    }
}

/// pact-rnc.10: lead with age and an explicit state. `remaining_secs` is a
/// crash-recovery ceiling, not a duration of work — printing it next to a
/// seconds-old lease read as "this long lease" and got a live agent's claim
/// force-released, so it only appears when it says something actionable. The
/// note comes along because "what is this agent doing" is the question an
/// operator is actually asking before they reach for --force.
/// Paths the event log remembers that have no lock on disk right now.
/// `--bead <id>` folded into the note, which is where every consumer already
/// looks (pact-x16.6).
///
/// **Not a seventh parameter through the lease core, and that is a measured
/// decision rather than a shortcut.** Threading a `bead` argument through
/// `acquire`, `acquire_inner`, `acquire_many` and their two `Store` impls is 67
/// call sites; putting the id at the front of the note reaches the same
/// destinations by the route fleets already use. `log_event` parses it back out
/// into `Event::bead`, so the lease event carries it as its own structured field
/// (pact-6hv), and `lease ls`, `pact log` and `agents --for` show it exactly as
/// they show a hand-written one — which is what the bead asked for.
///
/// So `--bead x-4xh --note "split the audit table"` and
/// `--note "x-4xh: split the audit table"` are the same lease. Two independent
/// fleets typed the second by hand, across 83 and 26 claims, without enforcement.
/// The flag does not replace that convention; it makes it un-misspellable.
///
/// If the note ALREADY begins with a bead id, the flag still wins:
/// `bead_id_in` takes the first match, and an explicit argument outranks prose.
fn with_bead(bead: Option<&str>, note: Option<String>) -> Option<String> {
    // `let bead = bead?` here would return None from the FUNCTION and silently
    // discard the note whenever `--bead` was absent — which is every lease any
    // existing fleet takes. Caught by the equivalence test below, and worth the
    // comment because the `?` reads correct.
    let Some(bead) = bead.map(str::trim).filter(|b| !b.is_empty()) else {
        return note;
    };
    Some(match note {
        Some(note) if !note.trim().is_empty() => format!("{bead}: {}", note.trim()),
        // A bare `--bead` with no note is still an announcement — it says which
        // work this hold is for, which is more than most notes manage.
        _ => bead.to_string(),
    })
}

/// The lease-hot path of `--wait`: retry inside THIS process until the paths are
/// free or the budget runs out (pact-x16.2).
///
/// **Waiting has to happen inside a tool call, because a subagent's process is
/// its turn loop.** pact's advice on exit 2 was "subscribe with `pact watch add`
/// and pick up other ready work"; for an agent that is a Claude Code Task — which
/// most fleet agents are — ending the turn to wait for that notification is
/// operationally identical to exiting, and neither `pact watch` nor any
/// harness-level monitor can re-enter a subagent that has stopped producing
/// turns. Measured on one 12-agent run: seven agents parked on that advice, four
/// never resumed at all, and the three that did resumed within fourteen seconds
/// of each other nine hours later, when a human woke the parent session. One of
/// them was holding four finished, tested, committed fixes.
///
/// So this is not the polling the protocol warns against — that is an AGENT
/// spending a turn per attempt, which is what produced `--check retry-storm`.
/// This spends no turns at all: one tool call that returns when the path is
/// free.
///
/// The sleep is sized from the holder's OWN remaining time, read with
/// `lease::peek` (which does not garbage-collect, so asking cannot change the
/// answer), and capped so a holder who releases early is not waited out. A lock
/// that cannot be read falls back to the cap rather than to a tight loop.
fn acquire_waiting(
    root: &Path,
    agent: &str,
    paths: &[String],
    ttl: u64,
    steal: bool,
    note: Option<String>,
    wait_secs: Option<u64>,
) -> Result<Vec<lease::AcquireOutcome>> {
    let Some(budget) = wait_secs else {
        return lease::acquire_many(root, agent, paths, ttl, steal, note);
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(budget);
    let mut announced = false;
    let mut attempt = 0u32;
    loop {
        let err = match lease::acquire_many(root, agent, paths, ttl, steal, note.clone()) {
            Ok(outcomes) => return Ok(outcomes),
            // Only a HELD path is worth waiting through. Every other failure —
            // a usage error, an unreadable lock, no git repo — is not going to
            // become true by waiting, and retrying it would turn a clear error
            // into a slow one.
            Err(e) if output::code_for(&e) == 2 => e,
            Err(e) => return Err(e),
        };
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            // The refusal the last attempt produced, unchanged: the exit code
            // stays 2 and the message still names the holder, so a caller that
            // branched on either before `--wait` existed still works.
            return Err(err);
        }
        if !announced {
            output::warn(&format!(
                "note: waiting up to {} for {}; this command is holding the wait so your \
                 turn does not have to end — it returns as soon as the path is free",
                lease::human_secs(budget as i64),
                paths.join(", ")
            ));
            announced = true;
        }
        std::thread::sleep(next_poll(root, paths, attempt).min(left));
        attempt += 1;
    }
}

/// How long to sleep before attempt `attempt`: gentle backoff, never past the
/// point where the hold being waited on could legally be reclaimed.
///
/// **Backoff rather than "sleep until their TTL expires", and the difference is
/// the whole responsiveness of `--wait`.** A holder's remaining TTL is its worst
/// case, not its expected one: pact's own history puts the median file hold at
/// 14 minutes against a 45-minute default and the median merge hold at 37
/// seconds against 30 minutes. A first cut here slept for the holder's remaining
/// time and took 30 seconds to notice a release that happened at 6 — correct,
/// and useless, because releasing early is the common case rather than the
/// exception.
///
/// So: start short, grow, and cap. The floor stops a lock that lapsed a moment
/// ago from spinning; the ceiling bounds how long a release can go unnoticed.
/// The holder's own remaining time is an upper bound on top of that, so the last
/// sleep before a lease becomes reclaimable lands on it rather than overshooting.
///
/// Read with `lease::peek`, which does not garbage-collect — asking who holds a
/// path must not change the answer. An unreadable lock yields the ceiling rather
/// than a tight loop.
fn next_poll(root: &Path, paths: &[String], attempt: u32) -> std::time::Duration {
    const FLOOR_SECS: u64 = 1;
    const CEILING_SECS: u64 = 15;
    let backoff = FLOOR_SECS
        .saturating_mul(1u64 << attempt.min(6))
        .min(CEILING_SECS);
    let soonest = lease::peek(root, true)
        .unwrap_or_default()
        .into_iter()
        // Normalized on both sides, because the caller spells a path however it
        // is standing — `./src/a.rs` from a subdirectory is the lock the holder
        // took as `src/a.rs`, and matching the raw strings would wait on nothing
        // and poll at the floor.
        .filter(|e| {
            paths
                .iter()
                .any(|p| lease::normalize_path(root, p) == e.lease.path)
        })
        // Plus the grace period, because that is when the lease actually becomes
        // reclaimable — waking exactly at the TTL would find it still held and
        // spend an attempt learning that.
        .map(|e| (e.remaining_secs + lease::GRACE_SECS).max(0) as u64)
        .min()
        .unwrap_or(CEILING_SECS);
    std::time::Duration::from_secs(backoff.min(soonest.max(FLOOR_SECS)))
}

fn released_paths(root: &Path, held: &[lease::LeaseEntry]) -> Vec<String> {
    events::owners(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|(path, _)| !held.iter().any(|e| e.lease.path == *path))
        .map(|(path, owner)| {
            let ago = age_of(&owner.at)
                .map(|s| format!("{} ago", human_secs(s)))
                .unwrap_or_else(|| owner.at.clone());
            format!("  {path}  {} by {} ({ago})", owner.kind, owner.agent)
        })
        .collect()
}

fn render_leases(entries: &[lease::LeaseEntry]) -> String {
    if entries.is_empty() {
        return "no active leases".to_string();
    }
    // The WHERE column appears only when at least one lease has somewhere to
    // report, which in practice means the repository uses worktrees. A repo that
    // does not gets byte-identical output to before — the same reason the two
    // fields are omitted from the lock file rather than written as null.
    let located = entries
        .iter()
        .any(|e| e.lease.worktree.is_some() || e.lease.branch.is_some());
    // VIA follows the same rule as WHERE, for the same reason (pact-c3y): a fleet
    // where nobody declares anything gets byte-identical output to before, rather
    // than a column of blanks inviting the reader to wonder what broke.
    let attributed = entries
        .iter()
        .any(|e| e.lease.harness.is_some() || e.lease.model.is_some());
    let mut header = vec![
        "PATH".to_string(),
        "AGENT".to_string(),
        "HELD".to_string(),
        "STATE".to_string(),
    ];
    if attributed {
        header.push("VIA".to_string());
    }
    if located {
        header.push("WHERE".to_string());
    }
    header.push("NOTE".to_string());
    let mut rows = vec![header];
    rows.extend(entries.iter().map(|e| {
        let mut row = vec![
            e.lease.path.clone(),
            e.lease.agent.clone(),
            human_secs(e.age_secs),
            e.state_label(),
        ];
        if attributed {
            row.push(lease_attribution(&e.lease));
        }
        if located {
            row.push(lease_location(&e.lease));
        }
        row.push(one_line(e.lease.note.as_deref().unwrap_or(""), 60));
        row
    }));
    table(&rows)
}

/// `claude-code / sonnet-4-6` for the VIA column.
///
/// The model is a DECLARATION, not a measurement — pact reads `PACT_MODEL` and
/// records what it is told (see [`crate::harness::model`]). It is rendered
/// unadorned here and marked in the TUI, where there is room; the honest
/// affordance in a fixed-width table is that this column is called VIA rather
/// than MODEL, so nothing claims the model was observed.
fn lease_attribution(lease: &lease::LeaseInfo) -> String {
    match (lease.harness.as_deref(), lease.model.as_deref()) {
        (Some(h), Some(m)) => format!("{h} / {m}"),
        (Some(h), None) => h.to_string(),
        (None, Some(m)) => format!("/ {m}"),
        (None, None) => String::new(),
    }
}

/// `branch @ worktree` for the WHERE column, as compactly as the pair allows.
fn lease_location(lease: &lease::LeaseInfo) -> String {
    match (lease.branch.as_deref(), lease.worktree.as_deref()) {
        (Some(b), Some(w)) => format!("{b} @ {w}"),
        (Some(b), None) => b.to_string(),
        (None, Some(w)) => format!("@ {w}"),
        (None, None) => String::new(),
    }
}

/// Whether a lease was taken from a live holder or simply claimed. The single
/// path wording is unchanged from before batching, so scripts and eyes that
/// grew up on "acquired lease on X for Y" still read it.
fn acquire_verb(o: &lease::AcquireOutcome) -> &'static str {
    if o.stolen {
        "stolen"
    } else {
        "acquired"
    }
}

/// `lease release --json`: the path, whoever's live claim `--force` destroyed
/// (pact-rnc.25), and which of the four outcomes this was (pact-mqw.7).
#[derive(serde::Serialize)]
struct Released {
    path: String,
    displaced: Option<String>,
    /// `released` | `force-released` | `already-expired` | `nothing-held`. A
    /// flat string rather than a tagged enum so a `jq` one-liner can branch on
    /// it without knowing serde's representation.
    outcome: &'static str,
    /// When the lease lapsed, for `already-expired`.
    #[serde(skip_serializing_if = "Option::is_none")]
    expired_at: Option<String>,
    /// How far past its TTL the holder ran — set whenever it overran, whether or
    /// not the lock survived long enough to be released.
    #[serde(skip_serializing_if = "Option::is_none")]
    past_ttl_secs: Option<i64>,
}

impl Released {
    fn from(path: String, outcome: &lease::ReleaseOutcome) -> Self {
        match outcome {
            lease::ReleaseOutcome::Released { past_ttl_secs } => Released {
                path,
                displaced: None,
                outcome: "released",
                expired_at: None,
                past_ttl_secs: *past_ttl_secs,
            },
            lease::ReleaseOutcome::ForceReleased { displaced } => Released {
                path,
                displaced: Some(displaced.clone()),
                outcome: "force-released",
                expired_at: None,
                past_ttl_secs: None,
            },
            lease::ReleaseOutcome::AlreadyExpired { at, since_secs, .. } => Released {
                path,
                displaced: None,
                outcome: "already-expired",
                expired_at: Some(at.clone()),
                past_ttl_secs: *since_secs,
            },
            lease::ReleaseOutcome::NothingHeld => Released {
                path,
                displaced: None,
                outcome: "nothing-held",
                expired_at: None,
                past_ttl_secs: None,
            },
        }
    }

    /// The line an agent reads. "released lease on X" is unchanged for the case
    /// that really is a release; the other three say what actually happened,
    /// because `release` is where an agent checks its own compliance and a
    /// uniform success line let it conclude it had complied when it had not.
    fn line(&self) -> String {
        match self.outcome {
            "released" => match self.past_ttl_secs {
                // Released, but late. Nobody reclaimed in the window, which is
                // luck rather than compliance, so say so where the agent will
                // read it.
                Some(over) => format!(
                    "released lease on {} — WARNING: {} past its ttl; the path was \
                     reclaimable by any peer for that long. Renew next time, or take a longer --ttl",
                    self.path,
                    human_secs(over)
                ),
                None => format!("released lease on {}", self.path),
            },
            "force-released" => format!("force-released lease on {}", self.path),
            "already-expired" => format!(
                "nothing to release on {} — your lease had already lapsed at {}{} and its lock was \
                 collected. The path was free for that window and any peer could have taken it: \
                 commit BEFORE the ttl runs out, or `pact lease renew` while you work",
                self.path,
                self.expired_at.as_deref().unwrap_or("(unknown)"),
                self.past_ttl_secs
                    .map(|s| format!(" ({} ago)", human_secs(s)))
                    .unwrap_or_default(),
            ),
            _ => format!(
                "nothing to release on {} — no lock held here and no expiry of yours on record",
                self.path
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::message;
    use super::*;

    fn lease_entry(
        path: &str,
        agent: &str,
        age: i64,
        remaining: i64,
        expired: bool,
    ) -> lease::LeaseEntry {
        lease::LeaseEntry {
            lease: lease::LeaseInfo {
                agent: agent.to_string(),
                path: path.to_string(),
                acquired_at: "2026-07-31T09:00:00+00:00".to_string(),
                ttl_secs: 900,
                note: Some("wiring the CLI".to_string()),
                branch: None,
                worktree: None,
                invoked_from: None,

                content_hash: None,
                harness: None,
                model: None,
                extra: Default::default(),
            },
            age_secs: age,
            remaining_secs: remaining,
            expired,
            holder_silent_secs: None,
            suspect: false,
        }
    }

    /// pact-m7j.10.4: a failed check on one path must render distinctly from
    /// both a genuine finding and a genuine clean result on a sibling path in
    /// the same batch acquire — before this fix, `about_path`'s `Err` and its
    /// `Ok(vec![])` were both a bare `.ok()?` inside one `filter_map`,
    /// indistinguishable from each other and from a clean path.
    ///
    /// Exercised directly against [`check_one_path`], the exact function
    /// `messages_about` calls once per path, rather than through two real
    /// subprocess calls: `about_path`'s argv is byte-identical for every path
    /// in a batch (filtering is client-side — see its own doc comment), so
    /// the only way to fail exactly one of two real calls is to target it by
    /// call order, and this sandbox's nested child processes do not reliably
    /// share file-based state across sibling invocations for that to key off
    /// of. The property under test — one path's `Result` cannot leak into
    /// another's — is a fact about this function's signature and match arms,
    /// not about process timing, so testing it here is not a downgrade.
    #[test]
    fn a_failed_check_on_one_path_renders_distinctly_from_a_clean_or_found_sibling() {
        let found = check_one_path(
            Ok(vec![message("m1", "peer", "flush the buffer", false)]),
            "patha",
            "checker",
        );
        assert!(
            matches!(found, MessageCheck::Found(ref s) if s.contains("patha") && s.contains("peer")),
            "expected a Found naming the path and sender"
        );

        let clean = check_one_path(Ok(vec![]), "pathb", "checker");
        assert!(matches!(clean, MessageCheck::Clean));

        let failed = check_one_path(
            Err(anyhow::anyhow!("boom: injected transient failure")),
            "pathc",
            "checker",
        );
        match failed {
            MessageCheck::Failed(line) => {
                assert!(line.contains("pathc"), "{line}");
                assert!(line.contains("boom"), "{line}");
            }
            other => panic!("expected Failed, got a variant that is not it: {other:?}"),
        }
    }

    /// The pact-rnc.10 incident verbatim: an 80s-old healthy lease must not
    /// render a four-digit countdown that reads as "this long lease".
    #[test]
    fn lease_ls_leads_with_age_and_state_not_remaining_ttl() {
        let out = render_leases(&[lease_entry("docs/m.md", "animator", 80, 3520, false)]);
        assert!(out.contains("1m20s"), "{out}");
        assert!(out.contains("active"), "{out}");
        assert!(!out.contains("3520"), "remaining ttl must not lead: {out}");
        assert!(
            out.contains("wiring the CLI"),
            "note is the operator's context: {out}"
        );
    }

    #[test]
    fn lease_ls_says_when_a_stale_lease_becomes_reclaimable() {
        let stale = render_leases(&[lease_entry("a.txt", "gone", 910, -10, false)]);
        assert!(stale.contains("stale (reclaimable in 20s)"), "{stale}");
        let expired = render_leases(&[lease_entry("a.txt", "gone", 9000, -8000, true)]);
        assert!(expired.contains("expired"), "{expired}");
        assert!(!expired.contains("reclaimable"), "{expired}");
    }
}
