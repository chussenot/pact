#!/usr/bin/env bash
# Guard against README/docs drift (pact-qbo).
#
# A docs review on 2026-08-02 found 13 defects that had accumulated over 14 code
# commits. The worst told readers to build with `cargo build --release`, which
# produces a binary with no `pact ui` at all; two links pointed at
# docs/pact-scaffolding-prompt.md, deleted in 305b456. Every one was mechanical.
# The managed AGENTS.md block already has a unit test asserting its command
# needles; this is the same guard for README.md and docs/.
#
# Three checks, in the bead's order of value per line:
#   1. every subcommand and flag the real CLI exposes is in README's Commands block
#      (and, in reverse, nothing in that block has stopped existing)
#   2. every relative markdown link in README.md and docs/ resolves to a real file
#   3. every `pact doctor` check name is named in docs/tui.md's Doctor section
#
# The CLI inventory is walked out of `--help` at runtime rather than hardcoded:
# a hardcoded list is the same drift problem one level down.
#
# Usage: scripts/check-docs.sh
#   PACT_BIN=path/to/pact scripts/check-docs.sh   # skip the cargo build

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

fail=0
problem() {
	printf 'DRIFT: %s\n' "$1" >&2
	fail=1
}

# --features ui matters: without it `pact ui` is not in the command tree at all,
# so a default build would silently under-report what README has to cover.
if [ -n "${PACT_BIN:-}" ]; then
	pact() { "$PACT_BIN" "$@"; }
else
	cargo build --quiet --features ui || exit 1
	pact() { ./target/debug/pact "$@"; }
fi

# ---------------------------------------------------------------- CLI inventory

# Global flags live in every single --help; README documents them once in prose
# under the Commands block, not per-command, so listing them per-command would
# be noise.
is_global() { case "$1" in --agent | --json | --help | --version) return 0 ;; *) return 1 ;; esac; }

commands=()   # "lease acquire", "msg send", …
flags=()      # "--ttl", "--body-file", … deduped
shortflags=() # same index as flags: "-n" for --limit, "" when there is no short form

seen_flag() { local f; for f in "${flags[@]}"; do [ "$f" = "$1" ] && return 0; done; return 1; }

walk() {
	local path="$1" help section line
	# shellcheck disable=SC2086 # word splitting is how we turn "lease acquire" into argv
	help=$(pact $path --help 2>&1) || { problem "\`pact $path --help\` failed"; return; }

	section=""
	while IFS= read -r line; do
		case "$line" in
		"Commands:") section=cmd; continue ;;
		"Options:") section=opt; continue ;;
		"Arguments:") section=arg; continue ;;
		"") section=""; continue ;;
		esac
		[ "$section" = cmd ] || [ "$section" = opt ] || continue

		if [ "$section" = cmd ]; then
			local sub=${line#"${line%%[![:space:]]*}"}
			sub=${sub%% *}
			[ "$sub" = "help" ] && continue
			commands+=("${path:+$path }$sub")
			walk "${path:+$path }$sub"
		else
			# "  -n, --limit <LIMIT>  How many…" or "      --ttl <TTL>  …"
			local short="" long=""
			[[ $line =~ (^|[[:space:]])(-[a-zA-Z]),[[:space:]] ]] && short="${BASH_REMATCH[2]}"
			[[ $line =~ (--[a-z][a-z0-9-]*) ]] && long="${BASH_REMATCH[1]}"
			[ -n "$long" ] || continue
			is_global "$long" && continue
			seen_flag "$long" || { flags+=("$long"); shortflags+=("$short"); }
		fi
	done <<<"$help"
}

walk ""

[ ${#commands[@]} -gt 0 ] || { problem "walked the CLI and found no subcommands — parser broken"; exit 1; }

# ------------------------------------------------- 1. README Commands block

# The fenced block right after the "## Commands" heading.
block=$(awk '
	/^## Commands$/      { want=1; next }
	want && /^```/       { infence = !infence; if (!infence) exit; next }
	infence              { print }
' README.md)

[ -n "$block" ] || { problem "README.md has no fenced code block under '## Commands'"; exit 1; }

for i in "${!flags[@]}"; do
	f=${flags[$i]}
	s=${shortflags[$i]}
	# Either spelling counts: clap treats -n and --limit as one flag, so a reader
	# who finds `-n <count>` in README has found the flag.
	grep -qF -- "$f" <<<"$block" && continue
	[ -n "$s" ] && grep -qE -- "(^|[^[:alnum:]-])$s([^[:alnum:]-]|$)" <<<"$block" && continue
	problem "flag $f exists in the CLI but is not in README's Commands block"
done

for c in "${commands[@]}"; do
	grep -qE "^pact $c([[:space:]]|$)" <<<"$block" ||
		problem "\`pact $c\` exists but is not in README's Commands block"
done

# Reverse direction — the acceptance criterion is that CI fails when README
# *references* something that does not exist, which is how a removed flag rots.
while IFS= read -r line; do
	[ -n "$line" ] || continue
	invocation=$(sed -E 's/^pact //; s/[[:space:]]*[-<[(].*$//; s/[[:space:]]+$//' <<<"$line")
	[ -n "$invocation" ] || continue
	printf '%s\n' "${commands[@]}" | grep -qxF "$invocation" ||
		problem "README's Commands block documents \`pact $invocation\`, which the CLI does not have"
done < <(grep '^pact ' <<<"$block")

for f in $(grep -oE -- '--[a-z][a-z0-9-]*' <<<"$block" | sort -u); do
	is_global "$f" && continue
	seen_flag "$f" ||
		problem "README's Commands block documents $f, which the CLI does not have"
done

# ----------------------------------------------------- 2. relative md links

# This class shipped twice (docs/pact-scaffolding-prompt.md, deleted in 305b456,
# stayed linked from two places) and is the cheapest check here.
while IFS= read -r hit; do
	# grep -r prefixes "file:", and the link regex excludes ':' so absolute URLs
	# never reach here — which makes the last colon the separator, always.
	src=${hit%%:*}
	target=${hit##*:}
	target=${target%%#*} # file.md#anchor — the file is what can vanish
	[ -n "$target" ] || continue
	[ -e "$(dirname "$src")/$target" ] ||
		problem "$src links to $target, which does not exist"
done < <(grep -roE '\]\([^):]+\)' README.md docs/ | sed 's/](/:/; s/)$//')

# ------------------------------------------------------ 3. doctor checks

# doctor exits 1 when a check fails (no `bd` on a CI runner, for one), so the
# exit code is deliberately ignored — only the emitted check names matter.
doctor_names=$(pact doctor --json 2>/dev/null | sed -nE 's/.*"name": "(.*)",?$/\1/p')
[ -n "$doctor_names" ] || { problem "\`pact doctor --json\` emitted no check names"; exit 1; }

# docs/tui.md describes the checks in prose ("`.pact/` presence", "the `bd`
# binary"), not as a verbatim list, so a literal match on the check name would
# fail on all eight today. Instead: at least one word of the name, 4+ chars,
# has to appear in tui.md's Doctor section. Scoping to that section is what
# gives the check teeth: "leases" appears all over that page, so a whole-file
# match shrugs at the recorded defect (a doctor check list missing 2 of 7
# entries) — deleting "stale-lease count, and corrupt-lock count" from the
# Doctor section still leaves the word "leases" three tabs away.
lower() { tr 'A-Z' 'a-z'; }
tui_doctor=$(awk '/^###?[[:space:]]+Doctor[[:space:]]*$/ {inside=1; next} inside && /^#/ {exit} inside' docs/tui.md | lower)
[ -n "$tui_doctor" ] || { problem "docs/tui.md has no Doctor section to check against"; exit 1; }

while IFS= read -r name; do
	hit=0
	for word in $(lower <<<"$name"); do
		[ ${#word} -ge 4 ] || continue
		grep -qF -- "$word" <<<"$tui_doctor" && { hit=1; break; }
	done
	[ $hit -eq 1 ] || problem "doctor check \"$name\" is not named in docs/tui.md's Doctor section"
done <<<"$doctor_names"

# ---------------------------------------------------------------------------

if [ $fail -eq 0 ]; then
	printf 'docs ok: %d commands, %d flags, %d doctor checks, all links resolve\n' \
		"${#commands[@]}" "${#flags[@]}" "$(wc -l <<<"$doctor_names")"
fi
exit $fail
