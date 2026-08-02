# ops/signoz

The SigNoz dashboard for the pact fleet, and the empirical notes behind it.
`pact-fleet.dashboard.json` is generated — edit `build_dashboard.py` and
regenerate:

```bash
python3 ops/signoz/build_dashboard.py > ops/signoz/pact-fleet.dashboard.json
```

## Importing

UI: Dashboards → **+ New** → **Import JSON** → paste the file.

API (needs an admin API key from Settings → API Keys):

```bash
curl -X POST http://localhost:8080/api/v1/dashboards \
  -H "SIGNOZ-API-KEY: $SIGNOZ_API_KEY" \
  -H 'Content-Type: application/json' \
  --data-binary @ops/signoz/pact-fleet.dashboard.json
```

> **Status:** the JSON is written against SigNoz's v4 builder-query schema and
> generated deterministically, but it has **not been round-tripped through a
> running SigNoz**. Every read endpoint on the local stack
> (`/api/v1/dashboards`, `/api/v3/autocomplete/*`, `/api/v4/query_range`)
> answers `401 {"type":"unauthenticated"}`, and no key was available. Expect to
> import it once and fix whatever the importer complains about; the metric
> names, attribute keys and attribute *values* below are all measured from real
> OTLP payloads, so the queries point at signals that exist.

## What pact actually emits

Measured, not assumed: a two-agent scenario (`alpha`, `beta`, `gamma`) was run
in a throwaway repo under `/tmp` with `--features otel`, exporting through a
recording tee that forwarded verbatim to the collector. 88 OTLP requests
captured.

### Resource attributes (both signals)

| key | value seen |
|---|---|
| `service.name` | `pact` (from `OTEL_SERVICE_NAME`) |
| `service.version` | `0.2.0` |
| `pact.agent` | `alpha` / `beta` / `gamma` — **only when `PACT_AGENT` is set and valid** |

No `host.name`, no `service.instance.id`, no `process.pid` — deliberately, per
`src/otel.rs`: a per-invocation id would mint a new metric series per CLI run.

### Metrics

Values in **bold** were seen in the capture; the rest are the remaining variants
`src/*.rs` can emit, listed so a panel's group-by is not surprised later.

| metric | type | unit | attributes |
|---|---|---|---|
| `pact.lease.transitions` | Sum | 1 | `pact.lease.outcome` = **acquired**, **released**, **conflicted**, **renewed**, **expired**, **reclaimed**, force_released, stolen, rolled_back |
| `pact.lease.hold.duration` | Histogram | ms | `pact.lease.outcome`, `pact.lease.overrun` (bool, both values seen) |
| `pact.lease.wait.duration` | Histogram | ms | *(none)* |
| `pact.msg.sent` | Sum | 1 | `pact.msg.addressing` = **to**, to-owner-of, mixed; `pact.msg.reply` (bool) |
| `pact.msg.read` | Sum | 1 | *(none)* |
| `pact.msg.read_latency` | Histogram | ms | *(none)* |
| `pact.msg.unread` | Gauge | 1 | `pact.msg.age_bucket` = lt_1m, 1m_5m, 5m_15m, 15m_1h, gt_1h |
| `pact.beads.duration` | Histogram | ms | `process.executable.name`, `pact.beads.subcommand`, `pact.outcome` = ok \| exit \| signal \| spawn |
| `pact.doctor.check.status` | Gauge | 1 | `pact.doctor.check` (11 fixed names) — value 0 fail, 1 warn, 2 pass |
| `pact.command.duration` | Histogram | ms | `pact.subcommand`, `pact.exit_code` |

### Spans

Root span is named after the subcommand (`lease acquire`, `msg send`, `doctor`,
…) and carries `pact.subcommand`, `pact.agent`, `pact.repo`, `pact.json`,
`pact.exit_code`. Children:

| span | attributes |
|---|---|
| `pact.lease.acquire` | `pact.path`, `pact.lease.ttl_secs`, `pact.lease.stolen`; status ERROR/`held` on exit 2 |
| `pact.lease.release` | `pact.path`; status ERROR/`held` when another agent holds it |
| `pact.beads.exec` | `process.executable.name`, `pact.beads.argv_shape`, `pact.beads.subcommand`, `pact.beads.version`, `process.exit.code` |
| `pact.init.write`, `pact.init.commit` | — |

`pact.path` is a **span** attribute only, never a metric label — which is why
"who blocks whom" is a traces panel and not a metrics panel.

## What Claude Code actually emits

Captured by pointing a throwaway `claude -p` session at a local OTLP sink with
`--settings` (the `env` block in `~/.claude/settings.json` overrides the
inherited environment, so exporting `OTEL_EXPORTER_OTLP_ENDPOINT` in the shell
does nothing — you must use `--settings`). Decoded with `protoc --decode_raw`.

**Resource attributes:** `host.arch`, `os.type`, `os.version`,
`service.name=claude-code`, `service.version`. That is the whole list — no
`host.name`, no session identity at the resource level.

**Metrics** (`claude_code.*`), all carrying the same data-point attributes
`user.id`, `user.account_uuid`, `user.account_id`, `user.email`,
`organization.id`, `session.id`, `terminal.type`:

- `claude_code.session.count` (attrs `start_type=fresh`, `source=cli`)
- `claude_code.cost.usage` — USD, `model`
- `claude_code.token.usage` — `type` = input \| output \| cacheRead \| cacheCreation, `model`
- `claude_code.active_time.total`

**Log records** (`event.name`): `claude_code.user_prompt`,
`claude_code.assistant_response`, `claude_code.api_request`,
`claude_code.hook_execution_start`, `claude_code.hook_execution_complete`,
`claude_code.hook_registered`, `claude_code.plugin_loaded`,
`claude_code.mcp_server_connection`. Every record carries `session.id`,
`event.timestamp`, `event.sequence` plus the same user/org identity.

**No traces.** `OTEL_TRACES_EXPORTER` is not set in `~/.claude/settings.json`,
and setting it to `console` produced nothing.

## The join

**Today there is no join key. Tomorrow there can be, for one line of code.**

Nothing pact emits appears in Claude Code's telemetry and vice versa:

| | pact | Claude Code |
|---|---|---|
| service.name | `pact` | `claude-code` |
| agent identity | `pact.agent` (resource) | — |
| session identity | — | `session.id` (data point / log record) |
| host identity | — | `host.arch`, `os.type`, `os.version` (not unique) |
| user identity | — | `user.id`, `user.email`, `organization.id` |

So "did this agent burn tokens waiting on a lease?" is currently answerable only
by eyeballing two panels on a shared time axis, which stops working the moment
two agents run concurrently — which is pact's entire premise.

### The key exists in the environment, already

Claude Code exports `CLAUDE_CODE_SESSION_ID` into the environment of **every
subprocess it spawns**, including `pact`. Verified: a nested `claude -p` session
reported `CLAUDE_CODE_SESSION_ID=18886d2a-f31f-41ce-94f9-8694ac635753` from
`printenv`, and the OTLP payload that same session exported carried
`session.id = 18886d2a-f31f-41ce-94f9-8694ac635753`. They are the same value.

**What pact would have to emit:** one more resource attribute in
`src/otel.rs::State::new`, alongside the existing `pact.agent`:

```rust
if let Some(id) = env("CLAUDE_CODE_SESSION_ID").filter(|s| is_uuid(s)) {
    resource.push(("session.id", Val::Text(id)));
}
```

`session.id` rather than a pact-flavoured name, so SigNoz groups the two
services on one key with no aliasing. The UUID filter matters for the same
reason `pact.agent` is validated: a resource attribute is folded into metric
series identity, and an unvalidated environment variable is a cardinality bomb.
Bounded by "one value per Claude Code session", which is the right cardinality —
it is exactly the thing being counted.

Caveat to state when this lands: a pact command run by a human in a terminal has
no `CLAUDE_CODE_SESSION_ID`, so the attribute is absent, not empty — those rows
simply do not join, which is correct.

## Panels, and what each showed on the scenario data

Numbers below are from the recorded OTLP, computed with `jq` over the captured
payloads (see `.pact/evidence-otel/dash.md` for the commands).

| panel | what it showed |
|---|---|
| Lease traffic by outcome | acquired 7, conflicted 4, released 3, expired 1, reclaimed 1, renewed 1 |
| Leaked leases (A−B) | **7 − 3 = 4**, and `pact lease ls` at the end showed exactly 4 held: `src/b.rs` (alpha), `src/d.rs` (beta), `src/e.rs` + `src/f.rs` (gamma). The gap is the leak. |
| Who is blocked, on which path | 4 failed `pact.lease.acquire` spans: beta×2 on `src/a.rs`, beta×1 on `src/d.rs`, gamma×1 on `src/b.rs` |
| Time blocked before winning | one sample, 51.7 ms (scripted scenario; a real fleet's is minutes) |
| Lease hold time / overrun | `overrun=false` p95 ≈ 2.4 s, `overrun=true` one sample at 72.6 s (the 1-second-TTL lease left to rot) |
| Messages sent vs read | sent 5, read 1 — a 4-message gap on a 3-agent scenario |
| Unread by age | 3 unread, all in `lt_1m`; the four older buckets reported 0, which is the point (an unreported bucket would read as permanently full) |
| Read latency | one sample; p50 = p95 = same value |
| Beads latency by subcommand | 43 spawns, avg 27.9 ms, max 80.0 ms — `br create` and `br show` dominate |
| Doctor check health | 11 checks. `AGENTS.md block current` and `CLAUDE.md reaches the protocol` recorded **0 → 2** across `pact init`; the other 9 stayed at 2 throughout |
| pact command latency | `msg send` p95-ish avg 96.0 ms vs `lease acquire` 0.3 ms — messaging costs ~300× a lease, and the Beads panel says why |
| Claude Code cost / tokens | real session data, on the same time axis, **not joined** |

## Reproducing the data

```bash
cargo build --release --features otel
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
export OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=http://127.0.0.1:4318/v1/metrics
# ... then drive pact in a scratch repo.
```

Point the per-signal variables at the collector, not the global
`OTEL_EXPORTER_OTLP_ENDPOINT`: on this machine the global one is already set to
`http://localhost:4317` with `OTEL_EXPORTER_OTLP_PROTOCOL=grpc` for Claude Code,
and pact speaks `http/json` only — it reads `grpc` and correctly stays silent.
