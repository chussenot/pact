---
title: worktrunk
description: Whether pact can use worktrunk, a worktree manager built for parallel agents, as a fleet's lifecycle layer — what composes unchanged, which worktrunk defaults collide with pact and must be overridden, and the pact defects worktrunk's documented layouts expose.
audience: contributors
---

# worktrunk

**Question.** [worktrunk](https://worktrunk.dev/) (`wt`, 0.81.0 on 2026-10-26) is
a git worktree manager built for running several coding agents at once. One command
creates a branch, its worktree and the agent in it. Hooks set the worktree up,
`wt list` shows every worktree's state, and `wt merge` lands a branch on the default
branch and removes it. pact's measured fleet shape is
[one worktree per agent](../fleet-patterns.md#the-orchestrated-wave-pattern), created
and merged by hand. Can pact use worktrunk, and where should the line between the two
run?

**Answer.** Yes, as the fleet's **lifecycle layer**, composed from outside, with no
code in pact that calls `wt`. worktrunk decides *where* each agent works; pact decides
how agents working in those places agree. The two meet cleanly because worktrunk makes
plain `git worktree`s, and pact already treats every linked worktree of a repository as
one coordination space. In four layouts (worktrunk's default, its nested and
bare-repository recipes, and an ordinary bare repository), a lease taken in one
worktree refused a second agent in another with exit 2, naming the holder's branch and
worktree.

Four collisions need handling:

- worktrunk's default squash landing strips the `Pact-Agent` trailer. Done after
  release, it produces false audit findings (measured below).
- A test gate in `pre-merge` brings back the red-branch livelock that
  `pact merge --verify` was rewritten to escape (pact's own record).
- `wt step copy-ignored` copies pact's lock files and bd's database into every new
  worktree (measured below), and, since the build cache made `target/` a symlink, the
  link itself.
- Claude Code subagents that worktrunk's plugin places in worktrees all inherit one
  `PACT_AGENT`, and pact accepts that silently (measured below).

What worktrunk adds is safe landing *from* a worktree. pact's own docs say an agent in a
worktree [cannot do that](../fleet-patterns.md#who-can-run-it-which-is-not-everybody),
and the landing still needs pact's merge mutex. The experiments also surfaced six pact
defects, none of them specific to worktrunk ([follow-ups](#follow-ups)).

## Two layers

| | worktrunk | pact |
|---|---|---|
| Owns | the worktree lifecycle: create, set up, list, land, remove | agreement between the agents in those worktrees: claims, mail, interface notices, the record |
| Unit | a branch and the directory it is checked out in | a repo-relative path |
| State | git config `worktrunk.*` and `.git/wt/`, local | `.pact/` at the shared root; the event log and message store are committed |
| Guards against | two agents sharing one working tree and one index | two agents writing one file, in any number of working trees |

The last row is why a fleet wants both. Separate worktrees stop agents trampling each
other's working copy. They do not stop two agents changing the same file on two
branches: that collision moves to merge time. It surfaces as a rebase conflict, or as no
conflict at all. The run behind [`--check merge-divergence`](../audit.md#--check-merge-divergence)
produced duplicate match arms and six duplicate test functions with no conflict marker
in any of them. A lease every worktree can see moves the collision back to claim time.

The two tools share a temperament: no daemon, every command runs and exits, and both
emit JSON for scripts. That makes them easy to compose.

One naming clash: worktrunk's *agent handoff* is spawning a second agent into a new
worktree. pact's `handoff` is findings left for whoever inherits a bead. They are
unrelated features.

## What rules some uses out

pact's invariants settle these before any experiment.

- **No runtime dependency on another tool's CLI.** *"A subprocess boundary insulates
  you from how another tool stores things, never from what its commands mean"*
  ([architecture.md](../architecture.md#and-since-090-no-backend-at-all)), learned
  when bd changed what a flag meant and pact's tests broke with no pact change.
  worktrunk is pre-1.0 and releases often. So pact never shells out to `wt`, and never
  reads `.git/wt/` or `worktrunk.*` git config on a command's path.
- **A claim is made by the agent doing the work.** The argument that keeps the MCP
  server read-only applies to hooks too. A `pre-start` hook that acquires a plan
  entry's files on behalf of an agent that has not started is a claim no process stands
  behind. If the agent never starts, every peer waits out the TTL.
- **pact never guesses an identity.** worktrunk can carry one, but only as the
  spawner's declaration.
- **The record is the product.** Anything that rewrites commits (a squash, a rebase
  under another identity) has to be checked against `--check commit-correlation`, not
  assumed neutral.
- **A landing gate asks whether this change added a failure, not whether the branch is
  green** ([fleet-patterns.md](../fleet-patterns.md#a-red-shared-branch-is-not-a-reason-to-hold-your-merge)).
  The first version of `pact merge --verify` asked the second question. Its record, in
  `src/merge.rs`: three regressions in 15 minutes, then 25 minutes in which no agent's
  merge landed.

## Opportunities, ranked

| # | Use | Verdict | Why |
|---|---|---|---|
| 1 | **worktrunk as the lifecycle layer**: `wt switch --create … -x <agent>` to spawn, hooks to set up, `wt list` to watch, `wt remove` and `wt step prune` to clean up | **Strong, now** | No pact code. pact's coordination space held in every layout tested. It replaces the hand-rolled `git worktree add`, `cd`, spawn and remove of the orchestrated-wave pattern |
| 2 | **Declare identity at spawn**, beside `wt switch` | **Required** | The spawner is the only party that knows the name. Two worktrees sharing one `PACT_AGENT` both "acquire" a file with exit 0 (measured). That is the default for Claude Code subagents that worktrunk's plugin places in worktrees |
| 3 | **Keep `.pact/`, `.beads/` and `target` out of `wt step copy-ignored`** | **Required** | worktrunk's recommended `post-start` hook for warm builds copies pact's live lock files and bd's embedded Dolt store into every new worktree, and the `target` symlink mbx manages, which would point every worktree at one build directory. One `exclude` line fixes it here; worktrunk's built-in exclude list is the right long-term home for the first two |
| 4 | **Land from the worktree with `wt merge --no-commit`, inside pact's merge mutex** | **Strong, with three conditions** | Self-landing from a worktree, which pact's docs call impossible. Every `Pact-Agent` trailer survives (4 of 4, even after `gc`). The mutex serialises landings and closes the race worktrunk's own source names. Conditions: never `--yes`, no absolute test gate in `pre-merge` where the branch can be red, and release the mutex from the main checkout, because `wt merge` is deleting the invoking worktree while the alias still runs (measured) |
| 5 | **A lease-aware `pre-remove` guard** | **Good** | One `jq` line refuses to delete a worktree whose branch still holds a lease, the claim-level twin of `wt remove` refusing a dirty tree. Without it the claim outlives its agent by a whole TTL |
| 6 | **`wt list --format=json` at wave end**: `default_branch.merge_conflicts` per branch, to land the clean ones first | **Optional** | Detects textual conflicts only; `merge-divergence` covers the silent ones afterwards. An orchestrator script, no code |
| 7 | **pact reading worktrunk's activity markers** (🤖 working, 💬 waiting) in `doctor`'s fleet liveness | **Speculative** | It would separate "quiet, waiting on a human" from "quiet, working". But it couples pact to a pre-1.0 tool's private state, and the signal belongs to the harness: worktrunk only relays Claude Code's hook events. If pact wants it, it should take it from the harness |
| 8 | worktrunk's default landing (squash, rebase, fast-forward) | **No** | 0 of 2 landed commits attributed. Done after release: 2 false `UNCOVERED COMMIT` findings and audit exit 1 (measured) |
| 9 | pact calling `wt`, or worktrunk hooks taking leases for agents | **No** | The first two invariants above |

## The measurements

Run on 2026-10-26 against pact 0.16.0 (`495924a`) with git 2.43.0, the minimum worktrunk
supports. worktrunk itself was **not executed**: the session that wrote this could not
install it. Each worktrunk operation was reproduced as the git calls its documentation
and source describe ([limits](#what-this-does-not-say)). Claims about worktrunk's
internals cite its source at `057037a` (0.81.0).

On 2026-10-27 the wiring in [How it should be wired](#how-it-should-be-wired) was run
under worktrunk 0.81.0 itself, against pact 0.17.0 (`c8a9582`), on throwaway branches
of this repository. That section says what running it changed. The measurements below
are the original, emulated ones.

### The coordination space survives every layout

Each layout was built with the `git worktree` call worktrunk makes for that
`worktree-path` template. agent-a acquires `src/a.rs` in one worktree, then agent-b tries
it from another.

| Layout | pact state resolved to | agent-b | `pact doctor` state placement |
|---|---|---|---|
| default, siblings: `{{ repo_path }}/../{{ repo }}.{{ branch \| sanitize }}` | `proj/.pact` | exit 2 | `main-worktree`, correct |
| nested: `{{ repo_path }}/.worktrees/{{ branch \| sanitize }}` | `nested/.pact` | exit 2 | `main-worktree`, correct |
| bare repo in `myproject/.git`, worktrees at `myproject/<branch>` ([worktrunk's recipe](https://worktrunk.dev/tips-patterns/#bare-repository-layout)) | `myproject/.pact` | exit 2 | `main-worktree`, **wrong**: `myproject` is not a worktree |
| bare repo named `repo.git`, for comparison | `repo.git/pact` | exit 2 | `common-gitdir`, correct |

pact resolves state from the worktree's `.git` file and git's `commondir`, never from the
directory's location
([the resolution chain](../architecture.md#one-coordination-space-per-repository-not-per-checkout)).
So every other template worktrunk documents (a central `~/worktrees/` directory,
`codename` leaves) behaves like one of these rows: what matters is whether the
repository is bare, not where the worktree sits. The refusal names where the holder
works, which is what a peer in another worktree needs:

```
error: lease on src/a.rs is held by agent-a [claude-code] on branch agent-a in worktree proj.agent-a (45m0s left on their hold, last active 0s ago, taken 0s ago); use --steal to override
```

The nested layout leaves `.worktrees/` untracked in the main checkout. Ignore it there.
Otherwise a `git add -A` in the main checkout records each worktree as an embedded
repository (`warning: adding embedded git repository: .worktrees/agent-a`).

The bare layouts are where pact goes wrong, twice.

- **A linked worktree named `main` is recorded as the main checkout.** worktrunk's bare
  recipe puts the default branch in `myproject/main`. pact stamps every event with
  `invoked_from`, which is a linked worktree's name or the literal `"main"` for the main
  checkout, and here those are the same string. An agent that did everything in a
  linked worktree fails the topology gate:

  ```
  $ pact audit --check topology --expect worktrees
  TOPOLOGY MISMATCH: 1 event(s) invoked from "main", which --expect worktrees does not allow
  ```

  The event reads `worktree: "main", invoked_from: "main"`. A worktree named `outside`
  collides the same way.
- **The committed history has nowhere to be committed.** In both bare layouts,
  `.pact/events.jsonl` and `.pact/messages.jsonl` live outside every checkout
  (`myproject/.pact/`, `repo.git/pact/`), so no worktree can commit them. `doctor` still
  reports `event log survives a clone: .pact/events.jsonl is not ignored and not yet
  committed — commit it and it travels`, because it checks that path in the invoking
  worktree rather than where the log lives.
  [architecture.md](../architecture.md#known-limits-of-the-resolution-chain) calls a
  bare repository in a directory named `.git` "pathological, and not worth code to
  detect". worktrunk documents that layout as one where *"no branch gets special
  treatment"*, so it is now a supported shape and worth the code.

### Landing, and what it does to the record

Two agents work in their own worktrees. Each leases one file, commits twice with
`--trailer Pact-Agent=<agent>` and releases, which is the protocol exactly. Then each
branch lands one way, and `pact audit --check commit-correlation` reads the result.

| Landing | Lands on `main` | Carry a `Pact-Agent` trailer | `UNCOVERED COMMIT` | audit exit |
|---|---|---|---|---|
| `pact merge`, by the orchestrator, after release (today's pattern) | 4 work + 2 merge commits | 6 of 6 | 0 | 0 |
| `wt merge` default (squash, rebase, fast-forward), by the orchestrator, after release | 2 squash commits | 0 of 2 | 2 | **1** |
| `wt merge` default, by each agent before it releases | 2 squash commits | 0 of 2 | 0 | 0 |
| `wt merge --no-squash` (rebase, fast-forward), by the orchestrator, after release | 4 rebased work commits | 4 of 4 | 0 | 0 |

The last two routes were rerun with the branches deleted, as `wt merge` does, and
`git gc --prune=now`:

- **Squash:** unchanged. 0 of 2 attributed, 2 findings. Both holds were now matched by
  timestamp, because their recorded HEAD range no longer resolves.
- **`--no-squash`:** still 4 of 4. A rebase keeps author dates and messages, so the
  timestamp fallback finds what the range did.

What it says:

- **A squash replaces who and when.** The squash message is new (worktrunk's own, or an
  LLM's), so the trailers are gone, and its author date is the moment of the squash.
  Under the orchestrated pattern the squash happens after release, so each one is an
  uncovered commit: a fleet that followed the protocol perfectly fails its audit. When
  the agent lands its own branch while still holding the lease, the finding disappears
  but the anonymity stays. That is the gap where
  [an agent working without a lease hides behind compliant peers](../audit.md#attribution-and-why-covered-was-answering-the-wrong-question).
- **`wt merge --no-commit` is the pact-compatible landing.** It skips the commit step
  and the squash (*"--no-commit implies --no-squash"*, worktrunk
  `src/commands/merge.rs`) and refuses a dirty tree. What lands is exactly what the
  agent committed and signed, rebased: the last row.

**The trailer hook in audit.md breaks when someone else rebases.** The
[`prepare-commit-msg` hook](../audit.md#attribution-and-why-covered-was-answering-the-wrong-question)
appends `Pact-Agent=$PACT_AGENT` to every message git builds, and `git rebase` builds one
per replayed commit. With the hook installed:

| Landing, with the documented hook | Attributed | `UNCOVERED COMMIT` | audit exit |
|---|---|---|---|
| squash, by each agent before release | 2 of 2 | 0 | 0 |
| squash, by the orchestrator after release | 2 of 2, **all to `orchestrator`** | 2 | 1 |
| `--no-squash`, by the orchestrator | 2 of 4: the replayed commits read `agent-b,orchestrator` | 0 | 0 |
| `--no-squash`, hook with `--if-exists doNothing` | 4 of 4 | 0 | 0 |

The audit reads two different trailers as "cannot tell", by design. The hook should add
a trailer only where there is none: `--if-exists doNothing`. That costs nothing in the
case the hook was written for, and the bug is not specific to worktrunk. Any rebase run
under a different `PACT_AGENT` does the same.

### `copy-ignored` carries coordination state

worktrunk's recommended `post-start` hook for warm builds copies every gitignored file
from the primary worktree into the new one. It skips a built-in list of VCS and tool-state
directories (`.bzr/ .conductor/ .entire/ .hg/ .jj/ .pijul/ .sl/ .svn/ .worktrees/`, in
worktrunk `src/commands/step/shared.rs`), on the stated grounds that tool state
*"shouldn't be shared between worktrees"*. `.pact/` and `.beads/` are not on the list.
With one lease live, its selection (git's own ignored-file listing, which is the
discovery worktrunk documents) was:

```
.beads/dolt/main/manifest
.pact/SCHEMA
.pact/activity/agent-a
.pact/clock_watermark
.pact/events.jsonl.trimlock
.pact/leases/src__a.rs.lock
.pact/watches.jsonl
.pact/watches.jsonl.trimlock
```

Under the default shared scope pact never reads the copy. After agent-a released, the
new worktree saw `no active leases`. Under `PACT_WORKTREE_SCOPE=local`, or the
per-worktree fallback a broken `.git` file triggers, the copy *is* the state:
`lease ls` showed the released lease as `active`, and an acquire was refused with exit 2
by a claim that no longer existed anywhere. `pact doctor` reported neither copy.

The `.beads/` half is the costlier one. A copied embedded Dolt store is a second task
database in every worktree, and bd's own
[worktree guide](https://beads.gascity.com/reference/worktrees) treats a worktree's
*"own accidental `.beads` directory"* as a fault to remove. Whether bd then reads the
copy or the shared store was not tested here (bd was not installed). Excluding the
directory makes the question moot.

### One identity, two worktrees

Two processes in two worktrees, both exporting `PACT_AGENT=parent`:

```
$ cd proj.agent-a && pact lease acquire src/b.rs
acquired lease on src/b.rs for parent
$ cd ../proj.agent-b && pact lease acquire src/b.rs
acquired lease on src/b.rs for parent
```

Both exited 0. The lock now names `proj.agent-b`, and the first process still believes
it holds the file. This is the re-entrant refresh
[leases.md](../leases.md#the-trust-boundary-pact_agent-is-self-asserted) documents, and
the reason it forbids shared identities. It matters more beside worktrunk, because
worktrunk's Claude Code plugin routes `isolation: "worktree"` subagents through
`wt switch --create`, and a spawned agent's environment is its parent's
([fleet-patterns.md](../fleet-patterns.md#declare-what-you-launched)). The result is N
subagents in N worktrees sharing one `PACT_AGENT`.

leases.md says the only trace of such a collision is a `renewed` event,
*"indistinguishable from genuine self-renewal"*. Across worktrees that no longer holds:
the log reads `acquired … worktree proj.agent-a`, then `renewed … worktree
proj.agent-b`. A PID check was rejected because it would misfire on an agent
legitimately refreshing its own claim, and an agent does that from the worktree it
works in. A refresh that changes worktree is a different case. pact can refuse it
(exit 2, naming both worktrees), or at least report it in `audit`, without the false
positives that sank the PID check.

### Concurrent landings, from worktrunk's source

`wt merge` advances the target branch with a compare-and-swap,
`git update-ref <target> <new> <old>`, which *"fails cleanly if another process advanced
the target"* after the merge snapshotted it. It then updates the target's worktree with
a two-tree merge that leaves unrelated uncommitted files in place (`advance_target` in
`src/commands/worktree/push.rs`). So two agents landing at once cannot overwrite each
other: the loser fails, and must rebase and verify again.

Without a mutex that is safe but wasteful. In the worst case, N agents finishing
together run on the order of N² `pre-merge` verifications instead of N, and each one is
a test run (`pact merge` holds measured 25 to 64 s, nearly all of it verification). The
mutex turns the retries into a queue.

The same function names a race it leaves open:

> a commit racing into the target worktree inside that window is built against the
> pre-sync index, so its tree reverts the pushed content while keeping it in the
> ancestry

worktrunk only warns. In a pact fleet the target worktree is the main checkout, where
the orchestrator commits its `.pact/` checkpoints. Reproduced by advancing `main` with
the same compare-and-swap and committing before syncing the checkout:

| Checkpoint commit made inside the window | `src/a.rs` on `main` afterwards |
|---|---|
| `git commit -- .pact/events.jsonl`, the protocol's explicit pathspec | the landed change is kept |
| `git add .pact/events.jsonl && git commit` | the landed change is **silently reverted** |

A pathspec commit builds its tree from `HEAD`, which has already moved. A plain commit
builds it from the index, which has not. So pact's rule to always commit with explicit
paths, written for a different hazard, already covers this one for anyone who follows
it. For anyone who does not, the mutex closes the race, provided the orchestrator takes
it around its checkpoint commits. [fleet-patterns.md](../fleet-patterns.md#the-merge-mutex-pact-merge)
already assigns those commits to whoever holds the mutex.

## How it should be wired

Ordered by how much each step changes. Stop where the fleet's needs stop.

**1. Spawn with worktrunk and declare pact's variables.** `-x` starts the agent in the
new worktree, with the spawner's environment, once `pre-start` hooks finish:

```bash
PACT_AGENT=agent-03 BEADS_ACTOR=agent-03 \
PACT_HARNESS=claude-code PACT_MODEL=<the model you requested> \
  wt switch --create agent-03 --no-cd -x claude -- "$PROMPT"
```

Run each spawn in its own multiplexer pane, as worktrunk's agent-handoff recipe does,
because `-x` holds the terminal until the agent exits. A Claude Code subagent that
worktrunk's plugin places in a worktree inherits its parent's environment, and nothing
on that path can change it. So the subagent's prompt has to carry its identity: every
pact command it runs passes `--agent <name>`, because an `export` does not survive
between Claude Code's separate shell calls.

**2. One committed project config, [`.config/wt.toml`](../../.config/wt.toml).** The
file carries its own reasons beside each entry. What matters here is what running the
first draft of it under worktrunk 0.81.0 changed:

- **`exclude` lists `target` beside `.pact/` and `.beads/`.** Since
  [the build cache](../development.md#the-build-cache) made `target/` a symlink into
  mbx's store, `copy-ignored` would copy the link itself (a dry run listed it as a
  file), pointing every worktree at one build directory. Excluded, each worktree gets
  its own managed target, restored from the shared cache.
- **`wt land` releases the merge mutex from `{{ repo_path }}`**, the main checkout,
  and holds it for 30 minutes like `pact merge`. The first draft released from the
  invoking worktree, and the release ran while `wt merge` was deleting that worktree
  in the background: pact found no repository, printed `nothing to release … no lock
  held here`, and the mutex stayed held for its whole TTL. Fixed and run again, the
  acquire and the release pair up in `pact log`.
- **The `pre-remove` guard names the path and its holder** in the refusal, and passes
  where pact is not installed.
- **`post-start` is a pipeline**: `mise trust` first, because mise trusts config by
  path; then, together, the warm build (`cargo test --no-run`, through mbx because
  `mise.toml` wraps cargo) and `wt config state vars set agent=$PACT_AGENT`, so
  `wt list` can say who owns a worktree without pact in the reader's shell.
- **`wt spawn --agent=<name> [--model=<model>] -- <claude arguments>`** is step 1 as
  an alias. Branch and agent share the name, so one identity cannot hold two
  worktrees.
- **The Agent column and a `fleet` view are user config**: `wt config show` warns that
  `list.custom-columns` in a project file is ignored. The operator's
  `~/.config/worktrunk/config.toml`, scoped to this repository, carries them and makes
  a hand-typed `wt merge` keep the trailers too (two commits landed unsquashed):

  ```toml
  [projects."github.com/chussenot/pact"]
  merge = { squash = false, commit = false }

  [projects."github.com/chussenot/pact".list.custom-columns.Agent]
  template = "{{ vars.agent }}"

  [projects."github.com/chussenot/pact".aliases]
  fleet = """wt --config-set 'list.columns=["branch","Agent","status","ahead-behind","age","path"]' list {{ args }}"""
  ```

worktrunk executed every hook and alias. `wt remove` refused a branch holding
`src/output.rs`, naming the holder, and passed with only the mutex held and with
nothing held. `wt switch --create` ran the pipeline, and the new worktree had its own
mbx target, the recorded agent, and no copied state (`pact doctor` ✓). `wt land` landed
a trailered commit on `master` and left no lease behind. `pre-remove` runs after the
fast-forward, so if it refuses, the branch has landed and the worktree is kept. Release
before running `wt land`.

**3. Approve once, never `--yes`.** worktrunk runs project hooks and aliases only after
approval. A human reviews the file and approves it once with `wt config approvals add`.
Before each wave the orchestrator checks that `wt config approvals list --format=json |
jq -r .state` prints `approved`. Agents never pass `--yes`: it approves whatever the
*invoking worktree's* `.config/wt.toml` says, for that run, and that file is on the
agent's own branch. An agent that edited a hook would run its own edit. Lease
`.config/wt.toml` like any shared file, and treat it as the orchestrator's.

The same rule has a second edge. Hooks and aliases are read from the worktree the
command runs in, and `wt switch --create` cuts the new branch from the *local* default
branch without fetching. So the file, and the `mise.toml` that wraps cargo in mbx, must
already be on that branch, and the main checkout must have fetched it
(`git fetch origin master:master`). A worktree cut from a stale local `master` ran its
warm build with no wrapper and left a real 1.1 GiB `target/` behind (measured).

**4. Choose the landing per fleet.**

| | `pact merge --verify`, orchestrator, main checkout | `wt land`, each agent, own worktree |
|---|---|---|
| Who lands | the orchestrator, one merge per worktree per wave | each agent, when it is done |
| History | `--no-ff` merge commits, signed | linear; every work commit signed, rebased |
| Verification | after merging, on the shared branch; reverted if it added a failure | before advancing, in the agent's worktree (`pre-merge`) |
| On an already-red branch | lands work that adds no new failure | an absolute test gate blocks every landing |
| Cleanup | `wt remove <branch>`, or `wt step prune --min-age=0s` at wave end (its default skips anything under a day old) | automatic |

Use `wt land` where the default branch is kept green by something else (CI with branch
protection, or a fix-first rule), or with no test gate in `pre-merge` at all. Everywhere
else keep `pact merge --verify` until pact can lend its baseline verdict to a hook
([`pact-02v.3`](#follow-ups)).

**5. Under `wt land`, the orchestrator commits its checkpoints with explicit paths,
inside the mutex.** Either one avoids the race in
[Concurrent landings](#concurrent-landings-from-worktrunks-source); both together
cost nothing.

## What this does not say

- **The measurements emulated worktrunk; the wiring did not.** Each measured
  operation is the git sequence worktrunk's documentation and source describe:
  - `wt switch --create`: `git worktree add`;
  - the squash: `reset --soft` to the merge base and one commit, with worktrunk's no-LLM
    message format;
  - the rebase: `git rebase`;
  - the fast-forward: `git merge --ff-only` from the main checkout;
  - cleanup: `git worktree remove` and `git branch -D`.

  worktrunk's own fast-forward is the compare-and-swap described above, not
  `merge --ff-only`, but both leave the same commits on `main`, which is all the audit
  reads. The hooks and aliases of [How it should be wired](#how-it-should-be-wired)
  were later executed by worktrunk 0.81.0 itself, on throwaway branches of this
  repository, one agent at a time. No wave has run on it yet (`pact-02v.1`).
- **One small scenario**: two agents, two commits each. It shows the mechanisms. It
  does not measure a fleet. Steps are spaced at least a second apart, because
  commit-correlation compares whole-second git dates with sub-second hold bounds, and a
  commit made in the same second as its `acquire` reads as uncovered
  ([`pact-56w`](#follow-ups)).
- **bd's behaviour with a copied store** is inferred from bd's documentation, not
  observed.
- **worktrunk moves fast.** Every claim about it is pinned to 0.81.0 and source commit
  `057037a`.

## Decision criteria

- **Move the composition into [fleet-patterns.md](../fleet-patterns.md)** once one wave
  has run with real worktrunk and `pact audit` over it is clean: every landed commit
  attributed, `--check topology --expect worktrees` with no mismatch, and no lease
  outliving its worktree.
- **Prefer `wt land` to orchestrator-side `pact merge`** when the orchestrator is the
  measured bottleneck (wave-end merging longer than the wave's own critical path) and
  the branch is kept green.
- **Write pact code for worktrunk** only when a field run shows a failure the
  composition cannot prevent. Reading worktrunk's state would also need it to commit to
  a stable format, which means 1.0.

## Reproduce

In an empty directory, with no worktrunk needed. The orchestrated landing that fails the
audit, as git:

```bash
git init -q -b main proj && cd proj
mkdir src && echo 'fn a() {}' > src/a.rs
git add src && git commit -qm init
PACT_AGENT=orchestrator pact init >/dev/null
sleep 2          # keep the seed commit out of the first hold's window

git worktree add -q -b agent-a ../proj.agent-a main
cd ../proj.agent-a
PACT_AGENT=agent-a pact lease acquire src/a.rs
sleep 1          # git dates are whole seconds (pact-56w)
echo '// edit' >> src/a.rs
git commit -q --trailer Pact-Agent=agent-a -m "feat: edit" -- src/a.rs
PACT_AGENT=agent-a pact lease release --all
sleep 2          # the orchestrator lands later, as it does at wave end

# wt merge's default, as git: squash, rebase, fast-forward, remove
git reset -q --soft "$(git merge-base main HEAD)"
git commit -q -m "Squash commits from agent-a"
git rebase -q main
cd ../proj && git merge -q --ff-only agent-a
git worktree remove ../proj.agent-a && git branch -q -D agent-a

pact audit --check commit-correlation
```

```
  0 commit(s) carry a Pact-Agent trailer, 1 do not — …
UNCOVERED COMMIT on src/a.rs: 8163d69429bc by t at 2026-10-26T14:33:59+00:00 — no hold on this path covered that moment
```

It exits 1. Leave out the `reset` and the squash commit, which is the
`wt merge --no-commit` route recommended above, and the same block reports
`1 commit(s) carry a Pact-Agent trailer, 0 do not` and exits 0. With worktrunk
installed, the landing lines are `wt merge`, run from `../proj.agent-a`.

Then, from the same directory, the topology defect:

```bash
cd .. && git clone -q --bare proj myproject/.git
git -C myproject/.git worktree add -q ../main main
cd myproject/main && PACT_AGENT=agent-m pact lease acquire src/a.rs
pact audit --check topology --expect worktrees
```

```
TOPOLOGY MISMATCH: 1 event(s) invoked from "main", which --expect worktrees does not allow
```

### With worktrunk installed

The wiring, in a scratch clone of this repository named `pact`, from its main checkout,
after approving the project config once (`wt config approvals add`):

```bash
PACT_AGENT=probe wt switch --create probe --no-cd --no-hooks
(cd ../pact.probe && PACT_AGENT=probe pact lease acquire src/output.rs --note "probe: the guard")
wt remove probe      # refused: branch probe still holds pact leases: src/output.rs held by probe
(cd ../pact.probe && PACT_AGENT=probe pact lease release --all)
(cd ../pact.probe && echo probe > probe.txt && git add probe.txt \
  && git commit -q -m "probe: work" --trailer Pact-Agent=probe)
(cd ../pact.probe && PACT_AGENT=probe wt land)
git log -1 --format='%h %s %(trailers:key=Pact-Agent)' master   # the probe commit, trailer intact
pact lease ls        # nothing held: the alias released the mutex from this checkout
```

## Follow-ups

| Bead | What |
|---|---|
| `pact-02v` | epic: worktrunk as the lifecycle layer for worktree fleets |
| `pact-02v.1` | field run: one wave on real worktrunk (`wt switch -x` spawns, the committed `.config/wt.toml`, `wt land` side by side with `pact merge`); measure merge wait and audit cleanliness |
| `pact-02v.2` | move the composition into [fleet-patterns.md](../fleet-patterns.md) once the field run is clean (blocked by `pact-02v.1`) |
| `pact-02v.3` | design spike: expose `pact merge --verify`'s "did this add a failure" as a command a `pre-merge` hook can call |
| `pact-02v.4` | `doctor`: warn when a linked worktree carries its own `.pact/` runtime files or a `.beads/` database |
| `pact-02v.5` | upstream, worktrunk: add `.pact/` and `.beads/` to `copy-ignored`'s built-in tool-state excludes; carry squashed commits' trailers into the squash message |
| `pact-02v.6` | only if a fleet adopts squash landing: credit a squash commit to the hold whose commit range has the same patch-id |

The six pact defects are filed as bugs linked `discovered-from` the epic rather than
under it, because none is specific to worktrunk:

| Bead | What |
|---|---|
| `pact-4ez` | a linked worktree named `main` or `outside` cannot be told from the main checkout by `--check topology`; distinguish them without reinterpreting old rows |
| `pact-6tv` | bare layouts: decide where the committed logs live when no checkout can commit them; stop `doctor` calling them committable; describe a bare repository in `<project>/.git` as bare |
| `pact-37o` | audit.md's `prepare-commit-msg` hook gains `--if-exists doNothing` |
| `pact-3tg` | a same-identity re-acquire from a different worktree: refuse with exit 2 or report in `audit`, and correct leases.md's "indistinguishable" |
| `pact-f4p` | `pact init` commits the `.gitattributes` it writes; today it writes the `merge=union` rule and leaves the file untracked |
| `pact-56w` | a commit authored in the same second as its `acquire` is reported uncovered: `w.open <= c.at` compares a nanosecond hold bound with a whole-second git date, even when the commit is inside the hold's own recorded HEAD range |
