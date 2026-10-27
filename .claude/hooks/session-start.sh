#!/bin/bash
# SessionStart hook for a Claude Code cloud session: make a fresh container the
# contributor's machine docs/development.md describes, so `mise run check`, `bd`
# and pact itself work from the session's first command.
# Runs only in the cloud (CLAUDE_CODE_REMOTE=true); a laptop keeps its own setup.
# Idempotent: every step is skipped or a no-op when already done, and the
# container's state is snapshotted after the hook, so later sessions pay only for
# what changed.
#
# What it does, and why (docs/development.md, "Cloud sessions"):
#   1. mise, pinned, from its GitHub release.
#   2. MISE_ENV=cloud, so mise.cloud.toml supplies mise.toml's tools from pinned
#      release URLs: the sandbox's api.github.com answers only for this
#      repository, so no backend that lists versions can work.
#   3. The current stable toolchain with rustfmt and clippy, through rustup, which
#      is what CI installs. The image's own stable lags it by months, and a
#      clippy older than CI's is a lint gate CI does not share.
#   4. `mise install`: bd, cog, shellcheck, mbx and recount, linked into
#      ~/.local/bin so a script or git hook finds them without mise being
#      activated.
#   5. mbx, the compile cache mise.toml wraps cargo with: its Cargo shim first
#      on PATH, so a plain `cargo` in the session is cached as well as
#      `mise run`'s, and its disk budget set explicitly. Left at its default
#      the free-space floor is 10% of the disk `df` reports, which here is far
#      larger than the session's fixed allowance: mbx then reads the disk as
#      always low and evicts on every build.
#   6. The beads database. A checkout carries .beads/config.yaml, metadata.json
#      and the committed export but no database: `bd bootstrap` builds one from
#      the export (never `bd init`, which rewrites the instruction files), and
#      the import after it catches a snapshotted container up with the export,
#      where a newer row wins and nothing is deleted.
#   7. A warm build of the test targets, the default build first and every
#      feature last (the order `mise run test` keeps, so target/debug/pact is the
#      all-features binary), through mbx, so the first build a session asks
#      for is a cache restore, and this checkout's own pact installed on PATH
#      (`mise run install`), which the protocol in AGENTS.md and
#      scripts/check-docs.sh call by name. PACT_SESSION_SKIP_BUILD=1 skips it.
#   8. A probe of the hosts the work depends on, handed to the session as
#      context, so a blocked host is known before a task needs it.
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

# Stdout is the hook's answer to Claude Code and carries one JSON object; every
# step below writes to stderr, which is the hook's log.
exec 3>&1 1>&2

MISE_VERSION="2026.9.12"
root="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "$root"

log() { printf '[session-start] %s\n' "$*" >&2; }

export MISE_ENV=cloud
export MISE_YES=1 # never prompt (trust, install)
MBX_SHIM_DIR="$HOME/.local/share/mbx/bin"
export PATH="$MBX_SHIM_DIR:$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

# Every later shell of the session sees the same environment. The single quotes
# are deliberate: $HOME and $PATH expand when the session sources the file.
# shellcheck disable=SC2016
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  {
    echo 'export MISE_ENV=cloud'
    echo 'export MISE_YES=1'
    echo 'export PATH="$HOME/.local/share/mbx/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH"'
  } >>"$CLAUDE_ENV_FILE"
fi

# 1. mise, pinned, from the release tarball.
if ! command -v mise >/dev/null 2>&1; then
  log "installing mise $MISE_VERSION"
  mkdir -p "$HOME/.local/bin"
  tmp=$(mktemp -d)
  curl -fsSL --retry 3 \
    "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-x64.tar.gz" \
    | tar -xz -C "$tmp"
  install -m 0755 "$tmp/mise/bin/mise" "$HOME/.local/bin/mise"
  rm -rf "$tmp"
fi
mise trust mise.toml >/dev/null 2>&1 || true
mise trust mise.cloud.toml >/dev/null 2>&1 || true
log "mise $(mise --version 2>/dev/null | head -1)"

# 3. The current stable, as CI installs it (ci.yml, dtolnay/rust-toolchain).
if ! command -v rustup >/dev/null 2>&1; then
  log "rustup is missing from the image; install it before this hook can continue"
  exit 1
fi
rustup toolchain install stable --component rustfmt --component clippy --no-self-update
rustup default stable >/dev/null
log "$(cargo --version), $(cargo clippy --version)"

# 4. The tools (mise.toml, overridden by mise.cloud.toml), each a pinned release
#    binary with its checksum: nothing is compiled.
mise install
for tool in bd cog shellcheck mbx recount; do
  if path=$(mise which "$tool" 2>/dev/null) && [ -n "$path" ]; then
    ln -sfn "$path" "$HOME/.local/bin/$tool"
  fi
done
log "$(bd --version 2>/dev/null | head -1), $(cog --version 2>/dev/null), shellcheck $(shellcheck --version 2>/dev/null | sed -n 's/^version: //p'), $(mbx --version 2>/dev/null), $(recount --version 2>/dev/null)"

# 5. The build cache. `mbx setup` outside an activated mise shell writes only
#    its Cargo shim (MBX_SHIM_DIR, already first on PATH above) and edits no
#    mise config: mise.toml's `wrappers` key already covers `mise run`.
#    The budget: a ceiling, so the cache cannot fill the session's allowance,
#    and a floor stated in bytes rather than as mbx's default 10% of a disk
#    this sandbox does not own. Either one the user set is kept.
mbx setup --yes 2>&1 | grep -E '^installed' | sed 's/^/[mbx setup] /' || true
[ -n "$(mbx settings get gc.min_free_size 2>/dev/null)" ] || mbx settings set gc.min_free_size 2GiB >/dev/null
[ -n "$(mbx settings get gc.max_total_size 2>/dev/null)" ] || mbx settings set gc.max_total_size 12GiB >/dev/null
mbx gc >/dev/null 2>&1 || true
log "mbx: cargo is $(command -v cargo); $(mbx stats 2>/dev/null | grep -E '^(shared cache|managed targets)' | tr -s ' ' | paste -sd ';' -)"

# 6. The beads database: bootstrap, then catch up with the export.
#    BD_GIT_HOOK=1 keeps bd from re-exporting after the import, should
#    export.auto ever be turned on: the export stays as git wrote it.
chmod 700 .beads
git config beads.role >/dev/null 2>&1 || git config beads.role maintainer
bd bootstrap --non-interactive 2>&1 | grep -v -e '^$' -e 'permissions 0755' | sed 's/^/[bd bootstrap] /'
if [ -d .beads/embeddeddolt ] && [ -f .beads/issues.jsonl ]; then
  BD_GIT_HOOK=1 bd import .beads/issues.jsonl 2>&1 | grep -v -e '^$' -e 'permissions 0755' | sed 's/^/[bd import] /' || true
fi

# 7. The warm build, through mbx, and the checkout's pact on PATH. The feature
#    list is read from mise.toml (PACT_ALL_FEATURES), the one place it is
#    defined.
if [ "${PACT_SESSION_SKIP_BUILD:-}" != "1" ]; then
  features=$(mise exec -- printenv PACT_ALL_FEATURES)
  log "warming the build: cargo test --no-run, default build then --features $features"
  cargo test --no-run --quiet
  cargo test --features "$features" --no-run --quiet
  mise run install
  log "$(pact --version 2>/dev/null | head -1) on PATH, built from $(git rev-parse --short HEAD)"
else
  log "warm build and pact install skipped (PACT_SESSION_SKIP_BUILD=1)"
fi

# 8. The network. The environment's egress policy decides which hosts answer;
#    each host below is one the work depends on.
blocked=""
for h in index.crates.io static.crates.io static.rust-lang.org docs.rs github.com release-assets.githubusercontent.com; do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 6 "https://$h/" 2>/dev/null || true)
  case "$code" in 000 | "") blocked="$blocked $h" ;; esac
done
if [ -n "$blocked" ]; then
  network="blocked by the egress policy:$blocked. cargo needs crates.io, rustup static.rust-lang.org, and the pinned tools github.com and release-assets.githubusercontent.com; the environment's allowed domains are the fix (docs/development.md, Cloud sessions)."
else
  network="every host the work depends on answers (crates.io, rust-lang, docs.rs, GitHub releases)."
fi
log "network: $network"
log "done"

context="[session-start] mise, bd, cog, shellcheck, mbx, recount and this checkout's pact are on PATH; cargo runs through mbx, a compile cache shared by every checkout and worktree (\`mbx stats\` shows what it saved); the beads database is bootstrapped from .beads/issues.jsonl. network: $network"
printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' \
  "$(printf '%s' "$context" | python3 -I -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" >&3
