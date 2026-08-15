---
title: Installing pact
description: Installing with mise or by hand, what a release contains and why it is one binary per platform, and what pact does and does not need installed alongside it.
audience: operators
---

# Installing pact

pact is one binary with no runtime of its own. **Since 0.9.0 it needs nothing
external but `git`** — no daemon, no database, and no Beads CLI. Every command,
`msg` included, works on a machine that has never had `bd` installed.

`bd` is still worth having, because it is what agents track work in and because
one `pact audit` check reads its committed audit log
([the Beads CLI](#the-beads-cli), below). It is no longer a prerequisite.

## Download a release

Every version tag publishes prebuilt tarballs, so installing pact does not
require a Rust toolchain — an odd prerequisite for a tool whose main audience is
CI fleets and coding agents.

### With a version manager

```bash
mise use -g github:chussenot/pact@latest
```

This is the shortest route and needs nothing installed but `mise` itself. It
works because a release publishes **exactly one tarball per platform**, so there
is no ambiguity for mise to resolve — see [One profile](#one-profile) for why
that constraint exists.

### By hand

```bash
TAG=0.7.6
TARGET=x86_64-unknown-linux-musl

BASE="https://github.com/chussenot/pact/releases/download/$TAG"
curl -fsSLO "$BASE/pact-$TAG-$TARGET.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"

# Verify before extracting. `--ignore-missing` because SHA256SUMS covers every
# tarball in the release and you downloaded one of them.
sha256sum --ignore-missing -c SHA256SUMS

tar -xzf "pact-$TAG-$TARGET.tar.gz"
install -m755 "pact-$TAG-$TARGET/pact" /usr/local/bin/
```

### One profile

A release ships one binary per platform, with `ui`, `otel` and `mcp` compiled
in, and each release asserts that before publishing — the binary must answer to
`pact ui` and `pact mcp serve` or the tag does not ship.

**That is a constraint imposed by version managers, not a preference.** They
pick a release asset by matching the OS and architecture in its name, so two
tarballs per platform are two candidates to break a tie between, and which one
wins is an implementation detail of the manager. Getting it wrong would hand
`mise use -g github:chussenot/pact@latest` a binary with no TUI and no MCP
server on somebody's workstation. One asset per platform makes the question
disappear rather than betting on the answer.

Nothing is paid for unasked: `otel` exports only when the standard `OTEL_*`
variables are set, and `mcp` does nothing until a client spawns
`pact mcp serve`. The whole binary is 3.0 MiB — a `strip`ped x86-64 release,
measured rather than estimated, and drifting upward as pact grows.

If the ~0.5 MiB matters — a container layer pulled by every job in a fleet —
build without the optional features:

```bash
cargo install --path . --no-default-features   # or --git https://github.com/chussenot/pact
```

That was a published `lean` tarball until releases had to become
unambiguous. It is still a supported build, just not a download.

### Targets

| Target | Runs on |
|---|---|
| `x86_64-unknown-linux-musl` | any x86-64 Linux, including `FROM scratch` |
| `aarch64-unknown-linux-musl` | any arm64 Linux (Graviton, Ampere, arm64 CI) |
| `x86_64-apple-darwin` | Intel macOS |
| `aarch64-apple-darwin` | Apple-silicon macOS |

The Linux builds are statically linked against musl, so there is no glibc
version to match and no base image to get right. **There is no Windows build**,
and that is a decision rather than a gap: pact's coordination model assumes unix
semantics — lease claims rely on `rename`/`hard_link` atomicity guarantees POSIX
makes and Windows does not, the telemetry code reads `/dev/urandom`, and `pact
init` writes instruction files referencing sh-based hooks. It would compile and
then be wrong in ways no test here would catch.

### Verifying where a binary came from

Each release carries a [build-provenance
attestation](https://docs.github.com/en/actions/concepts/security/artifact-attestations),
so a checksum can be checked against *what built it* rather than only against
the file next to it:

```bash
gh attestation verify "pact-$TAG-$TARGET-$PROFILE.tar.gz" --repo chussenot/pact
```

That answers a question `SHA256SUMS` cannot: the checksum file proves the tarball
was not altered in transit, the attestation proves which workflow, commit and run
produced it in the first place.

## Build from source

Needs a Rust toolchain. This is the path to take when you are working *on* pact
rather than with it — [development.md](development.md) covers the task list.

```bash
mise run install   # cargo install --path . --force with every feature
```

That is the `full` profile above. Or manually:

```bash
cargo install --path . --force --features ui,otel,mcp   # full
cargo install --path . --force                          # no optional features
```

```bash
cargo build --release --features ui,otel,mcp
cp target/release/pact /usr/local/bin/    # or anywhere on your PATH
```

## With mise

```bash
mise use -g github:chussenot/pact@latest
```

That is the whole thing. mise's **github backend** reads the release assets
directly, needs no Rust toolchain, and resolves unambiguously because a release
publishes exactly one tarball per platform ([One profile](#one-profile)).

Pin a version instead of `latest` with `@0.7.6`; `mise ls-remote
github:chussenot/pact` lists what exists.

### Two routes that look right and are not

`mise plugin add pact https://github.com/chussenot/pact.git` looks like the right
command and is not one. pact ships no asdf/vfox plugin, so mise clones this
repository, finds none of the `bin/list-all` and `bin/install` scripts a plugin is
made of, and reports success:

```
mise plugin:pact   ✓ https://github.com/chussenot/pact.git#cc18dcc
```

The failure arrives one command later, which is what makes it worth writing down
— `mise use pact` then answers `pact not found in mise tool registry`, naming
neither the clone nor the reason.

`ubi:chussenot/pact` also works today and should not be used: mise deprecated
that backend in favour of `github:`, and warns that it goes away in 2027.1.0.

### Building from source through mise

The **cargo backend** compiles from a git tag instead of downloading, which is
what you want if you need a build the release does not publish — the
feature-less one, say. Features are a tool option rather than part of the
version, so the table form is required:

```toml
# mise.toml — or ~/.config/mise/config.toml to have it everywhere
[tools]
"cargo:https://github.com/chussenot/pact" = { version = "tag:0.4.0", features = ["ui", "otel", "mcp"] }
```

```bash
mise install
pact --version    # features: mcp,otel,ui
```

Two pieces of syntax that fail in ways that don't point at themselves:

- **`tag:0.4.0`, not `0.4.0`.** A bare version means a crates.io release and pact
  is not published there, so mise stops at `Invalid cargo git version: 0.4.0`.
- **Bracket options are not read on the command line here.** `mise use
  "cargo:…@tag:0.4.0[features=ui,otel,mcp]"` hands the brackets to cargo as part
  of the ref, and the error is about a refspec rather than about features. The
  one-liner is only good for a build with no optional features:
  `mise use "cargo:https://github.com/chussenot/pact@tag:0.4.0"`.

`locked` defaults to true in this backend, so the build uses the `Cargo.lock` the
tag carries — the same lockfile the release workflow checks against the tag, so a
mise install and a release tarball resolve the same dependency versions.

### A mise-managed pact and MCP clients

mise installs into its own directory and reaches your shell through shims or
`mise activate`. An MCP client started outside that shell — a desktop app, or
anything launched by a session manager — has neither, so `command: "pact"`
resolves to nothing. Ask mise where the binary actually is and register that
path:

```bash
mise which pact
# /home/you/.local/share/mise/installs/cargo-https-github-com-chussenot-pact/tag-0.4.0/bin/pact
```

Per-client configuration — Claude Code, Claude Desktop, Codex — is in
[mcp.md](mcp.md#registering-it). Whichever client you use, `pact mcp serve` exists
only in a build with the `mcp` feature, so check `pact --version` first: an MCP
server that answers `unrecognized subcommand` was built without `mcp`, not a broken
config.

## The Beads CLI

**Optional since 0.9.0.** No pact command requires
[`bd`](https://github.com/gastownhall/beads) at run time — `msg` included, which
was the last holdout. `pact doctor` reports which `bd` it found and its version,
informationally, because "which bd is on this machine" is the first thing anyone
asks when the task tracker misbehaves:

```
✓ Beads CLI: bd (bd version 1.2.1 (634cbbc4b)), attributes writes to the acting agent (--actor)
```

There is no tested-version window any more, and nothing warns about one. pact used
to warn outside `1.1.0 <= v < 1.3.0`, and that warning had earned its place: bd 1.2
dropped `create --id --force`'s upsert, which pact's idempotent `msg send` was
built on. The only `bd` call left is one diagnostic that `pact doctor` runs —
`bd --version` — so a version pact has not tested cannot break anything, and
warning about every future bd release would be noise.

**Two reasons to install it anyway.** It is what agents track work in — the
protocol pact writes into `AGENTS.md` tells them to. And
[`pact audit --check claim-lease-divergence`](audit.md#--check-claim-lease-divergence)
reads bd's committed audit sidecar, `.beads/interactions.jsonl`, which bd only
writes when its audit sidecar is recording — off by default. Without it that check
reports "no beads data" and passes, so `pact doctor` warns when the file is absent.

Turn recording on either way:

```bash
export BD_AUDIT_ENABLED=1          # per-environment, no config write, no warning
bd config set audit.enabled true   # persists in .beads/config.yaml
```

**bd 1.2.1 answers the second one with `Warning: "audit.enabled" is not a
recognized config key` and then honours it anyway.** The warning is bd's config-key
allowlist disagreeing with the rest of bd — `bd audit --help`, `bd audit record`'s
own error text and the `.beads/config.yaml` bd generates all name the key — not the
switch failing. Verified end to end against bd 1.2.1 (634cbbc4b): with it unset
`bd audit record` exits 1 and writes nothing; with it set, or with
`BD_AUDIT_ENABLED=1`, a plain `bd update --assignee` appends the `field_change` row
this check replays. bd records from that point, not retroactively.

`bd` is Go with an embedded Dolt database, and its store is `.beads/embeddeddolt/`.
pact walks up from the working directory for the first `.beads/` and reads what made
it, so it reports on the same store as the main checkout rather than a fresh empty
one.

### `br` (beads-rust) is no longer supported

pact used to accept either backend. **`pact 0.7.9` was the last release that
supported [`br`](https://github.com/Dicklesworthstone/beads_rust).**

If you point a newer pact at a `br` workspace, `pact doctor` says so rather than
reporting a missing CLI — `br` will still be installed and working, and the thing
that went away is pact's support:

```
this repo's .beads/ is a br (beads-rust, SQLite) workspace, and pact no longer
supports br — pact 0.7.9 was the last release that did.

Either migrate the store to bd (https://github.com/gastownhall/beads), or pin
pact 0.7.9 if you need br. pact has not touched your .beads/ and will not.
```

**Why it went.** The two CLIs run the same model but not the same argv, so pact
carried a branch for every divergence: `--include-infra`, `--no-inherit-labels`,
the `list --json` envelope, replies as dependency edges. That was affordable. What
was not is that the two backends never offered the same *guarantees* — `br` has no
`--id`/`--force` equivalent, so a replayed `msg send` on `br` duplicated the
message, and pact's documented advice to re-send when you cannot confirm a send
was actively unsafe there. Two backends meant two contracts, one of them weaker,
described in every doc that touched messaging.

That story has a second act worth knowing, because it is why messaging left bd
entirely a version later: two CLIs diverging on guarantees is the same failure as
*one* CLI changing its own guarantees between releases, which bd 1.2 then did. See
[architecture.md](architecture.md#and-since-090-no-backend-at-all).

A stray `.beads/<name>.db` left beside a `bd` store is simply ignored, and
`pact doctor` names it — a second store nobody reads is worth a warning. It used
to be named on every `pact msg` call too, because an agent seeing `inbox empty` in
a repo with a shadowed store had no hint why. `pact msg` does not read a Beads
store at all now, so a shadowed one cannot explain an empty inbox and saying so
there would be telling an agent about a dependency the command no longer has.

## Which binary am I running?

`-V` prints the bare `pact <semver>` line scripts grep for. `--version` prints
the build stamp, which answers the question a version number can't — *is the
binary on my PATH the one I just built?*

```
$ pact --version
pact 0.4.0
commit:   cc18dcce39e3
built:    2026-08-03T07:16:34Z
rustc:    rustc 1.97.1 (8bab26f4f 2026-07-14)
target:   x86_64-unknown-linux-gnu
profile:  release
features: mcp,otel,ui
```

`target:` is how you tell a downloaded release apart from a local build: a
release tarball reports `x86_64-unknown-linux-musl`, a `cargo build` on the same
machine reports `-gnu`.

`profile: debug` means you're running `target/debug` rather than the installed
release build; `features: none` explains a missing `pact ui`; `-dirty` means
the build had uncommitted changes. A stale `pact` on `PATH` has silently
rewritten `AGENTS.md` from an old build before — this is how you catch it.

