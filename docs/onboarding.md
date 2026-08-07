# Onboarding

`pact init` teaches every agent in a repository the coordination protocol, once,
by writing it where those agents already look. The README explains why that is
the only reliable delivery mechanism; this page is what `init` actually does to
which files, and how to tell whether it worked.

See also [architecture.md](architecture.md) for why the protocol lives in a
repository file rather than in pact, and [cli.md](cli.md) for the flags.

## What `init` writes

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

Other tools have their own instruction file, and the same failure: an agent
reading `GEMINI.md` joined a pact fleet having never been told the protocol
exists. So `init` also points **any agent-instruction file the repo already
has** back at `AGENTS.md`:

| File | What `init` writes into it |
| --- | --- |
| `GEMINI.md` | prose + an `@AGENTS.md` import line (Gemini CLI inlines `@file.md`) |
| `.github/copilot-instructions.md` | prose + `@AGENTS.md` (Copilot CLI expands it; VS Code's Copilot doesn't, so both halves are covered) |
| `.cursorrules`, `.windsurfrules`, `.clinerules` | prose only — these formats have no import mechanism, and a dangling `@AGENTS.md` reads like a broken link |

Three properties, each of which is the interesting part:

- **It never creates one.** `CLAUDE.md` stays the only file pact writes from
  nothing. Existence is the configuration: creating `.windsurfrules` in a repo
  that has never seen Windsurf would be pact inventing a tool you don't use.
  (`.clinerules` is skipped outright when it's a *directory*, which newer Cline
  allows.)
- **It's always a pointer, never a copy.** Only one file can be checked for
  staleness, so a second copy of the protocol text is a second thing that drifts
  unpoliced — see [docs/architecture.md](architecture.md#one-copy-of-the-protocol-however-many-instruction-files).
- **`pact doctor` has the same opinion about them it has about `CLAUDE.md`**, as
  **other instruction files current**. A file pact writes and never re-checks
  goes stale in silence, which is the failure the `AGENTS.md` check already
  existed to prevent.

`.cursor/rules/` is deliberately *not* managed: it would mean creating a new
`.mdc` file, and an `.mdc` without the right frontmatter is silently never
applied — a rule pact writes and Cursor ignores is worse than no rule, because
it looks done.

## `init` refuses to write through a live lease

`AGENTS.md` tells every agent to lease anything it writes, and `init` rewrites
exactly that kind of shared file — but for a long time `init` was the one
writer exempt from its own instruction, and would silently overwrite a file a
peer was mid-edit on (a reproduced bug, not a narrowed race). It now peeks for
a live lease first and refuses the whole run with **exit 2**, naming the holder:

```
error: lease on AGENTS.md is held by alice; refusing to let `pact init` write through it (use --force to override)
```

Three things are worth knowing about the shape of that refusal:

- **It is all-or-nothing, checked before any file is touched.** A refusal
  halfway through — `AGENTS.md` rewritten, then a stop on `GEMINI.md` — leaves
  the repo in a state neither agent asked for, so the check runs up front, the
  same way several paths in one `lease acquire` are taken together.
- **It covers every file `init` writes**: `AGENTS.md`, `CLAUDE.md`, every
  instruction file it would point at `AGENTS.md` (the table above),
  `.gitignore`, and `.gitattributes`.
- **Your own lease is not a peer's.** If you hold the lease `init` is about to
  write through, it proceeds without `--force` — re-entrant, the same as
  acquiring a path you already hold.
- **`--force` writes through someone else's anyway**, mirroring
  `lease acquire --steal` and `lease release --force`: overriding a live claim
  is allowed, but never as a default and never silently.

The check is a peek — `init` reads the leases, it does not take one. A bounded
rewrite-and-exit does not need a claim of its own, only the courtesy of
honouring someone else's. `--print` writes nothing, so it is never refused.

**Use case:** you set up a new repo for multi-agent work. You run
`pact init` once and commit the result. From then on, cloning the repo and
pointing any agent at it is enough; re-running `pact init` after upgrading
pact keeps the block current without touching anything else you've written
in `AGENTS.md`.

**`pact init` commits what it wrote**, so "commit the result" isn't a step
you can forget. It stages exactly `AGENTS.md`, `CLAUDE.md`, `.gitignore` and any
instruction file it just pointed at `AGENTS.md`, and commits only those —
unrelated staged work stays staged, waiting for its own
commit. Re-running finds nothing to commit rather than piling up empties, and
`--no-commit` writes the files and stops. The message is a Conventional Commit,
because a generated non-conventional subject is exactly what makes `cog bump`
fail across a whole history later.

If the commit *can't* be made — a gitignored `AGENTS.md`, no configured git
identity, a rejecting hook — the files are still written, the exit status stays
`0`, and the reason goes to stderr. An init that did its job must not report
failure.

**And because that commit is load-bearing, `pact doctor` verifies it landed.** A
gitignored `AGENTS.md` fails in the worst possible way: `pact init` writes it,
`git add` refuses it without a word, every other check stays green, and the
clone that was supposed to be onboarded gets nothing. A global `~/.gitignore`
rule did exactly that to pact's own repo for its entire history. The
**protocol files reach a clone** check asks `git check-ignore` and names the
rule to go fix:

```
! protocol files reach a clone: AGENTS.md (ignored by .gitignore:1:AGENTS.md) — `git add`
  refuses these silently, so a clone gets no protocol; if that is not deliberate,
  un-ignore them (e.g. add `!AGENTS.md` to .gitignore) and commit

all checks passed, 1 warning
```

It **warns** (`!`, exit 0) rather than failing. Keeping the protocol local to one
machine is a legitimate setup, and pact doesn't get to overrule a decision you
already made — it says so out loud instead. A check left permanently red by a
deliberate choice is one people learn to skip, which would cost exactly the
visibility it exists for. The summary counts warnings so a `!` that scrolled off
the top still gets reported. `bd` outside its tested version range warns the same
way.

Untracked-but-committable is fine and says nothing at all — that's every repo
between `pact init` and its first commit.

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
is the honest way to see what your agents are actually being told. It prints the
raw markdown and nothing else: `--print --json` is still the raw block, because
what you asked for is the text, not a report about it. Every other `init`
invocation honours `--json`, whose report carries `instruction_files` alongside
`agents_md`, `claude_md` and the commit fields.


## Recipe: one agent per `git worktree`

A worktree per agent is the natural fleet layout — each gets its own branch and
its own checkout, and none of them trip over a colleague's half-finished edit.
pact needs nothing configured for it: all worktrees of one repository share a
single `.pact/`, so leases and messages already cross between them
([why](architecture.md#one-coordination-space-per-repository-not-per-checkout)).

The one thing worth setting per worktree is the identity, because pact never
guesses one:

```bash
git worktree add -b feat/auth ../wt-auth
echo 'export PACT_AGENT=agent-auth' > ../wt-auth/.envrc   # direnv
direnv allow ../wt-auth
```

Any mechanism works — `.envrc`, the agent's own launcher, `--agent` on each
command. What matters is that the name differs per worktree: two agents sharing
one `PACT_AGENT` are one agent as far as leases are concerned, so each will
happily "re-acquire" what the other holds, which is a re-entrant refresh and not
a conflict.

Run `pact init` once, in the main worktree. It writes `AGENTS.md` and friends,
which are tracked files — every worktree on a branch that has them is already
onboarded, and committing the block from two worktrees at once is the one
avoidable merge conflict here.

Check it with `pact doctor` from inside a worktree:

```
✓ worktree: linked worktree wt-auth of /home/you/code/pact
✓ coordination scope: shared (default) — state at /home/you/code/pact/.pact
✓ state placement: main-worktree — the main worktree at /home/you/code/pact
✓ state dir writable: /home/you/code/pact/.pact is writable
```

If `worktree` says `not a worktree` from inside one, or `coordination scope`
reports `local`, your agents are not sharing anything — see
[the resolution chain](architecture.md#one-coordination-space-per-repository-not-per-checkout).

One trade-off to know: because the Beads store lives in the main worktree, the
commits `pact msg` produces land on **whatever branch the main worktree has
checked out**, not on the branch of the worktree that sent the message.
