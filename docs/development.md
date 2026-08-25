---
title: Development
description: Build, test, the CI gates and why each exists, and the upstream canary.
audience: contributors
---

# Development

How to build, test and verify pact. The gates here exist because each one
caught something: the notes say which, because a check whose motivation is
forgotten is a check someone deletes.

## Tasks

Via [mise](https://mise.jdx.dev) tasks (`mise tasks ls` to list them):

```bash
mise run build      # cargo build with every feature
mise run test       # cargo test: no features, every feature, then with NO Beads CLI
mise run fmt        # cargo fmt
mise run lint       # clippy -D warnings, every feature and then none
mise run check-docs # scripts/check-docs.sh — README/docs vs the real CLI
mise run otel       # clippy + test the opt-in features in pairs, and prove they add no dependency
mise run check      # fmt-check + lint + test + otel + check-docs, same gates as CI
mise run install    # cargo install with every feature (the binary that ends up on PATH)
```

`check-docs` walks the built binary's `--help` output rather than a hardcoded
list, and fails if [cli.md](cli.md)'s `Commands` block is missing a subcommand or a
long flag *or* documents one the CLI no longer has, if a flag taking an enum has a
different set of values there than clap accepts, if any relative link in
`README.md` or `docs/` doesn't resolve, or if a `pact doctor` check isn't named
in [docs/tui.md](tui.md)'s Doctor section. It exists because a README link pointed at a doc that had been deleted, and
nothing noticed.

The enum-value check reads **both** of clap's layouts — the compact
`[possible values: a, b, c]` and the expanded `Possible values:` list it switches
to the moment a variant carries a doc comment. Reading only one covered two flags
out of three, which is the same defect the check exists to catch.

Or run the underlying `cargo` commands directly if you don't use mise.

**Local development builds every feature**, from one list — `PACT_ALL_FEATURES`
in `mise.toml` — so no two tasks can disagree about what "all of them" means. Add
a feature to `Cargo.toml` and it goes there too. What you test is then what you
install, and an opt-in feature nobody compiles rots: `ui` proved that by breaking
while every task was green.

Neither opt-in feature costs anything unasked, which is what makes installing
them all safe: `otel` exports only when the standard `OTEL_*` variables are set
(+0 ms unconfigured, measured), and `mcp` does nothing until a client spawns
`pact mcp serve`.

A third `test` leg runs with **no Beads CLI reachable**, which is the environment
CI actually has and the one a developer machine cannot reproduce — a developer has
`bd` installed. That gap was not hypothetical: exit 3 once had two causes ("no
backend" and "a bare worktree cannot message"), a test asserted the topology one,
and without a backend the other won. It passed locally and failed on every CI push
for four days while local runs were reported as green.

Since 0.9.0 the leg asserts something stronger and simpler: **every pact command
works with no Beads CLI at all.** A bare worktree can message, `msg` spawns
nothing, and the only `bd` call left is the `--version` that `doctor` and `whoami`
report. Keep the leg — it is the only thing standing between that claim and a
future change that quietly reintroduces the dependency.

The **default** build is still gated locally, not only in CI, because some checks
exist only there. `lint` runs clippy with every feature and then with none;
`test` runs the suite both ways. `tests/mcp_absent.rs` is gated
`not(feature = "mcp")` and asserts `pact mcp serve` does not exist without its
feature — an all-features run skips it silently, and it is the one build where
that assertion means anything. The default leg runs *first* on purpose: both legs
write the same `target/debug/pact`, so the last one wins, and a featureless
binary left there is how `./target/debug/pact ui` starts answering "unrecognized
subcommand" for no visible reason.

`mise run otel` deliberately uses feature *pairs* (`ui,otel`, `mcp,otel`) rather
than the full set. That is the opposite job: a `#[cfg]` item that only compiles
when two features are on is invisible to an all-features build and breaks for
whoever enabled just one.

`mise run check` runs its legs **serially**, which is slower and deliberate.
`depends` would run them in parallel, and every leg reaches the binary through
`target/debug/pact` — one path, owned by whichever build finished last. In
parallel, one leg overwrites the artifact another leg's integration tests are
mid-spawn on: `check` failed three times out of three with
`tests/mcp_absent.rs` seeing a binary that *had* the `mcp` subcommand, because
the all-features `test` leg had just replaced it. Serial is also what makes local
match CI, which is serial within a job.

That sharp edge is worth knowing before writing an integration test:
`CARGO_BIN_EXE_pact` is a shared path, not a per-feature-set artifact. Tests that
assert behaviour are immune; one that asserts a feature is *absent* is not, so
`mcp_absent` checks the `features:` line of `pact --version` before trusting what
it spawned and skips loudly if another build got there first.

`otel` is guarded the same way, and for the same reason `ui` needed it: an
off-by-default feature that nothing compiles rots. Its one load-bearing line is
the dependency comparison, because "the exporter adds nothing" is a promise
that has to be enforced rather than remembered:

```bash
test "$(cargo tree --edges normal,build,dev)" \
   = "$(cargo tree --edges normal,build,dev --features otel)"
```

State lives under `.pact/` at the repo root (found by walking up to `.git`):
`.pact/leases/*.lock`, `.pact/events.jsonl` (the bounded lease-event log behind
`pact log`), `.pact/messages.jsonl` and `.pact/read/<agent>.json`. `pact init`
writes `.pact/*` plus `!.pact/events.jsonl` and `!.pact/messages.jsonl`, so
everything under there is ignored except the two files that are history — and
anything else an agent invents is covered without a new rule.

## Beyond the gates: the fleet soak

`mise run fleet` runs scripted workers through the coordination protocol at
concurrency, to soak-test the primitives under contention. It is deliberately not
part of `check` — minutes long, probabilistic, and it needs a Beads CLI. See
[testing.md](testing.md), which also states plainly what it cannot prove.

## Opt-in features

Two features are off by default, and both are asserted to add **zero**
dependencies — `mise run otel` and CI compare `cargo tree --edges
normal,build,dev` with and without each of `otel`, `mcp` and `mcp,otel`, and
require the trees to be character-for-character equal.

| Feature | Adds | Built by |
|---|---|---|
| `ui` | `pact ui`, the ratatui dashboard (this one *does* add ratatui) | every mise task |
| `otel` | OpenTelemetry export, hand-rolled OTLP/HTTP+JSON over `std::net` | `mise run otel`, CI |
| `mcp` | `pact mcp serve`, hand-rolled JSON-RPC over stdio | `mise run otel`, CI |

The zero-dependency rule is why both are hand-written rather than built on an
SDK: `opentelemetry-otlp` pulls tonic and tokio in every feature combination
(measured — see `src/otel.rs`), and an MCP SDK would charge an async runtime for
newline-delimited JSON-RPC framing that fits in fifty lines of `serde_json` and
`std::io`.

Each needs its own CI leg because a feature nobody compiles rots: `ui` went
unbuilt long enough to break, which is the reason these legs exist at all. `mcp`
additionally needs the *default* build tested, because the thing to prove there
is an absence — `tests/mcp_absent.rs` asserts `pact mcp serve` does not exist
without the feature, and it is gated `not(feature = "mcp")` so it runs in exactly
the build it describes.

## Canary: pact against a real Beads CLI

> **Being repointed (pact-as5.7). Everything below describes what the canary
> asserted while messages were bd beads.** Most of those assumptions no longer
> exist: pact does not write to bd, so there is no send round-trip to replay and no
> version warning to fire. The one coupling left to protect is that pact's *read*
> of `.beads/interactions.jsonl` stays tolerant of what a real bd writes. Until
> that bead lands, treat this section as history and expect the `doctor`
> version-warning leg to fail — the behaviour it asserts was removed in 0.9.0.

`tests/cli.rs` stubs `bd`. That is right for a unit suite, and it means nothing
checks the assumptions pact makes about *somebody else's* CLI: `--include-infra`,
`--parent` threading, whether `--json` hydrates `labels`, and the shapes those
commands return. Each was verified by hand against one version, once, and
trusted from then on.

So a scheduled workflow installs a real `bd` release and runs
[`scripts/canary.sh`](../scripts/canary.sh) against it: `bd init`, `pact init`,
`pact doctor`, a two-identity message round-trip (send → inbox → read →
`--unread-only` is empty → the sender can see it was read), a lease
acquire/list/release, and one assertion about what `bd` must *not* do: that it
performs no git operations in the main worktree. Weekly, plus
`workflow_dispatch`.

That last one guards a decision rather than a behaviour. pact runs the Beads
backend in the main worktree so every linked worktree shares one store, which
means a sibling worktree causes `bd` to run in a checkout someone else may be
working in. Measured, `bd` neither commits nor touches the index there, so pact
ships no mitigation — and the canary stages decoy work, sends a message from a
real linked worktree, and fails if `HEAD` moved, if staging changed, or if an
index lock was left behind. See
[architecture.md](architecture.md#what-that-routing-does-not-do-and-why-it-is-checked-weekly). It is **not** in `ci.yml`
and **not** a required check — it depends on a third party's release process and
the network, and a canary that can block a merge gets disabled the first time
upstream has a bad day.

It runs two legs, and the pairing is the diagnosis:

| pinned | latest | what it means |
| --- | --- | --- |
| pass | pass | nothing to do |
| **fail** | **fail** | **pact broke.** The version pact claims to support no longer works. Look at your own change first. |
| pass | **fail** | **upstream drifted.** A newer `bd` changed something pact depends on. Decide whether to adapt or to widen the tested range. |
| **fail** | pass | odd, and worth reading closely — usually a flaky run or a yanked release rather than a real signal. |

*pinned* is the newest release inside pact's tested range; *latest* is whatever
`bd` shipped most recently. The range used to be read out of `TESTED_BD_MIN` /
`TESTED_BD_MAX_EXCLUSIVE` in `src/beads.rs` rather than restated in the workflow,
because a second copy of it drifts from the code it describes — which was the
failure the canary existed to catch. **Those constants no longer exist**: with no bd
call on any pact command's path there is nothing for a version window to gate, so
the two legs are now just "a pinned release" and "the newest release".

The *latest* leg used to assert that a `bd` outside the tested range made
`pact doctor` emit its version warning — the difference between "the warning logic
passes a unit test" and "the warning fires on real drift". That warning was removed
in 0.9.0, so that assertion now fails on a healthy build. It is pact-as5.7's to
replace.

A failure opens an issue labelled `canary` with the leg, the `bd` version and a
link to the run — or comments on the open one, so a persistent break is one
thread rather than a weekly pile. Run it locally with `scripts/canary.sh`; it
needs `bd`, `jq` and `git`.

## Releasing: push version tags one at a time

`release.yml` triggers on a version tag push. **GitHub creates no tag push events
at all when more than three tags arrive in one push** — no error, no skipped run,
nothing in the Actions tab to notice the absence of.

This repository ran the experiment by accident:

| pushed together | Release runs fired |
|---|---|
| 0.11.1, 0.12.0 | 2 |
| 0.13.0, 0.14.0, 0.15.0, 0.15.1 | 0 |

Master was pushed each time a version landed, so CI fired on all four and every
individual signal stayed green. The tags accumulated locally and went up together
afterwards, and four versions sat on master with no release for five days. A
downstream project proving out 0.15 was running a 0.12.0 binary the whole time,
because `mise install` and `gh release list` both agreed 0.12.0 was the newest
thing there was.

So: `git push origin <tag>`, one per tag.

[`.github/workflows/release-drift.yml`](../.github/workflows/release-drift.yml)
is the backstop — a nightly job that diffs the version tags on master against
`gh release list` and goes red on any tag with no release. It reports and does
not publish: republishing a tag has a blast radius, and the failure this catches
is "nobody looked", which a red job fixes. Re-push a missed tag on its own and
`release.yml` picks it up.
