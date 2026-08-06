#!/usr/bin/env bash
# Exercise pact against a REAL bd binary, so upstream CLI drift is found by a
# machine instead of by a user.
#
# tests/cli.rs stubs bd. That is the right call for a unit suite — it is fast
# and hermetic — but it means nothing checks the assumptions pact makes about
# somebody else's CLI: `--include-infra`, `--parent` threading, whether `--json`
# hydrates `labels`, and the shapes those commands return. Every one of those
# was verified once by hand against one version and then trusted forever.
#
# Run it locally exactly as CI does:
#   scripts/canary.sh                 # whatever bd is on PATH
#   CANARY_LEG=latest scripts/canary.sh
#
# CANARY_LEG only affects one assertion: on `latest`, a bd outside pact's tested
# range MUST produce doctor's version warning. That is the difference between
# "the warning logic works in a unit test" and "the warning fires on real drift".

set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"
LEG="${CANARY_LEG:-pinned}"

fail() {
	printf '\nCANARY FAILED: %s\n' "$1" >&2
	exit 1
}
step() { printf '\n=== %s\n' "$1"; }

# First line of a string, WITHOUT piping into `head`.
#
# `producer | head -1` under `set -o pipefail` is a trap: head exits after one
# line, the producer takes SIGPIPE, the pipeline reports 141 and `set -e` kills
# the script. It is a race, so it passes on a fast machine and fails on a CI
# runner — which is exactly what happened on the first real run of this canary,
# at the first pipeline, with no output at all. pact has a whole invariant about
# this failure mode (never `println!`, see output::line); the shell version of
# the same lesson is: don't pipe into head when you can slice a variable.
first_line() { printf '%s' "${1%%$'\n'*}"; }

need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is required but not on PATH"; }
need bd
need jq
need git

# ---------------------------------------------------------------- the range
#
# Read from src/beads.rs, never restated here. A second copy of this range is a
# thing that drifts silently from the code it claims to describe, which is the
# whole failure mode this canary exists to catch.
read_triplet() {
	first_line "$(sed -nE "s/^const $1: \(u64, u64, u64\) = \(([0-9]+), ([0-9]+), ([0-9]+)\);/\1.\2.\3/p" src/beads.rs)"
}
BD_MIN="$(read_triplet TESTED_BD_MIN)"
BD_MAX="$(read_triplet TESTED_BD_MAX_EXCLUSIVE)"
[ -n "$BD_MIN" ] && [ -n "$BD_MAX" ] ||
	fail "could not read TESTED_BD_MIN/TESTED_BD_MAX_EXCLUSIVE out of src/beads.rs — has the declaration changed shape?"

# Parse the version from ANYWHERE in the output, not from the first line. bd
# prefixes warnings when it finds something it does not like about the
# environment — on a CI runner, `.beads has permissions 0755 (recommended:
# 0700)`, because that is how checkout leaves the directory pact commits. An
# earlier cut took the first line and reported "could not parse a version out
# of: Warning: ...", which is a canary failing on the weather.
BD_OUTPUT="$(bd --version 2>&1)"
BD_VERSION="$(first_line "$(grep -oE '[0-9]+\.[0-9]+\.[0-9]+' <<<"$BD_OUTPUT")")"
[ -n "$BD_VERSION" ] || fail "could not parse a version out of: $BD_OUTPUT"
# The line that actually carries the version, for the log.
BD_VERSION_RAW="$(first_line "$(grep -F "$BD_VERSION" <<<"$BD_OUTPUT")")"

# Sort-based comparison: no arithmetic on version parts, no assumptions about
# how many components there are.
ver_lt() { [ "$1" != "$2" ] && [ "$(first_line "$(sort -V <<<"$1"$'\n'"$2")")" = "$1" ]; }
IN_RANGE=yes
if ver_lt "$BD_VERSION" "$BD_MIN" || [ "$BD_VERSION" = "$BD_MAX" ] || ! ver_lt "$BD_VERSION" "$BD_MAX"; then
	IN_RANGE=no
fi

step "environment"
printf '  leg:          %s\n' "$LEG"
printf '  bd:           %s\n' "$BD_VERSION_RAW"
printf '  tested range: %s <= v < %s (from src/beads.rs)\n' "$BD_MIN" "$BD_MAX"
printf '  in range:     %s\n' "$IN_RANGE"

# bd 1.1.x embeds Dolt ("no external server needed" per `bd init --help`), and a
# fresh `bd init` was verified to succeed with no `dolt` binary on PATH. If a
# future bd requires an external Dolt, this is where it would surface — as an
# init failure with a clear message, which is a fine way to find out.

# ---------------------------------------------------------------- build pact
step "build pact"
cargo build --quiet
PACT="$REPO_ROOT/target/debug/pact"
[ -x "$PACT" ] || fail "cargo build produced no binary at $PACT"
first_line "$("$PACT" --version)" && echo

# ------------------------------------------------------------- scratch repo
WORK="$(mktemp -d)"
# Scratch space that must NOT live inside the repo under test; see the sibling
# worktree section at the end.
OUTSIDE="$(mktemp -d)"
cleanup() { rm -rf "$WORK" "$OUTSIDE"; }
trap cleanup EXIT

step "bd init in a scratch repo"
cd "$WORK"
git init -q .
git config user.email canary@pact.invalid
git config user.name "pact canary"
# --non-interactive is the documented flag; BD_NON_INTERACTIVE=1 is belt and
# braces, since a canary that blocks on a prompt hangs the job until timeout.
BD_NON_INTERACTIVE=1 bd init --non-interactive --prefix canary >/dev/null ||
	fail "bd init failed — see output above"
[ -d .beads ] || fail "bd init reported success but created no .beads/"

# The realistic order: a user runs `bd init`, then `pact init`. Without it
# doctor rightly reports an uninitialised repo, which is a true answer to a
# question the canary did not mean to ask.
#
# --no-commit because the canary is asserting pact's behaviour against bd, not
# exercising git; a commit here would only add a way for the run to fail on
# something unrelated.
step "pact init"
"$PACT" init --no-commit >/dev/null || fail "pact init failed"

# ------------------------------------------------------------------- doctor
step "pact doctor"
DOCTOR_JSON="$WORK/doctor.json"
if ! PACT_AGENT=canary-a "$PACT" doctor --json >"$DOCTOR_JSON" 2>"$WORK/doctor.err"; then
	cat "$DOCTOR_JSON" "$WORK/doctor.err" >&2
	fail "pact doctor exited non-zero against real bd"
fi
jq -e '.checks | length > 0' "$DOCTOR_JSON" >/dev/null || fail "doctor emitted no checks"

BEADS_CHECK="$(jq -c '.checks[] | select(.name == "Beads CLI")' "$DOCTOR_JSON")"
[ -n "$BEADS_CHECK" ] || fail "doctor has no \"Beads CLI\" check: $(cat "$DOCTOR_JSON")"
printf '  %s\n' "$BEADS_CHECK"
jq -e '.ok' <<<"$BEADS_CHECK" >/dev/null || fail "doctor could not use the real bd: $BEADS_CHECK"

# The assertion that makes the two-leg split worth having. A unit test proves
# version_compat_warning() returns Some for a made-up string; this proves the
# whole path fires when a genuinely newer bd is installed.
if [ "$IN_RANGE" = no ]; then
	jq -e '.warn' <<<"$BEADS_CHECK" >/dev/null ||
		fail "bd $BD_VERSION is outside $BD_MIN..$BD_MAX but doctor did not warn: $BEADS_CHECK"
	printf '  drift detected and warned, as designed\n'
elif [ "$LEG" = latest ]; then
	printf '  latest bd is still inside the tested range; nothing to warn about\n'
fi

# ------------------------------------------------------- message round-trip
# The real target: --include-infra, --parent threading and label hydration are
# all bd behaviours pact assumes and never verifies against a running bd.
step "message round-trip"
PACT_AGENT=canary-a "$PACT" msg send --to canary-b --subject "canary" "ping" >/dev/null ||
	fail "msg send failed"

INBOX="$WORK/inbox.json"
PACT_AGENT=canary-b "$PACT" msg inbox --json >"$INBOX" || fail "msg inbox failed"
jq -e 'length == 1' "$INBOX" >/dev/null ||
	fail "expected exactly 1 message in canary-b's inbox, got $(jq length "$INBOX"): $(cat "$INBOX")"

MSG="$(jq -c '.[0]' "$INBOX")"
printf '  %s\n' "$MSG"
for expr in '.subject == "canary"' '.from == "canary-a"' '.to == "canary-b"' \
	'.body == "ping"' '(.thread | length) > 0' '(.id | length) > 0' '.read == false'; do
	jq -e "$expr" <<<"$MSG" >/dev/null || fail "inbox message failed [$expr]: $MSG"
done

MSG_ID="$(jq -r '.id' <<<"$MSG")"
PACT_AGENT=canary-b "$PACT" msg read "$MSG_ID" >/dev/null || fail "msg read failed for $MSG_ID"

# Read state lives in bd labels (read-by-<agent>), so this asserts label
# hydration actually round-tripped through the real binary.
UNREAD="$WORK/unread.json"
PACT_AGENT=canary-b "$PACT" msg inbox --unread-only --json >"$UNREAD" ||
	fail "msg inbox --unread-only failed"
jq -e 'length == 0' "$UNREAD" >/dev/null ||
	fail "message stayed unread after msg read — label round-trip broken: $(cat "$UNREAD")"

# The sender's side of the same fact.
SENT="$WORK/sent.json"
PACT_AGENT=canary-a "$PACT" msg sent --json >"$SENT" || fail "msg sent failed"
jq -e --arg id "$MSG_ID" 'any(.[]; .id == $id and (.read_by | index("canary-b")))' "$SENT" >/dev/null ||
	fail "sender cannot see that canary-b read it: $(cat "$SENT")"
printf '  read state visible to the sender\n'

# --------------------------------------------------------------- lease smoke
step "lease smoke"
PACT_AGENT=canary-a "$PACT" lease acquire src/canary.rs --note "canary run" >/dev/null ||
	fail "lease acquire failed"
LS="$WORK/ls.json"
PACT_AGENT=canary-a "$PACT" lease ls --json >"$LS" || fail "lease ls failed"
jq -e 'any(.[]; .lease.path == "src/canary.rs" and .lease.agent == "canary-a")' "$LS" >/dev/null ||
	fail "acquired lease is not in lease ls: $(cat "$LS")"

PACT_AGENT=canary-a "$PACT" lease release --all >/dev/null || fail "lease release --all failed"
PACT_AGENT=canary-a "$PACT" lease ls --json >"$LS" || fail "lease ls after release failed"
jq -e 'length == 0' "$LS" >/dev/null || fail "leases remain after release --all: $(cat "$LS")"
printf '  acquire, list and release round-tripped\n'

# ----------------------------------------------------- actor attribution
#
# Every mutating backend call pact makes must be recorded against the AGENT that
# caused it, not against whoever owns the checkout. Without that, a fleet's whole
# bead history is attributed to one human and the audit trail cannot answer the
# only question it exists for: who did this.
#
# Both backends take the same flag, and pact passes it — `bd` 1.1.2 documents
# precedence `--actor` > `$BEADS_ACTOR` > `git user.name` > `$USER`, and `br`
# 0.2.19 accepts `--actor` too. This asserts the end-to-end result rather than the
# flag: the canary's git user is deliberately NOT an agent name, so a match on
# `canary-a` can only come from attribution working.
step "backend attributes writes to the agent, not the git user"
GIT_USER="$(git config user.name)"
[ "$GIT_USER" = "pact canary" ] || fail "expected the scratch repo's git user to be 'pact canary', got '$GIT_USER'"

ACTOR_JSON="$WORK/actor.json"
if ! bd show "$MSG_ID" --json >"$ACTOR_JSON" 2>&1; then
	cat "$ACTOR_JSON" >&2
	fail "could not read $MSG_ID back from the backend"
fi
# bd returns a bare object from `show`; br has returned an array. Accept either
# rather than branching on the backend.
RECORDED="$(jq -r 'if type == "array" then .[0] else . end | .created_by // ""' "$ACTOR_JSON")"
printf '  git user.name: %s\n  created_by:    %s\n' "$GIT_USER" "$RECORDED"
[ "$RECORDED" = "canary-a" ] ||
	fail "message $MSG_ID is attributed to '$RECORDED', expected 'canary-a'. Either the backend
stopped honouring --actor, or pact stopped passing it — in both cases every agent's
bead activity is now recorded as whoever owns the checkout."
printf '  attributed to the sending agent\n'

# ------------------------------------------- bd must not touch git, still
#
# pact routes every bd invocation through the MAIN worktree (see `beads_root` in
# src/beads.rs), so that all linked worktrees of one repository share one Beads
# store. That is correct for messaging and it has a consequence: an agent in
# worktree B causes bd to run inside a checkout where another agent may be
# actively working.
#
# Two hazards were hypothesised (pact-zid): bd racing the main-worktree agent on
# `.git/index.lock`, and bd's auto-commit sweeping whatever that agent had
# staged. Measured against bd 1.1.2, neither can happen — bd performs NO git
# operations for the only mutating subcommands pact issues (`create` and `label
# add`). So pact deliberately ships no mitigation: a doctor check would warn
# about a hazard that does not exist, and wrapping bd calls in an internal lease
# would serialise operations that never conflict.
#
# That decision rests entirely on somebody else's behaviour, which is precisely
# what this canary is for. If a future bd starts committing, the reasoning behind
# "no mitigation needed" evaporates silently — a sibling worktree would begin
# rewriting history in a checkout it does not own, and nothing else here would
# notice.
step "bd does not touch git in the main worktree"

# The scratch repo has no commit yet (`pact init --no-commit`, above, on purpose).
# HEAD and `git worktree add` both need one, and this is the only section that
# cares about git at all, so the commit is made here rather than earlier.
printf 'tracked before the agent touches it\n' >README-canary.txt
git add -A >/dev/null 2>&1 || true
git commit -q -m "canary: baseline" >/dev/null 2>&1 || true
git rev-parse HEAD >/dev/null 2>&1 || fail "could not create a baseline commit in the scratch repo"

# What a main-worktree agent looks like mid-task: one staged NEW file and one
# staged MODIFICATION to a tracked one. Both shapes, because a broad `git add`
# and a `git commit -a` sweep different things — the second only picks up
# modifications to files git already knows.
printf 'work in progress\n' >agent-wip.txt
printf 'edited by the agent\n' >>README-canary.txt
git add agent-wip.txt README-canary.txt
HEAD_BEFORE="$(git rev-parse HEAD)"
STAGED_BEFORE="$(git diff --cached --name-only | sort)"
[ -n "$STAGED_BEFORE" ] || fail "could not stage the decoy changes"

# Outside the repository, both of them. `$WORK` *is* the scratch repo, so a
# worktree or a log file placed under it would be untracked content inside the
# tree being inspected — and a bd that ran `git add -A` would then sweep an
# entire second checkout into its commit. Found the hard way while proving this
# guard fails: the diagnostic listed `sibling-wt` and `wt.err` among the swept
# paths, which is noise the reader has to discount.
SIBLING="$OUTSIDE/sibling-wt"
if git worktree add -q -b canary-sibling "$SIBLING" HEAD 2>"$OUTSIDE/wt.err"; then
	# The end-to-end case: a message SENT FROM the sibling runs bd here.
	(cd "$SIBLING" && PACT_AGENT=canary-sib "$PACT" msg send --to canary-a "from the sibling" >/dev/null) ||
		fail "msg send from a linked worktree failed — sibling routing is broken"
	printf '  sibling worktree sent a message through the main checkout\n'
else
	# Not fatal: the point of the section is bd's git behaviour, and the local
	# calls below exercise it either way.
	printf '  note: git worktree add failed, testing local bd calls only (%s)\n' \
		"$(first_line "$(cat "$OUTSIDE/wt.err")")"
fi

# And the two mutating calls directly, so the assertion holds even if the
# worktree could not be created.
PACT_AGENT=canary-a "$PACT" msg send --to canary-b "second" >/dev/null || fail "msg send failed"
SECOND_ID="$(PACT_AGENT=canary-b "$PACT" msg inbox --json | jq -r '.[-1].id')"
PACT_AGENT=canary-b "$PACT" msg read "$SECOND_ID" >/dev/null || fail "msg read failed"

HEAD_AFTER="$(git rev-parse HEAD)"
STAGED_AFTER="$(git diff --cached --name-only | sort)"

if [ "$HEAD_BEFORE" != "$HEAD_AFTER" ]; then
	printf '\ncommits bd made:\n' >&2
	git log --oneline --stat "$HEAD_BEFORE..$HEAD_AFTER" >&2
	fail "bd moved HEAD ($HEAD_BEFORE -> $HEAD_AFTER). bd now performs git operations, so
pact's routing of bd through the main worktree lets a SIBLING worktree rewrite
history in a checkout it does not own. Re-open pact-zid: the mitigation options
are a doctor check recommending bd's no-git-ops mode when has_worktrees, or
wrapping pact's mutating bd calls in an internal lease on a reserved key."
fi

if [ "$STAGED_BEFORE" != "$STAGED_AFTER" ]; then
	printf '\nstaged before:\n%s\nstaged after:\n%s\n' "$STAGED_BEFORE" "$STAGED_AFTER" >&2
	fail "bd changed what was staged in the main worktree. Even without committing, that
loses or adds to another agent's in-progress work. See pact-zid."
fi

[ ! -e .git/index.lock ] || fail "bd left .git/index.lock behind in the main worktree — it is
now taking the git index lock, which races the main-worktree agent's own commits (pact-zid)."

printf '  HEAD unmoved, staging untouched, no index.lock left behind\n'

printf '\nCANARY PASSED (leg=%s, bd=%s)\n' "$LEG" "$BD_VERSION"
