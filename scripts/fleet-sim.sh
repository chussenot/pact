#!/usr/bin/env bash
# Soak-test pact at fleet concurrency with scripted workers instead of LLMs.
#
# Every previous finding about pact's behaviour under load came from watching real
# agent fleets — which is expensive, slow, and unrepeatable. The workers here do
# mechanically what AGENTS.md's managed block tells a real agent to do, so the
# concurrency can be turned up to twenty and run in a minute, on demand, in CI.
#
# What that buys and what it does not is the whole point, and is stated in
# docs/testing.md rather than assumed: this proves pact's PRIMITIVES hold under
# contention. It proves nothing about whether a language model understands the
# protocol, because these workers cannot misunderstand it — they were written
# from it.
#
# The workers branch ONLY on documented exit codes (0 ok, 2 held by another
# agent, 3 no backend, 4 not a repo, 5 usage). That is deliberately part of what
# is under test: if pact ever returns 1 where it used to return 2, a worker here
# stops coordinating and the verifier notices.
#
#   scripts/fleet-sim.sh                    # 10 workers, one shared checkout
#   scripts/fleet-sim.sh -n 20              # more contention
#   scripts/fleet-sim.sh --worktrees        # a linked worktree per worker
#   scripts/fleet-sim.sh --scope-local      # CONTROL: leases isolated, expect damage
#   scripts/fleet-sim.sh --steal-storm      # every worker races expired leases first
#
# Prints the run directory on the last line; hand that to fleet-verify.sh.

set -euo pipefail

WORKERS=10
TASKS=60
USE_WORKTREES=0
SCOPE_LOCAL=0
STEAL_STORM=0
NO_LEASES=0
BACKEND=""
KEEP=1
RUN_DIR=""

die() {
	printf 'fleet-sim: %s\n' "$1" >&2
	exit 1
}
say() { printf '%s\n' "$*"; }

usage() {
	sed -n '2,30p' "$0" | sed 's/^# \?//'
	exit 0
}

while [ $# -gt 0 ]; do
	case "$1" in
	-n | --workers)
		WORKERS="${2:?-n needs a count}"
		shift 2
		;;
	-t | --tasks)
		TASKS="${2:?-t needs a count}"
		shift 2
		;;
	--worktrees)
		USE_WORKTREES=1
		shift
		;;
	--scope-local)
		SCOPE_LOCAL=1
		# Isolation only means anything when each worker has its own checkout to
		# be isolated IN: with one shared directory every worker resolves the same
		# `.pact/` whatever the scope says.
		#
		# What this mode does and does NOT demonstrate, learned by getting it
		# wrong: it CANNOT produce lost updates, because a worktree gives each
		# worker its own copy of every file, so two workers editing "the same"
		# path are editing different inodes and no lease could matter. The first
		# cut of this harness reported 26 of 26 markers lost here and it was the
		# verifier reading the main worktree instead of the worker's own — a
		# control group passing for the wrong reason, which is worse than none.
		#
		# What it DOES demonstrate is that the coordination disappears: with
		# isolated `.pact/` directories no worker ever sees a peer's lease, so the
		# exit-2 conflict count drops to zero while the workload is unchanged.
		# That is the counterfactual for shared resolution. For lost updates, use
		# --no-leases.
		USE_WORKTREES=1
		shift
		;;
	--steal-storm)
		STEAL_STORM=1
		shift
		;;
	--no-leases)
		# The control group that CAN show damage. See the comment on
		# --scope-local: worktrees give every worker its own copy of every file,
		# so no amount of lease isolation can produce a lost update there. This
		# keeps one shared checkout and skips the lease calls, which is the actual
		# counterfactual — what the same workload does with pact's primitive
		# removed.
		NO_LEASES=1
		shift
		;;
	--backend)
		BACKEND="${2:?--backend needs bd or br}"
		shift 2
		;;
	--run-dir)
		RUN_DIR="${2:?--run-dir needs a path}"
		shift 2
		;;
	-h | --help) usage ;;
	*) die "unknown flag: $1 (try --help)" ;;
	esac
done

case "$WORKERS" in '' | *[!0-9]*) die "workers must be a number, got '$WORKERS'" ;; esac
case "$TASKS" in '' | *[!0-9]*) die "tasks must be a number, got '$TASKS'" ;; esac
[ "$WORKERS" -ge 2 ] || die "a fleet of $WORKERS cannot contend with itself"

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not on PATH"; }
need git
need jq
need python3

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The binary under test is the one just built, never whatever is on PATH — a
# stale `pact` has silently invalidated this repo's results before.
say "=== building pact"
(cd "$REPO_ROOT" && cargo build --quiet)
PACT="$REPO_ROOT/target/debug/pact"
[ -x "$PACT" ] || die "no binary at $PACT"

if [ -z "$BACKEND" ]; then
	if command -v bd >/dev/null 2>&1; then
		BACKEND=bd
	elif command -v br >/dev/null 2>&1; then
		BACKEND=br
	else
		die "no Beads CLI on PATH; the workers need one for tasks and messages"
	fi
fi
need "$BACKEND"

[ -n "$RUN_DIR" ] || RUN_DIR="$(mktemp -d -t pact-fleet-XXXXXX)"
mkdir -p "$RUN_DIR"
WORK="$RUN_DIR/repo"
LOGS="$RUN_DIR/logs"
mkdir -p "$WORK" "$LOGS"

# git must not read the developer's config: a global commit.gpgsign or a
# core.excludesFile mentioning .pact/ would make this run a measurement of the
# machine rather than of pact.
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME="fleet sim"
export GIT_AUTHOR_EMAIL="fleet@pact.invalid"
export GIT_COMMITTER_NAME="fleet sim"
export GIT_COMMITTER_EMAIL="fleet@pact.invalid"
export BD_NON_INTERACTIVE=1

say "=== scratch repo at $WORK"
cd "$WORK"
git init -q -b main .
git config user.name "fleet sim"
git config user.email "fleet@pact.invalid"

# ------------------------------------------------------ synthetic codebase
#
# Four modules, five files each, plus one `iface` file per module that many tasks
# must touch. The overlap IS the experiment: a codebase where every task owns its
# own files would run twenty workers with zero contention and prove nothing.
MODULES=(auth billing search notify)
say "=== synthetic codebase: ${#MODULES[@]} modules"
for m in "${MODULES[@]}"; do
	mkdir -p "$m"
	printf '// %s interface — several tasks touch this on purpose\n' "$m" >"$m/iface.txt"
	for i in 1 2 3 4; do
		printf '// %s unit %s\n' "$m" "$i" >"$m/unit$i.txt"
	done
done
git add -A
git commit -q -m "synthetic codebase"

say "=== $BACKEND init"
"$BACKEND" init --non-interactive --prefix sim >/dev/null 2>&1 ||
	"$BACKEND" init --prefix sim >/dev/null 2>&1 ||
	die "$BACKEND init failed"

say "=== pact init"
PACT_AGENT=sim-setup "$PACT" init --no-commit >/dev/null || die "pact init failed"
git add -A
git commit -q -m "bd + pact init" || true

# ------------------------------------------------------------- seed tasks
#
# Each task names 1-3 target files in its description, and the generator biases
# hard toward the four `iface` files so that roughly two in five tasks share a
# file with another task. The workers parse `TARGETS:` out of the description;
# nothing here depends on a bd field pact does not already use.
say "=== seeding $TASKS tasks"
python3 - "$TASKS" "$RUN_DIR/tasks.txt" "${MODULES[@]}" <<'PY'
import random, sys
n = int(sys.argv[1]); out = sys.argv[2]; mods = sys.argv[3:]
random.seed(1729)  # fixed: a failing run must be re-runnable
ifaces = [f"{m}/iface.txt" for m in mods]
units = [f"{m}/unit{i}.txt" for m in mods for i in (1, 2, 3, 4)]
lines = []
for t in range(n):
    k = random.choice([1, 1, 2, 2, 3])
    # ~40% of tasks include an iface file, which is what makes them collide.
    picks = []
    if random.random() < 0.45:
        picks.append(random.choice(ifaces))
    while len(picks) < k:
        c = random.choice(units + ifaces)
        if c not in picks:
            picks.append(c)
    lines.append(" ".join(sorted(picks)))
with open(out, "w") as f:
    f.write("\n".join(lines) + "\n")
PY

TASK_N=0
while IFS= read -r targets; do
	TASK_N=$((TASK_N + 1))
	"$BACKEND" create \
		--title="sim task $TASK_N" \
		--description="TARGETS: $targets" \
		--type=task --priority=2 --actor=sim-setup >/dev/null 2>&1 ||
		die "seeding task $TASK_N failed"
done <"$RUN_DIR/tasks.txt"
say "    seeded $TASK_N tasks over $(sort -u "$RUN_DIR/tasks.txt" | wc -l) distinct target sets"

# Contended files, for the report: how much overlap the seed actually produced.
tr ' ' '\n' <"$RUN_DIR/tasks.txt" | sort | uniq -c | sort -rn >"$RUN_DIR/target-frequency.txt"

git add -A
git commit -q -m "seed tasks" || true

# ------------------------------------------------------------- steal storm
#
# Every worker's first act becomes a race for a path whose lease has already
# lapsed. That exercises the takeover path — `expired` then `stolen` in the event
# log — which is where the residual race the guard-file bead (pact-ehi) describes
# would show up if it is real.
if [ "$STEAL_STORM" -eq 1 ]; then
	say "=== steal storm: planting expired leases"
	for m in "${MODULES[@]}"; do
		PACT_AGENT=ghost-holder "$PACT" lease acquire "$m/iface.txt" \
			--ttl 1 --note "ghost holder, about to lapse" >/dev/null || true
	done
	# Past ttl + GRACE_SECS, so the leases are genuinely reclaimable rather than
	# merely stale.
	sleep 32
fi

# -------------------------------------------------------------- worktrees
declare -a WORKER_DIR
for w in $(seq 1 "$WORKERS"); do
	if [ "$USE_WORKTREES" -eq 1 ]; then
		d="$RUN_DIR/wt-$w"
		git worktree add -q -b "sim-$w" "$d" HEAD || die "worktree $w failed"
		WORKER_DIR[w]="$d"
	else
		WORKER_DIR[w]="$WORK"
	fi
done
[ "$USE_WORKTREES" -eq 1 ] && say "=== $WORKERS linked worktrees"

# ---------------------------------------------------------------- workers
#
# One subshell per worker. Everything it does is appended to its own log with a
# timestamp, because the verifier's whole job is comparing what a worker BELIEVES
# it did against what the repository shows.
worker() {
	local w="$1" dir="${WORKER_DIR[$1]}" log="$LOGS/worker-$1.log"
	local agent="sim-w$1"
	local attempts=0 done_count=0 conflicts=0 commit_retries=0 claim_races=0
	local max_attempts=$((TASKS * 3))

	export PACT_AGENT="$agent"
	[ "$SCOPE_LOCAL" -eq 1 ] && export PACT_WORKTREE_SCOPE=local

	logline() { printf '%s %s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)" "$agent" "$*" >>"$log"; }

	cd "$dir" || return 1
	logline START dir="$dir" scope="${PACT_WORKTREE_SCOPE:-shared}"

	while [ "$attempts" -lt "$max_attempts" ]; do
		attempts=$((attempts + 1))

		# The protocol's first instruction: inbox and lease ls BEFORE touching a
		# file. Logged so the verifier can check every worker actually did it.
		"$PACT" msg inbox --json >/dev/null 2>&1 && logline PROTOCOL inbox-checked
		"$PACT" lease ls --json >"$dir/.sim-leases-$w.json" 2>/dev/null &&
			logline PROTOCOL lease-ls-checked

		local ready task targets
		ready="$("$BACKEND" ready --json 2>/dev/null || echo '[]')"
		task="$(jq -r 'if type=="object" then (.issues // []) else . end
			| map(select(.status == "open")) | .[0].id // ""' <<<"$ready" 2>/dev/null || echo "")"
		[ -n "$task" ] || {
			logline IDLE no-ready-tasks
			break
		}

		# Task-level exclusion comes from the backend: `--claim` is documented
		# atomic and a second claimer is refused, so two workers never work one
		# task. Losing that race is normal and cheap — take another.
		if ! "$BACKEND" update "$task" --claim --actor="$agent" >/dev/null 2>&1; then
			claim_races=$((claim_races + 1))
			logline CLAIM-LOST "$task"
			continue
		fi
		targets="$("$BACKEND" show "$task" --json 2>/dev/null |
			jq -r 'if type=="array" then .[0] else . end | .description // ""' |
			sed -n 's/^TARGETS: //p' | head -1)"
		[ -n "$targets" ] || {
			logline SKIP "$task" no-targets
			"$BACKEND" close "$task" --reason="no targets" >/dev/null 2>&1 || true
			continue
		}
		logline TASK "$task" targets="$targets"

		# All-or-nothing, exactly as the protocol says: several paths in one
		# acquire so a worker never holds half of what it needs.
		local rc=0
		if [ "$NO_LEASES" -eq 1 ]; then
			# The counterfactual: same workload, pact's primitive removed.
			logline NO-LEASE-MODE "$task"
		else
			# shellcheck disable=SC2086 # word splitting is how targets become argv
			"$PACT" lease acquire $targets --note "sim task $task" \
				>"$dir/.sim-acq-$w.txt" 2>&1 || rc=$?
		fi

		if [ "$rc" -eq 2 ]; then
			# Branch on the CODE, not the message text. Then do what the protocol
			# says: find the holder, address the FILE rather than the name, and go
			# find other work.
			conflicts=$((conflicts + 1))
			local first holder
			first="${targets%% *}"
			# FRESH, not the copy taken at loop top: by now somebody has acquired
			# the path and the cached listing predates them.
			holder="$("$PACT" lease ls --json 2>/dev/null |
				jq -r --arg p "$first" 'map(select(.lease.path==$p))|.[0].lease.agent // ""' 2>/dev/null || echo "")"
			# `--to <holder>` from `lease ls`, NOT `--to-owner-of <path>`, and the
			# harness found out why the hard way. The protocol block offers both
			# idioms and they answer different questions: `--to-owner-of` resolves
			# the LAST AGENT TO ACT on a path from the event log, which is the right
			# thing for a handoff to whoever picks the file up next. It is the wrong
			# thing at exit 2, because a worker that previously held and released
			# this path IS the last actor — so it resolves to itself, pact correctly
			# refuses to self-address, and the message never goes out. Three of four
			# conflicts failed exactly that way on the first run of this harness.
			#
			# For contention the protocol's other sentence is the correct one:
			# "`pact lease ls` names the holder; message them". `--to-owner-of` stays
			# as the fallback for when the holder has already released and there is
			# nobody current to name.
			#
			# stderr is CAPTURED, not discarded, which is how the above was diagnosed
			# at all — a harness that throws away the evidence for its own finding is
			# half a harness.
			local mrc=0
			if [ -n "$holder" ] && [ "$holder" != "$agent" ]; then
				"$PACT" msg send --to "$holder" \
					"blocked on $first for $task, taking other work" \
					>"$dir/.sim-msg-$w.out" 2>"$dir/.sim-msg-$w.err" || mrc=$?
			else
				"$PACT" msg send --to-owner-of "$first" \
					"blocked on $first for $task, taking other work" \
					>"$dir/.sim-msg-$w.out" 2>"$dir/.sim-msg-$w.err" || mrc=$?
			fi
			if [ "$mrc" -eq 0 ]; then
				logline CONFLICT "$task" path="$first" holder="${holder:-unknown}" messaged=yes
			else
				logline CONFLICT "$task" path="$first" holder="${holder:-unknown}" messaged=no \
					msg_exit="$mrc" why="$(tr '\n' ' ' <"$dir/.sim-msg-$w.err" | cut -c1-160)"
			fi
			"$BACKEND" update "$task" --status=open --actor="$agent" >/dev/null 2>&1 || true
			continue
		fi
		if [ "$rc" -ne 0 ]; then
			logline ERROR "$task" acquire-exit="$rc" "$(head -1 "$dir/.sim-acq-$w.txt" 2>/dev/null)"
			"$BACKEND" update "$task" --status=open --actor="$agent" >/dev/null 2>&1 || true
			continue
		fi
		logline ACQUIRED "$task" targets="$targets"

		# Read-modify-write, which is what an editor does and what a lease exists
		# to protect. A bare `>>` would be a single atomic small append and would
		# survive without any lease at all, so it would prove nothing: the failure
		# leases prevent is the LOST UPDATE, where two workers each read, then each
		# write back what they read plus their own line.
		local t seq
		for t in $targets; do
			seq="$RANDOM"
			local body
			body="$(cat "$t")"
			# 0.1-2.0s between the read and the write, and the width is the whole
			# experiment. An earlier cut used 0-0.2s and the --no-leases control
			# group lost NOTHING: workers spend seconds inside backend calls, so a
			# 200ms window almost never overlaps another worker's. A control that
			# cannot produce damage makes every passing run meaningless, so this is
			# tuned until the counterfactual fails.
			sleep "$(awk 'BEGIN { srand(); printf "%.2f", 0.1 + rand() * 1.9 }')"
			printf '%s\nMARK %s %s %s\n' "$body" "$agent" "$task" "$seq" >"$t"
			logline WROTE "$t" marker="MARK $agent $task $seq"
		done

		# Hold the lease a while longer, so overlapping hold windows are wide enough
		# for a double-win to be visible in the event log if one happens.
		sleep "$(awk 'BEGIN { srand(); printf "%.2f", 0.1 + rand() * 1.9 }')"

		# Several workers share one index outside --worktrees mode, so this
		# genuinely contends. Retried rather than treated as failure: an agent
		# would retry too, and the count is reported.
		local tries=0
		while [ "$tries" -lt 8 ]; do
			tries=$((tries + 1))
			if git add -A >/dev/null 2>&1 && git commit -q -m "sim $task by $agent" >/dev/null 2>&1; then
				break
			fi
			commit_retries=$((commit_retries + 1))
			sleep "0.$((RANDOM % 5 + 1))"
		done

		# Release before reporting finished, and release everything in one call so
		# nothing is half-forgotten.
		if [ "$NO_LEASES" -eq 0 ]; then
			"$PACT" lease release --all >/dev/null 2>&1 || logline ERROR release-all-failed
			logline RELEASED "$task"
		fi
		"$BACKEND" close "$task" --reason="sim: done by $agent" --actor="$agent" >/dev/null 2>&1 ||
			logline ERROR close-failed "$task"
		done_count=$((done_count + 1))
		logline DONE "$task"
	done

	logline SUMMARY attempts="$attempts" completed="$done_count" conflicts="$conflicts" \
		claim_races="$claim_races" commit_retries="$commit_retries"
	printf '%s %s %s %s %s\n' "$agent" "$done_count" "$conflicts" "$claim_races" "$commit_retries" \
		>>"$RUN_DIR/worker-summary.txt"
}

say "=== running $WORKERS workers (scope=$([ "$SCOPE_LOCAL" -eq 1 ] && echo local || echo shared), worktrees=$USE_WORKTREES, steal-storm=$STEAL_STORM, leases=$([ "$NO_LEASES" -eq 1 ] && echo OFF || echo on))"
START_EPOCH="$(date +%s)"
for w in $(seq 1 "$WORKERS"); do
	worker "$w" &
done
wait
END_EPOCH="$(date +%s)"
ELAPSED=$((END_EPOCH - START_EPOCH))

# --------------------------------------------------------------- manifest
#
# Everything fleet-verify.sh needs, so the two scripts share no assumptions
# beyond this file.
WORKER_DIRS_JSON="{"
for w in $(seq 1 "$WORKERS"); do
	[ "$w" -eq 1 ] || WORKER_DIRS_JSON="$WORKER_DIRS_JSON,"
	WORKER_DIRS_JSON="$WORKER_DIRS_JSON\"sim-w$w\":\"${WORKER_DIR[w]}\""
done
WORKER_DIRS_JSON="$WORKER_DIRS_JSON}"

cat >"$RUN_DIR/manifest.json" <<EOF
{
  "run_dir": "$RUN_DIR",
  "worker_dirs": $WORKER_DIRS_JSON,
  "no_leases": $([ "$NO_LEASES" -eq 1 ] && echo true || echo false),
  "repo": "$WORK",
  "logs": "$LOGS",
  "workers": $WORKERS,
  "tasks_seeded": $TASK_N,
  "backend": "$BACKEND",
  "worktrees": $([ "$USE_WORKTREES" -eq 1 ] && echo true || echo false),
  "scope_local": $([ "$SCOPE_LOCAL" -eq 1 ] && echo true || echo false),
  "steal_storm": $([ "$STEAL_STORM" -eq 1 ] && echo true || echo false),
  "elapsed_secs": $ELAPSED,
  "pact": "$PACT"
}
EOF

say ""
say "=== fleet done in ${ELAPSED}s"
if [ -f "$RUN_DIR/worker-summary.txt" ]; then
	say "    agent          done conflicts claim-races commit-retries"
	sort "$RUN_DIR/worker-summary.txt" | awk '{printf "    %-14s %4s %9s %11s %14s\n",$1,$2,$3,$4,$5}'
fi
say ""
say "verify with: scripts/fleet-verify.sh $RUN_DIR"
[ "$KEEP" -eq 1 ] && say "(run dir kept; delete it yourself when done)"
printf '%s\n' "$RUN_DIR"
