#!/usr/bin/env bash
# Assert the lease hot path is still inside its documented budget (pact-fib).
#
# Runs benches/lease.rs and fails if the median of ONE named benchmark —
# `lease/roundtrip_acquire_release` — exceeds a wall-clock budget. That
# benchmark is a full acquire-then-release cycle, which is what an agent costs
# the fleet per file it touches.
#
# ## Why one number and not a regression check
#
# Criterion's own strength is comparing a run against a stored baseline, and this
# script deliberately does not use it. A baseline comparison on a shared CI runner
# reports the runner's neighbours, not pact: the same commit swings by more than
# any real regression would. So this asserts an ABSOLUTE ceiling with a large
# margin — it cannot see a 20% slowdown, and it will catch the class of change
# that actually matters here, which is something turning a handful of syscalls
# into a subprocess, a network call, or an O(n) walk of the event log.
#
# The budget is a contract about ORDER OF MAGNITUDE. See docs/performance.md for
# the measured baseline it was derived from and the headroom it carries.
#
# Usage: scripts/bench-budget.sh [budget_ms]
#   BENCH_ARGS='--measurement-time 3'  scripts/bench-budget.sh   # faster, noisier

set -euo pipefail

cd "$(dirname "$0")/.." || exit 1

# 50 ms against a measured baseline of ~12 ms, so ~4x headroom.
#
# The first proposal was 10 ms, and the measurement rejected it: acquire+release
# is 11.9 ms on the reference machine because it spawns TWO `git rev-parse`
# subprocesses and parses the event log three times. A budget under the baseline
# is not a strict budget, it is a broken one.
#
# 4x is chosen for the runner, not for the code: a shared CI box is routinely 2-3x
# slower on process spawn and I/O than a developer machine, and this must not go
# red because a neighbour was busy. See docs/performance.md.
BUDGET_MS="${1:-50}"
BENCH="lease/roundtrip_acquire_release"
# Criterion slugifies the group and id into a directory pair.
EST="target/criterion/lease/roundtrip_acquire_release/new/estimates.json"

need() { command -v "$1" >/dev/null 2>&1 || {
	echo "::error::$1 is required" >&2
	exit 1
}; }
need jq
need cargo

# No `bd` on PATH: this measures the filesystem primitive, and a backend in the
# environment could only add noise to a path that must never reach one. Removing
# the directory it lives in is the same trick `mise run test`'s no-backend leg
# uses, and it is a no-op on a runner that has no bd.
p="$PATH"
if d="$(command -v bd 2>/dev/null)"; then
	p="$(printf '%s' "$p" | tr ':' '\n' | grep -vxF "$(dirname "$d")" | paste -sd: -)"
fi

echo "running $BENCH (bd on PATH: $(PATH="$p" command -v bd || echo absent))"
# shellcheck disable=SC2086 # BENCH_ARGS is a deliberate argument list
PATH="$p" cargo bench --bench lease -- "$BENCH" ${BENCH_ARGS:-} || {
	echo "::error::the benchmark itself failed to run" >&2
	exit 1
}

[ -f "$EST" ] || {
	echo "::error::no criterion estimates at $EST — did the benchmark name change?" >&2
	exit 1
}

# point_estimate is in NANOseconds.
MEDIAN_NS="$(jq -r '.median.point_estimate' "$EST")"
[ -n "$MEDIAN_NS" ] && [ "$MEDIAN_NS" != "null" ] || {
	echo "::error::could not read a median out of $EST" >&2
	exit 1
}

# Integer arithmetic only: no bc/python dependency for one comparison.
MEDIAN_US="$(printf '%.0f' "$(jq -r '.median.point_estimate / 1000' "$EST")")"
BUDGET_NS=$((BUDGET_MS * 1000000))

printf 'median: %s ns (%s us)\nbudget: %s ns (%s ms)\n' \
	"$(printf '%.0f' "$MEDIAN_NS")" "$MEDIAN_US" "$BUDGET_NS" "$BUDGET_MS"

if [ "$(printf '%.0f' "$MEDIAN_NS")" -gt "$BUDGET_NS" ]; then
	cat >&2 <<EOF
::error::$BENCH median $(printf '%.0f' "$MEDIAN_NS") ns exceeds the ${BUDGET_MS} ms budget.

This budget is an order-of-magnitude contract, so exceeding it by any amount
means something structural changed on the lease path — a subprocess, a network
call, an fsync, or a walk that grew with the event log. It is not the number to
tune; it is the number to explain.

See docs/performance.md for the baseline and what the budget is protecting.
EOF
	exit 1
fi

echo "within budget"
