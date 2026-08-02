mod agents;
mod agents_md;
mod beads;
mod doctor;
mod events;
mod identity;
mod lease;
#[cfg(feature = "ui")]
mod mascot;
mod msg;
mod otel;
mod output;
mod repo;
#[cfg(feature = "ui")]
mod tui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use lease::human_secs;

/// Build provenance for `--version`. `-V` keeps the bare `pact <semver>` that
/// scripts grep for; the long form answers "is the binary on PATH the one I
/// just built?", which a version number alone cannot.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit:   ",
    env!("PACT_GIT_SHA"),
    "\nbuilt:    ",
    env!("PACT_BUILD_TIME"),
    "\nrustc:    ",
    env!("PACT_RUSTC"),
    "\ntarget:   ",
    env!("PACT_TARGET"),
    "\nprofile:  ",
    env!("PACT_PROFILE"),
    "\nfeatures: ",
    env!("PACT_FEATURES"),
);

/// pact: a dependency-light CLI that coordinates multiple coding agents
/// working on the same repository (onboarding, messaging, leases).
#[derive(Parser)]
#[command(name = "pact", version, long_version = LONG_VERSION, about)]
struct Cli {
    /// Agent identity; falls back to PACT_AGENT if unset.
    #[arg(long, global = true)]
    agent: Option<String>,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inject/update the coordination-protocol block in AGENTS.md.
    Init {
        /// Print the block to stdout instead of writing it.
        #[arg(long)]
        print: bool,
        /// Write the files but don't commit them. By default `init` commits
        /// exactly what it wrote, because a protocol nobody committed reaches
        /// nobody who clones.
        #[arg(long)]
        no_commit: bool,
    },
    /// Show what pact resolved: identity, paths, and the bd it will use.
    Whoami,
    /// List the agent identities seen in this repo's leases and messages.
    Agents {
        /// Answer "whose file is this?" for one PATH instead of listing
        /// everyone: the last agent to act on it, from the lease event log.
        #[arg(long, value_name = "PATH")]
        r#for: Option<String>,
    },
    /// Advisory file leases.
    Lease {
        #[command(subcommand)]
        action: LeaseAction,
    },
    /// Threaded messages between agents, via the Beads CLI.
    Msg {
        #[command(subcommand)]
        action: MsgAction,
    },
    /// Chronological activity feed: lease events and messages, oldest first.
    Log {
        /// How many events to show.
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Check that pact, AGENTS.md, and the Beads CLI are all in a healthy state.
    Doctor,
    /// Interactive terminal dashboard over leases, messages, and doctor status.
    #[cfg(feature = "ui")]
    Ui,
}

#[derive(Subcommand)]
enum LeaseAction {
    /// Acquire a lease on one or more paths, all-or-nothing.
    Acquire {
        /// Paths to claim. Several are taken atomically (pact-rnc.21): if any
        /// is held by someone else, none are taken.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
        #[arg(long, default_value_t = lease::DEFAULT_TTL_SECS)]
        ttl: u64,
        /// Force takeover of a non-expired lease.
        #[arg(long)]
        steal: bool,
        #[arg(long)]
        note: Option<String>,
    },
    /// Refresh a lease you already hold, so a long task doesn't outlive it.
    Renew { path: String },
    /// Release a lease you hold, or all of them with --all.
    Release {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        path: Option<String>,
        /// Release even if held by another agent.
        #[arg(long, conflicts_with = "all")]
        force: bool,
        /// Release every lease you hold, in one command.
        #[arg(long)]
        all: bool,
    },
    /// List active leases.
    Ls {
        /// Include expired leases in the listing.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum MsgAction {
    /// Send a message to one or more agents, as one thread.
    Send {
        /// Recipient; repeat for several (`--to a --to b`). One send is one
        /// thread, however many recipients (pact-rnc.4).
        #[arg(long, required_unless_present = "to_owner_of")]
        to: Vec<String>,
        /// Address the last agent to work on a PATH instead of a name you have
        /// to already know. Repeatable, and combines with --to. A path is
        /// stable; the process that held it is not.
        #[arg(long, value_name = "PATH")]
        to_owner_of: Vec<String>,
        /// Reply within an existing thread; omit to start a new one.
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        /// Message body. Mutually exclusive with --body-file.
        #[arg(required_unless_present = "body_file", conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the body from a file, or "-" for stdin. No shell escaping.
        #[arg(long, value_name = "PATH|-")]
        body_file: Option<String>,
    },
    /// List messages addressed to you, one line each.
    Inbox {
        #[arg(long)]
        unread_only: bool,
        /// Print every body in full instead of one line per message.
        #[arg(long)]
        full: bool,
    },
    /// List messages you sent, newest first, and whether they were read.
    Sent,
    /// Read a message (or its thread) by id.
    Read { id: String },
}

/// Usage errors get their own code so exit 2 means only what the README table
/// says it means: another agent holds the lease.
///
/// clap exits 2 for any usage error, which collided with that. Two agents hit
/// it independently in one fleet run — an unrecognized subcommand, and a
/// `--thread` left valueless by shell word-splitting — and a wrapper branching
/// on 2 reads either as a lease conflict and goes off to negotiate with a peer
/// that does not exist. The flag case is the likelier one in a script, because
/// a flag value is exactly what gets interpolated from a variable.
///
/// The protocol block tells agents to branch on the exit code rather than the
/// message text, so leaving the collision documented-but-real would have made
/// that instruction unfollowable.
const USAGE_ERROR: i32 = 5;

/// How long a resolved recipient can have been silent before `msg send` says so.
/// Fifteen minutes is well past a normal think-and-edit gap and well short of a
/// session, so it flags an agent that has probably exited without nagging about
/// one that is merely busy.
const QUIET_AGENT_SECS: i64 = 15 * 60;

/// clap's verdict, as an exit code plus the argv *shape* that stands in for a
/// subcommand name we never got to parse.
///
/// Only an explicit `--help` / `-V` is a success. Deliberately NOT
/// DisplayHelpOnMissingArgumentOrSubcommand: there, clap prints help *because
/// the invocation was incomplete*, which is a usage error the user did not ask
/// for. Treating it as success made bare `pact` exit 0, so a script whose
/// variable expanded to nothing would read "worked" — the very shape of
/// interpolation bug this code exists to disambiguate.
fn clap_outcome(kind: clap::error::ErrorKind) -> (i32, &'static str) {
    match kind {
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => (0, "help"),
        _ => (USAGE_ERROR, "usage-error"),
    }
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
            telemetry.finish(code);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(e) => {
            output::warn(&format!("error: {e:#}"));
            let code = output::code_for(&e);
            // Before the exit, not after: `std::process::exit` skips
            // destructors, so a `Drop`-only flush would export exactly the
            // successful runs and lose every failure worth looking at.
            telemetry.finish(code);
            std::process::exit(code);
        }
    }
}

/// Argv *shape* for the root span and for `pact.subcommand`: one literal per
/// subcommand-and-action, from a fixed set in this file. Never an argument
/// value — a path, an agent name or a message body has no business in a span
/// name, and a span name is the one attribute you cannot drop later.
///
/// Down to the action (`lease acquire`, not `lease`) because "which pact
/// command is slow" has never once been answered by "lease". Thirteen
/// literals is a bounded dimension; the argv that produced them is not.
///
/// No `pact.` prefix: `service.name` is already `pact`, and OTel's naming
/// guidance is explicit that a span name should not repeat the service.
fn subcommand_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Whoami => "whoami",
        Command::Agents { .. } => "agents",
        Command::Lease { action } => match action {
            LeaseAction::Acquire { .. } => "lease acquire",
            LeaseAction::Renew { .. } => "lease renew",
            LeaseAction::Release { .. } => "lease release",
            LeaseAction::Ls { .. } => "lease ls",
        },
        Command::Msg { action } => match action {
            MsgAction::Send { .. } => "msg send",
            MsgAction::Inbox { .. } => "msg inbox",
            MsgAction::Sent => "msg sent",
            MsgAction::Read { .. } => "msg read",
        },
        Command::Log { .. } => "log",
        Command::Doctor => "doctor",
        #[cfg(feature = "ui")]
        Command::Ui => "ui",
    }
}

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

/// Returns the process exit code, so that no subcommand has to call
/// `std::process::exit` behind `main`'s back. `doctor` used to do exactly
/// that, which skipped both `Guard::finish` and `Drop` — an unhealthy repo,
/// the one run worth looking at, exported no telemetry at all (pact-aw7.2).
///
/// Keep it that way. A `std::process::exit` added to any subcommand does not
/// merely lose its span: it silently drops every metric the run buffered.
/// `pact lease acquire` on a contended path raises through `output::exit_with`
/// and so still exports `pact.lease.transitions{outcome=conflicted}` — swap
/// that for a `process::exit(2)` and the conflict counter goes quiet with
/// nothing anywhere reporting a failure.
fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir()?;

    match cli.command {
        // `doctor` reports failure as an exit code rather than an error,
        // because its report *is* the output; everything else succeeds or
        // raises, and `Ok(())` means exit 0.
        Command::Doctor => run_doctor(&cwd, cli.json),
        Command::Init { print, no_commit } => {
            run_init(&cwd, print, no_commit, cli.json).map(|()| 0)
        }
        Command::Whoami => run_whoami(&cwd, cli.agent.as_deref(), cli.json).map(|()| 0),
        Command::Agents { r#for } => run_agents(&cwd, cli.json, r#for.as_deref()).map(|()| 0),
        Command::Lease { action } => {
            run_lease(&cwd, cli.agent.as_deref(), cli.json, action).map(|()| 0)
        }
        Command::Msg { action } => {
            run_msg(&cwd, cli.agent.as_deref(), cli.json, action).map(|()| 0)
        }
        Command::Log { limit } => run_log(&cwd, cli.json, limit).map(|()| 0),
        #[cfg(feature = "ui")]
        Command::Ui => {
            let root = repo::find_repo_root(&cwd)?;
            let agent = identity::resolve_agent(cli.agent.as_deref()).ok();
            tui::run(root, agent).map(|()| 0)
        }
    }
}

/// Conventional Commits on purpose. `bd init` writes `bd init: initialize …`,
/// which is not a valid conventional subject and makes `cog bump` fail on the
/// whole history — pact must not hand anyone that problem.
const INIT_COMMIT_MESSAGE: &str = "chore(pact): sync the coordination protocol block

Written by `pact init`: the managed block in AGENTS.md, the pointer back to it
in every agent-instruction file this repo already has (CLAUDE.md, GEMINI.md,
copilot-instructions.md, …), and the .pact/ line in .gitignore.";

fn run_init(cwd: &Path, print: bool, no_commit: bool, json: bool) -> Result<()> {
    if print {
        // Through output::line like everything else, so `init --print | head`
        // cannot panic on a closed pipe (pact-rnc.26). trim_end because line()
        // supplies the trailing newline the block already ends with.
        output::line(agents_md::managed_block().trim_end());
        return Ok(());
    }
    let root = repo::find_repo_root(cwd)?;
    repo::pact_dir(&root)?;
    // Child span (pact-aw7.2): `init` has two distinct costs — writing the
    // instruction files, and the git commit below — and a single root span
    // cannot tell you which one you waited on. The count is a number, never a
    // filename: paths do not go into telemetry.
    //
    // `pact.`-prefixed where the root span is not, deliberately: the root is
    // the argv shape (`init`) and a prefixed child is instantly a module
    // instrument rather than another command. Settled with lease-metrics in
    // thread pact-wisp-1ur — a bare `init.write` under a bare `init` differs
    // by one word in a waterfall, which is no signal at all.
    let (path, claude, instruction_files) = {
        let mut sp = otel::span("pact.init.write");
        let path = agents_md::apply(&root)?;
        // Claude Code never loads AGENTS.md, so writing only that file left a
        // Claude-driven fleet with no protocol at all (see `ensure_claude_md`).
        let claude = agents_md::ensure_claude_md(&root)?;
        // Same failure, other tools: an agent reading GEMINI.md or
        // .github/copilot-instructions.md was never told the protocol exists.
        // Only files the repo already has are touched — pact does not conjure
        // an instruction file for a tool nobody here uses (pact-4zx).
        let instruction_files = agents_md::ensure_instruction_files(&root)?;
        agents_md::ensure_gitignore(&root)?;
        sp.set("pact.instruction_files", instruction_files.len());
        (path, claude, instruction_files)
    };

    #[derive(serde::Serialize)]
    struct InitReport {
        agents_md: PathBuf,
        /// `null` when `CLAUDE.md` is `AGENTS.md` under another name.
        claude_md: Option<PathBuf>,
        claude_md_status: &'static str,
        /// Other agent-instruction files pointed at `AGENTS.md`; empty when the
        /// repo has none, which is the common case.
        instruction_files: Vec<PathBuf>,
        /// `null` unless a commit was actually created.
        commit: Option<String>,
        committed_files: Vec<String>,
        commit_status: String,
    }

    let (claude_md, claude_md_status, claude_line) = match claude {
        agents_md::ClaudeMd::Managed(p) => {
            let line = format!(
                "updated {} (imports AGENTS.md for Claude Code)",
                p.display()
            );
            (Some(p), "managed", line)
        }
        agents_md::ClaudeMd::AlreadyImported(p) => {
            let line = format!("left {} alone — it already imports AGENTS.md", p.display());
            (Some(p), "already-imported", line)
        }
        agents_md::ClaudeMd::SameFileAsAgentsMd => (
            None,
            "same-file-as-agents-md",
            "CLAUDE.md is AGENTS.md (symlinked) — nothing to import".to_string(),
        ),
    };

    let (commit, committed_files, commit_status, commit_line) = if no_commit {
        (None, vec![], "skipped (--no-commit)".to_string(), None)
    } else {
        // The instruction files pact just rewrote have to be in the commit too,
        // or `pact init` leaves a dirty tree it claims to have committed.
        // The other half of the init trace: this one shells out to git, and it
        // is the part that is actually slow.
        let _sp = otel::span("pact.init.commit");
        let mut targets = vec!["AGENTS.md", "CLAUDE.md", ".gitignore"];
        targets.extend(
            instruction_files
                .iter()
                .filter_map(|p| p.strip_prefix(&root).ok()?.to_str()),
        );
        match repo::commit_paths(&root, &targets, INIT_COMMIT_MESSAGE) {
            repo::CommitOutcome::Committed { sha, files } => {
                let line = format!("committed {} file(s) ({sha})", files.len());
                (Some(sha), files, "committed".to_string(), Some(line))
            }
            repo::CommitOutcome::NothingToCommit => (
                None,
                vec![],
                "nothing to commit".to_string(),
                Some("nothing to commit — already up to date".to_string()),
            ),
            // Loud, but not fatal: the files landed, so reporting failure would
            // repeat the broken-pipe mistake of calling a done job undone. The
            // warning is deferred to after the report so the reader sees what
            // *did* happen before what didn't.
            repo::CommitOutcome::Skipped(why) => (None, vec![], format!("skipped: {why}"), None),
        }
    };

    let deferred_warning = commit_status
        .strip_prefix("skipped: ")
        .filter(|_| !no_commit)
        .map(|why| format!("warning: files written but not committed — {why}"));

    let report = InitReport {
        agents_md: path,
        claude_md,
        claude_md_status,
        instruction_files,
        commit,
        committed_files,
        commit_status,
    };
    output::emit(json, &report, |r: &InitReport| {
        let mut out = format!("updated {}\n{claude_line}", r.agents_md.display());
        for p in &r.instruction_files {
            out.push_str(&format!("\nupdated {} (points at AGENTS.md)", p.display()));
        }
        if let Some(line) = &commit_line {
            out.push('\n');
            out.push_str(line);
        }
        out
    });
    if let Some(warning) = deferred_warning {
        output::warn(&warning);
    }
    Ok(())
}

/// Everything pact resolved about its own environment. Every field is optional
/// on purpose: `whoami` is what you run *because* something is broken, so it
/// must never be the thing that fails (pact-rnc.12). Problems are collected and
/// reported, not raised.
#[derive(serde::Serialize)]
struct Whoami {
    /// The resolved identity — `None` when unset or invalid; see `problems`.
    agent: Option<String>,
    /// Where the identity came from: `--agent`, `PACT_AGENT`, or `unset`.
    agent_source: &'static str,
    pact_binary: Option<String>,
    repo_root: Option<String>,
    pact_dir: Option<String>,
    bd_binary: Option<String>,
    bd_version: Option<String>,
    problems: Vec<String>,
}

fn run_whoami(cwd: &Path, agent_flag: Option<&str>, json: bool) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();

    let agent_source = match agent_flag {
        Some(_) => "--agent",
        None if std::env::var_os("PACT_AGENT").is_some() => "PACT_AGENT",
        None => "unset",
    };
    // resolve_agent's error already quotes the offending value, so a rejected
    // PACT_AGENT shows up as a problem rather than silently reading as "none".
    let agent = match identity::resolve_agent(agent_flag) {
        Ok(a) => Some(a),
        Err(e) => {
            problems.push(format!("{e:#}"));
            None
        }
    };

    let root = match repo::find_repo_root(cwd) {
        Ok(r) => Some(r),
        Err(e) => {
            problems.push(format!("{e:#}"));
            None
        }
    };
    // Deliberately does not create it: whoami is a read-only question.
    let pact_dir = root.as_ref().map(|r| r.join(".pact"));

    let (bd_binary, bd_version) = match beads::BeadsCli::locate() {
        Ok(bd) => {
            let version = match root.as_deref() {
                Some(r) => {
                    let (version, found) = bd_health(&bd, r);
                    problems.extend(found);
                    version
                }
                None => None,
            };
            (Some(bd.binary().to_string()), version)
        }
        Err(e) => {
            problems.push(format!("{e:#}"));
            (None, None)
        }
    };

    let info = Whoami {
        agent,
        agent_source,
        pact_binary: std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
        repo_root: root.as_ref().map(|r| r.display().to_string()),
        pact_dir: pact_dir.as_ref().map(|p| p.display().to_string()),
        bd_binary,
        bd_version,
        problems,
    };

    let dir_missing = pact_dir.map(|p| !p.exists()).unwrap_or(false);
    output::emit(json, &info, |i: &Whoami| {
        let mut rows = vec![vec![
            "agent".to_string(),
            match &i.agent {
                Some(a) => format!("{a}  (from {})", i.agent_source),
                None => "(none)".to_string(),
            },
        ]];
        rows.push(vec![
            "pact".to_string(),
            i.pact_binary.clone().unwrap_or_else(|| "?".to_string()),
        ]);
        rows.push(vec![
            "repo root".to_string(),
            i.repo_root.clone().unwrap_or_else(|| "(none)".to_string()),
        ]);
        rows.push(vec![
            "pact dir".to_string(),
            match (&i.pact_dir, dir_missing) {
                (Some(p), true) => format!("{p}  (not created yet)"),
                (Some(p), false) => p.clone(),
                (None, _) => "(none)".to_string(),
            },
        ]);
        rows.push(vec![
            "bd".to_string(),
            match (&i.bd_binary, &i.bd_version) {
                (Some(b), Some(v)) => format!("{b}  ({v})"),
                (Some(b), None) => b.clone(),
                (None, _) => "(not found)".to_string(),
            },
        ]);
        let mut out = table(&rows);
        for p in &i.problems {
            out.push_str(&format!("\n! {p}"));
        }
        out
    });
    Ok(())
}

/// bd's version plus whatever is wrong with it, for `whoami` to report.
///
/// `bd --version` only answers "is the binary there", which is not the question
/// an operator has when a pact command just failed: in a repo with no beads
/// database `bd --version` is perfectly happy while every bd-backed pact command
/// exits 1. So probe with the query those commands actually run, and report the
/// failure as a problem rather than an exit code (pact-rnc.12). Deliberately
/// `cli.run` and not `msg::all_messages`: parsing every message in the repo to
/// answer "can bd read this database" is far more work than the question needs,
/// and whoami must stay a cheap read-only probe.
///
/// The probe is bare `list --json` and deliberately carries no filter. It used
/// to pass `--include-infra`, which br rejects outright, so on a br workspace
/// `pact whoami` reported "bd cannot read this repo's beads database … `pact
/// msg` will fail" while `pact msg` worked perfectly — a diagnostic that lies
/// about the thing you ran it to diagnose. Both backends answer the unfiltered
/// form, and both still fail it when there is no database, which is the only
/// property this probe needs.
fn bd_health(bd: &beads::BeadsCli, root: &Path) -> (Option<String>, Vec<String>) {
    let mut problems = Vec::new();
    let version = match bd.version(root) {
        Ok(v) => Some(v),
        Err(e) => {
            problems.push(format!("bd found but not runnable: {e:#}"));
            None
        }
    };
    if let Err(e) = bd.run(root, &["list", "--json"]) {
        problems.push(format!(
            "bd cannot read this repo's beads database, so `pact msg` and the message \
             half of `pact agents` will fail: {e:#}"
        ));
    }
    (version, problems)
}

fn run_agents(cwd: &Path, json: bool, for_path: Option<&str>) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;

    // "Whose file is this?" — the question `lease ls` could never answer once a
    // lease was released, and the scriptable half of pact-o38. Answered from the
    // event log, so there is no registry to keep in sync.
    if let Some(path) = for_path {
        #[derive(serde::Serialize)]
        struct OwnerReport<'a> {
            path: &'a str,
            agent: Option<String>,
            /// `acquired` / `released` / `renewed` / `expired` / `stolen`.
            last: Option<String>,
            at: Option<String>,
            note: Option<String>,
        }
        let owner = events::owner_of(&root, path)?;
        let report = OwnerReport {
            path,
            agent: owner.as_ref().map(|o| o.agent.clone()),
            last: owner.as_ref().map(|o| o.kind.clone()),
            at: owner.as_ref().map(|o| o.at.clone()),
            note: owner.as_ref().and_then(|o| o.detail.clone()),
        };
        // Exits 0 with agent: null when nobody has touched it. "No owner" is an
        // answer, not a failure — the same reason `whoami` never raises.
        output::emit(json, &report, |r: &OwnerReport| match &r.agent {
            None => format!("{}: no agent has acted on this path", r.path),
            Some(agent) => {
                let ago =
                    r.at.as_deref()
                        .and_then(age_of)
                        .map(|s| format!("{} ago", human_secs(s)))
                        .unwrap_or_else(|| "at an unknown time".to_string());
                let note = r
                    .note
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!("\n  note: {n}"))
                    .unwrap_or_default();
                format!(
                    "{}: {agent} ({} {ago}){note}",
                    r.path,
                    r.last.as_deref().unwrap_or("acted")
                )
            }
        });
        return Ok(());
    }

    // bd is optional here, exactly as it is for `pact lease`: without it we can
    // still name whoever holds a lease. `locate()` covers bd being absent;
    // `agents::list` covers bd being present but unable to answer, folding that
    // into the same lease-only listing with a warning on stderr. So this `?` is
    // now only for unreadable lease files (pact-rnc.6).
    let cli = beads::BeadsCli::locate().ok();
    let found = agents::list(cli.as_ref(), &root)?;

    output::emit(json, &found, |found: &Vec<agents::AgentInfo>| {
        if found.is_empty() {
            return "no agents seen in this repo yet".to_string();
        }
        let mut rows = vec![vec![
            "AGENT".to_string(),
            "LAST SEEN".to_string(),
            "LEASES".to_string(),
            "SENT".to_string(),
            "RECV".to_string(),
        ]];
        rows.extend(found.iter().map(|a| {
            vec![
                // AGENTS.md tells agents to check a recipient with this command,
                // so a name that has only ever been addressed must not read as
                // an agent — otherwise the check confirms typos (pact-rnc.5).
                if a.answers() {
                    a.name.clone()
                } else {
                    format!("{} ?", a.name)
                },
                since(&a.last_seen),
                a.leases_held.to_string(),
                a.messages_sent.to_string(),
                a.messages_received.to_string(),
            ]
        }));
        let mut out = table(&rows);
        if found.iter().any(|a| !a.answers()) {
            out.push_str(
                "\n\n? addressed but never seen acting — nobody has ever run pact under \
                 that name, so nobody is reading its mail (usually a typo'd --to)",
            );
        }
        out
    });
    Ok(())
}

/// Unread messages about the paths being leased, surfaced to whoever is taking
/// them over.
///
/// `--to-owner-of` addressed a file and then resolved it to an agent name, and
/// delivery stopped there. Over one fleet run, 30 of 44 agent-to-agent messages
/// went to agents who had already exited and none were ever read, while every
/// message to a live agent WAS read: addressing was never the failure,
/// deliverability was. Every one of the 30 was about a file, sent to the agent
/// who had just released it — so the moment someone leases that file is exactly
/// the moment the message becomes useful again (pact-4tj).
///
/// Silent when `bd` is absent: acquiring a lease must not start depending on
/// the messaging backend, which `lease` has never needed.
fn messages_about(root: &Path, paths: &[String], agent: &str) -> Vec<String> {
    let Ok(cli) = beads::BeadsCli::locate() else {
        return Vec::new();
    };
    paths
        .iter()
        .filter_map(|path| {
            let waiting: Vec<msg::Message> = msg::about_path(&cli, root, path)
                .ok()?
                .into_iter()
                // Yours already, or already read by you: not news.
                .filter(|m| m.from != agent && !m.read_by.iter().any(|r| r == agent))
                .collect();
            let first = waiting.first()?;
            Some(format!(
                "note: {} unread message(s) about {path}, oldest from {} — \"{}\". \
                 Read it before you edit: `pact msg read {}`",
                waiting.len(),
                first.from,
                first.subject.as_deref().unwrap_or("(no subject)"),
                first.id
            ))
        })
        .collect()
}

/// Advisory lines for paths someone else worked on recently.
///
/// A lease says nothing about a path once it is released, so `acquire` on a
/// file another agent finished with three minutes ago printed the same
/// four-word success line it prints for an untouched file. That is how one
/// word-fix in a nine-agent run got routed to the same agent by three peers and
/// was then nearly applied a second time, with worse wording, by an agent whose
/// `acquire` told it nothing (pact-o38).
///
/// Advisory only: it never blocks and never changes the exit code. The point is
/// that you find out before you edit, not that pact decides for you.
fn prior_owners(root: &Path, paths: &[String], agent: &str) -> Vec<String> {
    paths
        .iter()
        .filter_map(|p| {
            let owner = events::owner_of(root, p).ok().flatten()?;
            // Your own history is not news.
            if owner.agent == agent {
                return None;
            }
            let ago = age_of(&owner.at)
                .map(|s| format!("{} ago", human_secs(s)))
                .unwrap_or_else(|| owner.at.clone());
            let note = owner
                .detail
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| format!(" — their note: {d}"))
                .unwrap_or_default();
            Some(format!(
                "note: {p} was last {} by {} ({ago}){note}. `pact log` has the history; \
                 `pact msg send --to-owner-of {p}` reaches them.",
                owner.kind, owner.agent
            ))
        })
        .collect()
}

/// Seconds since an RFC3339 stamp, or `None` if it will not parse. Timestamps
/// are compared as parsed instants, never as strings: `bd` writes `…Z` and pact
/// writes `…+00:00`, which sort differently as bytes than as time.
fn age_of(at: &str) -> Option<i64> {
    let then = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    Some((chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds())
}

fn run_lease(cwd: &Path, agent_flag: Option<&str>, json: bool, action: LeaseAction) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    match action {
        LeaseAction::Acquire {
            paths,
            ttl,
            steal,
            note,
        } => {
            let agent = identity::resolve_agent(agent_flag)?;
            // Look up prior owners BEFORE acquiring: the acquire appends its own
            // event, which would make the caller the answer to its own question.
            let prior = prior_owners(&root, &paths, &agent);
            let mut outcomes = lease::acquire_many(&root, &agent, &paths, ttl, steal, note)?;
            // One path renders and serializes exactly as it always did — a
            // script doing `lease acquire f --json | jq .lease.path` must not
            // start getting an array back because the command learned to batch.
            if outcomes.len() == 1 {
                let outcome = outcomes.pop().expect("len == 1");
                output::emit(json, &outcome, |o: &lease::AcquireOutcome| {
                    format!(
                        "{} lease on {} for {}",
                        acquire_verb(o),
                        o.lease.path,
                        o.lease.agent
                    )
                });
            } else {
                output::emit(json, &outcomes, |os: &Vec<lease::AcquireOutcome>| {
                    format!(
                        "took {} lease(s) for {agent}:\n{}",
                        os.len(),
                        os.iter()
                            .map(|o| format!("  {} {}", acquire_verb(o), o.lease.path))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                });
            }
            // After the success line, never before it: what happened first,
            // then what you should know.
            for line in prior {
                output::warn(&line);
            }
            for line in messages_about(&root, &paths, &agent) {
                output::warn(&line);
            }
            Ok(())
        }
        LeaseAction::Renew { path } => {
            let agent = identity::resolve_agent(agent_flag)?;
            let renewed = lease::renew(&root, &agent, &path)?;
            output::emit(json, &renewed, |l: &lease::LeaseInfo| {
                format!(
                    "renewed lease on {} for {} ({} ttl)",
                    l.path,
                    l.agent,
                    human_secs(l.ttl_secs as i64)
                )
            });
            Ok(())
        }
        LeaseAction::Release { path, force, all } => {
            let agent = identity::resolve_agent(agent_flag)?;
            if all {
                let released = lease::release_all(&root, &agent)?;
                output::emit(json, &released, |paths: &Vec<String>| {
                    if paths.is_empty() {
                        format!("{agent} held no leases")
                    } else {
                        format!(
                            "released {} lease(s):\n{}",
                            paths.len(),
                            paths
                                .iter()
                                .map(|p| format!("  {p}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    }
                });
                return Ok(());
            }
            // clap enforces `path` unless `--all`, which returned above.
            let path = path.expect("clap requires <path> unless --all");
            let displaced = lease::release(&root, &agent, &path, force)?;
            if let Some(who) = &displaced {
                // pact-rnc.11: overriding someone else's claim is loud in the
                // `acquire --steal` direction; make it loud here too.
                output::warn(&format!(
                    "warning: force-released {path} — destroyed {who}'s live claim; \
                     they were not notified (`pact msg send --to {who}`)"
                ));
            }
            // pact-rnc.25: the displaced holder used to exist only in that
            // stderr prose, so `--json` callers — the scripted ones, the ones
            // that most need to go apologise — could not see whose claim they
            // destroyed. This changes `release --json` from a bare string to
            // an object; the human line is unchanged.
            let released = Released { path, displaced };
            output::emit(json, &released, |r: &Released| {
                format!("released lease on {}", r.path)
            });
            Ok(())
        }
        LeaseAction::Ls { all } => {
            let entries = lease::list(&root, all)?;
            // `--all` means "show me everything pact knows about paths", and
            // until now a released path vanished completely: `src/doctor.rs`
            // blocked two agents in sequence because nothing distinguished it
            // from a file nobody had ever opened (pact-o38).
            //
            // Human output only, deliberately. `lease ls --json` is an array of
            // LeaseEntry, a shape pact-er0 just pinned, and a released path has
            // no lock file to describe — synthesizing a LeaseEntry with an
            // invented ttl would be a lie in a typed field. `pact agents --for
            // <path>` is the scriptable answer.
            let released = if all {
                released_paths(&root, &entries)
            } else {
                Vec::new()
            };
            output::emit(json, &entries, |entries: &Vec<lease::LeaseEntry>| {
                let mut out = render_leases(entries);
                if !released.is_empty() {
                    out.push_str("\n\nrecently released (no lease held, last owner known):\n");
                    out.push_str(&released.join("\n"));
                }
                out
            });
            Ok(())
        }
    }
}

fn run_msg(cwd: &Path, agent_flag: Option<&str>, json: bool, action: MsgAction) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    let cli = beads::BeadsCli::locate()?;
    let agent = identity::resolve_agent(agent_flag)?;
    match action {
        MsgAction::Send {
            to,
            to_owner_of,
            thread,
            subject,
            body,
            body_file,
        } => {
            let body = match body_file {
                Some(p) => read_body(&p)?,
                None => body.unwrap_or_default(),
            };
            if body.trim().is_empty() {
                anyhow::bail!("empty message body — nothing to send");
            }
            // Resolve paths to the agents who last worked on them. This is
            // what makes a handoff survive its author: 51 of 59 messages in one
            // fleet run were never read, because they were addressed to
            // processes that had already exited rather than to the work itself
            // (pact-o38). A path outlives the agent holding it.
            let mut to = to;
            for path in &to_owner_of {
                match events::owner_of(&root, path)? {
                    Some(owner) if owner.agent == agent => {
                        output::warn(&format!(
                            "note: you are yourself the last agent to work on {path}; not adding a recipient"
                        ));
                    }
                    Some(owner) => {
                        // Say who the path resolved to and how stale they are.
                        // A resolved name looks like a delivered message, and
                        // it is not: every message to a live agent in the last
                        // fleet run was read, every message to an exited one
                        // was not. One agent worked around this by hand-adding
                        // `--to human` to all three of its sends; they were the
                        // only one who thought of it (pact-4tj).
                        let ago = age_of(&owner.at)
                            .map(human_secs)
                            .unwrap_or_else(|| "an unknown time".to_string());
                        let stale = age_of(&owner.at).is_some_and(|s| s > QUIET_AGENT_SECS);
                        output::warn(&format!(
                            "note: {path} resolved to {}, last seen {ago} ago{}",
                            owner.agent,
                            if stale {
                                " — they may have exited; whoever leases that path next                                  will be shown this message"
                            } else {
                                ""
                            }
                        ));
                        if !to.contains(&owner.agent) {
                            to.push(owner.agent);
                        }
                    }
                    None => anyhow::bail!(
                        "no agent has ever leased {path}, so it has no owner to address — \
                         `pact lease ls --all` lists every path pact knows"
                    ),
                }
            }
            if to.is_empty() {
                anyhow::bail!("no recipients resolved — nothing to send");
            }
            for recipient in &to {
                check_recipient(recipient)?;
            }
            // One registry lookup for all recipients, not one per --to.
            warn_if_unknown(&cli, &root, &to);
            let sent = msg::send(
                &cli,
                &root,
                &agent,
                &to,
                msg::Draft {
                    thread: thread.as_deref(),
                    subject: subject.as_deref(),
                    body: &body,
                    about: &to_owner_of,
                },
            )?;
            output::emit(json, &sent, |sent: &Vec<msg::Message>| {
                let root_msg = &sent[0]; // send() errors on an empty recipient list
                if sent.len() == 1 {
                    return format!(
                        "sent {} to {} (thread {})",
                        root_msg.id, root_msg.to, root_msg.thread
                    );
                }
                // The thread id ONCE, not once per recipient: the whole point
                // of pact-rnc.4 is that this is one conversation, so `msg read
                // <thread>` shows the announcement instead of N near-duplicates.
                format!(
                    "sent {} message(s) in thread {}\n{}",
                    sent.len(),
                    root_msg.thread,
                    sent.iter()
                        .map(|m| format!("  {} → {}", m.id, m.to))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            });
            Ok(())
        }
        MsgAction::Sent => {
            let messages = msg::sent(&cli, &root, &agent)?;
            output::emit(json, &messages, |messages: &Vec<msg::Message>| {
                if messages.is_empty() {
                    format!("{agent} has sent nothing yet")
                } else {
                    render_sent(messages)
                }
            });
            Ok(())
        }
        MsgAction::Inbox { unread_only, full } => {
            let messages = msg::inbox(&cli, &root, &agent, unread_only)?;
            output::emit(json, &messages, |messages: &Vec<msg::Message>| {
                if messages.is_empty() {
                    "inbox empty".to_string()
                } else if full {
                    render_full(messages)
                } else {
                    render_inbox(messages)
                }
            });
            Ok(())
        }
        MsgAction::Read { id } => {
            let thread = msg::read_thread(&cli, &root, &agent, &id)?;
            output::emit(json, &thread, |thread: &Vec<msg::Message>| {
                render_full(thread)
            });
            Ok(())
        }
    }
}

/// One row of the activity feed. Deliberately one flat shape for both sources,
/// because the question `pact log` answers — "is the fleet alive and what is it
/// doing" — does not care which storage a fact came from.
#[derive(serde::Serialize)]
struct LogEvent {
    at: String,
    agent: String,
    kind: String,
    /// The leased path, or the recipient of a message.
    target: Option<String>,
    detail: Option<String>,
}

/// pact-rnc.13: lease events and messages in ONE chronological stream, so
/// nobody has to `ls .pact/leases/` and `bd list --json | jq` to find out what
/// happened — the anti-pattern docs/architecture.md warns against.
///
/// The two halves have different histories and that is fine, not an error:
/// messages are derived from bd, so they go back as far as the repo does, while
/// `.pact/events.jsonl` only starts when a lease was first taken after this
/// feature shipped. An existing repo therefore shows message history with no
/// lease history until the next acquire; an empty (or missing) feed is normal.
/// bd is optional the same way it is for `pact agents`: without it you still
/// get the lease half, with a warning.
fn run_log(cwd: &Path, json: bool, limit: usize) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;

    let mut feed: Vec<LogEvent> = events::recent(&root, limit)?
        .into_iter()
        .map(|e| LogEvent {
            at: e.at,
            agent: e.agent,
            kind: e.kind,
            target: e.path,
            detail: e.detail,
        })
        .collect();

    match beads::BeadsCli::locate().and_then(|cli| msg::all_messages(&cli, &root)) {
        Ok(messages) => feed.extend(messages.into_iter().map(|m| LogEvent {
            at: m.created_at,
            agent: m.from,
            kind: "message".to_string(),
            target: Some(m.to),
            detail: Some(m.subject.unwrap_or(m.body)),
        })),
        Err(e) => output::warn(&format!("warning: message history unavailable: {e:#}")),
    }

    // Parsed instants, not string order: bd stamps end in `Z` and pact's in
    // `+00:00`, which sort differently as bytes than as time (pact-rnc.20).
    feed.sort_by_key(|e| instant(&e.at));
    if feed.len() > limit {
        feed.drain(..feed.len() - limit);
    }

    output::emit(json, &feed, |feed: &Vec<LogEvent>| render_log(feed));
    Ok(())
}

fn run_doctor(cwd: &Path, json: bool) -> Result<i32> {
    // Without a repo root none of the other checks mean anything, so this one
    // is a hard prerequisite rather than a soft check: propagate its exit
    // code (4) straight through instead of folding it into the report.
    let root = repo::find_repo_root(cwd)?;
    let report = doctor::checks(&root);

    output::emit(json, &report, |r| {
        let mut lines: Vec<String> = r
            .checks
            .iter()
            .map(|c| {
                let glyph = match (c.ok, c.warn) {
                    (false, _) => "✗",
                    (true, true) => "!",
                    (true, false) => "✓",
                };
                format!("{glyph} {}: {}", c.name, c.detail)
            })
            .collect();
        let warnings = r.checks.iter().filter(|c| c.warn).count();
        lines.push(String::new());
        lines.push(match (r.healthy, warnings) {
            (false, _) => "some checks failed".to_string(),
            (true, 0) => "all checks passed".to_string(),
            // Named in the summary too: a `!` scrolled off the top of a long
            // report is a warning nobody saw.
            (true, 1) => "all checks passed, 1 warning".to_string(),
            (true, n) => format!("all checks passed, {n} warnings"),
        });
        lines.join("\n")
    });

    // Handed back to `main` rather than exited here: `std::process::exit`
    // skips every destructor, so the failing run — the only one anybody
    // troubleshoots — used to export no trace at all. The code itself is
    // unchanged; exit codes are API.
    Ok(if report.healthy { 0 } else { 1 })
}

/// `-` means stdin, so a multi-paragraph body full of quotes and backslashes
/// never has to survive a shell (pact-rnc.3).
fn read_body(path: &str) -> Result<String> {
    let raw = if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .context("reading message body from stdin")?;
        s
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading message body from {path}"))?
    };
    // A file ends in a newline; that is punctuation, not content. Exactly one,
    // though: `trim_end()` ate trailing blank lines out of a deliberately
    // formatted body — an ASCII table, an indented code block — and --body-file
    // exists to promise byte fidelity (pact-rnc.25). An all-whitespace body is
    // still refused, by the caller's `body.trim().is_empty()` check.
    Ok(raw.strip_suffix('\n').unwrap_or(&raw).to_string())
}

/// A recipient that violates pact's own identity grammar is not merely unseen,
/// it is impossible: no process could ever pass that name to `--agent` or
/// `PACT_AGENT`, so the message can never be read by anyone. Refuse it, instead
/// of warning about something no send will ever fix (pact-rnc.5).
fn check_recipient(to: &str) -> Result<()> {
    identity::validate(to).with_context(|| {
        format!(
            "cannot send to {to:?}: no agent can run pact under that name, so nobody could read it"
        )
    })
}

/// pact-rnc.5: warn on stderr, then send anyway and exit 0. A bootstrapping
/// fleet legitimately messages agents that have not acted yet, so this must
/// never become a wall — and a lookup that only feeds a warning must never
/// break a send, hence the swallowed error.
fn warn_if_unknown(cli: &beads::BeadsCli, root: &Path, to: &[String]) {
    let known = agents::list(Some(cli), root).unwrap_or_default();
    for recipient in to {
        if let Some(warning) = unknown_recipient_warning(&known, recipient) {
            output::warn(&warning);
        }
    }
}

/// The warning text, or `None` when `to` has acted here. Split out from the
/// stderr write so the two ways this check used to go quiet — a name that was
/// only ever addressed, and an empty registry on a fleet's first send, which is
/// precisely when the protocol says to send — stay pinned by a test.
fn unknown_recipient_warning(known: &[agents::AgentInfo], to: &str) -> Option<String> {
    if agents::is_known(known, to) {
        return None;
    }
    let hits = agents::suggest(known, to);
    let did_you_mean = if hits.is_empty() {
        String::new()
    } else {
        format!(" — did you mean {}?", hits.join(", "))
    };
    Some(format!(
        "warning: no agent named {to:?} has acted in this repo \
         (no lease, no message sent){did_you_mean} (sending anyway)"
    ))
}

/// pact-rnc.10: lead with age and an explicit state. `remaining_secs` is a
/// crash-recovery ceiling, not a duration of work — printing it next to a
/// seconds-old lease read as "this long lease" and got a live agent's claim
/// force-released, so it only appears when it says something actionable. The
/// note comes along because "what is this agent doing" is the question an
/// operator is actually asking before they reach for --force.
/// Paths the event log remembers that have no lock on disk right now.
fn released_paths(root: &Path, held: &[lease::LeaseEntry]) -> Vec<String> {
    events::owners(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|(path, _)| !held.iter().any(|e| e.lease.path == *path))
        .map(|(path, owner)| {
            let ago = age_of(&owner.at)
                .map(|s| format!("{} ago", human_secs(s)))
                .unwrap_or_else(|| owner.at.clone());
            format!("  {path}  {} by {} ({ago})", owner.kind, owner.agent)
        })
        .collect()
}

fn render_leases(entries: &[lease::LeaseEntry]) -> String {
    if entries.is_empty() {
        return "no active leases".to_string();
    }
    let mut rows = vec![vec![
        "PATH".to_string(),
        "AGENT".to_string(),
        "HELD".to_string(),
        "STATE".to_string(),
        "NOTE".to_string(),
    ]];
    rows.extend(entries.iter().map(|e| {
        vec![
            e.lease.path.clone(),
            e.lease.agent.clone(),
            human_secs(e.age_secs),
            e.state_label(),
            one_line(e.lease.note.as_deref().unwrap_or(""), 60),
        ]
    }));
    table(&rows)
}

/// Whether a lease was taken from a live holder or simply claimed. The single
/// path wording is unchanged from before batching, so scripts and eyes that
/// grew up on "acquired lease on X for Y" still read it.
fn acquire_verb(o: &lease::AcquireOutcome) -> &'static str {
    if o.stolen {
        "stolen"
    } else {
        "acquired"
    }
}

/// `lease release --json`: the path, plus whoever's live claim `--force`
/// destroyed (pact-rnc.25).
#[derive(serde::Serialize)]
struct Released {
    path: String,
    displaced: Option<String>,
}

/// Sortable instant. Unparsable stamps sort oldest, so a corrupt line ends up
/// out of the way instead of pretending to be the latest news.
fn instant(rfc3339: &str) -> (i64, u32) {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => (t.timestamp(), t.timestamp_subsec_nanos()),
        Err(_) => (i64::MIN, 0),
    }
}

/// The feed, oldest last — a log reads top-to-bottom, newest at the bottom
/// where a terminal leaves the cursor. Ages rather than timestamps, because the
/// question is "is this happening now" (same reasoning as `pact agents`).
fn render_log(feed: &[LogEvent]) -> String {
    if feed.is_empty() {
        return "no activity recorded yet".to_string();
    }
    let mut rows = vec![vec![
        "WHEN".to_string(),
        "AGENT".to_string(),
        "EVENT".to_string(),
        "TARGET".to_string(),
        "DETAIL".to_string(),
    ]];
    rows.extend(feed.iter().map(|e| {
        vec![
            since(&e.at),
            e.agent.clone(),
            e.kind.clone(),
            e.target.clone().unwrap_or_default(),
            one_line(e.detail.as_deref().unwrap_or(""), 50),
        ]
    }));
    format!("{}\n\n{} event(s), oldest first", table(&rows), feed.len())
}

/// pact-rnc.7: the outbox. Same shape as the inbox with TO instead of FROM, and
/// the marker means something different and more useful: whether the *recipient*
/// has read it (pact-rnc.17's shared read state). An agent that cannot confirm a
/// send re-sends it — that is where the fleet's duplicate messages came from.
fn render_sent(messages: &[msg::Message]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        String::new(), // unread marker; a header would be wider than the column
        "TO".to_string(),
        "SUBJECT".to_string(),
        "BODY".to_string(),
    ]];
    rows.extend(messages.iter().map(|m| {
        vec![
            m.id.clone(),
            if read_by_recipient(m) { " " } else { "*" }.to_string(),
            m.to.clone(),
            one_line(m.subject.as_deref().unwrap_or(""), 50),
            one_line(&m.body, 60),
        ]
    }));
    let unread = messages.iter().filter(|m| !read_by_recipient(m)).count();
    format!(
        "{}\n\n{} message(s), {unread} not read yet (*) by the recipient",
        table(&rows),
        messages.len(),
    )
}

/// `Message.read` is read-by-*me*, which is always true for something I sent.
/// The sender's question is whether the person they told has looked.
fn read_by_recipient(m: &msg::Message) -> bool {
    m.read_by.contains(&m.to)
}

/// pact-rnc.1 + pact-rnc.2: one line per message with the sender and an unread
/// marker. Seven messages used to print ~9KB of full bodies, which burned an
/// agent's context on every check and made `msg read` pointless.
fn render_inbox(messages: &[msg::Message]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        String::new(), // unread marker; a header would be wider than the column
        "FROM".to_string(),
        "SUBJECT".to_string(),
        "BODY".to_string(),
    ]];
    rows.extend(messages.iter().map(|m| {
        vec![
            m.id.clone(),
            if m.read { " " } else { "*" }.to_string(),
            if m.from.is_empty() { "?" } else { &m.from }.to_string(),
            one_line(m.subject.as_deref().unwrap_or(""), 50),
            one_line(&m.body, 60),
        ]
    }));
    let unread = messages.iter().filter(|m| !m.read).count();
    format!(
        "{}\n\n{} message(s), {unread} unread (*) — `pact msg read <id>` for the full text",
        table(&rows),
        messages.len(),
    )
}

/// Full text with the envelope pact used to throw away: from, to, subject, time
/// (pact-rnc.1). Shared by `msg read` and `msg inbox --full`, so a sender can
/// finally read their own message back with its metadata.
fn render_full(messages: &[msg::Message]) -> String {
    messages
        .iter()
        .map(|m| {
            format!(
                "[{}] from: {}  to: {}{}\nsubject: {}\nat: {}  thread: {}\n\n{}",
                m.id,
                if m.from.is_empty() { "?" } else { &m.from },
                m.to,
                if m.read { "" } else { "  (unread)" },
                m.subject.as_deref().unwrap_or("(none)"),
                m.created_at,
                m.thread,
                m.body,
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// Pad every column but the last so a listing lines up without tabs. Listings
/// here are a handful of rows, so a two-pass width scan is plenty.
fn table(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..rows.iter().map(Vec::len).max().unwrap_or(0))
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == row.len() {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}", width = widths[i])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// "4m2s ago" answers the question `pact agents` is asked — is this identity
/// live right now, or archaeology? An RFC3339 stamp does not.
fn since(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => format!(
            "{} ago",
            human_secs((chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        ),
        Err(_) => rfc3339.to_string(),
    }
}

/// Collapse to a single line and cap at `max` chars. An inbox row must never
/// wrap or leak a multi-paragraph body (pact-rnc.2).
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!(
            "{}…",
            flat.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exit 5 is documented API and the protocol block tells agents to branch
    /// on it, but it produced no telemetry at all — `otel::init` runs after
    /// `Cli::try_parse`, so the clap-error path exported nothing: no span, no
    /// duration, no `pact.exit_code`. Splitting the verdict out of `main` is
    /// what makes both halves — the code and the span name that carries it —
    /// assertable without spawning a process.
    #[test]
    fn a_clap_error_is_a_usage_error_with_a_shape_to_trace_it_by() {
        use clap::error::ErrorKind::*;
        assert_eq!(clap_outcome(DisplayHelp), (0, "help"));
        assert_eq!(clap_outcome(DisplayVersion), (0, "help"));
        assert_eq!(
            clap_outcome(InvalidSubcommand),
            (USAGE_ERROR, "usage-error")
        );
        assert_eq!(clap_outcome(UnknownArgument), (USAGE_ERROR, "usage-error"));
        assert_eq!(clap_outcome(InvalidValue), (USAGE_ERROR, "usage-error"));
        // The one that made bare `pact` exit 0 before pact-rnc: clap prints
        // help, but nobody asked for it.
        assert_eq!(
            clap_outcome(DisplayHelpOnMissingArgumentOrSubcommand),
            (USAGE_ERROR, "usage-error")
        );
    }

    fn lease_entry(
        path: &str,
        agent: &str,
        age: i64,
        remaining: i64,
        expired: bool,
    ) -> lease::LeaseEntry {
        lease::LeaseEntry {
            lease: lease::LeaseInfo {
                agent: agent.to_string(),
                path: path.to_string(),
                acquired_at: "2026-07-31T09:00:00+00:00".to_string(),
                ttl_secs: 900,
                note: Some("wiring the CLI".to_string()),
            },
            age_secs: age,
            remaining_secs: remaining,
            expired,
        }
    }

    fn message(id: &str, from: &str, body: &str, read: bool) -> msg::Message {
        msg::Message {
            id: id.to_string(),
            thread: id.to_string(),
            from: from.to_string(),
            to: "cli-wire".to_string(),
            subject: Some("a subject".to_string()),
            body: body.to_string(),
            created_at: "2026-07-31T09:00:00Z".to_string(),
            read,
            read_by: if read {
                vec!["cli-wire".to_string()]
            } else {
                Vec::new()
            },
        }
    }

    /// The pact-rnc.10 incident verbatim: an 80s-old healthy lease must not
    /// render a four-digit countdown that reads as "this long lease".
    #[test]
    fn lease_ls_leads_with_age_and_state_not_remaining_ttl() {
        let out = render_leases(&[lease_entry("docs/m.md", "animator", 80, 3520, false)]);
        assert!(out.contains("1m20s"), "{out}");
        assert!(out.contains("active"), "{out}");
        assert!(!out.contains("3520"), "remaining ttl must not lead: {out}");
        assert!(
            out.contains("wiring the CLI"),
            "note is the operator's context: {out}"
        );
    }

    #[test]
    fn lease_ls_says_when_a_stale_lease_becomes_reclaimable() {
        let stale = render_leases(&[lease_entry("a.txt", "gone", 910, -10, false)]);
        assert!(stale.contains("stale (reclaimable in 20s)"), "{stale}");
        let expired = render_leases(&[lease_entry("a.txt", "gone", 9000, -8000, true)]);
        assert!(expired.contains("expired"), "{expired}");
        assert!(!expired.contains("reclaimable"), "{expired}");
    }

    /// pact-rnc.1 + pact-rnc.2: sender, unread marker, one line per message.
    #[test]
    fn inbox_shows_from_and_an_unread_marker_on_one_line_each() {
        let body = "para one\n\npara two with \"quotes\"\n".repeat(40);
        let out = render_inbox(&[
            message("pact-wisp-aaa", "msg-fix", &body, false),
            message("pact-wisp-bbb", "lease-fix", "short", true),
        ]);
        let rows: Vec<&str> = out.lines().take(3).collect();
        assert_eq!(rows.len(), 3, "header + one line per message: {out}");
        assert!(
            rows[1].contains("msg-fix") && rows[1].contains('*'),
            "{}",
            rows[1]
        );
        assert!(
            rows[2].contains("lease-fix") && !rows[2].contains('*'),
            "{}",
            rows[2]
        );
        assert!(
            rows[1].chars().count() < 200,
            "row must not be a wall: {}",
            rows[1]
        );
        assert!(out.contains("1 unread"), "{out}");
    }

    #[test]
    fn full_render_carries_the_envelope() {
        let out = render_full(&[message("pact-wisp-aaa", "msg-fix", "the body", true)]);
        assert!(out.contains("from: msg-fix"), "{out}");
        assert!(out.contains("to: cli-wire"), "{out}");
        assert!(out.contains("subject: a subject"), "{out}");
        assert!(out.contains("the body"), "{out}");
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("a\n\nb  c", 40), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
        // Multi-byte input must not panic or split a char.
        assert_eq!(one_line("héllo wörld", 6), "héllo…");
    }

    fn agent_info(name: &str, leases: usize, sent: usize, received: usize) -> agents::AgentInfo {
        agents::AgentInfo {
            name: name.to_string(),
            last_seen: "2026-07-31T09:00:00Z".to_string(),
            leases_held: leases,
            messages_sent: sent,
            messages_received: received,
        }
    }

    /// pact-rnc.5, the bead's own tui-dev/tuidev incident: the second send to a
    /// typo must warn exactly as loudly as the first, and the correction offered
    /// must be a name somebody answers to.
    #[test]
    fn a_typod_recipient_warns_every_time_not_once() {
        let known = [
            agent_info("tui-dev", 1, 0, 0),
            agent_info("tuidev", 0, 0, 2),
        ];

        let warning = unknown_recipient_warning(&known, "tuidev").expect("must still warn");
        assert!(warning.contains("did you mean tui-dev?"), "{warning}");
        assert!(unknown_recipient_warning(&known, "tui-dev").is_none());
        // The operator's mailbox is reserved by the protocol, not earned.
        assert!(unknown_recipient_warning(&known, agents::HUMAN).is_none());
    }

    /// The cold-start hole: `pact msg send` comes *before* `pact lease acquire`
    /// in the protocol, so the first sender legitimately has no trace — which is
    /// exactly when a typo used to ship silently.
    #[test]
    fn an_empty_registry_still_warns() {
        let warning = unknown_recipient_warning(&[], "alic").expect("cold start must warn");
        assert!(warning.contains("alic"), "{warning}");
        assert!(!warning.contains("did you mean"), "nobody to suggest yet");
    }

    /// A name pact's own grammar rejects can never be an agent, so warning about
    /// it is pointless: the message would be unreadable forever.
    #[test]
    fn an_impossible_recipient_is_refused_not_warned() {
        assert!(check_recipient("Not A Valid Agent").is_err());
        assert!(check_recipient("tui-dev").is_ok());
        assert!(check_recipient("human").is_ok());
    }

    /// pact-rnc.12: `bd --version` passes in a repo with no beads database, the
    /// exact repo where every bd-backed pact command exits 1. `git` stands in for
    /// that bd: it answers `--version` and cannot answer the real query.
    #[test]
    fn bd_health_probes_the_workspace_not_just_the_binary() {
        let root = std::env::current_dir().unwrap();
        let (version, problems) = bd_health(&beads::BeadsCli { binary: "git" }, &root);
        assert!(
            version.is_some(),
            "--version answered, so bd looks installed"
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("beads database"), "{problems:?}");
    }

    /// pact-rnc.25: --body-file promises byte fidelity, so trailing blank lines
    /// inside a deliberately formatted body are content. Exactly one newline
    /// comes off — the one a file or heredoc ends with.
    #[test]
    fn body_file_strips_one_trailing_newline_not_all_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("body.md");
        let table = "| a | b |\n|---|---|\n| 1 | 2 |\n\n\n";
        std::fs::write(&path, table).unwrap();

        let got = read_body(path.to_str().unwrap()).unwrap();
        assert_eq!(got, "| a | b |\n|---|---|\n| 1 | 2 |\n\n");
        // Trailing spaces are content too (indented code blocks).
        std::fs::write(&path, "x\n    \n").unwrap();
        assert_eq!(read_body(path.to_str().unwrap()).unwrap(), "x\n    ");
        // A body with no trailing newline at all is left alone.
        std::fs::write(&path, "no newline").unwrap();
        assert_eq!(read_body(path.to_str().unwrap()).unwrap(), "no newline");
        // And an all-whitespace body still has nothing in it to send: the
        // send path rejects on `body.trim().is_empty()`, which this satisfies.
        std::fs::write(&path, "\n\n  \n").unwrap();
        assert!(read_body(path.to_str().unwrap()).unwrap().trim().is_empty());
    }

    fn log_event(at: &str, agent: &str, kind: &str, target: &str) -> LogEvent {
        LogEvent {
            at: at.to_string(),
            agent: agent.to_string(),
            kind: kind.to_string(),
            target: Some(target.to_string()),
            detail: Some("wiring the CLI".to_string()),
        }
    }

    /// pact-rnc.13 + pact-rnc.20: the feed merges two sources that stamp time
    /// differently — bd writes `Z`, pact writes `+00:00` — and a byte compare
    /// interleaves them wrongly. `+02:00` is the trap: its digits are the
    /// largest while its instant is the earliest.
    #[test]
    fn log_merges_both_sources_in_real_time_order() {
        let mut feed = vec![
            log_event("2026-07-31T09:00:05Z", "msg-fix", "message", "cli-wire"),
            // 08:59:00Z — the earliest instant, but the largest byte string.
            log_event("2026-07-31T10:59:00+02:00", "lease-fix", "acquired", "l.rs"),
            log_event("2026-07-31T09:00:00+00:00", "cli-wire", "acquired", "m.rs"),
        ];
        feed.sort_by_key(|e| instant(&e.at));
        let order: Vec<&str> = feed.iter().map(|e| e.agent.as_str()).collect();
        assert_eq!(order, vec!["lease-fix", "cli-wire", "msg-fix"]);
        let mut by_bytes = feed.iter().map(|e| e.at.clone()).collect::<Vec<_>>();
        by_bytes.sort();
        assert_ne!(
            by_bytes.first().map(String::as_str),
            Some(feed[0].at.as_str()),
            "a string sort really does produce a different order here"
        );
        assert!(
            instant("not a timestamp") < instant("2026-07-31T09:00:00Z"),
            "garbage sorts out of the way, not to the top of the news"
        );

        let out = render_log(&feed);
        let rows: Vec<&str> = out.lines().take(4).collect();
        assert!(
            rows[1].contains("lease-fix") && rows[1].contains("acquired"),
            "{out}"
        );
        assert!(
            rows[3].contains("message") && rows[3].contains("cli-wire"),
            "{out}"
        );
        assert!(out.contains("3 event(s)"), "{out}");
        // An existing repo has message history and an empty lease feed; that
        // is normal, not an error.
        assert_eq!(render_log(&[]), "no activity recorded yet");
    }

    /// pact-rnc.7 + pact-rnc.17: the outbox exists so a sender can stop
    /// guessing. `read` is read-by-me and always true for my own sends, so the
    /// marker has to come from the recipient's own read-by label.
    #[test]
    fn sent_shows_the_recipient_and_whether_they_read_it() {
        let mut read_by_them = message("pact-wisp-aaa", "cli-wire", "the body", true);
        read_by_them.to = "lease-fix".to_string();
        read_by_them.read_by = vec!["lease-fix".to_string()];
        let mut unread_by_them = message("pact-wisp-bbb", "cli-wire", "the body", true);
        unread_by_them.to = "msg-fix".to_string();
        unread_by_them.read_by = vec!["cli-wire".to_string()];

        let out = render_sent(&[read_by_them, unread_by_them]);
        let rows: Vec<&str> = out.lines().take(3).collect();
        assert!(
            rows[1].contains("lease-fix") && !rows[1].contains('*'),
            "{out}"
        );
        assert!(
            rows[2].contains("msg-fix") && rows[2].contains('*'),
            "{out}"
        );
        assert!(out.contains("1 not read yet"), "{out}");
    }

    /// pact-aw7.2. The span name is the one attribute a backend cannot drop
    /// later, so nothing that came from argv may reach it: a path in a span
    /// name is unbounded cardinality in any real repo, and a lease note is
    /// user free text we have no business exporting. This is the check that
    /// fails the day someone reaches for `format!` here.
    #[test]
    fn a_span_name_never_carries_argv() {
        let acquire = Command::Lease {
            action: LeaseAction::Acquire {
                paths: vec!["src/secret.rs".to_string()],
                ttl: 900,
                steal: false,
                note: Some("rewriting the auth module".to_string()),
            },
        };
        let send = Command::Msg {
            action: MsgAction::Send {
                to: vec!["cli-wire".to_string()],
                to_owner_of: vec!["src/secret.rs".to_string()],
                thread: None,
                subject: Some("secret".to_string()),
                body: Some("the body".to_string()),
                body_file: None,
            },
        };
        for command in [&acquire, &send] {
            let name = subcommand_name(command);
            assert!(!name.contains("secret.rs"), "{name} leaks a path");
            assert!(!name.contains("cli-wire"), "{name} leaks an agent name");
            assert!(!name.contains("auth"), "{name} leaks a lease note");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == ' '),
                "{name} is not a bare argv shape"
            );
        }
        // Down to the action, or the histogram cannot tell `lease ls` (a file
        // read) from `lease acquire` (a write plus an event-log append).
        assert_eq!(subcommand_name(&acquire), "lease acquire");
        assert_eq!(subcommand_name(&send), "msg send");
        assert_eq!(subcommand_name(&Command::Doctor), "doctor");
    }

    #[test]
    fn table_pads_all_but_the_last_column() {
        let out = table(&[
            vec!["a".into(), "x".into()],
            vec!["longer".into(), "y".into()],
        ]);
        assert_eq!(out, "a       x\nlonger  y");
    }
}
