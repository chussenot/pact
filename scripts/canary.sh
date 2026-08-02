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

BD_VERSION_RAW="$(first_line "$(bd --version 2>&1)")"
BD_VERSION="$(first_line "$(grep -oE '[0-9]+\.[0-9]+\.[0-9]+' <<<"$BD_VERSION_RAW")")"
[ -n "$BD_VERSION" ] || fail "could not parse a version out of: $BD_VERSION_RAW"

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
cleanup() { rm -rf "$WORK"; }
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

printf '\nCANARY PASSED (leg=%s, bd=%s)\n' "$LEG" "$BD_VERSION"
