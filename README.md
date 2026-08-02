# pact

**pact** is a small, dependency-light CLI that helps multiple coding agents —
Claude Code, Codex, or anything else that can run shell commands — work on
the same repository without stepping on each other.

## The problem

Say you're running two or three agents against the same repo at once: one
fanning out a refactor, another fixing docs, a third writing tests. Without
any coordination between them:

- Agent A starts rewriting `src/api.rs` right as Agent B edits the same file
  for something unrelated. One of them loses work.
- Agent B renames a function Agent A was about to call. Agent A finds out
  the hard way, thirty minutes into a build failure.
- Neither agent has any way to say "I'm working on this" or "heads up, I
  changed the signature of X" without you personally relaying it.

pact doesn't prevent any of this by force — it gives agents a shared,
lightweight vocabulary to avoid it on their own: **leases** to claim files,
**messages** to hand off context, and **onboarding** so every agent learns
the protocol without you explaining it each session.

```mermaid
flowchart LR
    A[Agent A] -->|lease / msg| P(pact)
    B[Agent B] -->|lease / msg| P
    P --> F[".pact/ (leases, event log)"]
    P --> G["AGENTS.md (protocol)"]
    P --> D["bd (Beads)"]
```

## Core features

### Onboarding — teach every agent the protocol once

`pact init` writes a short block into `AGENTS.md`, between
`<!-- pact:begin -->` / `<!-- pact:end -->` markers. Every agent that reads
`AGENTS.md` at the start of a session — which most coding agents already
do — picks up the coordination protocol automatically, with nothing for you
to repeat by hand.

Claude Code is the exception: it loads `CLAUDE.md`, `.claude/CLAUDE.md`,
`CLAUDE.local.md` and `.claude/rules/`, and never `AGENTS.md`. So `pact init`
also puts a marked block in `CLAUDE.md` containing a single `@AGENTS.md`
import line — a pointer, not a second copy, because two copies drift and only
one of them can be checked for staleness. Three cases, all idempotent:

| `CLAUDE.md` | what `init` does |
| --- | --- |
| absent, or without the import | writes the marked block |
| already imports `AGENTS.md` by your own line | leaves the file untouched |
| is a symlink to `AGENTS.md` | writes nothing — a self-import, and already reachable |

`pact doctor` reports this as **CLAUDE.md reaches the protocol**. Without it,
a Claude-driven fleet reads no protocol at all and silently skips leases and
messaging, which looks identical to a fleet that never started.

**Use case:** you set up a new repo for multi-agent work. You run
`pact init` once and commit the result. From then on, cloning the repo and
pointing any agent at it is enough; re-running `pact init` after upgrading
pact keeps the block current without touching anything else you've written
in `AGENTS.md`.

The protocol itself is short:

- **Identity** comes from `PACT_AGENT` (or `--agent <name>`) — pact never
  guesses one for you. `pact whoami` shows what it resolved.
- **Announce intent before you research, not just before you write**:
  `pact msg inbox`, then a message saying what you're about to work on, then
  `pact lease acquire <path> --note "<what>"` — before you open the first
  file. A peer planning against the same file can renegotiate now instead of
  at the end, when both plans are sunk cost.
- **Lease before you edit** a file another agent might touch, and
  `pact lease renew <path>` if the task outlasts the TTL. Several paths in one
  `acquire` are taken all-or-nothing.
- **Release when done**: `pact lease release <path>`, or
  `pact lease release --all` so nothing gets half-forgotten.
- **Announce interface changes**: `pact msg send --to <agent> "..."`, after
  checking the recipient exists with `pact agents`. Repeat `--to` to tell
  several agents in one thread.
- **Everything is scriptable**: every command supports `--json`.

`pact init --print` writes the block to stdout instead of `AGENTS.md`, which
is the honest way to see what your agents are actually being told.

### Leases — claim a file before you edit it

A lease is an advisory claim on a path, backed by an atomic lock file under
`.pact/leases/`. "Advisory" means pact doesn't stop anyone from editing an
unleased file — it makes checking cheap enough that agents actually do it.

**Use case:** two agents are both told to "clean up the auth module."

```mermaid
sequenceDiagram
    participant A as Agent A
    participant L as .pact/leases/
    participant B as Agent B

    A->>L: pact lease acquire src/auth.rs
    L-->>A: acquired (900s TTL)
    B->>L: pact lease acquire src/auth.rs
    L-->>B: exit 2 — held by agent-a (12s old, 888s remaining)
    Note over B: picks different work instead
    A->>L: pact lease release src/auth.rs
    B->>L: pact lease acquire src/auth.rs
    L-->>B: acquired
```

If Agent A crashes instead of releasing, the lease expires on its own (TTL
plus a clock-skew grace period) and Agent B's next `acquire` steals it
automatically. `--steal` forces a takeover even before expiry, for when a
human (or another agent) knows better than the lease does.

Two commands exist because a real fleet needed them: `pact lease renew <path>`
refreshes a lease a long task would otherwise outlive, and
`pact lease release --all` frees everything one agent holds in a single call,
so an agent finishing up can't half-forget (it reports only the leases it
really held — an already-expired one is swept from disk but not claimed as a
release). `acquire` takes several paths at once, all-or-nothing:

```
$ pact lease acquire src/parser.rs src/main.rs --note "new module + its mod line"
took 2 lease(s) for cli-wire:
  acquired src/parser.rs
  acquired src/main.rs
```

If any path is held by someone else, none are taken — the ones already grabbed
in that call are rolled back and the error names the path you have to negotiate
over. An agent that needs a module *and* the line that registers it can claim
both or neither, instead of sitting on half a change.

`pact lease ls` leads with the
lease's age, an `active` / `stale` / `expired` state, and the holder's
`--note`:

```
PATH         AGENT     HELD    STATE                       NOTE
src/main.rs  cli-wire  13m35s  active                      wiring the new CLI surface
slow.rs      agent-a   1m15s   stale (reclaimable in 15s)  long refactor
```

See [docs/leases.md](docs/leases.md) for the full lifecycle, what `stale`
means, the path-encoding caveat, and which commands garbage-collect (only
`lease ls` and `acquire`; read-only commands no longer do).

### Messaging — hand off context without a human relay

`pact msg` is a thin wrapper over the [Beads](https://github.com/gastownhall/beads)
CLI (`bd`): a message is a Beads issue of type `message`, threaded via
parent-child links. pact doesn't run its own message store.

**Use case:** Agent A renames a function; Agent B is mid-task on a caller of
that same function.

```mermaid
sequenceDiagram
    participant A as Agent A
    participant BD as bd (Beads)
    participant B as Agent B

    A->>BD: pact msg send --to agent-b "renamed foo() to bar()"
    BD-->>A: sent msg-123
    Note over B: starts its next task
    B->>BD: pact msg inbox
    BD-->>B: msg-123 — "renamed foo() to bar()"
    B->>BD: pact msg read msg-123
    BD-->>B: full thread, marked read
    B->>BD: pact msg send --to agent-a --thread msg-123 "thanks, updated my callers"
```

`pact msg inbox` prints one line per message — sender, subject, and the head
of the body, with `*` marking unread — so checking your mail costs an agent a
few hundred tokens instead of every body in full:

```
ID                FROM       SUBJECT                                          BODY
pact-wisp-06l  *  lease-fix  src/lease.rs is ready to wire (rnc.8/9/10/11)    src/lease.rs done, contract exactly as frozen. To wire: lea…
pact-wisp-6jz     msg-fix    src/msg.rs ready: Message.from + all_messages()  src/msg.rs is done and compiles clean on its own. Contract …

2 message(s), 1 unread (*) — `pact msg read <id>` for the full text
```

`pact msg read <id>` (or `pact msg inbox --full`) prints the full text with
the envelope: from, to, subject, time, thread. `--body-file <path|->` reads
the body from a file or stdin, so a multi-paragraph message full of quotes,
backslashes and `->` never has to survive a shell.

**One decision, one thread, however many agents.** Repeat `--to` and the
recipients' messages are stitched into a single conversation, so a fleet-wide
announcement is one thing to read and reply to instead of N near-duplicates:

```
$ pact msg send --to cli-wire --to human --subject probe --body-file -
sent 2 message(s) in thread pact-wisp-8mz
  pact-wisp-8mz → cli-wire
  pact-wisp-8mz.1 → human
```

`pact msg read pact-wisp-8mz` then returns the whole announcement. A single
`--to` prints the old one-line form and is otherwise unchanged.

**`pact msg sent` is the outbox**, and its marker answers the sender's actual
question — whether the *recipient* has looked, not whether you have:

```
$ pact msg sent
ID                TO         SUBJECT                                          BODY
pact-wisp-mbw  *  human      docs-writer done: docs match the binary          docs updated from the built binary, not the…
pact-wisp-8mz     cli-wire   probe                                            probe body with a table…

2 message(s), 1 not read yet (*) by the recipient
```

That is possible because read state moved out of a local file and into shared
`bd` labels: an agent that reads a message labels the bead `read-by-<agent>`, so
every reader is visible to everyone — including the sender. Previously read
state was per-machine bookkeeping in `.pact/read.json`, which meant a sender
could never tell whether a decision had landed, retried on a false negative, and
delivered the same notice four times. The local file is *gone*, not
supplemented: one source of truth, in the place the message already lives.

Sending to a name nobody has ever acted under prints a warning — with a
suggestion, if one is close — and **sends anyway**:

```
$ pact msg send --to tuidev "..."
warning: no agent named "tuidev" has acted in this repo (no lease, no message
sent) — did you mean tui-dev? (sending anyway)
sent pact-wisp-tdv
```

See [docs/messaging.md](docs/messaging.md) for how this maps onto Beads
issues, why the warning is advisory, and why it doesn't rely on Beads' own
`--thread` flag.

### `pact log` — what has been happening in this repo

`lease ls` shows the instantaneous set of claims, and a lease that was taken and
released while you were away leaves nothing behind at all — releasing deletes the
only record of it. `pact log` is the chronological view: lease events from
`.pact/events.jsonl`, messages derived from `bd`, merged into one feed, oldest
first.

```
$ pact log -n 5
WHEN       AGENT        EVENT     TARGET                DETAIL
5m02s ago  fixer        released  src/lease.rs          fixing confirmed findings rnc.13/21/4/22
1m21s ago  docs-writer  acquired  README.md             pact-rnc.23 docs sync
1m21s ago  docs-writer  acquired  docs/leases.md        pact-rnc.23 docs sync

3 event(s), oldest first
```

`-n` defaults to 30. Ages, not timestamps, because the question is "is this
happening now". The two sources are merged on parsed instants rather than string
order, since `bd` writes `…Z` and pact writes `…+00:00` and those sort
differently as bytes than as time. `bd` is optional, as it is for `pact agents`:
without it you get the lease half and a warning.

The history is deliberately asymmetric. Messages reach back as far as the Beads
database, while lease events start at the first `acquire` after this shipped — an
empty or missing feed is normal, not an error.

### `pact whoami` and `pact agents` — answer questions about pact with pact

`pact whoami` prints the identity, the pact binary, the repo root, `.pact/`,
and the `bd` it will use. It reports problems instead of raising them, so it
still exits 0 with no identity, no `bd`, or outside a git repo — a command you
run *because* something is broken must not break too.

```
$ pact whoami
agent      docs-writer  (from PACT_AGENT)
pact       /home/you/repo/target/debug/pact
repo root  /home/you/repo
pact dir   /home/you/repo/.pact
bd         bd  (bd version 1.1.0)
```

`pact agents` lists the identities pact has seen holding leases or in message
traffic — no registry, just the traces already on disk and in Beads:

```
$ pact agents
AGENT       LAST SEEN   LEASES  SENT  RECV
cli-wire    7m46s ago   1       0     3
agents-new  13m56s ago  0       3     0
reviewer ?  21h59m ago  0       0     1

? addressed but never seen acting — nobody has ever run pact under that name,
so nobody is reading its mail (usually a typo'd --to)
```

That last row is why the command exists: `reviewer` has received a message and
never acted, so its mail has been sitting unread for a day. `bd` is optional
here — without it you still get whoever holds a lease.

### `pact ui` — see and act on all of it at once

An interactive terminal dashboard (built on [ratatui](https://ratatui.rs))
over the leases table, your message inbox, and a live `pact doctor` panel,
with keyboard or mouse instead of re-typing CLI invocations. `Tab` or a
click switches views; `j/k`, the arrows, the scroll wheel, or a click
navigate; `Enter` opens a thread or releases a lease. Still a single
foreground process — no daemon, nothing left running after you quit.

```bash
pact ui
```

See [docs/tui.md](docs/tui.md) for the full keybindings reference.

## Install

```bash
mise run install   # cargo install --path . --force
```

Or manually:

```bash
cargo build --release
cp target/release/pact /usr/local/bin/  # or anywhere on your PATH
```

Requires `bd` (beads) on `PATH` for the `msg` subcommands; `init`, `lease`,
`whoami`, and `agents`, `log` and `doctor` (partially — the lease half plus a
warning) work without it. v0.1.0 targets `bd` only; `br`
(beads-rust) compatibility is a deliberate later phase.

## Commands

```
pact init [--print]
pact whoami
pact agents
pact lease acquire <path>... [--ttl <seconds>] [--steal] [--note <text>]
pact lease renew <path>
pact lease release <path> [--force]
pact lease release --all
pact lease ls [--all]
pact msg send --to <agent> [--to <agent>...] [--thread <id>] [--subject <text>] (<body> | --body-file <path|->)
pact msg inbox [--unread-only] [--full]
pact msg sent
pact msg read <id>
pact log [-n <count>]
pact doctor
pact ui
```

Every subcommand accepts a global `--agent <name>` (or `PACT_AGENT` env var)
and `--json` flag. `--all` on `release` is mutually exclusive with both
`<path>` and `--force`; `--body-file` is mutually exclusive with the positional
body. clap rejects those combinations rather than silently ignoring one.

Batching doesn't change the shape a one-path script already parses: a single-path
`lease acquire --json` still emits the lease *object* (several paths emit an
array), and a single `--to` still prints `sent <id> to <who> (thread <id>)`.
`lease release --json` now emits an object — `{"path": …, "displaced": …}` — so a
scripted caller can see whose claim a `--force` destroyed.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | generic error |
| 2 | lease held by another agent (or you don't hold the lease you're releasing) |
| 3 | Beads CLI (`bd`) not found on `PATH` |
| 4 | not in a git repository |

`pact doctor` exits 1 when a check fails. `pact whoami` is the one command
that always exits 0: a missing identity, a missing `bd`, or an unreadable repo
root are reported as `!` problems, not raised.

**A closed pipe is not one of these codes.** `pact … | head -1` used to panic
mid-write and exit 101, which an agent reading only the status could not tell
from "the send failed" — so it retried, and the fleet got duplicate messages.
pact now drops the unwritten bytes silently and keeps whatever status its actual
work earned, normally 0. That is deliberate rather than the conventional
SIGPIPE-emulating 141: the side effect (the bead created, the lock file written)
has already landed by the time anything is printed, and losing the tail of a
report whose reader walked away is cheaper than making a completed action look
failed.

## FAQ

**Why advisory locking instead of mandatory?** Coding agents already fail in
ways a mandatory lock can't prevent — crashing mid-edit, ignoring the tool
entirely, or editing through a channel pact doesn't see. A lease that can be
stolen (`--steal`) or expires on its own (TTL + a clock-skew grace period)
degrades to "nothing happened" instead of a stuck repository nobody can
unblock. Advisory coordination costs agents a moment of politeness; mandatory
locking costs you a deadlock at 2am with no daemon to ask why.

**Why no daemon or MCP server?** Because then pact would be one more thing
that can crash or drift out of sync. Every command is a single invocation
that reads state, maybe changes it, and exits — see
[docs/architecture.md](docs/architecture.md).

## Where these features came from

Almost nothing in the sections above was designed on a whiteboard. pact was
used to coordinate a real fleet of agents building something else in this repo,
and then a second fleet was pointed at pact itself — every agent required to
report, with quoted commands and exit codes, what pact had actually done to it.
The findings are the `pact-rnc` epic in this repo's Beads database
(`bd show pact-rnc`); each child bead cites evidence rather than an opinion.
A few examples, because the shape of the evidence matters more than the list:

- `pact lease ls` used to lead with remaining TTL. A lease 80 seconds old
  printed `3520s`, an operator read that as "long-held", and force-released a
  live agent's claim. Hence age-first plus an explicit state, and the note
  column — "what is this agent doing" is the question you have right before you
  reach for `--force`.
- `pact msg inbox` used to print every body in full and never showed a sender.
  Seven messages were ~9KB of an agent's context, and agents identified senders
  by reading prose in the bodies. Hence one line each, `from`, and `*`.
- One agent's `--to` typo sent a message into the void; nothing ever said so,
  and the ghost recipient sat unread for a day. Hence `pact agents`, the
  `?` marker for names that receive but never act, and the send-time warning.
- Five of five agents in the last pass opened their reports with the same
  complaint: they could not tell which pact binary they were running. Hence
  `pact whoami` printing the resolved path.

- `pact msg inbox | head -1` panicked and exited 101. The message had already
  been sent; the agent read the status, concluded the send failed, and re-sent.
  Hence a closed pipe that changes nothing about the exit code.

**The four findings the previous batch deferred have now shipped**, each because
a later fleet supplied the data-model evidence the first one lacked:
`pact-rnc.4` (one send, one thread, N recipients), `pact-rnc.7`
(`pact msg sent`), `pact-rnc.13` (`pact log`, backed by `.pact/events.jsonl`)
and `pact-rnc.17` (read state as shared `bd` labels, so a sender can see who
looked). Two residues are still open and honestly so: the owner of a *new* file
still cannot `cargo build` it when the line that registers it belongs to another
agent (`pact-rnc.21`/`pact-v66` — multi-path `acquire` helps, but the real
problem is ownership, not claiming), and the `AGENTS.md` block `pact init`
generates does not yet teach the commands above (`pact-sri`).

The habit is the point: if you run agents against your own repo, ask each one
what the tooling did to it, and require a quoted command as evidence. That is
where this list came from.

## Learn more

- [docs/architecture.md](docs/architecture.md) — how pact, agents, and Beads
  fit together, and what pact deliberately doesn't do.
- [docs/leases.md](docs/leases.md) — the full lease lifecycle: TTL, grace
  period, steal vs. expiry, path encoding.
- [docs/messaging.md](docs/messaging.md) — how `pact msg` maps onto Beads
  issues, multi-recipient threading, and read state as shared `bd` labels.
- [docs/tui.md](docs/tui.md) — `pact ui`'s tabs and full keybindings
  reference.

## Development

Via [mise](https://mise.jdx.dev) tasks (`mise tasks ls` to list them):

```bash
mise run build   # cargo build
mise run test    # cargo test
mise run fmt     # cargo fmt
mise run lint    # cargo clippy --all-targets -- -D warnings
mise run check   # fmt-check + lint + test, same gates as CI
mise run install # cargo install --path . --force
```

Or run the underlying `cargo` commands directly if you don't use mise.

State lives under `.pact/` at the repo root (found by walking up to `.git`):
`.pact/leases/*.lock` and `.pact/events.jsonl` (the bounded lease-event log
behind `pact log`). Message read state is not there — it lives in `bd`, as one
`read-by-<agent>` label per reader. `pact init` gitignores the whole directory
with a single `.pact/` line, so anything else an agent writes there is covered
without a new rule.
