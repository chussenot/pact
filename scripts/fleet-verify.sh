#!/usr/bin/env bash
# Assert what a fleet-sim run proves. Run it on the directory fleet-sim.sh printed.
#
#   scripts/fleet-verify.sh /tmp/pact-fleet-XXXX
#
# Four assertions, in order of what they are worth:
#
#  1. NO LOST UPDATES. Every marker a worker logged as written is present in the
#     file. This is the invariant leases exist to protect, and it is checked
#     against the workers' own logs rather than against a count — "the file has
#     40 markers" proves nothing without knowing 41 were written.
#  2. NO DOUBLE-WIN. No two agents held one path with overlapping windows,
#     reconstructed from .pact/events.jsonl. Finding one is a SUCCESS of this
#     harness: it is the written trigger condition for the guard-file backlog
#     item (pact-ehi), which says implement iff a double-win appears in a real
#     events log. So it exits 1 loudly with full forensics rather than being
#     smoothed over.
#  3. MESSAGE ROUND-TRIP. Every exit-2 encounter produced a message that reached
#     somebody's inbox. A conflict that coordinates nothing is a conflict that
#     silently serialises a fleet.
#  4. LIVENESS. Every task closed, so lease ordering deadlocked nobody.
#
# In --scope-local mode the expectations INVERT: that run is the control group,
# and lost updates there are the point. A harness that cannot demonstrate the
# failure pact prevents proves nothing when it passes.

set -euo pipefail

RUN_DIR="${1:-}"
[ -n "$RUN_DIR" ] || {
	printf 'usage: %s <run-dir>\n' "$0" >&2
	exit 2
}
[ -f "$RUN_DIR/manifest.json" ] || {
	printf 'fleet-verify: no manifest.json in %s\n' "$RUN_DIR" >&2
	exit 2
}

command -v jq >/dev/null 2>&1 || {
	printf 'fleet-verify: jq is required\n' >&2
	exit 2
}

REPO="$(jq -r .repo "$RUN_DIR/manifest.json")"
LOGS="$(jq -r .logs "$RUN_DIR/manifest.json")"
WORKERS="$(jq -r .workers "$RUN_DIR/manifest.json")"
SEEDED="$(jq -r .tasks_seeded "$RUN_DIR/manifest.json")"
BACKEND="$(jq -r .backend "$RUN_DIR/manifest.json")"
SCOPE_LOCAL="$(jq -r .scope_local "$RUN_DIR/manifest.json")"
NO_LEASES="$(jq -r '.no_leases // false' "$RUN_DIR/manifest.json")"
ELAPSED="$(jq -r .elapsed_secs "$RUN_DIR/manifest.json")"
# Two different control modes, asserting two different things. --no-leases is the
# one that can show DAMAGE; --scope-local shows the COORDINATION disappearing.
IS_NO_LEASE=0
[ "$NO_LEASES" = true ] && IS_NO_LEASE=1
IS_SCOPE_LOCAL=0
[ "$SCOPE_LOCAL" = true ] && IS_SCOPE_LOCAL=1

FAIL=0
problem() {
	printf '\nFAIL: %s\n' "$1" >&2
	FAIL=1
}
step() { printf '\n=== %s\n' "$1"; }

printf '=== fleet-verify %s\n' "$RUN_DIR"
printf '    workers=%s tasks=%s backend=%s elapsed=%ss%s\n' \
	"$WORKERS" "$SEEDED" "$BACKEND" "$ELAPSED" \
	"$([ "$IS_NO_LEASE" -eq 1 ] && printf ' CONTROL(no-leases)' || true)$([ "$IS_SCOPE_LOCAL" -eq 1 ] && printf ' CONTROL(scope=local)' || true)"

# ------------------------------------------------ 1. lost-update detection
#
# The workers' logs are the ground truth for intent: each WROTE line records the
# exact marker that worker believes it put in a file. A marker that is missing
# from the file means a second worker read the file before this one wrote and
# then wrote back its own version — the classic lost update, and precisely what a
# lease is for.
step "no lost updates"
MISSING="$RUN_DIR/lost-updates.txt"
: >"$MISSING"
WROTE_TOTAL=0
# Each marker is checked in the directory the WRITING WORKER used, which in
# --worktrees mode is its own linked worktree and not the main checkout. Getting
# this wrong is not a small error: the first cut resolved every path against the
# main repo, so a worktree run reported 26 of 26 markers lost and the control
# group "passed" by finding damage that had not happened.
while IFS= read -r line; do
	# `<ts> <agent> WROTE <path> marker=MARK <agent> <task> <seq>`
	agent="$(awk '{print $2}' <<<"$line")"
	path="$(awk '{for(i=1;i<=NF;i++) if($i=="WROTE"){print $(i+1); exit}}' <<<"$line")"
	marker="${line#*marker=}"
	[ -n "$path" ] && [ -n "$marker" ] || continue
	WROTE_TOTAL=$((WROTE_TOTAL + 1))
	wdir="$(jq -r --arg a "$agent" '.worker_dirs[$a] // ""' "$RUN_DIR/manifest.json")"
	[ -n "$wdir" ] || wdir="$REPO"
	if [ ! -f "$wdir/$path" ]; then
		printf '%s\tMISSING FILE\t%s\t%s\n' "$path" "$marker" "$wdir" >>"$MISSING"
		continue
	fi
	grep -qxF "$marker" "$wdir/$path" ||
		printf '%s\tLOST\t%s\t%s\n' "$path" "$marker" "$wdir" >>"$MISSING"
done < <(grep -h ' WROTE ' "$LOGS"/worker-*.log 2>/dev/null || true)

LOST="$(wc -l <"$MISSING" | tr -d ' ')"
printf '    markers written: %s\n    markers lost:    %s\n' "$WROTE_TOTAL" "$LOST"
[ "$WROTE_TOTAL" -gt 0 ] || problem "no WROTE lines in any worker log — the sim did no work, so nothing here is a measurement"

if [ "$IS_NO_LEASE" -eq 1 ]; then
	# The counterfactual, and the assertion that makes every passing run mean
	# something. Same workload, one shared checkout, lease calls removed: if THAT
	# does not lose updates then the workload never contended and a clean run with
	# leases proves nothing at all.
	if [ "$LOST" -eq 0 ]; then
		problem "CONTROL RUN LOST NOTHING. With --no-leases the workers do concurrent
read-modify-write on shared files with no coordination whatsoever, so updates must
be lost. Zero losses means this workload does not actually contend, and a passing
run WITH leases therefore proves nothing. Raise -n, raise the overlap in the task
seed, or widen the sleep between read and write."
	else
		printf '    control lost %s of %s markers, as it must — the workload really contends,\n' "$LOST" "$WROTE_TOTAL"
		printf '    so a clean run with leases is a result rather than an accident\n'
	fi
else
	if [ "$LOST" -ne 0 ]; then
		problem "$LOST marker(s) written by a worker are absent from the file. Leases did not
protect the invariant they exist for. Forensics in $MISSING:"
		head -20 "$MISSING" >&2
	else
		printf '    every marker survived\n'
	fi
fi

# -------------------------------------------------- 2. double-win detection
#
# Reconstruct hold windows from the event log and look for two agents holding one
# path at once. `acquired` and `stolen` open a window; `released`,
# `force-released` and `expired` close one. `renewed` is neither — the holder
# keeps holding.
#
# `expired` is logged under the DEAD holder's name (lease.rs logs
# `&existing.agent`), which is what makes a routine reclaim read correctly here:
# the old window closes before the `stolen` row opens the new one.
step "no double-win (two agents holding one path at once)"
EVENTS="$REPO/.pact/events.jsonl"
FINDINGS="$RUN_DIR/double-wins.json"
if [ "$IS_NO_LEASE" -eq 1 ]; then
	# No leases were taken, so there are no hold windows to overlap. Skipped
	# rather than failed: a missing event log is the definition of this mode.
	printf '    skipped: --no-leases takes no leases, so there is nothing to overlap\n'
elif [ "$IS_SCOPE_LOCAL" -eq 1 ]; then
	# Each worker wrote to its OWN .pact/events.jsonl, so cross-worker overlap is
	# not merely undetected here, it is undetectable — which is the very thing this
	# mode exists to demonstrate. Skipped with the reason rather than failed on a
	# main-repo log that was never going to exist.
	printf '    skipped: scope=local gives each worker its own event log, so cross-worker\n'
	printf '             overlap is structurally invisible — that IS what this mode shows\n'
elif [ ! -f "$EVENTS" ]; then
	problem "no event log at $EVENTS — nothing to reconstruct"
else
	# Tolerant of a truncated final line: an append-only log can be cut mid-write.
	jq -sR '
		split("\n") | map(select(length > 0))
		| map(. as $l | try fromjson catch empty)
		| map(select(.path != null))
		| group_by(.path)
		| map(
			sort_by(.at)
			| reduce .[] as $e ({open: {}, found: []};
				if ($e.kind == "acquired" or $e.kind == "stolen") then
					(if ((.open | keys | map(select(. != $e.agent)) | length) > 0)
					 then .found += [{
						path: $e.path,
						at: $e.at,
						incoming: $e.agent,
						incoming_kind: $e.kind,
						already_holding: (.open | to_entries | map({agent: .key, since: .value})),
						detail: $e.detail
					 }]
					 else . end)
					| .open[$e.agent] = $e.at
				elif ($e.kind == "released" or $e.kind == "force-released" or $e.kind == "expired") then
					.open |= del(.[$e.agent])
				else . end)
			| .found)
		| flatten
	' "$EVENTS" >"$FINDINGS" 2>/dev/null || echo '[]' >"$FINDINGS"

	EVENT_COUNT="$(wc -l <"$EVENTS" | tr -d ' ')"
	DW="$(jq 'length' "$FINDINGS")"
	printf '    events scanned: %s\n    double-wins:    %s\n' "$EVENT_COUNT" "$DW"
	if [ "$DW" -ne 0 ]; then
		# Loud on purpose. This is the trigger condition pact-ehi was written
		# around, and the harness earning its keep — not something to soften.
		problem "DOUBLE-WIN DETECTED. Two agents held one path with overlapping windows.

This is the written trigger condition for the guard-file backlog item (pact-ehi),
which says: implement the guard file if and only if a double-win appears in a real
events log. It just did. Attach this output to that bead — it is the evidence the
bead is waiting for, and the reason not to implement on suspicion.

Forensics ($FINDINGS):"
		jq -C '.[0:5]' "$FINDINGS" >&2
	else
		printf '    no overlapping hold windows\n'
	fi
fi

# ------------------------------------------------- 3. message round-trip
#
# The protocol says an exit-2 encounter should message the holder and go find
# other work. Conflicts that produce no message mean a fleet that serialises
# silently, which is the failure mode `--to-owner-of` exists to prevent.
step "every conflict produced a message"
CONFLICT_LINES="$RUN_DIR/conflicts.txt"
grep -h ' CONFLICT ' "$LOGS"/worker-*.log 2>/dev/null >"$CONFLICT_LINES" || true
CONFLICTS="$(wc -l <"$CONFLICT_LINES" | tr -d ' ')"
MESSAGED="$(grep -c 'messaged=yes' "$CONFLICT_LINES" || true)"
# A conflict with no message is acceptable in exactly one case, and pretending
# otherwise makes this assertion unachievable rather than strict. If the holder
# RELEASED between the exit-2 and the lookup, there is no current holder to name;
# `--to-owner-of` then resolves to the last actor on the path, which under churn
# is often the blocked worker itself, and pact rightly refuses to self-address.
# Nobody to tell is not a failure to tell — the worker's real obligation at exit 2
# is to go find other work, and it did.
NOBODY="$(grep -c 'no recipients resolved' "$CONFLICT_LINES" || true)"
UNEXPLAINED=$((CONFLICTS - MESSAGED - NOBODY))
printf '    exit-2 encounters:        %s\n' "${CONFLICTS:-0}"
printf '    messaged the holder:      %s\n' "${MESSAGED:-0}"
printf '    holder already gone:      %s (benign: nobody left to tell)\n' "${NOBODY:-0}"
printf '    unexplained failures:     %s\n' "$UNEXPLAINED"
if [ "${CONFLICTS:-0}" -eq 0 ]; then
	printf '    (no contention this run — not a failure, but this assertion proved nothing)\n'
elif [ "$UNEXPLAINED" -ne 0 ]; then
	problem "$UNEXPLAINED of $CONFLICTS conflicts sent no message for a reason that is NOT
the benign already-released race. Messaging broke under load; the why= field on
each CONFLICT line in $CONFLICT_LINES says how."
	grep -v 'messaged=yes' "$CONFLICT_LINES" | grep -v 'no recipients resolved' | head -5 >&2
else
	# Sent is not delivered, so ask the RECIPIENTS. Via `pact msg inbox` rather
	# than the backend directly: the first cut of this ran `bd list --json` and
	# counted zero, because bd hides message beads unless `--include-infra` is
	# passed and br has no such flag at all — argv differences pact already
	# encapsulates and a verifier has no business reimplementing.
	PACT_BIN="$(jq -r .pact "$RUN_DIR/manifest.json")"
	DELIVERED=0
	for w in $(seq 1 "$WORKERS"); do
		n="$(cd "$REPO" && PACT_AGENT="sim-w$w" "$PACT_BIN" msg inbox --json 2>/dev/null |
			jq 'length' 2>/dev/null || echo 0)"
		DELIVERED=$((DELIVERED + n))
	done
	printf '    messages sitting in worker inboxes: %s\n' "$DELIVERED"
	[ "$DELIVERED" -ge 1 ] ||
		problem "workers logged $MESSAGED messages sent, but no worker inbox holds one — sent is
not the same as delivered, and this is the half that matters."
fi

# --------------------------------------- 3b. scope-local removes coordination
#
# What --scope-local can actually prove. It cannot produce lost updates — each
# worker has its own worktree, so "the same path" is a different file for each of
# them and no lease could matter. What it does show is the coordination vanishing:
# with an isolated `.pact/` per worker, nobody ever sees a peer's lease, so exit-2
# encounters go to zero on a workload that produces them in shared mode.
if [ "$IS_SCOPE_LOCAL" -eq 1 ]; then
	step "scope-local: coordination is absent, as it must be"
	printf '    exit-2 encounters: %s (expected 0)\n' "${CONFLICTS:-0}"
	if [ "${CONFLICTS:-0}" -ne 0 ]; then
		problem "PACT_WORKTREE_SCOPE=local produced $CONFLICTS conflicts. Isolated scope means
each worker resolves its own .pact/, so no worker should ever see a peer's lease.
Conflicts here mean the isolation is not isolating — which would make the shared
mode's guarantees suspect too, since both come from the same resolution code."
	else
		printf '    no worker saw a peer, so shared resolution is what produces the coordination\n'
	fi
fi

# -------------------------------------------------------------- 4. liveness
#
# Lease ordering can deadlock a fleet: two workers each holding one of two paths
# the other needs. pact's all-or-nothing multi-path acquire is what prevents it,
# so "every task closed" is that guarantee under load.
step "liveness: every task closed"
OPEN_LEFT=0
CLOSED=0
if command -v "$BACKEND" >/dev/null 2>&1; then
	ALL="$(cd "$REPO" && "$BACKEND" list --status=open --json 2>/dev/null || echo '[]')"
	OPEN_LEFT="$(jq '[ (if type=="object" then (.issues // []) else . end)[]
		| select(.issue_type != "message") ] | length' <<<"$ALL" 2>/dev/null || echo 0)"
	CLOSED_JSON="$(cd "$REPO" && "$BACKEND" list --status=closed --json 2>/dev/null || echo '[]')"
	CLOSED="$(jq '[ (if type=="object" then (.issues // []) else . end)[]
		| select(.issue_type != "message") ] | length' <<<"$CLOSED_JSON" 2>/dev/null || echo 0)"
fi
printf '    seeded=%s closed=%s still open=%s\n' "$SEEDED" "$CLOSED" "$OPEN_LEFT"
if [ "${OPEN_LEFT:-0}" -ne 0 ]; then
	problem "$OPEN_LEFT task(s) never closed. Either a worker hit its attempt cap while
paths stayed locked, or the fleet deadlocked on lease ordering. Per-worker logs in $LOGS."
fi

# Leases must not outlive the fleet either: a worker that exited holding one has
# left a claim no process stands behind.
LEFTOVER="$(cd "$REPO" && PACT_AGENT=verify "$(jq -r .pact "$RUN_DIR/manifest.json")" \
	lease ls --json 2>/dev/null | jq 'length' 2>/dev/null || echo 0)"
printf '    leases still held: %s\n' "$LEFTOVER"
[ "${LEFTOVER:-0}" -eq 0 ] ||
	problem "$LEFTOVER lease(s) survived the run — 'lease release --all' did not run, or did not work"

# --------------------------------------------------------------- per-worker
step "per-worker contention"
if [ -f "$RUN_DIR/worker-summary.txt" ]; then
	printf '    %-14s %5s %10s %11s %14s\n' 'agent' 'done' 'conflicts' 'claim-races' 'commit-retries'
	sort "$RUN_DIR/worker-summary.txt" |
		awk '{printf "    %-14s %5s %10s %11s %14s\n",$1,$2,$3,$4,$5}'
	IDLE="$(awk '$2 == 0' "$RUN_DIR/worker-summary.txt" | wc -l | tr -d ' ')"
	[ "$IDLE" -eq 0 ] ||
		printf '    note: %s worker(s) completed nothing — starvation, not necessarily a bug\n' "$IDLE"
else
	problem "no worker summary — did any worker finish?"
fi

printf '\n'
if [ "$FAIL" -eq 0 ]; then
	printf 'FLEET VERIFY PASSED (%s workers, %s tasks, %ss)\n' "$WORKERS" "$SEEDED" "$ELAPSED"
else
	printf 'FLEET VERIFY FAILED — see above; logs in %s\n' "$LOGS" >&2
fi
exit "$FAIL"
