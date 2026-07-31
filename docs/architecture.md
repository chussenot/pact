# Architecture

pact is a coordinator, not a platform: it has no server, no daemon, and no
database of its own. Everything it does is either a file it writes under
`.pact/` at your repo root, or a command it shells out to (`bd`, for
messaging). This is deliberate — the moment coordination needs its own
long-running process, it becomes one more thing that can crash, drift out of
sync, or need babysitting. pact would rather do less and stay honest about it.

```mermaid
flowchart TB
    subgraph Agents
        A[Agent A]
        B[Agent B]
    end

    A -->|pact lease / msg / agents / whoami / init / doctor| P[pact CLI]
    B -->|pact lease / msg / agents / whoami / init / doctor| P

    P -->|reads/writes| L[".pact/leases/*.lock"]
    P -->|reads/writes| R[".pact/read.json"]
    P -->|writes| M["AGENTS.md
    (managed block)"]
    P -->|shells out to| BD[bd CLI]
    BD -->|reads/writes| DB[(Beads database)]

    style P fill:#4a5568,color:#fff
    style BD fill:#4a5568,color:#fff
```

Every box other than "pact CLI" and "bd CLI" is a plain file or an existing
tool. There's nothing in this diagram pact needs to keep alive between
invocations.

## Where state lives

All of pact's own state lives under `.pact/` at the repo root, which it finds
by walking up from your current directory looking for `.git` — the same way
`git` itself finds its repo root. That means you can run `pact` from any
subdirectory and it'll find the right place.

| Path | What | Committed? |
|------|------|------------|
| `.pact/leases/*.lock` | one JSON file per active lease | no |
| `.pact/read.json` | per-agent read/unread state for messages | no |
| `AGENTS.md` (managed block) | the coordination protocol, for agents to read | yes |

`pact init` gitignores the whole directory with a single `.pact/` line rather
than one rule per file, so anything an agent writes under `.pact/` is already
covered. Re-running `init` on a repo that has the older `.pact/leases/` +
`.pact/read.json` pair recognises them and appends nothing.

Leases and read-state are transient, per-machine, per-agent bookkeeping —
committing them would just create merge conflicts between agents that have
nothing to do with each other. The `AGENTS.md` block is the opposite: it's
the one artifact meant to travel with the repo, so every agent that clones it
learns the protocol on its own.

## Introspection: derived, never stored

Two commands answer questions *about* pact, and neither adds state.

`pact whoami` reports the identity it resolved and where it resolved it from,
the pact binary actually running (`current_exe`), the repo root, `.pact/`, and
the `bd` it will shell out to. Three properties are deliberate:

- **It never fails.** No identity, no `bd`, not in a git repo — each becomes a
  reported problem, and the command still exits 0. You run `whoami` *because*
  something else broke; it must not break too.
- **It probes `bd`, not just `bd`'s existence.** `bd --version` is happy in a
  repo with no reachable Beads database, while every `bd`-backed pact command
  fails. So `whoami` runs the query those commands actually run and reports the
  failure as a problem.
- **It creates nothing**, including `.pact/` — a read-only question shouldn't
  write. It says `(not created yet)` instead.

`pact agents` answers "who is working in this repo" with **no registry**: it
unions the identities already visible in the two places pact writes them —
lease holders (with `acquired_at`) and message traffic (`from` and `to`) — keyed
by name, and sorts by most recent sighting. There is nothing to enrol in, and
nothing to keep in sync with reality, because it *is* the reality. `bd` is
optional: without it you get the lease half, the same way `pact lease` works
without `bd`.

That derivation is also why `pact agents` distinguishes an identity that has
*acted* (held a lease or sent a message) from one that has only been *addressed*
— the latter is what a typo leaves behind, and the command marks it `?` rather
than confirming it as an agent.

## What pact deliberately doesn't do

- **No daemon or background process.** Every command is a single invocation
  that reads state, maybe changes it, and exits.
- **No MCP server.** pact is a CLI; wire it into an agent however that agent
  already runs shell commands.
- **No direct Beads database or JSONL access.** Messaging always shells out
  to `bd`, never reads `.beads/*.db` or `issues.jsonl` directly. If Beads
  changes its storage format, pact doesn't need to know.
- **No mandatory locking.** Leases are advisory — see
  [docs/leases.md](leases.md) for why that's a feature, not a gap.
- **No config file, no network I/O.** Everything is either a CLI flag, an
  environment variable (`PACT_AGENT`), or a file under `.pact/`.

## Exit codes are part of the contract

Because pact is meant to be driven by other programs (agents) as much as by
humans, its exit codes are documented behavior, not incidental:

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | generic error |
| 2 | lease held by another agent (or you don't hold the lease you're releasing) |
| 3 | Beads CLI (`bd`) not found on `PATH` |
| 4 | not in a git repository |

An agent scripting against pact can branch on these without parsing error
text — check the exit code, and only fall back to reading stderr for the
human-readable reason.

Two conventions follow from that. `pact doctor` exits 1 when a check fails, so
it works in a CI gate. And an **advisory warning never changes the exit code**:
`acquire --steal`, `release --force` on someone else's claim, and
`msg send` to an unseen recipient all write to stderr and exit 0. Warnings are
for the reader; exit codes are for the caller, and conflating them would make
every polite heads-up look like a failure.

## Further reading

- [docs/leases.md](leases.md) — the full lease lifecycle: TTL, the
  clock-skew grace period, steal vs. expiry, and the path-encoding caveat.
- [docs/messaging.md](messaging.md) — how `pact msg` maps onto Beads issues,
  and why it reconstructs threads itself instead of using `bd show --thread`.
- [docs/pact-scaffolding-prompt.md](pact-scaffolding-prompt.md) — the
  original design brief this project was built from.
