mod agents;
mod agents_md;
mod audit;
mod beads;
mod cli;
mod doctor;
mod events;
mod git_history;
mod identity;
mod lease;
#[cfg(feature = "mcp")]
mod mcp;
mod merge;
mod msg;
mod otel;
mod output;
mod plan;
mod repo;
#[cfg(feature = "ui")]
mod tui;
mod watch;

use clap::Parser;

use cli::{clap_outcome, run, subcommand_name, Cli};

/// `tui` renders the same activity feed `pact log` does, and reaches for these
/// two through the crate root. Re-exported here so that moving them under `cli`
/// left the path it already used unchanged.
#[cfg(feature = "ui")]
pub(crate) use cli::{one_line, since};

/// The repository's directory *name* for `pact.repo` — never its path.
///
/// A path is the wrong value twice over: it is unbounded as a dimension (the
/// rule this whole epic is under), and it ships the operator's home-directory
/// layout to a collector for no benefit. The basename is what a human reads
/// on a dashboard and what joins two agents working the same checkout.
///
/// Read-only and best-effort: `None` outside a git repo, which is exactly the
/// case (`exit 4`) whose trace is most worth having.
fn repo_name() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = repo::find_repo_root(&cwd).ok()?;
    Some(root.file_name()?.to_string_lossy().into_owned())
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // --help and -V arrive here too, and are not errors. clap picks the
            // right stream for each (help to stdout, errors to stderr); the
            // result is dropped because a closed pipe must not turn into a
            // panic after the work is done — same rule as output::line.
            let _ = e.print();
            let (code, shape) = clap_outcome(e.kind());
            // Traced too, which it was not: `otel::init` sits below
            // `try_parse`, so exit 5 — the one code a mis-scripted agent
            // actually hits, and the one the protocol block tells agents to
            // branch on — was the only documented exit code that put nothing on
            // the wire at all. `shape` is a literal from the two arms above, so
            // the dimension stays as bounded as `subcommand_name`'s.
            otel::init(shape).finish(code);
            std::process::exit(code);
        }
    };
    // One trace per invocation. `otel` is a no-op struct unless the off-by-
    // default `otel` feature is on, so this line costs nothing in the build
    // everyone actually ships (see src/otel.rs).
    let subcommand = subcommand_name(&cli.command);
    let mut telemetry = otel::init(subcommand);
    // The attributes that make the trace joinable (pact-aw7.2). Set here and
    // not inside otel.rs: identity is main's job, and the exporter has no
    // business walking the filesystem for a repo root.
    telemetry.set("pact.subcommand", subcommand);
    telemetry.set("pact.json", cli.json);
    // Captured before `run(cli)` moves `cli` by value: the error arm below
    // needs to know whether `--json` was requested, to decide whether a
    // structured error (pact-m7j.6.5) prints as JSON or as the usual text.
    let json = cli.json;
    // The *resolved* identity, so `--agent` shows up too — otel.rs can only
    // see PACT_AGENT. An unresolvable identity is simply absent: `whoami` and
    // `doctor` exist to be run when it is broken, and they must still trace.
    if let Ok(agent) = identity::resolve_agent(cli.agent.as_deref()) {
        telemetry.set("pact.agent", agent);
    }
    if let Some(repo) = repo_name() {
        telemetry.set("pact.repo", repo);
    }
    match run(cli) {
        // A subcommand can report a non-zero code without being an error —
        // `doctor` on an unhealthy repo is the case (see `run`). Flush first
        // either way.
        Ok(code) => {
            // A `Placement::LocalFallback` degradation used to be visible only
            // via a separately-run `pact doctor` — every other command that
            // resolved the same topology dropped the warning on the floor.
            // Read once here, regardless of how many times `resolve_topology`
            // ran inside `run(cli)`, and surface it on the command that
            // actually hit it.
            if let Some(w) = repo::take_warning() {
                output::warn(&w);
            }
            telemetry.finish(code);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(e) => {
            let code = output::code_for(&e);
            // A `--json` caller gets a single parseable document on stdout for
            // EVERY failing exit code (pact-m7j.5.1). There used to be a second,
            // richer shape here for a partially-failed `msg send`
            // (`already_sent`/`--skip`, pact-m7j.6.5); a send is one append now, so
            // it cannot partially fail and that shape has nothing left to report.
            // Without `--json`, the human-readable text stays on stderr.
            if json {
                output::emit_error_json(&e, code);
            } else {
                output::warn(&format!("error: {e:#}"));
            }
            if let Some(w) = repo::take_warning() {
                output::warn(&w);
            }
            // Before the exit, not after: `std::process::exit` skips
            // destructors, so a `Drop`-only flush would export exactly the
            // successful runs and lose every failure worth looking at.
            telemetry.finish(code);
            std::process::exit(code);
        }
    }
}
