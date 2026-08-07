#!/usr/bin/env bash
# Exercise pact against a REAL br binary, so upstream CLI drift is found by a
# machine instead of by a user. br's sibling to canary.sh's bd coverage.
#
# Deliberately not a byte-for-byte copy of canary.sh: br is a SQLite backend,
# not bd's git-embedded Dolt, so canary.sh's "bd must not touch git in the
# main worktree" section (pact-zid) tests a hazard specific to bd's storage
# model and does not carry over here without its own investigation. This
# covers the part that does carry over unchanged: doctor, the message
# round-trip, lease smoke, and actor attribution.
#
# Run it locally exactly as CI does:
#   scripts/canary-br.sh                 # whatever br is on PATH
#   CANARY_LEG=latest scripts/canary-br.sh
#
# CANARY_LEG only affects one assertion: on `latest`, a br outside pact's
# tested range MUST produce doctor's version warning.

set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"
LEG="${CANARY_LEG:-pinned}"

fail() {
	printf '\nCANARY FAILED: %s\n' "$1" >&2
	exit 1
}
step() { printf '\n=== %s\n' "$1"; }

# First line of a string, WITHOUT piping into `head` — see canary.sh for why
# `producer | head -1` is a trap under `set -o pipefail`.
first_line() { printf '%s' "${1%%$'\n'*}"; }

need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is required but not on PATH"; }
need br
need jq
need git

# ---------------------------------------------------------------- the range
#
# Read from src/beads.rs, never restated here — same reasoning as canary.sh.
read_triplet() {
	first_line "$(sed -nE "s/^const $1: \(u64, u64, u64\) = \(([0-9]+), ([0-9]+), ([0-9]+)\);/\1.\2.\3/p" src/beads.rs)"
}
BR_MIN="$(read_triplet TESTED_BR_MIN)"
BR_MAX="$(read_triplet TESTED_BR_MAX_EXCLUSIVE)"
[ -n "$BR_MIN" ] && [ -n "$BR_MAX" ] ||
	fail "could not read TESTED_BR_MIN/TESTED_BR_MAX_EXCLUSIVE out of src/beads.rs — has the declaration changed shape?"

# Parse the version from ANYWHERE in the output, not just the first line —
# same defensive reasoning as canary.sh's BD_VERSION.
BR_OUTPUT="$(br --version 2>&1)"
BR_VERSION="$(first_line "$(grep -oE '[0-9]+\.[0-9]+\.[0-9]+' <<<"$BR_OUTPUT")")"
[ -n "$BR_VERSION" ] || fail "could not parse a version out of: $BR_OUTPUT"
BR_VERSION_RAW="$(first_line "$(grep -F "$BR_VERSION" <<<"$BR_OUTPUT")")"

ver_lt() { [ "$1" != "$2" ] && [ "$(first_line "$(sort -V <<<"$1"$'\n'"$2")")" = "$1" ]; }
IN_RANGE=yes
if ver_lt "$BR_VERSION" "$BR_MIN" || [ "$BR_VERSION" = "$BR_MAX" ] || ! ver_lt "$BR_VERSION" "$BR_MAX"; then
	IN_RANGE=no
fi

step "environment"
printf '  leg:          %s\n' "$LEG"
printf '  br:           %s\n' "$BR_VERSION_RAW"
printf '  tested range: %s <= v < %s (from src/beads.rs)\n' "$BR_MIN" "$BR_MAX"
printf '  in range:     %s\n' "$IN_RANGE"

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

step "br init in a scratch repo"
cd "$WORK"
git init -q .
git config user.email canary@pact.invalid
git config user.name "pact canary"
br init --prefix canary >/dev/null || fail "br init failed — see output above"
[ -d .beads ] || fail "br init reported success but created no .beads/"

step "pact init"
"$PACT" init --no-commit >/dev/null || fail "pact init failed"

# ------------------------------------------------------------------- doctor
step "pact doctor"
DOCTOR_JSON="$WORK/doctor.json"
if ! PACT_AGENT=canary-a "$PACT" doctor --json >"$DOCTOR_JSON" 2>"$WORK/doctor.err"; then
	cat "$DOCTOR_JSON" "$WORK/doctor.err" >&2
	fail "pact doctor exited non-zero against real br"
fi
jq -e '.checks | length > 0' "$DOCTOR_JSON" >/dev/null || fail "doctor emitted no checks"

BEADS_CHECK="$(jq -c '.checks[] | select(.name == "Beads CLI")' "$DOCTOR_JSON")"
[ -n "$BEADS_CHECK" ] || fail "doctor has no \"Beads CLI\" check: $(cat "$DOCTOR_JSON")"
printf '  %s\n' "$BEADS_CHECK"
jq -e '.ok' <<<"$BEADS_CHECK" >/dev/null || fail "doctor could not use the real br: $BEADS_CHECK"

# The assertion that makes the two-leg split worth having, mirrored from
# canary.sh: a unit test proves version_compat_warning() returns Some for a
# made-up string; this proves the whole path fires against a real br.
if [ "$IN_RANGE" = no ]; then
	jq -e '.warn' <<<"$BEADS_CHECK" >/dev/null ||
		fail "br $BR_VERSION is outside $BR_MIN..$BR_MAX but doctor did not warn: $BEADS_CHECK"
	printf '  drift detected and warned, as designed\n'
elif [ "$LEG" = latest ]; then
	printf '  latest br is still inside the tested range; nothing to warn about\n'
fi

# ------------------------------------------------------- message round-trip
# The real target: br's `list --json` envelope, its lack of `--include-infra`
# and `--parent`, and label hydration via a second `show` are all pact
# assumptions (see msg.rs's module docs) never before verified against a
# running br.
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

UNREAD="$WORK/unread.json"
PACT_AGENT=canary-b "$PACT" msg inbox --unread-only --json >"$UNREAD" ||
	fail "msg inbox --unread-only failed"
jq -e 'length == 0' "$UNREAD" >/dev/null ||
	fail "message stayed unread after msg read — label round-trip broken: $(cat "$UNREAD")"

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
# Same reasoning as canary.sh: the scratch repo's git user is deliberately
# NOT an agent name, so a match on `canary-a` can only come from attribution
# working. br 0.2.19+ accepts `--actor` and pact passes it (see msg.rs).
step "backend attributes writes to the agent, not the git user"
GIT_USER="$(git config user.name)"
[ "$GIT_USER" = "pact canary" ] || fail "expected the scratch repo's git user to be 'pact canary', got '$GIT_USER'"

ACTOR_JSON="$WORK/actor.json"
if ! br show "$MSG_ID" --json >"$ACTOR_JSON" 2>&1; then
	cat "$ACTOR_JSON" >&2
	fail "could not read $MSG_ID back from the backend"
fi
# br's `show` has returned a bare object; accept an array too rather than
# depending on that shape (mirrors canary.sh's bd/br-agnostic handling).
RECORDED="$(jq -r 'if type == "array" then .[0] else . end | .created_by // ""' "$ACTOR_JSON")"
printf '  git user.name: %s\n  created_by:    %s\n' "$GIT_USER" "$RECORDED"
[ "$RECORDED" = "canary-a" ] ||
	fail "message $MSG_ID is attributed to '$RECORDED', expected 'canary-a'. Either the backend
stopped honouring --actor, or pact stopped passing it — in both cases every agent's
bead activity is now recorded as whoever owns the checkout."
printf '  attributed to the sending agent\n'

printf '\nCANARY PASSED (leg=%s, br=%s)\n' "$LEG" "$BR_VERSION"
