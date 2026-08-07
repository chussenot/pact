# Installing pact

pact is one binary with no runtime of its own. The only external thing it needs
is a Beads CLI, and only for the `msg` subcommands — the README explains why
messaging is layered on somebody else's issue tracker instead of a store pact
would have to maintain.

## Download a release

Every version tag publishes prebuilt tarballs, so installing pact does not
require a Rust toolchain — an odd prerequisite for a tool whose main audience is
CI fleets and coding agents.

```bash
TAG=0.4.0
TARGET=x86_64-unknown-linux-musl
PROFILE=lean

BASE="https://github.com/chussenot/pact/releases/download/$TAG"
curl -fsSLO "$BASE/pact-$TAG-$TARGET-$PROFILE.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"

# Verify before extracting. `--ignore-missing` because SHA256SUMS covers every
# tarball in the release and you downloaded one of them.
sha256sum --ignore-missing -c SHA256SUMS

tar -xzf "pact-$TAG-$TARGET-$PROFILE.tar.gz"
install -m755 "pact-$TAG-$TARGET-$PROFILE/pact" /usr/local/bin/
```

### Two profiles

Each target ships twice. The profile is in the filename because the name is a
promise about the contents, and each release asserts that promise in both
directions before publishing — the `full` binary must have `ui` and `mcp serve`,
the `lean` one must have neither.

| Profile | Contains | For |
|---|---|---|
| `lean` | no optional features: `init`, `lease`, `msg`, `log`, `doctor`, `agents`, `whoami` — 1.6 MiB | agents and CI images, which never open a dashboard |
| `full` | `ui` + `otel` + `mcp` — the TUI, OpenTelemetry export, the read-only MCP server — 2.2 MiB | humans, and anything registering pact as an MCP server |

Neither optional feature costs anything unasked, so `full` is the right default
for a workstation: `otel` exports only when the standard `OTEL_*` variables are
set, and `mcp` does nothing until a client spawns `pact mcp serve`. Pick `lean`
when the 0.6 MiB matters — a container layer pulled by every job in a fleet.

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
cargo install --path . --force                          # lean
```

```bash
cargo build --release --features ui,otel,mcp
cp target/release/pact /usr/local/bin/    # or anywhere on your PATH
```

## With mise

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

What works is mise's **cargo backend**, pointed at a git tag. Features are a tool
option rather than part of the version, so the `full` profile needs the table
form:

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
  one-liner is only good for the `lean` profile:
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
server that answers `unrecognized subcommand` is a `lean` binary, not a broken
config.

## The Beads CLI

Requires a **Beads CLI** on `PATH` for the `msg` subcommands; `init`, `lease`,
`whoami`, `agents`, `log` and `doctor` (partially — the lease half plus a
warning) work without one. Either implementation will do:

| Backend | What it is | What its `.beads/` looks like |
|---|---|---|
| [`bd`](https://github.com/gastownhall/beads) | Go, embedded Dolt | `.beads/embeddeddolt/` |
| [`br`](https://github.com/dicklesworthstone/beads-rust) | Rust, SQLite | `.beads/<name>.db` |

**The store on disk picks the backend, not a preference.** The two don't share
data, so pact walks up for the first `.beads/`, reads which tool made it, and
uses that one. Only a repo with no Beads workspace yet gets a preference (`br`,
then `bd`), and if both stores are present — what one stray `br init` inside a
`bd` repo leaves behind — `bd` wins, because that is where the data is. The
alternative, always preferring `br`, would open an empty SQLite database in
every existing `bd` repo and cheerfully report an empty inbox.

`pact doctor` has always warned about that second store. Every `pact msg`
invocation now prints the same warning on stderr, and the two MCP message tools
carry it as a `store_conflict` key. Which store gets queried is deliberately
unchanged — the point is that an agent seeing `inbox empty` in a repo with a
shadowed store had no hint why, and nobody runs `doctor` about an inbox that
merely looks quiet.

Exit code `3` means the Beads backend is unavailable. Usually that is no usable
CLI on `PATH`, and the message names *which* one to install and why the other
one you already have isn't a substitute; it is also what you get when a
`bd`/`br` had to be killed for not exiting
(see [cli.md](cli.md#exit-codes)).

Tested ranges are per backend — `bd` `1.1.0 <= v < 1.2.0`, `br`
`0.2.0 <= v < 0.3.0` — and outside them everything still runs while `pact
doctor` adds a warning, since a Beads CLI that changed its output is the
likeliest cause of a puzzling `msg` failure:

```
✓ Beads CLI: bd (bd version 1.1.2 (20e493e56))
✓ Beads CLI: br (br 0.2.19)
```

`br` is younger and its CLI still moves; the differences pact has to absorb are
listed in [docs/messaging.md](messaging.md#two-backends-two-argv).

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

