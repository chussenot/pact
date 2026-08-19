---
title: Harness detection
description: Which program is driving an agent, which model it declared, and which session it belongs to — what pact reads, what it refuses to guess, and how to add a fingerprint for another harness.
audience: everyone
---

# Harness detection

`PACT_AGENT` says *which agent* acted. It does not say which program was running
it, which model was behind it, or which transcript the action would be found in.
This page is about the four fields that answer the rest, what pact reads to get
them, and — at least as important — what it deliberately does not.

Everything here is read with `std::env::var` and nothing else. No subprocess, no
filesystem, no API call, no probing. These fields are stamped on every event of
every kind by `events::stamp_context`, which is the lease hot path; the one
subprocess already there is isolated by name in `benches/lease.rs` precisely so a
second cannot be added without someone noticing.

## The four fields

| Field | Source | Kind |
|---|---|---|
| `harness` | `PACT_HARNESS`, else a fingerprint | observed |
| `model` | `PACT_MODEL` | **declared** |
| `harness_session` | `PACT_HARNESS_SESSION`, else a fingerprint | observed |
| `harness_subagent` | `PACT_HARNESS_SUBAGENT` | declared |

They land on `.pact/events.jsonl` rows and on `.pact/messages.jsonl` senders.
`harness` and `model` additionally land on the lock file, so a refusal and
`pact lease ls` can name what is holding a path without scanning the log.

### Declared is not observed, and pact will not blur them

`model` is declared by the launcher and pact records what it is told. It does not
fingerprint a model, ask an API which one is in force, or infer one from a
response. The spawner knows what it requested; that is where a declaration is
cheap and truthful, and everywhere else it is a guess wearing a fact's clothes.

That is why the value is marked wherever it is rendered:

- `pact audit` prints `models by events (declared)` and `model X (declared)`.
- `pact ui`'s detail views write `model X (declared)` in words.
- `pact lease ls` names the column `VIA`, not `MODEL`.

The verification story is a join, not a detection: a session record made by the
harness itself knows what actually ran, and `recount` compares the two. See
[audit.md](audit.md) for the join ladder and what a mismatch means.

### Absence is a value; `"unknown"` is not

Every field is absent rather than defaulted when nothing is known. A row with no
`model` says "nobody declared one", which is a fact about the run and dates the
log to before whoever starts declaring. A row reading `model: "unknown"` says the
same thing while looking like data, and the models-by-events summary would count
it as a model.

This is the discipline `ttl_secs`, `chain_hash` and `invoked_from` already
follow. A field that cannot distinguish "not applicable" from "not recorded" is
worse than no field.

## Empirical captures

**Everything below is reverse-engineered from a running harness. None of these
variables is documented or promised by anyone, and any of them can disappear in
any release.** That is exactly why each one degrades to absence rather than to a
guess, and why each capture is dated: when a fingerprint stops working, the date
is what tells you how old the observation was.

### Claude Code 2.1.235 — Linux — 2026-08-19

Captured by dumping the full environment from a Bash tool call inside a real
session.

```
CLAUDECODE=1
CLAUDE_CODE_SESSION_ID=0bb1638c-ef3b-454f-9ce8-c9bb6fb6d0e8
CLAUDE_CODE_ENTRYPOINT=cli
CLAUDE_CODE_EXECPATH=~/.local/share/claude/versions/2.1.235
CLAUDE_CODE_CHILD_SESSION=1
CLAUDE_PID=1313445
```

**`CLAUDECODE=1` is the harness fingerprint.** Only that exact value; `0` is a
harness saying no. `CLAUDE_CODE_ENTRYPOINT` and `CLAUDE_CODE_EXECPATH` were both
present and are deliberately *not* used — the first varies by how the session was
started and the second embeds a version, so neither is a stable answer to "which
harness".

**`CLAUDE_CODE_SESSION_ID` is the session fingerprint**, and it was confirmed to
name the transcript file: `~/.claude/projects/<encoded-cwd>/<that uuid>.jsonl`
existed.

Reading it is gated on the harness being `claude-code`, and the gate is not
bureaucracy. This variable is inherited by *every* child process a Claude Code
session ever spawns — a shell the user is driving by hand hours later, another
harness launched from inside one. Reading it unconditionally would attribute
those to a session they are not part of.

### The subagent id: measured absent

Also captured 2026-08-19, by spawning a real subagent and having it dump its
complete environment. The variable-name list was compared against the parent's in
full, not filtered.

- The `CLAUDE*` set was **identical** to the parent's.
- `CLAUDE_CODE_SESSION_ID` carried the **parent** session's uuid.
- **The subagent's own id appeared nowhere in its environment.**

That id was `a99940ee56bb11045`. Its transcript was
`<session-uuid>/subagents/agent-a99940ee56bb11045.jsonl`, whose first line reads:

```json
{"agentId":"a99940ee56bb11045","sessionId":"0bb1638c-…","isSidechain":true,
 "promptId":"af1c8be4-…","parentUuid":null,"gitBranch":"master",
 "cwd":"/home/…/pact","slug":"buzzing-marinating-boole","version":"2.1.235"}
```

with a sidecar `agent-<id>.meta.json` carrying `agentType`, `description`,
`toolUseId`, `spawnDepth` and `model`.

So the id exists on disk, and in the parent's tool-result metadata. It is not
something a subagent can read about itself from its environment.

**Consequence:** `harness_subagent` has no fingerprint on this harness, so
`PACT_HARNESS_SUBAGENT` is the only way it is ever set — a declaration, from a
spawner or a harness that knows the id. Under Claude Code, nothing does. Expect
the field to be absent here.

**pact does not go to disk for it, and neither should you.** The id names a
transcript file, so an agent could in principle find its own by rummaging through
the harness's state directory. pact will not: that layout is undocumented and
reverse-engineered, it is one refactor from breaking, and an agent reading its
harness's internals to label its own log entries is a coupling nobody wants to
own. Every function in `harness.rs` is a `std::env::var` call and nothing else —
no filesystem, no subprocess — and that is a property of the module, not an
accident of what was convenient.

Absence is the honest signal, and it costs less than it looks: `recount` falls
back to the topological join it has always used and says which tier it took. See
[audit.md](audit.md#the-recount-join-ladder).

### Observed and deliberately unused

`AI_AGENT=claude-code_2-1-235_agent` was present in both parent and subagent. It
is outside the `CLAUDE*` namespace and was set by that machine's shell profile,
not by the harness — a local convention, not a portable fingerprint.

## Testing your own fingerprint

```console
$ pact doctor | grep attribution
✓ attribution: agent=agent-01, harness=claude-code, model=<absent, declare with PACT_MODEL>, …
```

`pact doctor` resolves and prints the exact chain this process would stamp on its
next event, naming each link even when it is absent and saying what would supply
it. That is the whole reason it is in `doctor` rather than only in `whoami`: you
can check what pact resolved without acquiring a lease and reading the log back.

The check never fails. An undeclared model is not a defect and a harness pact
cannot fingerprint is not a defect. It warns in exactly one case — a harness
detected but no session id resolved — because that is the shape of a fingerprint
that used to work and has stopped, and everything downstream degrades silently by
design.

## Adding a harness

**Other harnesses have no fingerprint here yet, and one sentence of observation
is all it takes to add one.** Run its equivalent of the capture above — dump the
environment from inside a real session — find the variable that identifies the
harness and, if there is one, the variable that identifies the session, and add
them to `src/harness.rs` with a dated entry on this page. Absence of a fingerprint
is a gap in this document, not a decision about your harness.

Until then, `PACT_HARNESS` and `PACT_HARNESS_SESSION` work everywhere: the
spawner declares what it launched. See [fleet-patterns.md](fleet-patterns.md).
