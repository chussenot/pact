#!/usr/bin/env bash
# Guard against README/docs drift (pact-qbo).
#
# A docs review on 2026-08-02 found 13 defects that had accumulated over 14 code
# commits. The worst told readers to build with `cargo build --release`, which
# produces a binary with no `pact ui` at all; two links pointed at
# docs/pact-scaffolding-prompt.md, deleted in 305b456. Every one was mechanical.
# The managed AGENTS.md block already has a unit test asserting its command
# needles; this is the same guard for the README and docs/.
#
# Three checks, in the bead's order of value per line:
#   1. every subcommand and flag the real CLI exposes is in docs/cli.md's Commands block
#      (built with every optional feature on, so `ui` and `mcp` count)
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

# --features ui,mcp matters: without them `pact ui` and `pact mcp serve` are not
# in the command tree at all, so a default build would silently under-report what
# cli.md has to cover — and the reverse check would then call both of them
# documented-but-nonexistent. Every optional subcommand has to be compiled in
# here for the two directions to mean anything. `otel` adds no subcommand and is
# in the list only so that the artifact this leaves in target/debug matches what
# `mise run build` produces: this script runs last in `mise run check`, and a
# developer whose binary silently lost a feature to the final gate has no way to
# guess why.
if [ -n "${PACT_BIN:-}" ]; then
	# Must itself have been built --features ui,mcp, or the reverse check below
	# reports every optional subcommand as documented-but-missing.
	pact() { "$PACT_BIN" "$@"; }
else
	cargo build --quiet --features ui,otel,mcp || exit 1
	pact() { ./target/debug/pact "$@"; }
fi

# ---------------------------------------------------------------- CLI inventory

# Global flags live in every single --help; cli.md documents them once in prose
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
		# A blank line ends the Commands list but NOT the Options list, because clap
		# has two help layouts and picks between them on its own: a short doc
		# comment gives the compact "flag  description" form, while a long one
		# (several paragraphs, as `pact audit` has) gives an expanded form with a
		# blank line between every option. Resetting on blank lines meant the
		# expanded form was read as one flag followed by nothing — `pact audit`'s
		# --check and --since were reported as documented-but-nonexistent while
		# both were right there in --help.
		"") [ "$section" = opt ] || section=""; continue ;;
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

# ------------------------------------------------ 1. cli.md Commands block

# The fenced block right after the "## Commands" heading, which lives in
# docs/cli.md. It used to be in README.md; when the README was reduced to
# reasoning and the reference moved out, this guard was the first thing to
# break — which is the correct outcome, because a checker that silently stops
# finding what it checks is worse than one that fails loudly.
block=$(awk '
	/^## Commands$/      { want=1; next }
	want && /^```/       { infence = !infence; if (!infence) exit; next }
	infence              { print }
' docs/cli.md)

[ -n "$block" ] || { problem "docs/cli.md has no fenced code block under '## Commands'"; exit 1; }

for i in "${!flags[@]}"; do
	f=${flags[$i]}
	s=${shortflags[$i]}
	# Either spelling counts: clap treats -n and --limit as one flag, so a reader
	# who finds `-n <count>` in cli.md has found the flag.
	grep -qF -- "$f" <<<"$block" && continue
	[ -n "$s" ] && grep -qE -- "(^|[^[:alnum:]-])$s([^[:alnum:]-]|$)" <<<"$block" && continue
	problem "flag $f exists in the CLI but is not in docs/cli.md's Commands block"
done

for c in "${commands[@]}"; do
	grep -qE "^pact $c([[:space:]]|$)" <<<"$block" ||
		problem "\`pact $c\` exists but is not in docs/cli.md's Commands block"
done

# Reverse direction — CI must fail when the docs *reference* something that does
# not exist, which is how a removed flag rots.
while IFS= read -r line; do
	[ -n "$line" ] || continue
	invocation=$(sed -E 's/^pact //; s/[[:space:]]*[-<[(].*$//; s/[[:space:]]+$//' <<<"$line")
	[ -n "$invocation" ] || continue
	printf '%s\n' "${commands[@]}" | grep -qxF "$invocation" ||
		problem "docs/cli.md's Commands block documents \`pact $invocation\`, which the CLI does not have"
done < <(grep '^pact ' <<<"$block")

for f in $(grep -oE -- '--[a-z][a-z0-9-]*' <<<"$block" | sort -u); do
	is_global "$f" && continue
	seen_flag "$f" ||
		problem "docs/cli.md's Commands block documents $f, which the CLI does not have"
done

# ----------------------------------------------------- 2. relative md links

# This class shipped twice (docs/pact-scaffolding-prompt.md, deleted in 305b456,
# stayed linked from two places) and is the cheapest check here.
while IFS= read -r hit; do
	# grep -r prefixes "file:", and the link regex excludes ':' so absolute URLs
	# never reach here — which makes the last colon the separator, always.
	src=${hit%%:*}
	link=${hit##*:}
	target=${link%%#*}
	frag=${link#"$target"}
	frag=${frag#\#}

	# A bare "#anchor" points into the source file itself.
	path=${target:+$(dirname "$src")/$target}
	path=${path:-$src}

	if [ ! -e "$path" ]; then
		problem "$src links to $target, which does not exist"
		continue
	fi
	[ -n "$frag" ] || continue

	# Fragments rot exactly like filenames do — this round added four deep links
	# and none were verified. GitHub slugs a heading by lowercasing it, dropping
	# anything that is not a word character, space or hyphen, then joining on
	# hyphens; reproduce that rather than guessing.
	grep -qxF "$frag" < <(
		grep -E '^#{1,6}[[:space:]]' "$path" |
			sed -E 's/^#+[[:space:]]*//; s/[[:space:]]+$//' |
			tr 'A-Z' 'a-z' |
			sed -E 's/`//g; s/[^a-z0-9 _-]//g; s/[[:space:]]+/-/g'
	) || problem "$src links to $link, but $path has no heading anchored #$frag"
done < <(grep -roE '\]\([^):]+\)' README.md docs/ | sed 's/](/:/; s/)$//')

# ------------------------------------------------------ 3. doctor checks

# doctor exits 1 when a check fails (no `bd` on a CI runner, for one), so the
# exit code is deliberately ignored — only the emitted check names matter.
doctor_names=$(pact doctor --json 2>/dev/null | sed -nE 's/.*"name": "(.*)",?$/\1/p')
[ -n "$doctor_names" ] || { problem "\`pact doctor --json\` emitted no check names"; exit 1; }

# docs/tui.md's Doctor section lists the check names VERBATIM in backticks, so
# this is an exact set comparison in both directions.
#
# It used to be prose, matched by "any word of the name, 4+ chars, appears in
# the section". That was weak both ways: docs naming a check the CLI does not
# have passed (appending "and dolt remote reachability" was invisible), and
# deleting a whole check's sentence passed too, because words like "files" and
# "current" survived elsewhere in the paragraph. A verbatim list is what makes
# the comparison exact, and it is better documentation than the prose was.
tui_doctor=$(awk '/^###?[[:space:]]+Doctor[[:space:]]*$/ {inside=1; next} inside && /^#/ {exit} inside' docs/tui.md)
[ -n "$tui_doctor" ] || { problem "docs/tui.md has no Doctor section to check against"; exit 1; }
# Only the "| Check |" table — the same section carries a keybindings table
# whose first cell is also backticked, and `r` is not a doctor check.
documented=$(awk -F'|' '
	/^\|[[:space:]]*Check[[:space:]]*\|/ {intable=1; next}
	intable && !/^\|/ {exit}
	intable && $2 ~ /`/ {gsub(/^[[:space:]]*`|`[[:space:]]*$/, "", $2); print $2}
' <<<"$tui_doctor" | sort -u)
[ -n "$documented" ] || { problem "docs/tui.md's Doctor section names no checks in backticks"; exit 1; }

while IFS= read -r name; do
	grep -qxF -- "$name" <<<"$documented" ||
		problem "doctor check \"$name\" is not named in docs/tui.md's Doctor section"
done <<<"$doctor_names"

# The reverse direction, which the criterion asks for and the prose version
# never had: a check documented here that the CLI stopped emitting. Same rot as
# a removed flag still listed in README, already guarded both ways above.
while IFS= read -r name; do
	grep -qxF -- "$name" <<<"$doctor_names" ||
		problem "docs/tui.md documents doctor check \"$name\", which \`pact doctor\` does not emit"
done <<<"$documented"

# ---------------------------------------------------------------------------

if [ $fail -eq 0 ]; then
	printf 'docs ok: %d commands, %d flags, %d doctor checks, all links resolve\n' \
		"${#commands[@]}" "${#flags[@]}" "$(wc -l <<<"$doctor_names")"
fi
exit $fail
