# pact ui

`pact ui` is an interactive terminal dashboard over everything else in this
project: the leases under `.pact/leases/` and the messages `pact msg`
sends/reads, plus a live `pact doctor` panel — all in one screen, with keys
instead of re-typing CLI invocations. It's built on
[ratatui](https://ratatui.rs) + its bundled [crossterm](https://github.com/crossterm-rs/crossterm)
backend, chosen because they're the actively-maintained standard for Rust
TUIs rather than something to build from scratch.

Like every other pact command, it's a single foreground process: no daemon,
nothing left running after you quit.

## Requires the `ui` Cargo feature

Everything on this page is gated behind the optional `ui` feature, so a repo
that only wants leases and messaging doesn't compile ratatui. **A default build
has no `ui` subcommand at all** — `pact ui` answers `error: unrecognized
subcommand 'ui'`, which looks like a missing install rather than a missing
feature. Build with `--features ui`:

```bash
mise run install                          # already passes --features ui
cargo install --path . --force --features ui
cargo build --release --features ui
```

`pact --version` ends with the enabled features, so `features: none` is the
one-line confirmation that this is what happened.

```bash
pact ui
```

## Tabs

```mermaid
stateDiagram-v2
    [*] --> Leases
    Leases --> Messages: Tab / 2
    Messages --> Doctor: Tab / 3
    Doctor --> Leases: Tab / 1
    Messages --> Thread: Enter
    Thread --> Messages: Esc
```

`Tab` / `Shift+Tab` cycle through the three tabs; `1`/`2`/`3` jump directly
to one, or click a tab's label directly. The status line at the bottom
always shows the keys that apply to whatever you're looking at.

## Mouse

Every keyboard-driven selection also works with the mouse:

| Action | Effect |
|---|---|
| Hover a tab label or a row | highlighted, so you can see what a click would do before clicking |
| Click a tab label | switch to that tab |
| Click a row (Leases or Messages) | select it, same as moving there with `j`/`k` |
| Scroll wheel | move the selection up/down, same as `j`/`k` |

Each tab's clickable area is exactly the rect its label was rendered into —
not an equal-width guess across the header — so hit-testing can't drift out
of sync with what's on screen no matter how the label text or terminal width
changes. Hovering highlights confirm this before you commit to a click.

Clicking never triggers a release or opens a thread by itself — those stay
explicit keypresses (`Enter`/`d`, and the confirm-before-force-release
step), since they have real side effects. A click only selects.

### Leases

A live table of everything under `.pact/leases/` — path, holder, age,
remaining TTL, active/expired, and any `--note`. It refreshes every second on
its own, or immediately on `r`.

| Key | Action |
|---|---|
| `j` / `↓`, `k` / `↑` | move selection |
| `r` | refresh now |
| `Enter` / `d` | release the selected lease |
| `Esc` / `n` | cancel a pending force-release |

Releasing your own lease (matched against `--agent`/`PACT_AGENT`) happens
immediately. Releasing someone else's asks for a second `Enter`/`d` before
doing it — same principle as the CLI's `--steal`: overriding another agent's
claim is always an explicit, visible action, never silent. See
[docs/leases.md](leases.md) for the underlying lease semantics.

### Messages

Your inbox (`bd list --assignee=<agent> --include-infra`, same as
`pact msg inbox`), with unread messages marked `*`. `Enter` opens the full
thread — root message plus replies — in a detail pane and marks it read;
`Esc` goes back to the list.

| Key | Action |
|---|---|
| `j` / `↓`, `k` / `↑` | move selection |
| `r` | refresh now |
| `Enter` | open the selected thread |
| `Esc` (in a thread) | back to the list |

If `bd` isn't on `PATH`, or no `--agent`/`PACT_AGENT` is set, this tab shows
that inline instead of failing the whole UI to launch — Leases and Doctor
stay fully usable either way. See [docs/messaging.md](messaging.md) for how
messages map onto Beads issues.

### Doctor

The same checks as `pact doctor`, rendered live: git repo, `.pact/` presence,
`AGENTS.md` freshness, whether `CLAUDE.md` reaches the protocol, whether those
two files would survive a clone (i.e. aren't gitignored), the `bd` binary and
version (warning outside the tested range), stale-lease count, and corrupt-lock
count.
Lazy-loaded like Messages (only checked once you visit the tab, or press
`r`), since it shells out to `bd --version` the same way Messages does.

| Key | Action |
|---|---|
| `r` | re-run the checks |

## Quitting

`q` or `Ctrl-C`, from any tab. The terminal is restored even if the app
panics — a crashed TUI leaving your shell in raw mode is exactly the kind of
papercut pact tries not to introduce.
