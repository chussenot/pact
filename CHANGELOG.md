# Changelog

## 0.1.0

Initial release.

- `pact init`: idempotently inject/update the coordination-protocol block in
  `AGENTS.md`; manage a `.pact/leases/` line in `.gitignore`.
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
