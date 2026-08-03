# Development

How to build, test and verify pact. The gates here exist because each one
caught something: the notes say which, because a check whose motivation is
forgotten is a check someone deletes.

## Tasks

Via [mise](https://mise.jdx.dev) tasks (`mise tasks ls` to list them):

```bash
mise run build      # cargo build --features ui
mise run test       # cargo test --features ui
mise run fmt        # cargo fmt
mise run lint       # cargo clippy --all-targets --features ui -- -D warnings
mise run check-docs # scripts/check-docs.sh — README/docs vs the real CLI
mise run otel       # clippy + test the otel feature, and prove it adds no dependency
mise run check      # fmt-check + lint + test + otel + check-docs, same gates as CI
mise run install    # cargo install --path . --force --features ui
```

`check-docs` walks the built binary's `--help` output rather than a hardcoded
list, and fails if [cli.md](cli.md)'s `Commands` block is missing a subcommand or a
long flag *or* documents one the CLI no longer has, if any relative link in `README.md` or `docs/` doesn't resolve, or if a `pact doctor` check isn't named
in [docs/tui.md](tui.md)'s Doctor section. It exists because a README link pointed at a doc that had been deleted, and
nothing noticed.

Or run the underlying `cargo` commands directly if you don't use mise.

Every task builds with `--features ui`, so what you test is what you install.
CI runs clippy and test **both** ways — with and without the feature — so the
dependency-light default stays guarded even though no local task exercises it.

`otel` is guarded the same way, and for the same reason `ui` needed it: an
off-by-default feature that nothing compiles rots. Its one load-bearing line is
the dependency comparison, because "the exporter adds nothing" is a promise
that has to be enforced rather than remembered:

```bash
test "$(cargo tree --edges normal,build,dev)" \
   = "$(cargo tree --edges normal,build,dev --features otel)"
```

State lives under `.pact/` at the repo root (found by walking up to `.git`):
`.pact/leases/*.lock` and `.pact/events.jsonl` (the bounded lease-event log
behind `pact log`). Message read state is not there — it lives in `bd`, as one
`read-by-<agent>` label per reader. `pact init` gitignores the whole directory
with a single `.pact/` line, so anything else an agent writes there is covered
without a new rule.

## Canary: pact against a real Beads CLI

`tests/cli.rs` stubs `bd`. That is right for a unit suite, and it means nothing
checks the assumptions pact makes about *somebody else's* CLI: `--include-infra`,
`--parent` threading, whether `--json` hydrates `labels`, and the shapes those
commands return. Each was verified by hand against one version, once, and
trusted from then on.

So a scheduled workflow installs a real `bd` release and runs
[`scripts/canary.sh`](../scripts/canary.sh) against it: `bd init`, `pact init`,
`pact doctor`, a two-identity message round-trip (send → inbox → read →
`--unread-only` is empty → the sender can see it was read), and a lease
acquire/list/release. Weekly, plus `workflow_dispatch`. It is **not** in `ci.yml`
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
`bd` shipped most recently. Both are resolved at run time — the range is read
out of `TESTED_BD_MIN` / `TESTED_BD_MAX_EXCLUSIVE` in `src/beads.rs` rather than
restated in the workflow, because a second copy of that range is a thing that
drifts from the code it describes, which is the failure the canary exists to
catch. While the newest release is still inside the range both legs resolve to
the same version, which is fine: they separate the moment upstream ships one
that isn't.

On the *latest* leg, a `bd` outside the tested range **must** make `pact doctor`
emit its version warning. That is the difference between "the warning logic
passes a unit test" and "the warning fires on real drift".

A failure opens an issue labelled `canary` with the leg, the `bd` version and a
link to the run — or comments on the open one, so a persistent break is one
thread rather than a weekly pile. Run it locally with `scripts/canary.sh`; it
needs `bd`, `jq` and `git`.
