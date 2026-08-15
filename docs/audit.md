---
title: pact audit
description: What each check proves, what `--compare` answers, and what audit deliberately cannot see.
audience: operators
---

# `pact audit`

Offline analysis of a repository's coordination history, from
`.pact/events.jsonl`.

```bash
pact audit                            # summary
pact audit --check double-win         # exit 1 if two agents ever held one path
pact audit --check stale-holds        # exit 1 on holds past TTL with no renew
pact audit --check chain-integrity    # exit 1 if a chain-tracked line was edited
pact audit --check commit-correlation # exit 1 on a real concurrent write or a commit with no lease
pact audit --check topology --expect worktrees   # exit 1 if the fleet did not run where it was told
pact audit --compare base.json        # what moved since a previous --export
pact audit --since 7d --json          # narrow the window, machine-readable
```

Exit **0** clean, **1** findings. Those are the documented codes reused, not
extended: a finding is a *result*, so it is not a usage error (5) and not a lease
conflict (2). `pact audit` with no `--check` always exits 0 — a summary has
nothing to fail.

## Why it exists

pact has always recorded every acquire, renew, release, steal and expiry. Nothing
read it back except `pact log`, which prints the tail. So the questions a fleet
actually raises needed a human with `jq` and a hypothesis:

- is one file a bottleneck for the whole fleet, or merely busy?
- does anybody hold leases for far longer than their peers?
- **did two agents ever hold one path at the same time?**

The last one is different in kind, because it has a written trigger condition
attached. The guard-file backlog item (**pact-ehi**) says: implement the guard
file *if and only if* a double-win appears in a real events log. That is a
falsifiable claim, and until now it had no detector — which makes it a claim
nobody can act on, and an invitation to implement the guard file on suspicion.

`--check double-win` is that detector. The bead names this command; this
command's `--help` names the bead. If it ever exits 1, its output **is** the
evidence the bead is waiting for.

## What the summary says about contention

Before any named check, the summary relates contention to what it bought:

```
  conten 124 refusal(s), 2.0 per successful claim; 9 path(s) refused and never acquired (16 refusal(s) abandoned)
```

A bare refusal count is uninterpretable — 124 reads equally well as "healthy
contention that resolved" and "a fleet thrashing". The **ratio** is where the meaning
is, and the abandoned count is where the waste is: nine (agent, path) pairs were
refused and never claimed, sixteen refusals spent asking for something their asker
never got.

The line prints only when something was refused; `--json` carries the zeroes either
way so [`--compare`](#--compare) can track the trend, which is the only form in which
these numbers mean much. The three earlier field runs recorded **zero** refusals
between them — wave scheduling pre-resolved contention entirely — so this is the first
run there was anything to relate.

One thing it cannot distinguish: a log written before the `refused` kind existed also
reads as zero refusals. `pact_version` on the events is how to tell.

## The checks

### `--check double-win`

Reconstructs hold windows per path and reports any moment two different agents
had one open. `acquired` and `stolen` open a window; `released`,
`force-released` and `expired` close one; `renewed`, `restored` and `refused`
are neither — `restored` is a multi-path `lease acquire` retracting a refresh
it had to undo when a later path in the same batch failed (see
[leases.md](leases.md)), and it un-counts that refresh's `renewed` so a
rolled-back hold can still be flagged by `--check stale-holds`. `refused` is a
denied acquire, logged under the agent who was refused rather than the holder
— it never had a window to open, so it can't skew the holder's own.

Four decisions in that reconstruction are what make it usable rather than noisy,
and each has a test:

| Situation | Verdict | Why |
|---|---|---|
| `acquired` → `released` → `acquired` | clean | sequential holds |
| `expired` → `stolen` | clean | a routine reclaim. `expired` is logged under the **dead** holder's name, so the old window closes before the new one opens |
| `acquired` → `stolen` with no `expired` | **finding** | `--steal` over a live lease really is an overlap |
| `acquired` → `acquired` by the same agent | clean | pact re-acquires to refresh; that is one window, not two |

Get the second row wrong and every takeover in every log is a false finding, at
which point nobody reads the check. Get the third wrong and the one case worth
catching is invisible.

Forensics name both agents, both timestamps, and the **line numbers** in
`events.jsonl`. Events carry no id of their own, and inventing one would mean
rewriting an append-only log whose only virtue is being append-only — "line 47"
is stable for a given file and a human can go and look.

### `--check stale-holds`

Holds that ran past their TTL **and never renewed**, plus any hold that lapsed
into `expired` whatever its length.

**Each hold is judged against the TTL it recorded**, from `ttl_secs` in the
event — never against the TTL the binary happens to be compiled with. That is
what makes the default recalibratable: when
[`DEFAULT_TTL_SECS` moved from 900 to 2700](leases.md#lifecycle-expiry-and-stealing)
on 2026-08-06, no historical finding changed. A hardcoded threshold would have
silently cleared 22 of them, with nothing having changed about the holds.

Events written before pact recorded a TTL fall back to **900s, the default of
their era**, and the report marks those rows `*` and counts them. Judging old
history against today's default is how raising a default quietly rewrites the
past.

The "never renewed" half is the point. The protocol says a long task must not
outlive its lease and that `pact lease renew` refreshes it, so a two-hour hold
that renewed is an agent following instructions. Reporting it would train people
to ignore the check. A two-hour hold that never renewed is a window where the
lease was reclaimable by anyone while its holder still believed it owned the path.

Rows sharing an agent and a duration are one `lease acquire` that named several
paths. The output says so rather than reporting a distinct-incident count,
because grouping them would need a timestamp tolerance — one `acquire` writes one
row per path, microseconds apart — and a number that depends on an arbitrary
tolerance is worse than no number.

### `--check chain-integrity`

Every event pact appends carries `chain_hash`: a hash of the line's own content
mixed with the `chain_hash` of the line physically before it (or a fixed
`"genesis"` value, for the first line ever tracked). Edit a chain-tracked line
after the fact — by hand, or by anything other than `pact` itself — and its
recorded hash no longer matches what this check recomputes from its neighbour.
This is a different failure mode from a torn append (a truncated final line,
already covered by `unparseable_lines`): a torn line fails to *parse* at all,
where a tampered one parses fine and reads as ordinary history unless something
recomputes its hash.

This is **strictly additive, not a change in what pact trusts**. `owner_of`,
`actors`, `audit::summary` and every other existing reader still trust every
line exactly as they did before this field existed — this check is a new,
separate surface, not a new gate in front of an old one. That scope is
deliberate: making the chain the thing those consumers trust would change what
"the log is authoritative" means for all of them, which is a maintainer
decision this feature does not make on anyone's behalf.

A line with **no** `chain_hash` is not a finding. It is counted and reported —
"N line(s) predate chain tracking or were not written by pact" — but never
flagged as tampered, because every line written before this shipped has no
`chain_hash`, including this repository's own committed history. Treating an
absent field as evidence of tampering would flag every pre-existing repository
the moment this check landed. What the check actually flags is narrower and
stronger: a line that DOES carry a `chain_hash`, but the wrong one — which is
exactly the shape a hand-edit or forgery leaves, since nothing except
`pact`'s own append path can compute one that verifies.

Chain continuity resets to `"genesis"` after any untracked line, rather than
reaching back through it for an earlier tracked ancestor. That is what lets a
log that is part pre-existing history and part chain-tracked (the shape every
real repository has, indefinitely, from the moment this shipped) verify
cleanly: a tracked line only ever answers for the line immediately before it.

Alone among the checks, this one reads the **raw, unfiltered** log —
`--since` and `--include-annotated` do not apply to it. The chain is a
property of physical line adjacency as written; an annotation line and
whatever it covers are still real entries the writer's hash chain ran
through, whatever a lease-history statistic later excludes them from.

### `--check commit-correlation`

Every other check looks only at `.pact/events.jsonl` — this one asks
whether real git history backs up what that log claims, by shelling out to
`git log` (`git_history.rs`), the same way `repo.rs` and `doctor.rs` already
shell out to `git` for other checks. It reports three things, only two of
which are findings:

**How a hold is matched to commits: the range first, the clock only if it must.**

When the opening and closing events both recorded a `head` (pact stamps it on
`acquired`/`stolen`/`released`/`force-released`), the hold brackets an exact commit
set — `git log <open-head>..<close-head>` — so "what did this agent land under this
lease" is a lookup rather than an inference. That matters on a busy fleet: a timestamp
window credits your hold with any commit that happened to land in the same minutes,
including a peer's.

The window remains, and always will. Every log written before pact stamped `head` has
none, and a recorded hash stops resolving after a worktree branch is deleted and
garbage-collected, after a force-push, or when the run is analysed in a shallow clone.
Each of those falls back to the timestamp window, and the report **says which route it
took** — `correlated_by_head` and `correlated_by_time` in `--json`, and a line in the
text output. A check that got quietly less precise would be worse than one that stayed
imprecise.

Measured on the quern run (37 agents, one worktree each), the first fleet whose log
carries the field: **154 holds correlated by range, 3 by window.** Each agent records
its own worktree's HEAD, which is what makes the range per-agent and therefore worth
having — 111 distinct heads across the run, and 59 of 62 agents saw more than one.

- **A hold with no commit anywhere in its own window** — informational only,
  never a finding. A read-only lease (research, review, a task that turned up
  nothing to change) closes exactly like this, and flagging every one would
  train a reader to ignore the check. `--json`'s `holds_with_no_commit`.
- **A concurrent write** — two holds of the same path with overlapping
  windows where **two or more real commits**, not just the lease events,
  landed during the overlap. Stronger evidence than `--check double-win`,
  which only proves the *lease* events overlapped: this proves work actually
  landed more than once during the disputed period. It does not try to
  attribute which commit belongs to which hold by matching commit author
  against agent name — that correlation is exactly as unreliable as the one
  `pact doctor`'s "Beads actor attribution" check exists to flag (a shared
  checkout collapses every agent's commits to one git identity) — so it
  reports every commit found in the overlap and leaves attribution to a
  human. `--json`'s `concurrent_writes`.
- **An uncovered commit** — a commit touching a path with no hold covering
  its author date at all: work done with no lease, which the whole protocol
  exists to prevent. Scoped to paths that were leased at *some* point in the
  window audited — a path nobody has ever leased is a different question
  ("is this file even under pact's protocol") this check does not try to
  answer, since most of a real repository is never leased at any given
  moment and flagging all of it would be pure noise. `--json`'s
  `uncovered_commits`.
- **A cross-held commit** — a commit that fell inside a hold, but a hold held by
  a **different agent** than the one that made the commit. A finding, and a
  louder one than an uncovered commit: committing where nobody held the path
  risks your own work; committing where somebody else held it corrupts theirs.
  `--json`'s `cross_held_commits`. Requires attribution — see below.

#### Attribution, and why `covered` was answering the wrong question

`uncovered` asks whether **anyone** held the path across a commit. It never asked
whether the **committer** did, because git cannot say: every agent in every fleet
so far commits under one git identity, so `author` is the same string for all of
them.

That gap is not theoretical, and it fails in the worst possible direction. One
agent working without a lease is invisible whenever a compliant peer happens to
hold the path — and in a real run that is exactly what happened. A rogue agent's
worst commit touched five files in one unleased shot and **passed clean**, because
at that instant every one of those five paths was under an active lease held by a
compliant peer. **The better the rest of the fleet behaves, the better a rogue
hides**, and the miss cost a hand three-way merge reconciliation
([evidence](studies/field-runs.md#run-4-crucible-built-to-hurt)).

So a commit can say which agent made it, with a trailer:

```bash
git commit --trailer Pact-Agent=$PACT_AGENT -m "..."
```

The protocol block asks agents to do that, and every `pact init` refresh carries
the line. To make it automatic for a whole checkout, a one-line hook does it
without anyone remembering:

```bash
# .git/hooks/prepare-commit-msg  (chmod +x)
[ -n "$PACT_AGENT" ] && git interpret-trailers --in-place \
  --trailer "Pact-Agent=$PACT_AGENT" "$1"
```

**`pact init` does not write that hook**, deliberately. Hooks are a shared,
user-owned file outside pact's namespace, frequently already occupied (by
`pre-commit`, for instance), and clobbering one to improve an audit check is a
worse trade than printing it here.

**Attribution is reported whether or not it is present**, on its own line, clean
or not:

```
  0 commit(s) carry a Pact-Agent trailer, 12 do not — with none attributed, a
  commit counts as covered when ANY agent held the path, so an agent working
  without a lease is invisible whenever a compliant peer holds it
```

Every commit that exists today predates the trailer, so an unattributed commit
behaves **exactly** as it did before this class existed: covered by any hold. It is
never guessed at from `author`. The counts are what tell a reader which question a
clean result answered — and they are scope rather than findings, so a clean
history adds nothing to `--export`'s observations.

**Degrades cleanly when git can't answer.** A brand-new repository with zero
commits, a `git` that fails to run, or a `.pact/` whose `.git` is not
actually a readable repository all read as "no commit history to correlate
against" rather than crashing or reporting a false blanket set of findings —
`--json`'s `git_unavailable` names the reason when `git` itself could not be
run at all.

Two known gaps, both cheaper to document than to close:

- **Merge commits carry no file list** from a plain `git log --name-only`,
  so a merge that brought in changes to a leased path is invisible here.
  Findings degrade to "no commit seen" — the same shape as a read-only
  lease, never a false positive.
- **A rename is two identities.** No `--follow` is passed, so a lease taken
  under one name only correlates against commits recorded under that same
  name; a renamed file's history before the rename does not connect.

This is the one check where widening scope past `.pact/` was a conscious
call, not an oversight — see
[What audit deliberately cannot see](#what-audit-deliberately-cannot-see)
below for why that does not touch the Beads-store invariant the rest of this
page rests on.

### `--check merge-divergence`

**A lease is exclusive in TIME. It is not exclusive across COPIES.**

In one shared checkout that distinction does not exist — the file has a single
state and the second writer sees the first writer's bytes. Under
[one worktree per agent](fleet-patterns.md), which pact explicitly supports and
every field run so far has used, it does:

```text
agent A  acquires, edits, commits to branch A, releases      (compliant)
agent B  acquires, edits a DIFFERENT COPY on branch B that
         never contained A's change, commits, releases       (compliant)
```

Both leases were honoured. The conflict is deferred to a merge performed later,
by someone else, with no lease held by anyone and pact not involved — and the
merge window is where the corruption lands.

One run produced three instances — duplicate match arms, six duplicate test
functions, and a near-miss that would have silently reverted a peer's change — and
**no conflict marker in any of them**, because git merges textually non-adjacent
insertions cleanly. Two were caught by a diff review and a compile failure; none by
pact ([evidence](studies/field-runs.md#run-4-crucible-built-to-hurt)).

pact cannot fix git. What it can do is compare the content hash a releasing agent
left against the hash the next acquirer's own copy has, which is a fact both
`acquired` and `released` events already carry. `lease acquire` does that live and
[warns on stderr](leases.md), exit 0. This check does it offline, over the whole
log.

It pairs each **close** that recorded a hash with the **next open** of the same
path that recorded one, and reports the pairs that disagree. Two deliberate
narrowings:

- **Adjacent pairs only, in that order.** A hash that differs two holds later says
  nothing — the intervening holder was entitled to change the file. The claim is
  exactly "the copy this agent started from is not the copy the last agent
  finished with", and anything wider would flag ordinary sequential work.
- **One close anchors one comparison.** A third and fourth acquire of an unchanged
  path do not each re-report it. Renewals are skipped entirely: a renewal is the
  same hold continuing, so treating it as an open would compare an agent against
  itself.

**A close with no content hash is scope, not a finding**, and is reported on its
own line whether or not the check is clean. Every log written before pact stamped
releases is entirely in that state, so flagging it would fail every existing
repository the moment the check shipped — the same discipline `chain_untracked`
and `topology_unstamped` follow. An unhashed close also *clears* the anchor rather
than leaving a stale one, because comparing an acquire against a release two holds
back would invent a finding out of nothing.

### `--check claim-lease-divergence`

A fleet on this protocol runs **two mutual-exclusion mechanisms answering two
halves of one question**:

| Lock | Decides |
|---|---|
| `bd update --claim` | who owns the WORK |
| `pact lease acquire` | who may edit the FILES |

Neither consulted the other. Nothing prevented agent A holding the bead while
agent B held every file that bead names — and it happened three times in one run,
self-correcting only because an agent noticed and volunteered a release
([evidence](studies/field-runs.md#run-4-crucible-built-to-hurt)).

The protocol tells agents to claim first and lease second, which makes the bead
claim *look* like the serialization point. It is not: the lease is what actually
protects the file, and it was grantable to whoever lost the claim.

So this check asks that question over the whole log — and since 0.9.0 it is the
**only** place pact asks it at all.

### It reads a committed export, and spawns nothing

The assignee of each bead is reconstructed by replaying `field=assignee` rows from
`.beads/interactions.jsonl`, the git-tracked audit log bd exports
([what that file is](architecture.md#beadsinteractionsjsonl-is-committed-and-is-not-the-passive-export)).
No `bd` subprocess, so this check runs on a machine that has never had the issue
tracker installed.

It replaced a live `bd show` per distinct bead, and it is **strictly less
sensitive than what it replaced**: it finds fewer divergences, never more, and
never a false one. Three limits, all verified rather than assumed:

1. **The export records CHANGES.** A bead assigned at creation and never
   reassigned has no `field=assignee` row and cannot be resolved at all. Measured
   in this repository: 5 of 264 rows are assignee changes; 257 are status.
2. **The sidecar is opt-in, and off by default.** bd writes `interactions.jsonl`
   only when its audit sidecar is recording — verified against bd 1.2.1 (634cbbc4b)
   by running `bd update --assignee` with and without it. In a default bd repository
   the file never appears and this check reports "no beads data" forever. Two levers
   turn it on: `BD_AUDIT_ENABLED=1` in the environment bd runs in, or
   `bd config set audit.enabled true` to persist it.

   **bd 1.2.1 warns that `audit.enabled` is "not a recognized config key" and then
   honours it anyway.** Only bd's config-key allowlist disagrees with the rest of
   bd, which names the key in `bd audit --help`, in `bd audit record`'s own error
   text and in the `.beads/config.yaml` it generates. Measured both directions:
   unset, `bd audit record` exits 1 and no file appears; set — or with the env var
   — `bd update --assignee` appends the `field_change` row this check replays. Take
   the warning as cosmetic, not as a failed remediation.

   `pact doctor`'s **Beads audit sidecar** check warns when the file is absent, and
   names both levers along with that warning. It deliberately does *not* claim
   recording is currently on: `BD_AUDIT_ENABLED=1` lives in someone else's
   environment and leaves nothing pact can read, while `bd config get audit.enabled`
   answers for keys nobody set — so a config-derived verdict is wrong in both
   directions, and doctor stopped asking for one (pact-83r.6).
3. **Even recording, it lags.** It is a committed export, so an assignment made in
   the current session may not be in it yet.

Absent, empty, unparseable, or present with no assignee row at all: one answer,
"nothing to check against", and all of them **pass**. A single malformed line is
skipped, never fatal.

`claim_unavailable` carries the reason and reads as "this check could not run",
never as "nothing found" — the same contract `git_unavailable` has. The other scope
line, printed clean or not, is how many holds named no bead at all.

### The live cross-check is gone, and losing it cost nothing measurable

`pact lease acquire` used to run the same lookup at claim time and warn when the
bead in your note belonged to somebody else. That was a `bd show` **on the lease
hot path** — a subprocess between an agent and the file it is about to edit, which
is exactly the runtime dependency 0.9.0 removed everywhere else.

It was dropped rather than repointed at the offline source, and the number is why:
replayed against this repository's entire event log, the offline version would have
warned **zero** times. 100 acquire notes named a bead, 8 of those beads resolved to
an assignee in the export, and all 8 resolved to their own acquirer. And that is the
generous reading — in a default bd repository the sidecar does not exist at all, so
it would have been 0 of 100 forever, for a file read on every acquire.

**So the retrospective answer is the only one now, and it is honest about being
retrospective.** `assignee` is the assignee *when you run audit*, not at acquire
time — the log records the note, not the bead's state when it was written — so a
hold that legitimately handed its bead on afterwards appears here too.

This is the second place audit reaches outside `.pact/`, after
`--check commit-correlation` reached for `git`. Same rule both times, and this one
obeys it more strictly than its predecessor did: it never opens the Beads DB, and
now never even runs its CLI.

### `--check retry-storm`

**The only check about what the fleet wasted, rather than what pact got wrong.**
Nothing it reports broke a rule: a refused agent is entitled to ask again.

One run supplied the first real contention data — 124 refusals, where the three
runs before it produced zero between them — and it shows agents busy-polling. The
worst offender refused one path **33 times**, and the spacing is the finding:
twenty-seven of its thirty-two gaps were *exactly 15 seconds*, a hardcoded poll
loop rather than adaptive retry, while the refusal in front of it said the holder
had a median 355 seconds left and the holder's own note said it would renew. It
retried roughly 24x more often than its own screen told it to
([evidence](studies/field-runs.md#run-4-crucible-built-to-hurt)).

Two independent shapes flag, and either is enough:

- **volume** — more than 5 refusals of one path by one agent. Works on any log,
  including every one written before the holder's remaining lease was recorded.
- **impatience** — median spacing below a quarter of the holder's advertised
  remaining lease. This is the shape that names the mistake, and it needs
  `holder_remaining_secs`.

Both thresholds are deliberately crude, because the data does not need precision:
the observed storms were 33, 20, 14, 13 and 8 refusals while ordinary resolved
contention sat at **1**. There is a wide empty band between them, so any threshold
inside it gives the same answer — which is the argument for having one rather than
tuning it.

`chaos-ghost` is excluded. That is [`scripts/chaos.sh`](testing.md) planting a stale
lease, and its failed acquire is a rail firing correctly; counting it would credit
the fleet with waste the fault injector caused on purpose.

Refusals with no recorded holder-remaining are reported on their own line, clean or
not, so a reader knows which half of the check could run.

### `--check silent-contention`

Somebody wanted a path, was told no, and learned nothing when it came free.

This check was deferred for months because "communicated in the same window" seemed
to need an arbitrary cutoff. **It does not, once the boundary is the hold itself.** A
refusal happens while some agent holds the path, `reconstruct` already computes that
hold exactly, and the question becomes: *between the refusal and that holder's
release, did anything communicate about this path?* No cutoff, nothing to tune — the
same all-or-nothing move that unblocked `--check topology`.

Three things count as communication:

1. a `notified` delivery for the path — the holder's release told a subscriber;
2. a message tagged with the path;
3. **the refused agent already held a covering watch at the moment of the refusal** —
   it had arranged to be told, which is using the channel that works.

(3) is counted and reported, but deliberately **does not net out of the contention
numbers**. In one run, 24 of 124 refusals came from an agent that was already
subscribed — and those same agents then polled 13, 6 and 3 more times. Crediting the
subscription while the agent busy-retries would score the run as communicating well
at exactly the moment it wasted the most work. So the count sits beside the findings,
and `--check retry-storm` says what the agent did with the channel it had.

Subscriptions are judged **at the refusal**, by replaying `watches.jsonl` to that
instant rather than reading the live registry — otherwise a later `watch rm` rewrites
whether the agent had a channel, the same way judging a hold against today's default
TTL would rewrite whether it was stale.

An **open** hold is skipped: it has not had its chance to communicate yet, and
flagging a fleet mid-run for something it may be about to do is noise.

## Where the run actually happened

The summary counts events by the worktree pact was invoked from, so "did this
fleet use the topology I asked for" has an answer:

```
  run in 31 from main, 12 from wt-render
```

Events written before pact recorded this are counted as **predating context
stamping** and never dressed up as a topology — every log that exists today is
in that state, and a check that guessed would be worse than one that abstains.
See [leases.md](leases.md#every-event-records-where-pact-was-invoked-from) for
the fields themselves.

One note fires, and only on unambiguous evidence — the repository has linked
worktrees *right now*, at least one event is context-stamped, and not one names
a worktree:

```
  note   this repository has linked worktrees, but no event was invoked from one
         — agents may be editing in worktrees while running pact from the main
         checkout, in which case the lease/edit binding rests on convention and
         cannot be verified from this log
```

That is the shape a real 20-agent run had: one git worktree per agent, and
every `pact lease` run from the main checkout. It is not a defect — repo-
relative lock keys mean the shared namespace still worked — but it is the
difference between a binding that was verified and one that was assumed.

**Deliberately not inferred from merge commits.** That was the obvious
heuristic and it is a bad one: a repo merging ordinary feature branches has
exactly the same commit shape, so the hint would fire nearly everywhere and
mean nothing — the same failure this page already records under
[Worked example](#worked-example-measuring-the-things-that-motivated-this),
where a metric returned the same answer regardless of the behaviour it claimed
to measure. `has_worktrees` is a fact about the repository, so this cannot
false-positive. Its one blind spot is honest: worktrees deleted after the run
leave nothing to detect.

### `--check topology --expect <worktrees|main|any>`

The summary reports where a run happened; this turns it into an assertion, so
CI can answer "did the fleet use the topology I asked for" with an exit code.

```
$ pact audit --check topology --expect worktrees
topology: scanned 43 event(s)
  expected worktrees; 0 event(s) carry no invocation context

TOPOLOGY MISMATCH: 43 event(s) invoked from "main", which --expect worktrees does not allow
```

**Every context-stamped event must satisfy the expectation. There is no
proportion threshold, and that strictness is the point.** Any looser rule needs
a cutoff — what fraction of events counts as "worktrees"? — and a verdict that
depends on a cutoff nobody derived from data is exactly the failure this page
records under [Worked example](#worked-example-measuring-the-things-that-motivated-this).
All-or-nothing is explainable in one sentence and cannot drift. A genuinely
mixed run therefore satisfies neither `worktrees` nor `main`, which is the
honest answer rather than an inconvenient one.

`outside` never satisfies `--expect worktrees`: it means pact ran somewhere not
under this repository at all, the one value that says the lease/edit binding
cannot be assumed.

`--expect any` (also what a bare `--check topology` means) never fails — it
reports the distribution. That is what `--export` records, so a stored
retrospective does not fail on an expectation nobody declared when it was
written.

**A log with no invocation context exits 0 whatever was expected**, and says how
many events it could not speak for. Every log written before pact 0.7.0 is
entirely in that state; failing them would have failed every repository on the
day this shipped.

### `--allow-main <agent>` — the declared main-checkout participant

Repeatable. Excuses the named identity from `main`, and only from `main`.

This exists because `--expect worktrees` could not pass for any real fleet. In the
topology pact documents somebody *must* sit in the main checkout — it is the only
place the coordination logs can be committed from, which is why the protocol block
now says so explicitly — so an orchestrator necessarily acts from `main`. One field
run failed the check with **19 offending events, not one of which was an agent
working in the wrong place**.

```bash
pact audit --check topology --expect worktrees --allow-main orchestrator
```

The count of excused events is printed **even when the check passes**, in the same
header that says how many events carry no context at all. Without that, a reader
cannot tell "the fleet ran where it was asked" from "the exception list was wide
enough to cover where it did not".

It names identities rather than a number, because "one agent may work from main"
would pass a run where the wrong one did.

### An expiry describes the holder, not the sweeper

An `expired` row is written by whichever process happens to collect the lapsed
lock — often `pact lease ls` in the main checkout, minutes after the holder has
gone. Until 0.9.5 the row inherited *that* process's invocation context, so an
agent that let a lease lapse from its worktree got an expiry stamped `main`, and
this check counted it as an agent working in the wrong place.

Measured in one field run: **2 of 3 expiries carried a worktree attribution that
was not the holder's.** No later fix repairs a log already written, which is why
the data was corrected before the check that it broke.

The holder's context is now recorded on the lock at acquire time and copied onto
the expiry. The sweeping process is recorded separately, as `collected_from`, which
this check deliberately ignores: where somebody swept a lock says nothing about
whether the fleet ran where it was asked to.

## `--compare`

```bash
pact audit --export base.json        # before a change
# … change the protocol, the module layout, the fleet recipe …
pact audit --compare base.json       # what moved
```

```
compared against base.json

FIELD                        BASELINE        NOW      DELTA
events                              2          4         +2
agents                              1          3         +2
refusals (contention)               0          1         +1

15 field(s) unchanged
```

An instrument that reports each run in isolation cannot tell you whether a
change improved anything. Establishing that took three hand-run `jq`
comparisons in a single afternoon of improving pact from field runs, and **two
of the three produced a wrong conclusion** a human had to correct.

**Movement, never a verdict.** Which direction is good depends on what you
changed and why, which the log cannot know. Scoring a run would need weights
nobody derived from data — the failure this page records under
[Worked example](#worked-example-measuring-the-things-that-motivated-this) —
so `--compare` always exits 0 and leaves the judgement where it belongs.

**A protocol shift is called out first**, before any number:

```
PROTOCOL CHANGED between these runs: 6cd5cc61 -> 97b43b5d.
They are not a controlled comparison — anything below may be the
protocol rather than the fleet.
```

That is the exact mistake the feature exists to prevent. 223 messages from
pact's own fleet were once cited as evidence agents message voluntarily; every
one predated the protocol change that suppressed them, and nothing said so.
The era stamp ([onboarding.md](onboarding.md#what-init-writes)) is what makes
this mechanical.

**A field the baseline does not carry is "not comparable", never a delta from
zero.** An older pact's export has no `unacknowledged_messages` at all, and
reporting `0 → 3` would be a fabricated finding. The one exception is a count
in a map that only lists what occurred — `by_kind/refused` is absent from a run
with no contention, and that genuinely is zero.

`--compare` is its own mode and conflicts with `--check`: both want to be the
single thing stdout says, and two JSON values on one stdout breaks every
`--json` caller.

## `--export`

```bash
pact audit --export report.json                      # summary + every check + doctor, as one file
pact audit --check double-win --export report.json    # --export rides along with any --check
pact audit --export report.json --since 7d --json     # combines with every other flag
```

One JSON file bundling `summary`, `double_win`, `stale_holds`,
`chain_integrity`, `commit_correlation`, `topology`, `doctor` (`pact doctor`'s
own checks) and `unacknowledged_messages` — the exact set of things a real field
audit (pact-juz) had to assemble by hand: a separate `pact doctor`, a separate
`pact audit` per named check, and a raw grep of `.pact/events.jsonl`. Meant to
be read directly by a human, or handed to another agent session asking "how is
pact actually being used, and where does it fall down".

`unacknowledged_messages` lists every message its own recipient never marked
read, distinguishing "nobody has read it" from "read only by someone who was
not the addressee" — see
[messaging.md](messaging.md#and-pact-audit---export-asks-it-for-everybody),
which also explains why it stays here rather than moving to `pact doctor` now that
the original obstacle (it needed a `bd list`, which takes a write lock, and doctor
is served over MCP as strictly read-only) is gone.

A top-level `observations` array pulls out short, human-readable highlights
— a nonzero finding count from any check, or a `doctor` check that is not a
clean `ok`/no-`warn` — so a reader does not have to re-derive "is this worth
looking at" from raw counts and thresholds. An empty list means nothing rose
to that bar; the structured fields above it are always the complete data
regardless of what lands there.

**Orthogonal to `--check` and `--json`.** `--export` only ever adds a file;
it never changes what prints to stdout or which exit code a `--check` or
plain summary produces — pass it alongside either, or alone. Under `--json`
its own confirmation is skipped rather than printed as a second top-level
object on stdout: every other command's `--json` promises exactly one
parseable value, and a caller piping through `jq` would break on two. In
human mode it prints the path it wrote.

## Closes with nothing open

Reconstruction pairs every `released` / `force-released` / `expired` with the
`acquired` / `stolen` it closes, keyed by agent and path. A close matching no
open hold used to be dropped where it was found — no hold, no counter, no
trace — so `by_kind`'s raw count of close events could disagree with how many
holds actually closed, and nothing said why. The summary and every `--check`
now report it, and `--json` carries it as `orphaned_closes`:

```
  note   8 close event(s) with no matching open — not counted as a Hold
```

It counts "this did not add up" and never guesses what: audit does not
synthesize a best-effort hold for history it cannot reconstruct. A nonzero
count is normal. This repository's own 8 are all explained, and none is a
defect:

- **Four are `force-released`, from before pact-m7j.2.6.** That event is
  logged under the agent *doing* the forcing, unlike `expired`, which is
  deliberately logged under the dead holder — so `reconstruct` looked for an
  open window under the *forcer's* name, found none, and let the real
  holder's window run on until they next touched the path (why a hold
  spanning one used to read as a single long window rather than two). Fixed
  going forward: `force-released` now also carries `displaced`, the holder's
  own name, as a structured field, and `reconstruct` closes that agent's
  window instead. These four predate the field and stay orphaned exactly as
  before — `displaced` is absent on them, not wrong, and nothing rewrites
  history — but a force-release logged from here on closes correctly.
- **Four have an acquire the log never recorded.** `cli-wire` released four
  source files eight minutes into this log's history and no `acquired` for any
  of them appears anywhere in the file: it claimed them before the first line
  was written. A hold that straddles the start of recorded history has a close
  and no open, permanently.

Two more causes look identical to those and are equally benign: a `--since`
window that begins after the open, and an annotation covering an open line but
not its close. What is left after all four is a line the log actually lost — a
torn append, or a trim that raced a writer — which is the only reason the
counter is worth printing.

## Provenance: the log is append-only, corrections are annotations

`.pact/events.jsonl` is committed and never rewritten. Entries are not edited or
deleted, including wrong ones — because the file is *evidence*, and a log somebody
can quietly tidy is not evidence. The guard-file bead (**pact-ehi**) reads it to
decide whether a real defect exists; that only means anything if nobody can remove
an inconvenient line.

So a wrong entry is **annotated**. An `annotation` event names the lines it
covers, says why, and attributes the claim:

```json
{"at":"2026-08-06T…","agent":"maintainer","kind":"annotation",
 "detail":"synthetic: manual expiry experiment, agents victim/ghost/grabber",
 "covers_lines":[40,41,42,43,44,45],"actor":"maintainer"}
```

Rules that follow from that:

- Annotated lines are **excluded from every statistic and every check by
  default**, and the count is always reported — `6 event(s) excluded by
  annotation`. A statistic that quietly omits data is one nobody can check, which
  is the same defect as an editable log.
- `--include-annotated` shows the raw log as written, so an annotation can be
  **disputed** rather than merely trusted. The lines are still there.
- The annotation row itself is never counted as an event, in either mode.
  Otherwise a correction would inflate the totals with a record that describes the
  log rather than the fleet.
- The `actor` is **format-checked and flagged, never enforced**. One that fails
  `identity::validate` (the same `[a-z0-9][a-z0-9-]{1,31}` every agent name
  must match) prints `[INVALID ACTOR — does not match [a-z0-9][a-z0-9-]{1,31}]`
  after the name, and the annotation still takes effect. Rejecting it would let
  one malformed field silently swallow the correction it was written to record,
  which is worse than the forgeable field already was. An absent `actor` prints
  as `unknown` and is *not* flagged — nobody signed this is a different
  condition from somebody signed it with garbage. pact writes no annotation
  itself, so read time is the only gate there is to put this behind.
- Older pact binaries need no change: `kind` is a `String`, so an annotation
  parses as an unknown kind, opens no hold window and closes none. They just do
  not apply the exclusion — which over-reports rather than hiding events, the safe
  direction.

**The `pact-ehi` double-win trigger counts unannotated events only.** An
annotated overlap is not evidence, and there is a test asserting exactly that: the
same two overlapping events fire the check unannotated and do not fire it
annotated.

### The incident (2026-07-31)

Six events in this repository's log are not real coordination history:

| line | agent | kind | path |
|---|---|---|---|
| 40 | `victim` | acquired | `shared.rs` |
| 41 | `grabber` | acquired | `new.rs` |
| 42 | `grabber` | released | `new.rs` |
| 43 | `ghost` | acquired | `ghost.rs` |
| 44 | `ghost` | expired | `ghost.rs` |
| 45 | `victim` | expired | `shared.rs` |

None of those paths has ever existed here. They came from hand-run expiry and
all-or-nothing atomicity experiments executed **from the repository root** on
2026-07-31 — before `PACT_STATE_DIR` existed — and they were committed along with
the log on 2026-08-06 when `.pact/events.jsonl` was first preserved.

Provenance was established by measurement rather than by reading code: the log was
hashed before and after the full test suite and a fleet-sim run, and came back
byte-identical both times. So no test or script writes there today. `lease.rs`,
`cli.rs`, `mcp.rs`, `events_log.rs` and `worktree.rs` all use tempdirs;
`mcp_absent.rs` was the one file spawning pact without a `current_dir`, and though
neither command it runs writes state, it now uses a tempdir plus
`PACT_STATE_DIR` — a test that *cannot* reach real state is worth more than one
that currently happens not to.

Two structural changes came out of it:

- **`PACT_STATE_DIR`** overrides state resolution entirely, so tests, the fleet
  harness and demos can be pointed somewhere harmless. `pact doctor` reports it
  loudly, because a repository with it set by accident is one whose history is
  going somewhere nobody is looking.
- **`scripts/fleet-sim.sh` refuses to start** unless the state directory pact
  resolves is inside its tempdir. Asserted rather than forced: that harness exists
  partly to exercise the real resolution chain, including `--worktrees` and
  `--scope-local`, so it has to let resolution happen and then check the result.

What the correction changed, concretely: 153 events became 147, 23 agents became
20, and **both** `expired` events in the whole history turned out to be
synthetic — so no lease in pact's real history has ever actually lapsed.
`stale-holds` went from 24 findings to 22.

One embarrassment worth recording, since this page is about evidentiary hygiene:
the first annotation appended covered only line 45, because the shell command
building it assigned the line list to `LINES` — a variable bash owns and
overwrites with the terminal height. It happened to be 45. That annotation was
uncommitted and had been read by nothing, so it was replaced rather than
corrected-in-place; the append-only discipline protects recorded history, and a
botched write nobody has seen is not history.

## What audit deliberately cannot see

**The Beads store.** Audit never opens `.beads/`, a Dolt directory or a SQLite
file, and never will: an analytics command is exactly where that would be
convenient to break, because the data is right there. Bead titles, labels, types
and provenance are therefore **not** audit's subject; they live in
[`scripts/beads-retro.sh`](../scripts/beads-retro.sh), which is best-effort,
jq-based, and says so in its own header.

**One `.beads/` file is an exception, and it is the committed one.**
`--check claim-lease-divergence` reads `.beads/interactions.jsonl`: an append-only,
already-committed audit log, read-only, parse-tolerant, and the same *kind* of file
`.pact/events.jsonl` is on pact's own side. It is read as a file rather than
through a subprocess because neither bd nor br exposes "list every actor that has
ever acted" as a query — this is the one source that has it. The rule that matters
is about live transactional state, which this is not.

`--check commit-correlation` reads git history the same way, for the same reason
scaled differently: `git` is a hard requirement of running pact at all, not a store
pact reaches through an indirection layer, and `repo.rs` and `doctor.rs` already
shell out to it for other checks.

**Messages and read state are no longer on the Beads side at all**, so audit reads
them directly — `--export`'s `unacknowledged_messages` is a read of
`.pact/messages.jsonl` and `.pact/read/`, exactly like every other check.

**Anything before the history was preserved.** `.pact/` used to be gitignored
wholesale, so every clone started with an empty log
([architecture.md](architecture.md#pacteventsjsonl-is-committed-and-it-is-not-runtime-state)).
Audit can only see what was kept. A repository that has not re-run `pact init`
since that change still has no history to analyse, and `pact doctor`'s **event
log survives a clone** check is where that gets noticed.

**Conflicts.** The log records what *happened*, not what was refused. An exit-2
conflict writes no event, so audit cannot tell you how often agents blocked each
other. `.pact/waits/` exists for that and is telemetry, not history.

**Anything about intent.** A stale hold might be an agent that crashed or one
doing genuinely long work badly. Audit reports the shape; the reason needs a
human.

## Worked example: measuring the things that motivated this

Three findings from a fleet retrospective were the reason `pact audit` was
written. Two of them did not survive being measured, which is worth more than the
feature.

**"Zero preserved lease history"** — true, and now fixed. `.pact/` was gitignored
wholesale, so nothing could be asked after the fact. Narrowing that rule is
`pact audit`'s prerequisite, not a nice-to-have: the command has nothing to read
without it.

**"Dangling commit hashes, ~30%"** — an artefact of how you count. A naive scan
for 7-40 hex characters over closed beads in this repo gives 17 dangling out of
50, or 34%, which matches. But **9 of those 17 are not commit references at all**:
UUID fragments, `CLAUDE_CODE_SESSION_ID` pieces, a bd version hash (`20e493e56`),
trace ids from a telemetry table. Of the 8 that are genuine, most are deliberate
citations of *another* repository's commits, which cannot resolve here and should
not. Filtering to hashes actually introduced by a provenance word gives **0
dangling out of 15**. The 30% was measuring the regex, not the repository.

**"Claim skip 87%"** — the number was wrong, and so was the correction that
replaced it. This page said for a while that claim adherence is "not measurable
from the available data, in either direction", because bd writes a
`field_change/status` interaction only on close. The status half of that is
true. The conclusion was not.

`bd update --claim` "sets assignee to you, status to `in_progress`" — and bd
logs **assignee** changes as interactions of their own, with a distinctive
self-claim shape. It was measurable all along, through a field nobody had looked
at, and the per-run figures show one run claiming almost every bead and the next
claiming none while still closing them all
([the numbers](studies/field-runs.md#how-to-read-a-number-on-this-page)).

The likeliest source of the original scratch-store result is that `--claim` is
documented as **idempotent**: claiming an issue already assigned to you changes
nothing, so it logs nothing. Testing the no-op path and generalising from it is
the mistake.

The lesson this bullet exists for still stands, just aimed one step further
back: a metric that returns the same answer regardless of behaviour is worse
than no metric — and so is concluding a thing cannot be measured because the
first field you looked at did not move.

Whether pact should *report* claim adherence is a separate question and
deliberately still open: this data is Beads-side, and
[audit reads `.pact/` and never the Beads store](#what-audit-deliberately-cannot-see).

The general lesson is the one this repo keeps relearning: a metric that returns
the same answer regardless of the behaviour it claims to measure is worse than no
metric, because it looks like evidence.

## What it says about pact's own history

At the time of writing, on 147 preserved events from 20 agents — six more are in
the file and excluded by annotation, see [Provenance](#provenance-the-log-is-append-only-corrections-are-annotations):

```
$ pact audit --check double-win
double-win: scanned 147 event(s)
  6 event(s) excluded by annotation — this check did not look at them
no overlapping hold windows — no two agents ever held one path at once
```

So **pact-ehi's trigger condition has not fired**. The guard file remains
unjustified, and now that is a measured statement rather than an absence of
complaints.

`--check stale-holds` does find 22 holds past TTL with no renew, the longest
36m6s against the 15m TTL those leases recorded. That is a protocol-adherence
finding about the agents, not a defect in pact: they held paths their leases no
longer covered. It is also the measurement that
[recalibrated the default to 45 minutes](leases.md#lifecycle-expiry-and-stealing) —
and those 22 findings still stand after the bump, because each hold is judged
against its own recorded TTL.

## Where it runs

- **[`scripts/fleet-verify.sh`](../scripts/fleet-verify.sh)** calls
  `--check double-win --json` instead of the 40 lines of jq it used to carry. Two
  implementations of one invariant is one too many, and the jq copy would have had
  to independently know all four rows of the table above.
- **[`.github/workflows/audit.yml`](../.github/workflows/audit.yml)** runs
  `double-win` and `stale-holds` weekly against this repository's own committed
  history, so pact audits its own development continuously and files an issue
  when either turns up something. The three newer checks —
  `chain-integrity`, `commit-correlation` and `topology` — are **not** wired
  into that workflow yet. Said plainly rather than left to be assumed: a
  reader who sees a green weekly audit should know exactly which questions it
  asked.
- **MCP**: `pact_audit_summary` exposes the summary as a sixth read-only tool
  ([mcp.md](mcp.md)). The named checks stay CLI-only, because their contract is an
  exit code and a tool result cannot express one.
