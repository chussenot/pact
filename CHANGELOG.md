# Changelog

## Unreleased

Everything in this batch came from running a real multi-agent fleet against
pact itself and recording what pact did to it; each item traces to an
evidence-backed bead under the `pact-rnc` epic. See
[README](README.md#where-these-features-came-from) for the provenance, including
the four findings deliberately deferred (`pact-rnc.4`/`.7`/`.13`/`.17`).

### Added

- `pact whoami`: the resolved identity and its source, the running pact binary,
  repo root, `.pact/`, and the `bd` it will use. Reports problems instead of
  raising them, so it exits 0 with no identity, no `bd`, or outside a git repo;
  probes `bd`'s ability to read the repo's database, not just its presence; and
  creates nothing.
- `pact agents`: the identities seen holding leases or in message traffic,
  most-recent first, with lease/sent/received counts. Derived from existing
  state — no registry. `bd` optional. Names that have only ever been *addressed*
  are marked `?`, since that is what a typo'd `--to` leaves behind.
- `pact lease renew <path>`: refresh a lease a long task would otherwise
  outlive, keeping the original TTL and note. Refuses to create a missing lease,
  and exits 2 on another agent's lease.
- `pact lease release --all`: release every lease the calling agent holds in one
  call. Holding nothing is success. Mutually exclusive with a path and with
  `--force`.
- `pact msg send --body-file <path|->`: read the body from a file or stdin, so a
  message containing quotes, backslashes and aligned tables never has to survive
  a shell. Rejects an all-whitespace body.
- `pact msg inbox --full`: every message in full, through the same renderer as
  `pact msg read`.

### Changed

- `pact msg inbox` prints one line per message with the sender and an unread
  marker, plus a footer pointing at `pact msg read`, instead of every body in
  full. `--json` output is unchanged and complete.
- `pact msg read` and `--full` show the envelope — from, to, subject, time,
  thread — which was previously discarded.
- `Message.from` is now populated on every path from bd's `created_by`. Passed
  through verbatim, so it is a label and not a guaranteed pact identity.
- `pact lease ls` leads with the lease's age and an explicit
  `active` / `stale (reclaimable in Ns)` / `expired` state, and shows the
  holder's `--note`. Remaining TTL appears only where it is actionable: printed
  first, it read as "long-held" on a seconds-old lease and got a live agent's
  claim force-released.
- `pact lease release --force` warns on stderr and names the agent whose live
  claim it destroyed, mirroring `acquire --steal`. Exit code stays 0.
- `pact msg send` warns on stderr when the recipient has never acted in this
  repo, suggesting close names, and **sends anyway** — a bootstrapping fleet
  legitimately messages agents that haven't acted yet. A recipient that violates
  pact's identity grammar is refused instead, since no send could ever fix it.
- The `AGENTS.md` protocol block now tells agents to announce intent *before*
  they research rather than before they write, states file ownership and its one
  carve-out in a single bullet, and teaches `lease renew`, `lease release --all`,
  `pact agents` and `pact whoami`.
- `pact init` gitignores `.pact/` as one line instead of a rule per file, and
  recognises the older `.pact/leases/` + `.pact/read.json` pair without
  appending to it.

## 0.1.0

Initial release.

- `pact init`: idempotently inject/update the coordination-protocol block in
  `AGENTS.md`; manage a `.pact/` line in `.gitignore`.
- `pact lease acquire|release|ls`: advisory file leases as atomic lock files
  under `.pact/leases/`, with TTL, clock-skew grace period, steal, and
  re-entrant refresh semantics.
- `pact msg send|inbox|read`: threaded agent-to-agent messages, implemented
  as `bd create --type=message` issues (bd-only backend; `br` support is a
  later phase). Read/unread state is tracked locally in `.pact/read.json`
  since Beads has no read lifecycle for message issues.
- `pact doctor`: checks the repo is a git repo, `.pact/` is present, the
  `AGENTS.md` block is current, the Beads CLI is found (with version), and
  reports/garbage-collects stale leases.
- Every command supports `--json` for machine-readable output, and a
  documented exit-code contract (0/1/2/3/4).
