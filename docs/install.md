# Installing pact

pact is one binary with no runtime of its own. The only external thing it needs
is a Beads CLI, and only for the `msg` subcommands — the README explains why
messaging is layered on somebody else's issue tracker instead of a store pact
would have to maintain.

## Install

```bash
mise run install   # cargo install --path . --force --features ui
```

Or manually:

```bash
cargo build --release --features ui       # drop --features ui to skip `pact ui`
cp target/release/pact /usr/local/bin/    # or anywhere on your PATH
```

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

Exit code `3` still means "no usable Beads CLI on `PATH`", and it now names
*which* one to install and why the other one you already have isn't a
substitute. Tested ranges are per backend — `bd` `1.1.0 <= v < 1.2.0`, `br`
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
pact 0.3.2
commit:   e3bf274b82cd-dirty
built:    2026-08-03T05:21:37Z
rustc:    rustc 1.97.1 (8bab26f4f 2026-07-14)
target:   x86_64-unknown-linux-gnu
profile:  release
features: otel,ui
```

`profile: debug` means you're running `target/debug` rather than the installed
release build; `features: none` explains a missing `pact ui`; `-dirty` means
the build had uncommitted changes. A stale `pact` on `PATH` has silently
rewritten `AGENTS.md` from an old build before — this is how you catch it.

