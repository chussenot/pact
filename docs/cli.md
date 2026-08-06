# CLI reference

Every surface pact exposes, in one place. The reasoning behind each is in the
README and in the topic pages ([leases.md](leases.md),
[messaging.md](messaging.md), [onboarding.md](onboarding.md)); this page is the
contract.

`scripts/check-docs.sh` compares the `Commands` block below against the built
binary's `--help` in both directions, so a subcommand or long flag that exists
in one and not the other fails CI. Do not hand-edit it to match a change you
have not made.

## Commands

```
pact init [--print] [--no-commit]
pact whoami
pact agents
pact lease acquire <path>... [--ttl <seconds>] [--steal] [--note <text>]
pact lease renew <path>
pact lease release <path> [--force]
pact lease release --all
pact lease ls [--all]
pact msg send (--to <agent>... | --to-owner-of <path>...) [--thread <id>] [--subject <text>] (<body> | --body-file <path|->)
pact msg inbox [--unread-only] [--full]
pact msg sent
pact msg read <id>
pact log [-n | --limit <count>]
pact doctor
pact audit [--check <double-win|stale-holds>] [--since <rfc3339|duration>] [--include-annotated]
pact ui
pact mcp serve
```

Plus `pact -V` (bare version) and `pact --version` (version plus build stamp —
see [install.md](install.md#which-binary-am-i-running)).

Two of those exist only in a build that asked for them: `pact ui` needs the `ui`
feature and `pact mcp serve` needs `mcp`. In a build without one, the subcommand
is absent from `--help` entirely and invoking it is a usage error (exit 5, not
2 — see below). `pact --version` lists the features compiled in, which is the
fast answer to `unrecognized subcommand`. `pact mcp serve` is documented in
[mcp.md](mcp.md); it is read-only and speaks MCP on stdio.

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
| 1 | generic error — and `pact audit --check …` found something (a finding is a result, not a fault) |
| 2 | lease held by another agent (or you don't hold the lease you're releasing) |
| 3 | Beads CLI (`bd` or `br`) not found on `PATH` |
| 4 | not in a git repository |
| 5 | usage error — unknown subcommand, bad or missing flag value |

**5 exists so that 2 means only one thing.** clap emits 2 for any usage error,
which collided with "lease held by another agent" — and a wrapper branching on 2
read a typo as a lease conflict and went off to negotiate with a peer that does
not exist. Two agents hit that in one fleet run: an unrecognized subcommand, and
a `--thread` left valueless by shell word-splitting. The flag case is the likelier
one in a script, because a flag value is exactly what gets interpolated from a
variable. `pact --help` and `pact -V` still exit 0; bare `pact` is a usage error
and exits 5, so a script whose variable expanded to nothing cannot read it as
success.

`pact doctor` exits 1 when a check **fails** (`✗`). A check can also **warn**
(`!`) — it passed, but you should know: a Beads CLI outside its tested version range,
or protocol files a clone won't see. Warnings never change the exit code, and
`--json` carries them as `"warn": true` alongside `"ok": true`, so a script can
tell the two apart. `pact whoami` is the one command that always exits 0: a
missing identity, a missing `bd`, or an unreadable repo root are reported as
`!` problems, not raised.

**A closed pipe is not one of these codes.** `pact … | head -1` used to panic
mid-write and exit 101, which an agent reading only the status could not tell
from "the send failed" — so it retried, and the fleet got duplicate messages.
pact now drops the unwritten bytes silently and keeps whatever status its actual
work earned, normally 0. That is deliberate rather than the conventional
SIGPIPE-emulating 141: the side effect (the bead created, the lock file written)
has already landed by the time anything is printed, and losing the tail of a
report whose reader walked away is cheaper than making a completed action look
failed.

