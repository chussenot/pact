#!/usr/bin/env bash
# Inject faults into a running pact fleet, to test recovery rather than mechanics.
#
# scripts/fleet-sim.sh proves the primitives hold under CONTENTION: twenty
# workers racing one checkout, every one of them behaving. This proves what
# happens when they stop behaving — an agent SIGKILLed mid-hold, the Beads CLI
# vanishing from PATH, a lock file truncated to nothing, a lease backdated past
# its own TTL. Sim answers "does coordination work"; chaos answers "does it
# recover", which is a different question and the one a real fleet eventually
# asks on its own.
#
# THIS SCRIPT IS DESTRUCTIVE BY PURPOSE. It kills processes and mutates files
# it did not create. Nearly all of the code below is the blast radius, not the
# faults — the faults are a dozen lines each.
#
# ## The rails, and why each exists
#
# 1. TWO MARKERS, not one. `--repo` must contain `.pact/` (it is a pact repo)
#    AND `.chaos-armed` (somebody decided THIS repo is disposable). A single
#    marker is one typo away from a real repository; `.pact/` alone is present
#    in every repo pact has ever touched, including this one.
# 2. NEVER THE PACT REPO. Refused by canonical path against this script's own
#    checkout, and again by looking for pact's own sources. A fault injector
#    that can kill the fleet developing it is a footgun with a scope.
# 3. A PID ALLOWLIST, RE-VERIFIED PER KILL. chaos never signals a PID that is
#    not in `--pids`, and before every signal it re-reads that PID's working
#    directory and confirms it is under `--repo`. PIDs are recycled; a stale
#    entry naming a PID the OS has since handed to something else is the exact
#    way a bounded tool becomes unbounded. A PID that fails verification is
#    skipped and logged, never killed.
# 4. RESTORE IS A TRAP, NOT A CODE PATH. `backend-outage` renames a binary, so
#    it must be put back even if chaos is killed. The trap fires on EXIT, INT
#    and TERM. An outage must never outlive the process that caused it.
# 5. THE HOME PREFIX ONLY. `backend-outage` refuses to rename a binary outside
#    $HOME. Hiding /usr/bin/bd would break the machine, not the fleet.
#
# ## The join contract
#
# Every decision appends one line to `<repo>/chaos-log.jsonl`:
#
#   {"ts":…,"seed":…,"action":…,"target":…,"detail":…,"dry":…}
#
# INCLUDING skips and refusals. That is the point of the file rather than a
# nicety: an analysis joining effects to causes also has to join every
# NON-effect to the rail that prevented it, or "chaos did nothing here" is
# indistinguishable from "chaos tried and a rail stopped it". A rail that
# fires silently is a rail nobody can audit.
#
# ## Determinism
#
# Randomness comes from sha256("<seed>:<counter>"), not from $RANDOM and not
# from awk's srand(). $RANDOM is unseedable across processes. awk's srand() is
# seedable but its generator is implementation-defined — mawk, gawk and busybox
# awk produce DIFFERENT sequences for the same seed, so a plan built with it
# would reproduce on one machine and not another, which is worse than obvious
# non-determinism because it looks reproducible until it is not.
#
# The whole fault plan is computed BEFORE anything is touched, so `--dry-run`
# emits exactly the decision sequence a real run would execute.
#
#   scripts/chaos.sh --repo /tmp/fleet --pids /tmp/fleet/pids --dry-run
#   scripts/chaos.sh --repo /tmp/fleet --pids /tmp/fleet/pids --seed 7 --duration 30
#   scripts/chaos.sh --repo /tmp/fleet --pids /tmp/fleet/pids --actions kill-holder
#   scripts/chaos.sh … --time-unit sec        # duration AND gaps in seconds (tests)
#
# MUST NEVER RUN OUTSIDE A DISPOSABLE FLEET REPO. See docs/testing.md.

set -euo pipefail

REPO=""
PIDS_FILE=""
SEED=""
DURATION_MIN=10
ACTIONS="kill-holder,backend-outage,lock-vandal,stale-lock"
INTERVAL_MIN=4
INTERVAL_MAX=7
TIME_UNIT=min
DRY_RUN=0
PATHS_HINT=""
OUTAGE_SECS=90

die() {
	printf 'chaos: %s\n' "$1" >&2
	exit 1
}
say() { printf '%s\n' "$*"; }

usage() {
	sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
	exit 0
}

while [ $# -gt 0 ]; do
	case "$1" in
	--repo)
		REPO="${2:-}"
		shift 2
		;;
	--pids)
		PIDS_FILE="${2:-}"
		shift 2
		;;
	--seed)
		SEED="${2:-}"
		shift 2
		;;
	--duration)
		DURATION_MIN="${2:-}"
		shift 2
		;;
	--actions)
		ACTIONS="${2:-}"
		shift 2
		;;
	--interval-min)
		INTERVAL_MIN="${2:-}"
		shift 2
		;;
	--interval-max)
		INTERVAL_MAX="${2:-}"
		shift 2
		;;
	--time-unit)
		TIME_UNIT="${2:-}"
		shift 2
		;;
	--paths-hint)
		PATHS_HINT="${2:-}"
		shift 2
		;;
	--outage-secs)
		OUTAGE_SECS="${2:-}"
		shift 2
		;;
	--dry-run)
		DRY_RUN=1
		shift
		;;
	-h | --help) usage ;;
	*) die "unknown flag: $1 (try --help)" ;;
	esac
done

# ---------------------------------------------------------------- validation

[ -n "$REPO" ] || die "--repo is required"
[ -n "$PIDS_FILE" ] || die "--pids is required"

for n in DURATION_MIN INTERVAL_MIN INTERVAL_MAX OUTAGE_SECS; do
	v="${!n}"
	case "$v" in '' | *[!0-9]*) die "--${n,,} must be a number, got '$v'" ;; esac
done
[ "$INTERVAL_MAX" -ge "$INTERVAL_MIN" ] || die "--interval-max ($INTERVAL_MAX) is below --interval-min ($INTERVAL_MIN)"
[ "$INTERVAL_MIN" -ge 1 ] || die "--interval-min must be at least 1"

# The unit for `--duration` AND the intervals, together. Minutes are the useful
# scale for a real soak and the default for that reason.
#
# It scales both on purpose. A unit that applied only to the intervals would
# make `--time-unit sec --duration 5` mean "five MINUTES of one-second gaps" —
# three hundred faults and five minutes of real sleeping. The first cut did
# exactly that and hung the test suite, which is a fair warning about what it
# would do to somebody expecting a quick smoke run.
#
# Seconds exist because the tests drive real faults end to end, and a test that
# waits a minute for the first one is a test nobody runs.
case "$TIME_UNIT" in
min) UNIT_SECS=60 ;;
sec) UNIT_SECS=1 ;;
*) die "--time-unit must be min or sec, got '$TIME_UNIT'" ;;
esac

if [ -z "$SEED" ]; then
	# A run with no seed still has to be reproducible AFTER the fact, so the
	# seed it picked is logged like every other decision. An unlogged random
	# seed makes a failure unrepeatable, which is the one thing a fault
	# injector must never be.
	SEED="$(date +%s)"
fi
case "$SEED" in '' | *[!0-9]*) die "--seed must be a number, got '$SEED'" ;; esac

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not on PATH"; }
need jq
need git

# ------------------------------------------------------------- rails 1 and 2

[ -d "$REPO" ] || die "--repo is not a directory: $REPO"
REPO="$(cd "$REPO" && pwd -P)"

[ -d "$REPO/.pact" ] || die "REFUSING TO RUN: no .pact/ under
  $REPO
That is not a repository a pact fleet has ever run in, so there is nothing here
to inject faults into."

# The pact repo itself, by canonical path. `$0` may be a relative path or a
# symlink, so resolve the script's own checkout rather than trusting the
# invocation.
SELF_DIR="$(cd "$(dirname "$0")/.." && pwd -P)"
if [ "$REPO" = "$SELF_DIR" ]; then
	die "REFUSING TO RUN: --repo is pact's own checkout
  $REPO
Its .pact/events.jsonl is committed and is the evidence base the guard-file
decision (pact-ehi) reads. A fault injector pointed at the repository that
develops it would corrupt exactly the history used to judge whether the faults
matter."
fi

# And again by content, because a copy of pact's checkout is still pact's
# sources and a fleet does not develop pact.
if [ -f "$REPO/src/lease.rs" ] && [ -f "$REPO/Cargo.toml" ] &&
	grep -qE '^name *= *"pact"' "$REPO/Cargo.toml" 2>/dev/null; then
	die "REFUSING TO RUN: --repo looks like a pact source checkout
  $REPO
(it has src/lease.rs and a Cargo.toml naming the pact package). Point chaos at
the fleet's target repository, not at pact."
fi

[ -f "$REPO/.chaos-armed" ] || die "REFUSING TO RUN: no .chaos-armed marker in
  $REPO
This script kills processes and mutates lock files. It runs only where somebody
has deliberately declared the repository disposable:

  touch $REPO/.chaos-armed

.pact/ alone is not enough on purpose — every repository pact has ever touched
has one, including pact's own."

[ -f "$PIDS_FILE" ] || die "--pids file does not exist: $PIDS_FILE
The orchestrator creates it and appends 'pid<TAB>agent' as each agent spawns."

LOG="$REPO/chaos-log.jsonl"

# ------------------------------------------------------------------- logging

# One JSON object per line, built with jq so a path or a note containing a quote
# cannot produce a line the analysis fails to parse.
log_decision() {
	local action="$1" target="$2" detail="$3"
	jq -cn \
		--arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
		--argjson seed "$SEED" \
		--arg action "$action" \
		--arg target "$target" \
		--arg detail "$detail" \
		--argjson dry "$([ "$DRY_RUN" -eq 1 ] && echo true || echo false)" \
		'{ts:$ts, seed:$seed, action:$action, target:$target, detail:$detail, dry:$dry}' \
		>>"$LOG"
}

# A refusal is a decision. Logged with the action that was refused so the
# analysis can tell "chaos never tried" from "a rail stopped it".
log_skip() { log_decision "$1" "$2" "SKIPPED: $3"; }

# ---------------------------------------------------------------------- PRNG

RAND_N=0
sha_hex() {
	if command -v sha256sum >/dev/null 2>&1; then
		printf '%s' "$1" | sha256sum | cut -c1-8
	elif command -v shasum >/dev/null 2>&1; then
		printf '%s' "$1" | shasum -a 256 | cut -c1-8
	else
		printf '%s' "$1" | openssl dgst -sha256 | sed 's/.*= *//' | cut -c1-8
	fi
}

# Uniform-ish integer in [0, max), left in $RAND_OUT.
#
# It returns through a global instead of stdout, and that is not a style choice.
# `x="$(rand_below 4)"` runs the function in a SUBSHELL, so `RAND_N` advances in
# the child and is discarded — every draw then hashes the same "<seed>:0" and
# returns the same number. The first cut of this script did exactly that: five
# draws from one max all returned 50, so every interval gap was identical and
# the plan varied only as the action pool shrank. It still looked seeded,
# because a different seed gives a different constant, which is the precise
# failure mode the header warns about one paragraph up.
#
# Biased by the modulo for max values that do not divide 2^32, which is
# irrelevant here: these choices pick between four actions and a few minutes,
# not cryptographic material.
RAND_OUT=0
rand_below() {
	local max="$1" hex
	[ "$max" -gt 0 ] || die "rand_below called with max=$max"
	hex="$(sha_hex "${SEED}:${RAND_N}")"
	RAND_N=$((RAND_N + 1))
	RAND_OUT=$((0x$hex % max))
}

# ------------------------------------------------------------- action parsing

ONCE_ACTIONS=" lock-vandal stale-lock "
declare -a ENABLED=()
IFS=',' read -r -a _requested <<<"$ACTIONS"
for a in "${_requested[@]}"; do
	case "$a" in
	kill-holder | backend-outage | lock-vandal | stale-lock) ENABLED+=("$a") ;;
	'') ;;
	*) die "unknown action: $a
known: kill-holder, backend-outage, lock-vandal, stale-lock" ;;
	esac
done
[ "${#ENABLED[@]}" -gt 0 ] || die "--actions selected nothing"

# ------------------------------------------------------------------- the plan
#
# Built in full before a single fault lands, so --dry-run prints exactly the
# sequence a real run would perform. Each entry is "offset_seconds action".
declare -a PLAN=()
plan_build() {
	local t=0 total=$((DURATION_MIN * UNIT_SECS)) span=$((INTERVAL_MAX - INTERVAL_MIN + 1))
	local -a pool=("${ENABLED[@]}")
	while :; do
		rand_below "$span"
		t=$((t + (INTERVAL_MIN + RAND_OUT) * UNIT_SECS))
		[ "$t" -le "$total" ] || break
		[ "${#pool[@]}" -gt 0 ] || break
		rand_below "${#pool[@]}"
		local action="${pool[$RAND_OUT]}"
		PLAN+=("$t $action")
		# A once-per-run action leaves the pool after it is scheduled, so the
		# plan cannot contain two of them however long the run is.
		case "$ONCE_ACTIONS" in
		*" $action "*)
			local -a rest=()
			local p
			for p in "${pool[@]}"; do [ "$p" = "$action" ] || rest+=("$p"); done
			pool=("${rest[@]+"${rest[@]}"}")
			;;
		esac
	done
}
plan_build

# ------------------------------------------------------------ PID verification

# The working directory of a live process, or empty if it cannot be read.
pid_cwd() {
	local pid="$1"
	if [ -r "/proc/$pid/cwd" ]; then
		readlink "/proc/$pid/cwd" 2>/dev/null || true
	elif command -v lsof >/dev/null 2>&1; then
		lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1
	else
		printf ''
	fi
}

# Is this PID one the orchestrator registered, AND still living under --repo?
# Both halves are re-checked at signal time. The allowlist alone is not enough:
# PIDs are recycled, so an entry naming a process that has exited can name
# something else entirely by the time chaos reads it.
pid_is_ours() {
	local pid="$1" cwd
	grep -qE "^${pid}[[:space:]]" "$PIDS_FILE" || return 1
	kill -0 "$pid" 2>/dev/null || return 1
	cwd="$(pid_cwd "$pid")"
	[ -n "$cwd" ] || return 1
	case "$cwd" in "$REPO" | "$REPO"/*) return 0 ;; *) return 1 ;; esac
}

agent_pid() {
	local agent="$1"
	awk -F'\t' -v a="$agent" '$2 == a { print $1; exit }' "$PIDS_FILE"
}

# ------------------------------------------------------------------- teardown

HIDDEN_BINARY=""
OUTAGE_SLEEP_PID=""
restore_backend() {
	# The pending outage timer first, so the wait below returns and the shell
	# can actually get to its own exit.
	if [ -n "$OUTAGE_SLEEP_PID" ]; then
		kill "$OUTAGE_SLEEP_PID" 2>/dev/null || true
		OUTAGE_SLEEP_PID=""
	fi
	# Idempotent and quiet: the trap runs on every exit, including the clean
	# one where nothing is hidden.
	if [ -n "$HIDDEN_BINARY" ] && [ -e "$HIDDEN_BINARY.chaos-hidden" ]; then
		mv -f "$HIDDEN_BINARY.chaos-hidden" "$HIDDEN_BINARY" 2>/dev/null || true
		log_decision backend-outage "$HIDDEN_BINARY" "restored by trap"
		HIDDEN_BINARY=""
	fi
}
# EXIT restores. INT/TERM restore AND exit, which is not the same trap and not
# a redundancy: a handler that only restores lets the script CONTINUE, so chaos
# put the binary back on TERM and then re-hid it at the next planned outage —
# the signal looked handled and was not. 143 is the conventional 128+SIGTERM.
trap restore_backend EXIT
trap 'restore_backend; exit 143' INT TERM

# Sleep in a way a trap can actually interrupt.
#
# `sleep N` in the foreground does NOT work here, and the failure is quiet:
# bash defers a trap until the current foreground command finishes, so a TERM
# arriving during a 90-second outage runs the handler ninety seconds later —
# after the outage would have ended on its own. Rail 4 claims an outage never
# outlives chaos; with a foreground sleep, chaos does not outlive the sleep.
#
# Backgrounding it and `wait`ing is what makes the trap prompt: `wait` is
# interruptible, and the handler kills the timer before restoring.
interruptible_sleep() {
	sleep "$1" &
	OUTAGE_SLEEP_PID=$!
	wait "$OUTAGE_SLEEP_PID" 2>/dev/null || true
	OUTAGE_SLEEP_PID=""
}

# -------------------------------------------------------------------- actions

do_kill_holder() {
	local leases holders agent pid paths
	leases="$(cd "$REPO" && pact lease ls --json 2>/dev/null || echo '[]')"
	holders="$(jq -r '[.[] | select(.expired == false) | .lease.agent] | unique | .[]' <<<"$leases" 2>/dev/null || true)"
	if [ -z "$holders" ]; then
		log_skip kill-holder "" "no live lease holders to kill"
		return
	fi
	# Only holders the orchestrator registered are candidates at all.
	local -a candidates=()
	while IFS= read -r agent; do
		[ -n "$agent" ] || continue
		pid="$(agent_pid "$agent")"
		[ -n "$pid" ] && candidates+=("$pid	$agent")
	done <<<"$holders"
	if [ "${#candidates[@]}" -eq 0 ]; then
		log_skip kill-holder "" "no lease holder maps to a registered pid"
		return
	fi
	rand_below "${#candidates[@]}"
	pid="${candidates[$RAND_OUT]%%	*}"
	agent="${candidates[$RAND_OUT]##*	}"
	paths="$(jq -r --arg a "$agent" '[.[] | select(.lease.agent == $a) | .lease.path] | join(",")' <<<"$leases")"

	if ! pid_is_ours "$pid"; then
		log_skip kill-holder "$agent pid=$pid" "pid not in --pids, not alive, or cwd outside --repo"
		return
	fi
	if [ "$DRY_RUN" -eq 1 ]; then
		log_decision kill-holder "$agent pid=$pid" "would SIGKILL; holds: $paths"
		return
	fi
	kill -9 "$pid" 2>/dev/null || true
	log_decision kill-holder "$agent pid=$pid" "SIGKILL sent; held: $paths"
}

do_backend_outage() {
	local bin path
	for bin in bd br; do
		path="$(command -v "$bin" 2>/dev/null || true)"
		[ -n "$path" ] && break
	done
	if [ -z "$path" ]; then
		log_skip backend-outage "" "no bd or br on PATH to hide"
		return
	fi
	path="$(cd "$(dirname "$path")" && pwd -P)/$(basename "$path")"
	# Rail 5. Renaming a binary out from under the machine is not a fleet fault.
	case "$path" in
	"$HOME"/*) ;;
	*)
		log_skip backend-outage "$path" "outside \$HOME; refusing to touch a system path"
		return
		;;
	esac
	if [ "$DRY_RUN" -eq 1 ]; then
		log_decision backend-outage "$path" "would hide for ${OUTAGE_SECS}s"
		return
	fi
	mv "$path" "$path.chaos-hidden" || {
		log_skip backend-outage "$path" "rename failed"
		return
	}
	HIDDEN_BINARY="$path"
	log_decision backend-outage "$path" "hidden for ${OUTAGE_SECS}s"
	interruptible_sleep "$OUTAGE_SECS"
	restore_backend
}

do_lock_vandal() {
	local dir="$REPO/.pact/leases" target before
	local -a locks=()
	if [ -d "$dir" ]; then
		while IFS= read -r f; do locks+=("$f"); done < <(find "$dir" -maxdepth 1 -name '*.lock' -type f | sort)
	fi
	if [ "${#locks[@]}" -eq 0 ]; then
		log_skip lock-vandal "" "no .lock files under .pact/leases"
		return
	fi
	rand_below "${#locks[@]}"
	target="${locks[$RAND_OUT]}"
	before="$(tr -d '\n' <"$target" 2>/dev/null || echo '<unreadable>')"
	if [ "$DRY_RUN" -eq 1 ]; then
		log_decision lock-vandal "$target" "would truncate to 0 bytes; was: $before"
		return
	fi
	: >"$target"
	log_decision lock-vandal "$target" "truncated to 0 bytes; was: $before"
}

# A stale lock is written by pact itself and then backdated, rather than
# composed here from a hardcoded template. That is the drift guard: pact owns
# the lock's shape, so the only field chaos invents is the timestamp, and the
# keys it depends on are asserted before anything is written. If the shape ever
# changes, this fails loudly instead of writing a file pact will call corrupt.
do_stale_lock() {
	local hint_path lock encoded now_ttl backdated content
	if [ -z "$PATHS_HINT" ] || [ ! -f "$PATHS_HINT" ]; then
		log_skip stale-lock "" "no --paths-hint file to choose a path from"
		return
	fi
	local -a hints=()
	while IFS= read -r p; do [ -n "$p" ] && hints+=("$p"); done <"$PATHS_HINT"
	if [ "${#hints[@]}" -eq 0 ]; then
		log_skip stale-lock "$PATHS_HINT" "paths-hint file is empty"
		return
	fi
	rand_below "${#hints[@]}"
	hint_path="${hints[$RAND_OUT]}"

	if [ "$DRY_RUN" -eq 1 ]; then
		log_decision stale-lock "$hint_path" "would plant a lock backdated past ttl+grace"
		return
	fi

	# Let pact write it, so the shape is pact's.
	if ! (cd "$REPO" && PACT_AGENT=chaos-ghost pact lease acquire "$hint_path" \
		--note "chaos: planted stale lease" >/dev/null 2>&1); then
		log_skip stale-lock "$hint_path" "pact lease acquire failed (already held?)"
		return
	fi
	encoded="$(printf '%s' "$hint_path" | sed 's|/|__|g')"
	lock="$REPO/.pact/leases/${encoded}.lock"
	if [ ! -f "$lock" ]; then
		log_skip stale-lock "$hint_path" "expected lock at $lock after acquire, found none"
		return
	fi
	content="$(cat "$lock")"
	# The drift assertion. Every key chaos relies on must be present and of the
	# type it expects, or this refuses rather than guessing.
	if ! jq -e 'has("acquired_at") and has("ttl_secs") and (.ttl_secs|type=="number") and has("agent")' \
		<<<"$content" >/dev/null 2>&1; then
		log_skip stale-lock "$lock" "LOCK SHAPE DRIFTED: no acquired_at/ttl_secs/agent; refusing to write garbage"
		return
	fi
	now_ttl="$(jq -r '.ttl_secs' <<<"$content")"
	# ttl + grace + a minute, so the lease is provably past the documented
	# GRACE_SECS window rather than sitting on its boundary.
	backdated="$(date -u -d "@$(($(date +%s) - now_ttl - 30 - 60))" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null ||
		date -u -r "$(($(date +%s) - now_ttl - 30 - 60))" +%Y-%m-%dT%H:%M:%SZ)"
	jq --arg t "$backdated" '.acquired_at = $t' <<<"$content" >"$lock.chaos-tmp"
	mv -f "$lock.chaos-tmp" "$lock"
	log_decision stale-lock "$hint_path" "backdated acquired_at to $backdated (ttl ${now_ttl}s + 30s grace)"
}

# ----------------------------------------------------------------------- main

if [ "$DRY_RUN" -eq 0 ]; then
	need pact
fi

log_decision run-start "$REPO" "seed=$SEED duration=${DURATION_MIN}${TIME_UNIT} actions=$ACTIONS interval=${INTERVAL_MIN}-${INTERVAL_MAX}${TIME_UNIT} unit=${TIME_UNIT} planned=${#PLAN[@]}"
say "chaos: seed $SEED, ${#PLAN[@]} fault(s) planned over ${DURATION_MIN}${TIME_UNIT} in $REPO"
[ "$DRY_RUN" -eq 1 ] && say "chaos: DRY RUN — every decision logged, nothing touched"

# Iterated by index, never by word-splitting the array, so an action name can
# not be re-split into two decisions.
elapsed=0
for i in "${!PLAN[@]}"; do
	at="${PLAN[$i]%% *}"
	action="${PLAN[$i]##* }"
	if [ "$DRY_RUN" -eq 0 ]; then
		# Same reasoning as interruptible_sleep: a TERM waiting out a
		# five-minute gap is a TERM that looks ignored.
		interruptible_sleep $((at - elapsed))
	fi
	elapsed="$at"
	case "$action" in
	kill-holder) do_kill_holder ;;
	backend-outage) do_backend_outage ;;
	lock-vandal) do_lock_vandal ;;
	stale-lock) do_stale_lock ;;
	esac
done

log_decision run-end "$REPO" "executed ${#PLAN[@]} planned fault(s)"
say "chaos: done — $LOG"
