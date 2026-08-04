# MCP server (read-only)

`pact mcp serve` exposes pact's **observation** surface over the Model Context
Protocol on stdio. It answers five questions about coordination state and can
change none of it.

Off by default, twice over: it needs a build with the `mcp` feature, and even
then nothing runs until an MCP client spawns `pact mcp serve`.

```bash
cargo install --path . --force --features mcp     # or: ui,otel,mcp
pact --version | grep features                    # confirm `mcp` is in there
```

## What it is for

Every pact command assumes the asker can run a shell command. Not every observer
can. An orchestrator supervising a fleet, or a dashboard agent whose entire tool
surface is MCP, has no way to ask "who holds what, and is anybody blocked"
without a human relaying the answer — which is the failure `pact msg` exists to
remove one level down.

The intended callers are watchers, not workers:

- an orchestrator deciding whether to start a new agent on a path, or wait
- a status pane that polls `pact_lease_list` and `pact_events_tail`
- a triage agent reading an inbox to see whether a `BLOCKER` is sitting unread
- anything that would otherwise ask a human "is the fleet still moving?"

## What it is not

**Not a write path.** No lease can be acquired, renewed, released or stolen; no
message sent, replied to, or marked read; nothing written to `.pact/` or to the
Beads store. Every mutation stays on the CLI.

That is a design decision, not an unfinished feature. A lease is a promise made
*by a named agent* that it is working on a path, and its whole value is that the
promise is true — `pact log` is the record of who said they would do what. An
observer holds no files, does no work, and cannot honour a claim, so a lease
acquired on its behalf would put a claim in the log that no process stands
behind. The next agent then renegotiates against a peer that does not exist,
which is strictly worse than seeing the path unclaimed.

**Not a daemon.** There is no port, no background process and no state to keep
in sync. The client spawns pact, writes newline-delimited JSON-RPC to its stdin,
reads responses from its stdout, and closes stdin when done; pact exits 0. The
["no daemon, no server" non-goal](architecture.md#what-pact-deliberately-doesnt-do)
is intact: this is a subprocess the client owns for as long as it wants an
answer.

## Reading a message here does not mark it read

The one difference from the CLI worth stating on its own, because a caller who
gets it wrong makes a wrong decision rather than getting an error.

`pact msg read <id>` **writes**: it adds a `read-by-<agent>` label to the
message bead, and that label is what a *sender* checks — via `pact msg sent` —
to decide whether a decision landed or needs re-sending. See
[messaging.md](messaging.md) for the model.

`pact_msg_inbox` and `pact_msg_thread` return full bodies and write no label. So:

| | CLI | MCP |
|---|---|---|
| See the bodies | `pact msg read <id>` | `pact_msg_thread` |
| Marks it read for the reader | yes | **no** |
| Sender sees it as delivered afterwards | yes | **no** |
| Recipient still needs to read it | no | **yes** |

Both tool descriptions say this in the text the client shows the model, so an
orchestrator that reads an inbox to decide whether to nudge an agent knows that
looking did not count as delivery — and that the nudge may still be warranted.

The lease side has the same shape and is easier to miss: `pact lease ls`
garbage-collects expired lock files as it lists them, which is its documented
job. `pact_lease_list` uses the non-sweeping read instead, so an expired lease
can still be reported here after the CLI would have swept it. A dashboard
polling every few seconds must not be quietly reclaiming other agents' paths.

## The five tools

All five are read-only, and each says so twice: in the first words of its
description, and as machine-readable [tool
annotations](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)
a client can filter on without reading prose.

```json
{
  "name": "pact_doctor",
  "title": "Repository health",
  "description": "Read-only. Runs pact's health checks on this repository …",
  "inputSchema": { "type": "object", "additionalProperties": false },
  "annotations": { "readOnlyHint": true, "openWorldHint": false }
}
```

`openWorldHint: false` because every answer comes from this repository — files
under `.pact/` and a local Beads CLI; nothing reaches a network.
`destructiveHint` and `idempotentHint` are deliberately absent: the schema
documents them as meaningful only when `readOnlyHint` is false, so stating them
here would invite a reader to wonder which one wins. Clients are required to
treat annotations as untrusted, which is exactly why the same fact leads every
description.

| Tool | Answers | CLI equivalent |
|---|---|---|
| `pact_lease_list` | who holds which paths, for how long, and why | `pact lease ls --json` |
| `pact_msg_inbox` | what is waiting for one agent, and what is unread | `pact msg inbox --json` |
| `pact_msg_thread` | one conversation in full, bodies included | `pact msg read <id> --json` |
| `pact_doctor` | is this repository's pact setup healthy | `pact doctor --json` |
| `pact_events_tail` | what has happened, newest last | `pact log --json` |

Arguments:

- `pact_lease_list` — `include_expired` (bool, default false)
- `pact_msg_inbox` — `agent` (string, **required**), `unread_only` (bool,
  default false). Required because an observer may be watching several
  identities, so there is no sensible default.
- `pact_msg_thread` — `id` (string, **required**; any member of the thread, not
  just its root), `agent` (string, optional — whose `read` flag to report)
- `pact_doctor` — none
- `pact_events_tail` — `limit` (integer, default 50, maximum 500; larger values
  are clamped rather than refused)

Results carry the JSON twice: once as `structuredContent` and once serialized
into a text block, which is what the protocol asks of a tool returning structured
content.

`structuredContent` is always a JSON **object**, so the four tools that answer
with a list put it under a named key — `leases`, `messages`, `events` — while
`pact_doctor` already had `{checks, healthy}`:

```json
{ "leases": [ { "lease": { "agent": "worker-a", … }, "age_secs": 0, … } ] }
```

The elements are the CLI's `--json` elements unchanged, so anything that parses
`pact lease ls --json` parses `structuredContent.leases`. The wrapper is not
cosmetic: in revision `2025-06-18`, which this server advertises,
`structuredContent` is typed `{ [key: string]: unknown }`. Only `2026-07-28`
widened it to any JSON value, and a bare array — which is what pact returned at
first — is rejected outright by a client validating against the revision it
negotiated. An object satisfies both revisions, which is why every tool returns
one regardless of era.

## What a session looks like

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"pact_lease_list","arguments":{}}}
```

The `initialize` reply, verbatim:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2025-06-18",
    "capabilities": { "tools": {} },
    "serverInfo": { "name": "pact", "version": "0.3.3" },
    "instructions": "pact coordinates coding agents working on one repository. This server is strictly read-only: …"
  }
}
```

and the `structuredContent` of the `pact_lease_list` call against a repo with one
lease held:

```json
[
  {
    "lease": {
      "agent": "worker-a",
      "path": "src/api.rs",
      "acquired_at": "2026-08-03T06:23:26.571310216+00:00",
      "ttl_secs": 900,
      "note": "rewriting the response shape"
    },
    "age_secs": 0,
    "remaining_secs": 900,
    "expired": false
  }
]
```

## Registering it

pact resolves the repository from the working directory it is spawned in, so the
client must start it inside the repo. A client that spawns from somewhere else
gets exit 4 (`not in a git repository`) — set `cwd` if your client supports it,
or wrap the command.

### Claude Code

`.mcp.json` in the repository root. Claude Code spawns project-scoped servers
with the project directory as the working directory, so nothing else is needed:

```json
{
  "mcpServers": {
    "pact": {
      "command": "pact",
      "args": ["mcp", "serve"]
    }
  }
}
```

`claude mcp add pact --scope project -- pact mcp serve` writes exactly that.

### Codex CLI

`[mcp_servers.<name>]` in TOML rather than JSON. Either `.codex/config.toml` in
the repository — read once the project is trusted, which is the scope that
matches one-server-per-repository — or `~/.codex/config.toml` for every repo you
open:

```toml
[mcp_servers.pact]
command = "pact"
args = ["mcp", "serve"]
cwd = "/home/you/code/your-repo"
```

`cwd` is set even though `codex` is normally launched from the repository and the
server would inherit that directory: what Codex guarantees about a stdio server's
working directory is not written down, and the failure if it differs is exit 4 at
startup with no tool call to attach the error to. One line makes the question not
arise. Add `enabled = false` to park the server without deleting the block.

### Claude Desktop

`claude_desktop_config.json` — `~/Library/Application Support/Claude/` on macOS,
`%APPDATA%\Claude\` on Windows. A desktop client is not launched from your
repository and may not inherit your `PATH`, so give both an absolute path and a
working directory:

```json
{
  "mcpServers": {
    "pact": {
      "command": "/Users/you/.cargo/bin/pact",
      "args": ["mcp", "serve"],
      "cwd": "/Users/you/code/your-repo"
    }
  }
}
```

If your client does not support `cwd`, wrap it:

```json
{
  "mcpServers": {
    "pact": {
      "command": "sh",
      "args": ["-c", "cd /Users/you/code/your-repo && exec pact mcp serve"]
    }
  }
}
```

One server per repository — and per repository is exactly right, because that is
the scope of the state: several `git worktree`s share one `.pact/`, so a server
spawned in any of them reports the same leases, messages and events. pact has
nothing *global* to serve. See
[architecture.md](architecture.md#one-coordination-space-per-repository-not-per-checkout).

## Failures

A tool that cannot answer returns an MCP **tool error** (`isError: true`) rather
than a JSON-RPC error, so the client hands the text to the model and it can
recover. The text names the exit code the CLI would have produced for the same
condition, because those codes are pact's documented API
([cli.md](cli.md#exit-codes)) and an orchestrator should branch on the same
number either way:

- **exit 3** — no Beads CLI on `PATH`. Only the two message tools need `bd` or
  `br`; leases, doctor and the event log are plain files and keep working.
- **exit 4** — not spawned inside a git repository. This one fails at startup,
  before any tool call.

A malformed line gets `-32700` and the session continues. An unknown method or
unknown tool gets `-32601`. A request declaring a protocol version this server
does not implement gets `-32022` with the list to retry from — see
[Protocol versions](#protocol-versions).

## Protocol versions

MCP has two eras, and pact serves both. Revision `2026-07-28` removed the
handshake: rather than agreeing a version once via `initialize`, every request
declares its own in `_meta`, and `server/discover` replaces the capability
exchange. The spec calls these **legacy** (`2025-11-25` and earlier) and
**modern**, and a server doing both **dual-era**.

| | Legacy | Modern |
|---|---|---|
| Opens with | `initialize` | any request, or `server/discover` |
| Version declared in | `initialize` params, once | every request's `_meta` |
| Results carry `resultType` | no | yes |
| `tools/list` cacheable | no | yes (`ttlMs`, `cacheScope`) |

The era is decided **per request**, because the modern revisions have no session
for a connection-wide answer to live in. The five tools, their arguments and
their JSON are identical either way — all of the difference is envelope, so
nothing you build against one era needs rewriting for the other.

You do not have to care which your client speaks. If it sends `initialize`, that
works. If it probes `server/discover` first, that works:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "resultType": "complete",
    "supportedVersions": ["2026-07-28", "2025-06-18", "2025-03-26", "2024-11-05"],
    "capabilities": { "tools": {} },
    "instructions": "pact coordinates coding agents working on one repository. This server is strictly read-only: …",
    "ttlMs": 60000,
    "cacheScope": "public",
    "_meta": {
      "io.modelcontextprotocol/serverInfo": { "name": "pact", "version": "0.3.3" }
    }
  }
}
```

A version outside that list gets `-32022 UnsupportedProtocolVersionError` naming
what is supported, which is the error a modern client is told to retry from
rather than fall back on. `2025-11-25` is deliberately not in the list: it is
initialization-based and would very likely work, but pact advertises only the
revisions whose shapes were checked field by field, so a client asking for it is
answered `2025-06-18` and downgrades — which the negotiation rule provides for.

## Verifying the read-only claim yourself

The claim is tested, not asserted: `tests/mcp.rs` drives the real binary over
pipes, calls every tool that does not need `bd`, and compares a byte-and-mtime
snapshot of `.pact/` and `.beads/` taken before and after. To check by hand:

```bash
find .pact .beads -type f -exec md5sum {} + | sort > /tmp/before
# …run your client, call every tool…
find .pact .beads -type f -exec md5sum {} + | sort > /tmp/after
diff /tmp/before /tmp/after && echo "unchanged"
```

Details of how the server is built — the hand-rolled JSON-RPC, the zero-dependency
guard, the span emitted per call — are in
[development.md](development.md#opt-in-features) and `src/mcp.rs`.
