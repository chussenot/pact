#!/usr/bin/env bash
# Beads-side retrospective questions. BEST-EFFORT, and that is not modesty.
#
# ## Why this is a script and not part of `pact audit`
#
# `pact audit` reads `.pact/` and nothing else. pact never touches the Beads
# store directly — every backend interaction goes through the `bd`/`br` CLI — and
# an analytics command is exactly where that invariant would be convenient to
# break, because the data is right there in a JSONL file. Doing it here keeps the
# rule intact and keeps the fragile part clearly labelled as fragile.
#
# ## What "best-effort" means concretely
#
# This reads `.beads/interactions.jsonl` and shells out to `bd`. Both are somebody
# else's formats:
#
#   - `interactions.jsonl` field names (`kind`, `actor`, `extra.field`,
#     `extra.old_value`, `extra.new_value`, `extra.reason`) are what bd 1.1.2
#     writes. A future bd may rename or restructure them, and this script will go
#     quiet or wrong rather than loudly broken.
#   - `br` writes a different store entirely and is NOT supported here.
#   - Nothing in `mise run check` or CI depends on this. If it breaks, nothing
#     goes red, which is the correct blast radius for a tool that reads another
#     project's internal file.
#
# If a question here becomes load-bearing, that is the signal to ask bd for a
# supported way to answer it — not to harden this script.
#
#   scripts/beads-retro.sh              # in a repo with a .beads/
#   scripts/beads-retro.sh --json

set -euo pipefail

JSON=0
[ "${1:-}" = "--json" ] && JSON=1

die() {
	printf 'beads-retro: %s\n' "$1" >&2
	exit 1
}

command -v jq >/dev/null 2>&1 || die "jq is required"
command -v git >/dev/null 2>&1 || die "git is required"

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not in a git repository"
cd "$ROOT"

LOG=".beads/interactions.jsonl"
[ -f "$LOG" ] || die "no $LOG — this only understands bd's store, not br's (see the header)"

if ! command -v bd >/dev/null 2>&1; then
	printf 'beads-retro: no bd on PATH; reading %s only\n' "$LOG" >&2
fi

# ------------------------------------------------------- claim discipline: NO
#
# Not measured, because it cannot be. Measured rather than assumed: bd 1.1.2
# writes a `field_change/status` interaction **only on close**. Running
# `bd update <id> --claim`, which sets status to in_progress, appends nothing at
# all — verified on a scratch store, where the log had 0 lines after a claim and 1
# after the close.
#
# So "how many beads were closed without being claimed first" reads 100% on every
# repository, whatever anybody did, because no claim was ever recorded. An earlier
# cut of this script shipped that number. A metric that returns the same answer
# regardless of the behaviour it claims to measure is worse than no metric: it
# looks like evidence.
#
# If claim discipline matters, it needs a supported source — bd growing an
# interaction row for in_progress, or the claim being observed live rather than
# reconstructed after the fact. It does not need a cleverer query over this file.
CLOSED="$(jq -sr 'map(select(.kind == "field_change" and .extra.field == "status"
	and .extra.new_value == "closed")) | length' "$LOG" 2>/dev/null || echo 0)"

# ------------------------------------------------------------- provenance rot
#
# Close reasons cite commit hashes as evidence. A hash that resolves to nothing
# is provenance that has rotted — the reasoning survives, the proof does not.
#
# HEAVILY filtered, because a naive hex scan is mostly wrong: measured on this
# repo, 9 of 17 "dangling hashes" were not commit references at all but UUID
# fragments, CLAUDE_CODE_SESSION_ID pieces, a bd version hash and trace ids. So a
# candidate must be introduced by a provenance word, and even then several of the
# survivors are deliberate citations of ANOTHER repository's commits, which are
# correctly unresolvable here. Treat the number as a smell, never a defect count.
REASONS="$(jq -r '
	select(.kind == "field_change" and .extra.field == "status")
	| .extra.reason // ""
' "$LOG" 2>/dev/null || true)"

ALIVE=0
DEAD=0
DEAD_LIST=""
while IFS= read -r h; do
	[ -n "$h" ] || continue
	if git cat-file -e "${h}^{commit}" 2>/dev/null; then
		ALIVE=$((ALIVE + 1))
	else
		DEAD=$((DEAD + 1))
		DEAD_LIST="$DEAD_LIST $h"
	fi
done < <(
	printf '%s\n' "$REASONS" |
		grep -oiE '(commit|in|see|fixed in|shipped in) +[0-9a-f]{7,12}\b' |
		grep -oE '[0-9a-f]{7,12}$' |
		sort -u
)
CITED=$((ALIVE + DEAD))
DEAD_PCT=0
[ "$CITED" -gt 0 ] && DEAD_PCT=$((100 * DEAD / CITED))

# ------------------------------------------------------------------ who acts
#
# Attribution: how much of the bead history is recorded against an agent rather
# than a human's git identity. pact passes `--actor` on every mutating call, so a
# fleet's activity should be attributed to agents — if it is not, something is
# not going through pact.
ACTORS="$(jq -sr '
	map(select(.actor != null)) | group_by(.actor)
	| map({actor: .[0].actor, events: length})
	| sort_by(-.events) | .[0:10]
' "$LOG" 2>/dev/null || echo '[]')"

if [ "$JSON" -eq 1 ]; then
	jq -n \
		--argjson closed "$CLOSED" \
		--argjson hashes_cited "$CITED" \
		--argjson hashes_dangling "$DEAD" \
		--argjson dangling_pct "$DEAD_PCT" \
		--argjson top_actors "$ACTORS" \
		'{
			best_effort: true,
			caveat: "reads bd 1.1.2 interactions.jsonl; field names are not a supported API",
			closes_recorded: $closed,
			claim_discipline: "unmeasurable: bd 1.1.2 logs a status interaction only on close, so a claim leaves no trace",
			provenance: {cited: $hashes_cited, dangling: $hashes_dangling, dangling_pct: $dangling_pct},
			top_actors: $top_actors
		}'
	exit 0
fi

printf '=== beads retrospective (best-effort; see this script header)\n\n'
printf 'closes recorded\n'
printf '  status->closed interactions: %s\n' "$CLOSED"
printf '  Claim discipline is NOT reported here and cannot be: bd 1.1.2 logs a status\n'
printf '  interaction only on close, so a claim leaves no trace. Any "closed without\n'
printf '  claiming" figure from this file is 100%% for every repo — see the header.\n\n'

printf 'provenance of close reasons\n'
printf '  commit hashes cited:  %s\n' "$CITED"
printf '  dangling:             %s (%s%%)\n' "$DEAD" "$DEAD_PCT"
if [ "$DEAD" -gt 0 ]; then
	printf '  dangling:%s\n' "$DEAD_LIST"
fi
printf '  A smell, NOT a defect count: some of these are deliberate citations of another\n'
printf '  repository, which cannot resolve here and should not.\n\n'

printf 'busiest actors\n'
jq -r '.[] | "  \(.actor)  \(.events) event(s)"' <<<"$ACTORS"
printf "\nFor pact-side history — leases, holds, contention, double-wins — use 'pact audit',\n"
printf 'which reads .pact/ only and is covered by tests.\n'
