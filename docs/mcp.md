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

All five are read-only, and each says so in its own description.

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
into a text block, which is what the protocol asks of a tool returning
structured content. The shapes are the CLI's `--json` shapes, unchanged — so
anything already parsing `pact lease ls --json` parses this.

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

One server per repository. pact has nothing global to serve — leases, messages
and the event log all belong to one checkout.

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

A malformed line gets a `-32700` parse error and the session continues. An
unknown method or unknown tool gets `-32601`; that is also what a client probing
`server/discover` receives, which is the signal the spec's stdio
backward-compatibility rules tell it to read as "this server speaks the
`initialize` handshake" — see `src/mcp.rs` for which
protocol revisions are implemented and why.

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
