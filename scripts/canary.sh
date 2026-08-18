#!/usr/bin/env bash
# Exercise pact's ONE remaining coupling to bd against a REAL bd binary, so
# upstream drift is found by a machine instead of by a user.
#
# ## What this used to be for, and why it changed
#
# pact used to STORE its messages as bd beads, so it depended on somebody else's
# CLI semantics: `--include-infra`, `--parent` threading, whether `--json`
# hydrated `labels`, and the exact shapes those commands returned. Each was
# verified by hand once and then trusted forever, and tests/cli.rs stubs bd — fast
# and hermetic, and blind to all of it. This canary existed to replay a real send
# against a real bd and catch the day one of those assumptions moved.
#
# Since 0.9.0 pact writes messages to `.pact/messages.jsonl` and issues NO bd
# writes at all, so there is no send round-trip left to protect and every
# assumption listed above is gone. Three whole sections went with them:
#
#   - the message round-trip, because pact's messages never reach bd;
#   - actor attribution on pact's writes, because pact makes none (bd's `--actor`
#     support still matters for the bd commands AGENTS run themselves, which is
#     what doctor's "Beads actor attribution" check reports on);
#   - "bd must not touch git in the main worktree" (pact-zid), whose entire
#     premise was that pact routed mutating bd calls through the main checkout.
#     It does not route anything any more.
#
# ## What it is for now
#
# Exactly one coupling remains: pact READS the committed
# `.beads/interactions.jsonl` export to reconstruct bead assignees for
# `pact audit --check claim-lease-divergence`. That reader is best-effort and
# parse-tolerant by contract, which is a promise about somebody else's file
# format — precisely the kind of assumption that rots quietly.
#
# So this canary proves, against an export a real bd actually wrote:
#
#   1. pact reconstructs assignees from it and finds a divergence that is really
#      there;
#   2. a truncated or corrupted export degrades to "no beads data" and PASSES,
#      never an error and never a false finding;
#   3. pact's own surface — msg, watch delivery, lease — works with bd removed
#      from PATH entirely, which is the invariant a future subprocess would break.
#
# Run it locally exactly as CI does:
#   scripts/canary.sh                 # whatever bd is on PATH
#   CANARY_LEG=latest scripts/canary.sh
#
# CANARY_LEG is informational now: pact has no tested-version gate to assert
# against, because it has no runtime dependency to gate. The legs still differ in
# WHICH bd gets installed, which is the point — reading a real export written by a
# newer bd is the thing under test.

first_line() { printf '%s' "${1%%$'\n'*}"; }

need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is required but not on PATH"; }
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

step "environment"
printf '  leg: %s\n' "$LEG"
printf '  bd:  %s\n' "$BD_VERSION_RAW"

# No tested-range assertion. pact deleted TESTED_BD_MIN/MAX and
# version_compat_warning in 0.9.0: a tested range is a statement about a
# dependency, and reading one committed JSONL file is not a dependency worth
# gating a command on. doctor still REPORTS the version it found.

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

# The sidecar check must reflect reality, both ways — it is what tells a user the
# claim-lease-divergence check cannot run.
#
# Asserted against the FILE, not against a belief about bd's defaults. This
# check used to demand a warning unconditionally, on the reasoning that "the
# sidecar is OFF (bd's default)", and that was wrong twice over by the time it
# next ran: a7614a6 changed doctor to report the export's presence rather than a
# config key that answers for nobody, and bd's own `init` now writes the export.
# A canary that hardcodes somebody else's default cannot detect that default
# changing — it just breaks, which is exactly what happened.
SIDECAR_CHECK="$(jq -c '.checks[] | select(.name == "Beads audit sidecar")' "$DOCTOR_JSON")"
[ -n "$SIDECAR_CHECK" ] || fail "doctor has no \"Beads audit sidecar\" check: $(cat "$DOCTOR_JSON")"
printf '  %s\n' "$SIDECAR_CHECK"
if [ -f .beads/interactions.jsonl ]; then
	jq -e '.warn | not' <<<"$SIDECAR_CHECK" >/dev/null ||
		fail "the export exists, so doctor must NOT warn: $SIDECAR_CHECK"
	printf '  sidecar export present, correctly not warned\n'
else
	jq -e '.warn' <<<"$SIDECAR_CHECK" >/dev/null ||
		fail "no export, so doctor must warn that the check cannot run: $SIDECAR_CHECK"
	printf '  sidecar export absent, correctly warned\n'
fi

# ---------------------------------------- the real target: reading a real export
#
# Everything below runs against an `interactions.jsonl` that a real bd wrote. That
# is the whole point: pact's reader makes claims about somebody else's file format
# (rows of kind=field_change carrying extra.field/new_value, replayed in
# created_at order) and nothing else checks them against a running bd.
step "bd writes a real export, pact reads it"

bd config set audit.enabled true >/dev/null 2>&1 ||
	fail "could not enable bd's audit sidecar (bd config set audit.enabled true)"
jq -n 'true' >/dev/null # keep jq in the required set even if the greps below change

# A real bead, reassigned through real bd commands, so the export's rows are bd's
# own and not a fixture pact wrote to please itself.
BEAD="$(BEADS_ACTOR=canary-a bd create --title="canary work" --description="d" \
	--type=task --priority=2 --json 2>/dev/null | jq -r '.id // empty')"
[ -n "$BEAD" ] || BEAD="$(bd list --json 2>/dev/null | jq -r '.[0].id // empty')"
[ -n "$BEAD" ] || fail "could not create a bead with the real bd"
BEADS_ACTOR=canary-a bd update "$BEAD" --assignee=canary-owner >/dev/null 2>&1 ||
	fail "bd update --assignee failed"
BEADS_ACTOR=canary-a bd update "$BEAD" --assignee=canary-final >/dev/null 2>&1 ||
	fail "second bd update --assignee failed"

EXPORT=".beads/interactions.jsonl"
[ -f "$EXPORT" ] ||
	fail "bd wrote no $EXPORT even with audit.enabled — the sidecar's shape or switch has changed"
printf '  bd wrote %s rows\n' "$(wc -l <"$EXPORT" | tr -d ' ')"
grep -q '"field":"assignee"' "$EXPORT" || grep -q '"field": "assignee"' "$EXPORT" ||
	fail "no assignee row in bd's own export — pact's reconstruction key has changed shape:
$(head -3 "$EXPORT")"

# pact must now see canary-final (the LAST assignment) as the owner, and flag a
# hold taken by anyone else. A hold whose note names the bead, taken by a
# different agent, is exactly a divergence.
PACT_AGENT=canary-other "$PACT" lease acquire src/diverge.rs --note "$BEAD: not my bead" >/dev/null ||
	fail "lease acquire failed"

DIVERGE="$WORK/diverge.json"
set +e
PACT_AGENT=canary-a "$PACT" audit --check claim-lease-divergence --json >"$DIVERGE" 2>"$WORK/diverge.err"
set -e
jq -e --arg who canary-final \
	'any(.claim_divergences[]?; .path == "src/diverge.rs" and .assignee == $who)' \
	"$DIVERGE" >/dev/null ||
	fail "pact did not reconstruct '$BEAD' -> canary-final from bd's own export.
This is the coupling the canary exists for: bd's interactions.jsonl no longer says
what src/beads.rs's interaction_assignees expects.
report: $(cat "$DIVERGE")
stderr: $(cat "$WORK/diverge.err")
export head: $(head -3 "$EXPORT")"
printf '  reconstructed the last assignee from a real export, and found the divergence\n'
PACT_AGENT=canary-other "$PACT" lease release --all >/dev/null || fail "lease release failed"

# ------------------------------------------------------------- read tolerance
#
# The contract is that a damaged export degrades to "no beads data" and PASSES.
# Never an error, never a non-zero exit, never a false finding — a repository with
# a half-written export does not have a coordination problem.
step "a damaged export degrades instead of failing"

# <label> <findings: any|none>
#
# `any` and `none` are both correct answers, for different damage. A torn final
# line is SKIPPED and the intact rows above it are still used — that is the
# tolerance contract, not a degradation — so findings from those rows are real and
# must not be suppressed. Only when NOTHING parses is there no data to judge
# against, and then the check must report "no beads data" and find nothing.
#
# What is never acceptable, for either shape, is an error: a repository with a
# half-written export does not have a coordination problem.
tolerate() {
	set +e
	PACT_AGENT=canary-a "$PACT" audit --check claim-lease-divergence --json \
		>"$WORK/tol.json" 2>"$WORK/tol.err"
	local code=$?
	set -e
	[ "$code" -le 1 ] ||
		fail "$1: exit $code — a damaged export must never be an error.
stdout: $(cat "$WORK/tol.json")
stderr: $(cat "$WORK/tol.err")"
	jq -e '.' "$WORK/tol.json" >/dev/null ||
		fail "$1: --json emitted nothing parseable: $(cat "$WORK/tol.err")"
	if [ "$2" = none ]; then
		jq -e '.claim_divergences | length == 0' "$WORK/tol.json" >/dev/null ||
			fail "$1: nothing in the export parses, so any finding is invented: $(cat "$WORK/tol.json")"
		jq -e '.claim_unavailable != null' "$WORK/tol.json" >/dev/null ||
			fail "$1: must say WHY it could not check, not pass silently: $(cat "$WORK/tol.json")"
	fi
	printf '  %s: exit %s, findings=%s\n' "$1" "$code" \
		"$(jq '.claim_divergences | length' "$WORK/tol.json")"
}

cp "$EXPORT" "$WORK/export.bak"

# A torn final line: the shape an interrupted write leaves.
head -c $(($(wc -c <"$WORK/export.bak") - 20)) "$WORK/export.bak" >"$EXPORT"
# `any`: the rows ABOVE the tear are intact and still count.
tolerate "truncated mid-line" any

# Wholly unparseable.
printf 'not json at all\nnor this\n' >"$EXPORT"
tolerate "corrupted" none

# Absent entirely — the default bd repo, and the common case.
rm -f "$EXPORT"
tolerate "absent" none

cp "$WORK/export.bak" "$EXPORT"

# ------------------------------------------ pact works with bd off PATH entirely
#
# The invariant a future subprocess would break, asserted where it is cheap. Unit
# tests cover it too; this proves it through the real binary in a repo that has a
# real Beads store sitting right there, which is the case most likely to tempt a
# regression.
step "pact needs no bd on PATH"
# bd removed, git KEPT. pact has always needed git — it resolves the repo root and
# reads HEAD for the diff a watch notice carries — and this section is about the
# Beads dependency, not about running pact outside a git checkout. An earlier cut
# used a wholly empty PATH and watch delivery produced no notice, which read as a
# messaging failure and was really `git diff` having no git.
EMPTY="$WORK/no-bd"
mkdir -p "$EMPTY"
ln -sf "$(command -v git)" "$EMPTY/git"
command -v bd >/dev/null && [ ! -e "$EMPTY/bd" ] ||
	fail "the no-bd PATH must not contain bd"
nobd() { env PATH="$EMPTY" PACT_AGENT="$1" "$PACT" "${@:2}"; }

# Prove the shim really does hide bd, so a passing section cannot be a PATH that
# quietly still had it.
env PATH="$EMPTY" command -v bd >/dev/null 2>&1 &&
	fail "bd is still reachable on the stripped PATH — this section proves nothing"

nobd canary-a msg send --to canary-b --subject canary "ping with no backend" >/dev/null ||
	fail "msg send needs a backend again"
nobd canary-b msg inbox --json >"$WORK/nobd-inbox.json" || fail "msg inbox needs a backend again"
jq -e 'length == 1' "$WORK/nobd-inbox.json" >/dev/null ||
	fail "expected 1 message with no bd on PATH: $(cat "$WORK/nobd-inbox.json")"
NOBD_ID="$(jq -r '.[0].id' "$WORK/nobd-inbox.json")"
nobd canary-b msg read "$NOBD_ID" >/dev/null || fail "msg read needs a backend again"
nobd canary-b msg inbox --unread-only --json >"$WORK/nobd-unread.json" || fail "unread-only failed"
jq -e 'length == 0' "$WORK/nobd-unread.json" >/dev/null ||
	fail "read state did not persist with no bd: $(cat "$WORK/nobd-unread.json")"
nobd canary-a msg sent --json >"$WORK/nobd-sent.json" || fail "msg sent failed"
jq -e --arg id "$NOBD_ID" 'any(.[]; .id == $id and (.read_by | index("canary-b")))' \
	"$WORK/nobd-sent.json" >/dev/null ||
	fail "sender cannot see the read with no bd: $(cat "$WORK/nobd-sent.json")"
printf '  send, inbox, read and sent all round-tripped with an empty PATH\n'

# Watch delivery is the one path that runs without an agent choosing to run it.
# The file must EXIST before the acquire: a lease records the content hash it
# found, and a release with nothing to diff against notifies nobody by design.
mkdir -p src
printf 'before\n' >src/watched.rs
nobd canary-c watch add src/watched.rs >/dev/null || fail "watch add failed"
nobd canary-a lease acquire src/watched.rs >/dev/null || fail "acquire for watch failed"
printf 'after\n' >src/watched.rs
nobd canary-a lease release src/watched.rs >/dev/null || fail "release for watch failed"
nobd canary-c msg inbox --watch-only --json >"$WORK/nobd-notice.json" || fail "watch-only failed"
jq -e 'length >= 1' "$WORK/nobd-notice.json" >/dev/null ||
	fail "watch delivery produced no notice with no bd on PATH: $(cat "$WORK/nobd-notice.json")"
printf '  watch delivery landed a notice with an empty PATH\n'

# ------------------------------------------------------- message round-trip
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

printf '\nCANARY PASSED (leg=%s, bd=%s)\n' "$LEG" "$BD_VERSION"
