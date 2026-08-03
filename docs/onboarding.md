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

