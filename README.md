---
title: pact
description: Why pact is built the way it is; the problem multi-agent repositories create, and the four primitives that answer it.
audience: everyone
---

# pact

**pact** is a small, dependency-light CLI that helps multiple coding agents —
Claude Code, Codex, or anything else that can run shell commands — work on the
same repository without stepping on each other.

New here: download a binary or `cargo install` — [docs/install.md](docs/install.md)
— then run `pact init` in a repo.

Everything below is *why* pact is built the way it is. The reference material —
what each command does and how to use it — lives in [docs/](#documentation), and
the evidence behind the design lives in
[the studies](#where-these-features-came-from). Some pact commands are for the
agents, some are for you; [cli.md](docs/cli.md#who-runs-what) says which.

## The problem

Say you're running two or three agents against the same repo at once: one
fanning out a refactor, another fixing docs, a third writing tests. Without any
coordination between them:

- Agent A starts rewriting `src/api.rs` right as Agent B edits the same file for
  something unrelated. One of them loses work.
- Agent B renames a function Agent A was about to call. Agent A finds out the
  hard way, thirty minutes into a build failure.
- Neither agent has any way to say "I'm working on this" or "heads up, I changed
  the signature of X" without you personally relaying it.

pact doesn't prevent any of this by force. It gives agents a shared, lightweight
vocabulary to avoid it on their own.

```mermaid
flowchart LR
    A[Agent A] -->|"lease · msg · watch"| P(pact)
    B[Agent B] -->|"lease · msg · watch"| P
    P --> G["AGENTS.md<br/>the protocol agents read"]
    P --> F[".pact/<br/>leases · watches · event log"]
    P --> D["Beads CLI (bd)<br/>messages, as issues"]
    D --> M[["messages, as issues"]]
    P -.->|"on lease release,<br/>the diff goes to watchers"| D
```

Everything pact owns is a file in the repository, and the one thing it does not
own — messages — it hands to a tool that already exists. The dotted edge is the
only automatic step: releasing a lease delivers what you changed to whoever
subscribed to that path.

## Why these four primitives

### Onboarding, because a protocol nobody reads is not a protocol

An agent can only follow a convention it has been told. `pact init` writes the
protocol into the files agents already read at the start of a session, so you
never explain it again — and points every other instruction file in the repo
back at that one copy, because two copies drift and only one of them can be
checked for staleness.

The failure this prevents is silent: an agent that never learned the protocol
skips leases and messaging entirely, and looks exactly like a fleet that never
started. That is why `pact doctor` has an opinion about whether the protocol is
current, reachable, and would survive a clone.

`init` is also bound by the protocol it writes: it refuses to rewrite an
instruction file that is under a live lease. A tool that tells every agent to
lease what it writes, and then writes through those leases itself, is teaching
that the protocol is optional.

→ [docs/onboarding.md](docs/onboarding.md)

### Leases, because advisory beats mandatory when the participants crash

A lease is a claim on a path, not a lock the filesystem enforces. Coding agents
fail in ways a mandatory lock cannot prevent — crashing mid-edit, ignoring the
tool, editing through a channel pact never sees. A claim that expires on its own
or can be stolen degrades to "nothing happened"; a real lock degrades to a stuck
repository nobody can unblock.

So the design goal is not exclusion, it is making the check cheap enough that
agents actually do it, and making the answer honest enough to act on: who holds
this, for how long, and what are they doing.

→ [docs/leases.md](docs/leases.md)

### Messaging, because a human relay does not scale

Agents need to hand off context — a renamed function, a changed contract, a
defect in someone else's file. pact does not run a message store: a message is a
[Beads](https://github.com/gastownhall/beads) issue, threaded via parent links.
One less thing to keep consistent, and the messages live where the project's
other work items already do.

Three properties came from watching this fail. Read state is shared rather than
local, because a sender who cannot see whether a decision landed re-sends it.
A message can be addressed to a *path* rather than a name, because names belong
to processes that exit while the work stays. And since that first property
invites a re-send, a new thread's id is derived from its own content on `bd`, so
the retry lands on the message it is repeating instead of minting a second one —
advice that manufactures duplicates is worse than no advice.

→ [docs/messaging.md](docs/messaging.md)

### Watching, because asking agents to remember is not a mechanism

The protocol has always asked agents to announce an interface change by hand.
That request was tuned twice, in opposite directions, and overshot both times.
Unrestrained, one fleet run produced 85 messages of which 41 were status pings,
and a real `BLOCKER` sat unread for 38 minutes inside the noise. Restrained —
"the lease note is the announcement; message only when you need something back"
— the next three runs sent four messages between 28 agents, and the collapse
took the load-bearing ones with it. One of those four is the only reason a
runtime panic did not ship.

The lesson is not that agents are lazy. It is that a voluntary step off the
critical path is bimodal under prose: spam or silence, with no reachable middle.
So `pact watch` asks for nothing at announce time. You subscribe to a path once,
and the diff is delivered as a side effect of `pact lease release` — a command
those same runs performed 31 times out of 31. Adherence stops being aspirational
and becomes structural.

There is no daemon and nothing that waits: a subscription is a registry entry,
and release does the lookup and the sending before it exits.

→ [docs/watch.md](docs/watch.md)

## What pact deliberately is not

**No daemon.** Every command reads state, maybe changes it, and exits. A daemon
is one more thing that crashes, drifts out of sync, or has to be restarted before
anyone can work — and pact would then be part of the problem it exists to solve.

**No MCP *write* path**, though there is a read-only MCP server. Build it with
`--features mcp` and an observer that cannot run shell commands — an
orchestrator, a status pane — can ask who holds what, what is unread, and whether
the fleet is still moving. It cannot acquire a lease or send a message, and that
is not a gap to close: a lease is a promise made *by a named agent doing the
work*, so a claim no process stands behind is worse than no claim at all. The
no-daemon line above still holds — the client spawns it on stdio and ends it by
closing stdin. See [docs/mcp.md](docs/mcp.md).

**No mandatory locking.** See above: advisory degrades safely, mandatory
deadlocks at 2am with nothing to ask why.

**No config file.** Every knob is a flag, an environment variable, or a file
under `.pact/`. Configuration is a second description of your intent that can
disagree with the first.

**No Windows build.** The coordination model assumes unix semantics rather than
merely running on unix: a lease claim depends on `rename` and `hard_link`
atomicity guarantees POSIX makes and Windows does not, and the protocol pact
writes into your instruction files points agents at sh-based commands. A Windows
binary would compile and then be wrong in the one place correctness is the whole
point, so releases ship four unix targets — statically linked on Linux, because a
coordination tool that needs a matching glibc is one more thing to get right
before anyone can work.

**No cross-machine coordination.** Everything is files on one filesystem, so
two agents coordinate when they can see each other's `.pact/`. Several
`git worktree`s of one repository *do* share it — they are one repository being
edited from several directories, and isolating their leases would produce
advisory locks that advise nobody. Two clones on two machines do not, and closing
that gap needs a consensus story pact has no business inventing.

**No database of its own.** Leases are files, history is an append-only log,
messages are Beads issues, and identities are derived from all three rather than
registered. Nothing to migrate, and nothing that can be out of date with itself.

The reasoning and the boundaries are in
[docs/architecture.md](docs/architecture.md).

## Where these features came from

Almost nothing here was designed on a whiteboard. Four repositories have been
built by agent fleets coordinating through pact, and a fifth fleet was pointed at
pact itself — every agent required to report, with quoted commands and exit codes,
what pact had actually done to it. Every finding became a tracked issue citing
evidence rather than an opinion.

Four examples of the shape that takes:

- `pact lease ls` used to lead with remaining TTL. A lease 80 seconds old printed
  `3520s`, an operator read that as "long-held", and force-released a live agent's
  claim. Hence age first, an explicit state, and the holder's note.
- `pact msg inbox` used to print every body in full and never showed a sender.
  Seven messages were ~9KB of an agent's context. Hence one line each.
- `pact msg inbox | head -1` panicked and exited 101. The message had already been
  sent; the agent read the status, concluded failure, and sent again. Hence a
  closed pipe that changes nothing about the exit code.
- 51 of 59 messages in one run were never read, because they were addressed to
  agents that had already exited. Hence delivery that follows the file.

**The habit is the point:** if you run agents against your own repo, ask each one
what the tooling did to it, and require a quoted command as evidence.

The full evidence, and the design decisions each run forced, is in the studies:

| | |
|---|---|
| [studies/field-runs.md](docs/studies/field-runs.md) | the four repositories built on pact — [arkanoid](https://github.com/chussenot/arkanoid), [megablast](https://github.com/chussenot/megablast), [grimcast](https://github.com/chussenot/grimcast), [crucible](https://github.com/chussenot/crucible) — what each measured and what changed |
| [studies/dogfooding.md](docs/studies/dogfooding.md) | building pact with pact: the CLI findings, and the two protocol reversals that overshot in both directions |
| [studies/experiments.md](docs/studies/experiments.md) | the soak, the fault injector, TLA+ and the property search — what each bounds, and why a green run alone proves nothing |

## Documentation

| | |
|---|---|
| [install.md](docs/install.md) | `mise use -g github:chussenot/pact@latest`, downloading a release or building from source, choosing a Beads backend |
| [cli.md](docs/cli.md) | every command, flag, exit code and `--json` shape |
| [onboarding.md](docs/onboarding.md) | what `pact init` writes, to which files, and how to check it |
| [leases.md](docs/leases.md) | the lease lifecycle: TTL, grace period, steal vs. expiry, path identity |
| [messaging.md](docs/messaging.md) | how messages map onto Beads issues, threading, read state, addressing a path |
| [architecture.md](docs/architecture.md) | how pact, agents and Beads fit together, and the non-goals in full |
| [mcp.md](docs/mcp.md) | the optional read-only MCP server: the six tools, and why it cannot write |
| [tui.md](docs/tui.md) | `pact ui` — tabs, keybindings, and the `ui` build feature |
| [telemetry.md](docs/telemetry.md) | the optional OpenTelemetry export: what leaves the machine and what never does |
| [watch.md](docs/watch.md) | `pact watch`: subscribing to paths, and why interface notification rides `lease release` instead of asking |
| [audit.md](docs/audit.md) | `pact audit`: what each check proves, and what it deliberately cannot see |
| [fleet-patterns.md](docs/fleet-patterns.md) | how to run a fleet: the orchestrated-wave topology, and the two rules that make its record trustworthy |
| [testing.md](docs/testing.md) | the fleet soak and the fault injector: how to run each, and what they cannot prove |
| [development.md](docs/development.md) | build, test, the CI gates and why each exists, the upstream canary |
| [performance.md](docs/performance.md) | what the lease hot path costs, measured — and where the time actually goes |
| [mascot-animations.md](docs/mascot-animations.md) | the mascot in `pact ui`: gestures, triggers, frame data |

Those pages answer *what* and *how*. For *what happened when this was used for
real* — the four field runs, the dogfooding findings, the synthetic harnesses —
see [the studies](#where-these-features-came-from) above.
