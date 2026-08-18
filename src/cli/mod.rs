#[cfg(feature = "mcp")]
use crate::mcp;
#[cfg(feature = "ui")]
use crate::tui;
use crate::{agents, audit, events, identity, lease, msg, output, repo};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use lease::human_secs;

mod commands;
mod util;

use commands::{
    run_agents, run_audit, run_completion, run_context_set, run_doctor, run_init, run_lease,
    run_log, run_merge, run_plan_lint, run_watch, run_whoami, AuditArgs,
};

use util::{age_of, table};
pub(crate) use util::{one_line, since};

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
pub(crate) struct Cli {
    /// Agent identity; falls back to PACT_AGENT if unset.
    #[arg(long, global = true)]
    pub(crate) agent: Option<String>,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
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
        /// Write through a live lease another agent holds on AGENTS.md,
        /// CLAUDE.md, or another managed instruction file. Without it, `init`
        /// refuses (exit 2) rather than overwriting a file someone else is
        /// mid-edit on — the same override every other takeover in pact needs
        /// an explicit flag for.
        #[arg(long)]
        force: bool,
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
    /// Merge a branch under the merge mutex, prove it, and release.
    ///
    /// The self-merge sequence a fleet with no orchestrator has to run by hand,
    /// as one auditable command: take the reserved key, merge with `--no-ff`,
    /// sign the merge commit with `Pact-Agent` (which `git merge` cannot do on
    /// its own), run `--verify`, and release. A failing verification reverts the
    /// merge and DELIBERATELY keeps the mutex, so no peer merges onto a branch
    /// that has just failed its own oracle. See docs/fleet-patterns.md.
    Merge {
        /// The branch to merge into the current one.
        branch: String,
        /// Command that proves the merge, run from the repository root after it
        /// lands — typically the test suite. Omitted means nothing proved it,
        /// which is reported rather than assumed to pass.
        #[arg(long)]
        verify: Option<String>,
        /// How long to hold the merge mutex.
        ///
        /// Deliberately shorter than the 45-minute default for a file lease. A
        /// merge hold is bounded by a test run, not by a task: measured across
        /// eight self-merges the median was 37s and the longest 64s. Half an
        /// hour is already two orders of magnitude of headroom, and a shorter
        /// TTL means a fleet blocked behind a crashed merger recovers in
        /// minutes rather than three quarters of an hour.
        #[arg(long, default_value = "30m")]
        ttl: String,
        /// Merge even though the working tree has uncommitted tracked changes.
        ///
        /// Coordination state under `.pact/`, `.beads/` and `.harness/` is
        /// already exempt and is preserved across a failed verification's
        /// revert, so this is only for real work you have decided is safe to
        /// lose to the hard reset that revert performs.
        #[arg(long)]
        allow_dirty: bool,
    },
    /// Threaded messages between agents, in pact's own append-only store.
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
    /// Print a shell completion script for pact's commands and flags.
    ///
    /// Generated from the same command tree clap parses, so it cannot drift
    /// out of step with the binary the way a hand-written script would.
    /// Writes to stdout; see docs/cli.md for where each shell wants it.
    Completion {
        /// The shell whose syntax to emit. One clap cannot generate is a
        /// usage error, not an empty script.
        // Deliberately not a list: clap prints `[possible values: ...]` from
        // `clap_complete::Shell` itself, and a second copy here is one more
        // thing to forget when that enum grows.
        shell: clap_complete::Shell,
    },
    /// Record the constraints this run operates under, in the same log as its
    /// behaviour.
    ///
    /// Behaviour events say what a fleet did; they cannot say whether it was a
    /// choice or an instruction, and a reader with only half the record will
    /// supply a mechanism for something that was simply ordered. See
    /// docs/audit.md, "what the log cannot tell you".
    Context {
        #[command(subcommand)]
        action: ContextAction,
    },
    /// Check a wave plan before you spawn a fleet.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Check that pact, AGENTS.md, and the Beads CLI are all in a healthy state.
    Doctor {
        /// Repair what pact owns: the managed block, the files that must point
        /// at it, the ignore rules that decide whether the logs reach a clone,
        /// and pact's own staging debris.
        ///
        /// Every write is `pact init`'s, so the two produce the same
        /// repository. Never commits, and refuses the checks pact does not own
        /// — a corrupt lease, another tool's block, a symlink escaping the
        /// repository — naming each and why.
        #[arg(long)]
        fix: bool,
    },
    /// Subscribe to paths, and be sent the diff when a holder releases one.
    ///
    /// A registry, not a watcher: nothing runs in the background and nothing
    /// waits. `pact lease release` looks the subscriptions up and sends the
    /// messages as a side effect, so you receive them at your next
    /// `pact msg inbox`. See docs/watch.md.
    Watch {
        #[command(subcommand)]
        action: WatchAction,
    },
    /// Analyse this repo's coordination history in `.pact/events.jsonl`.
    ///
    /// Reads `.pact/` and, for `--check commit-correlation` or `--export`,
    /// this repository's own git history — never the Beads store. Exits 1
    /// when a check finds something, 0 when it does not.
    ///
    /// `--check double-win` is the detector for the guard-file backlog item
    /// (pact-ehi), whose written trigger condition is "implement the guard file
    /// if and only if a double-win appears in a real events log". If this check
    /// ever exits 1, that output IS the evidence that bead is waiting for.
    Audit {
        /// Which check to run. Omit for a summary.
        ///
        /// `--expect` and `--allow-main` apply to `topology` only; every other
        /// check ignores them.
        // The possible values come from `audit::Check::NAMES` rather than from a
        // hand-written doc comment, which is what had drifted to four of the
        // nine (pact-98u). Not stated in the help itself: a reader wants the
        // list, not its history.
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(audit::Check::NAMES))]
        check: Option<String>,
        /// Only events at or after this point: RFC3339, or a duration back from
        /// now such as `90m`, `24h`, `7d`, `2w`.
        #[arg(long)]
        since: Option<String>,
        /// Include events an annotation marked as not-real-history.
        ///
        /// The log is append-only, so a wrong entry is corrected by appending an
        /// `annotation` naming its lines rather than by editing it. Those lines
        /// are excluded from every statistic and check by default, and the count
        /// is always reported. Pass this to see the raw log as written.
        #[arg(long)]
        include_annotated: bool,
        /// What `--check topology` should assert. Every stamped event must
        /// satisfy it — there is no proportion threshold, deliberately (see
        /// docs/audit.md).
        // The possible values come from `audit::Expect::NAMES` for the reason
        // `--check`'s come from `Check::NAMES`: the list that used to be written
        // out here, and the one in `Expect::parse`'s error, were pact-98u's
        // exact shape one flag over — two hand-written copies that happened to
        // still agree.
        #[arg(
            long,
            requires = "check",
            value_parser = clap::builder::PossibleValuesParser::new(audit::Expect::NAMES)
        )]
        expect: Option<String>,
        /// An identity allowed to act from the main checkout under
        /// `--expect worktrees`; repeat for several.
        ///
        /// In the worktree topology pact documents somebody must sit in the main
        /// checkout — it is where the coordination logs are committed from — so an
        /// orchestrator necessarily acts from `main`. Without this the check could not
        /// pass for any real fleet: one field run failed it with 19 offending events,
        /// not one of which was an agent working in the wrong place.
        #[arg(long, value_name = "AGENT", requires = "expect")]
        allow_main: Vec<String>,
        /// Compare this repository against a previously written `--export`
        /// report and print what moved.
        ///
        /// Its own mode: conflicts with `--check`, because both want to be
        /// the single thing stdout says, and two JSON values on one stdout
        /// breaks every `--json` caller.
        #[arg(long, conflicts_with = "check")]
        compare: Option<PathBuf>,
        /// Write the summary, every named check and `pact doctor`'s checks —
        /// combined into one JSON file — to this path.
        ///
        /// Orthogonal to `--check`/`--json`, which still control only what
        /// prints to stdout: pass this alongside either, or alone. Meant to
        /// turn a by-hand field audit (grep the event log, run doctor and
        /// audit separately, write up what stood out) into one command whose
        /// output a human — or another agent session — can read directly.
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// Interactive terminal dashboard over leases, messages, and doctor status.
    #[cfg(feature = "ui")]
    Ui,
    /// Serve pact's read-only observation surface over MCP on stdio.
    #[cfg(feature = "mcp")]
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
}

/// `serve` is the only action, and it is still spelled out rather than folded
/// into `pact mcp`, because the noun alone gives a later read-write mode or a
/// `pact mcp tools` inspector somewhere to go that does not change this one's
/// spelling. A client config that says `pact mcp serve` should keep working.
#[derive(Subcommand)]
#[cfg(feature = "mcp")]
pub(crate) enum McpAction {
    /// Read JSON-RPC from stdin and answer on stdout until stdin closes.
    ///
    /// Strictly read-only: it observes leases, messages, doctor checks and the
    /// event log, and can neither claim a lease nor send a message nor mark a
    /// message read. Spawned by an MCP client, never run by hand.
    Serve,
}

#[derive(Subcommand)]
pub(crate) enum WatchAction {
    /// Subscribe to a path, or to everything under a directory.
    Add {
        /// A file, or a directory (or a path with a trailing `/`) to subscribe
        /// to everything beneath it. No globs.
        path: String,
    },
    /// Unsubscribe from a path you are watching.
    Rm {
        /// The path to stop watching, spelled as you subscribed to it.
        path: String,
    },
    /// List the subscriptions currently in force, for every agent.
    Ls,
}

#[derive(Subcommand)]
pub(crate) enum LeaseAction {
    /// Acquire a lease on one or more paths, all-or-nothing.
    Acquire {
        /// Paths to claim. Several are taken atomically (pact-rnc.21): if any
        /// is held by someone else, none are taken.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
        /// How long to hold. A bare number is SECONDS (the default 2700 is 45
        /// minutes); or give a unit, as `--since` takes: 45m, 2h, 1d, 2w.
        #[arg(long, default_value_t = lease::DEFAULT_TTL_SECS.to_string())]
        ttl: String,
        /// Take a lease its holder is still legitimately inside the TTL of.
        /// Warns on stderr naming them, and records a `stolen`/`displaced`
        /// pair — an override on your word. `lease sweep` reclaims on pact's
        /// own evidence instead, for a holder you believe is gone.
        #[arg(long)]
        steal: bool,
        /// Why you are claiming these paths. Recorded with the lease and shown
        /// by `lease ls`, `pact log` and `agents --for` — this IS the
        /// announcement, so a message repeating it is a message nobody needed.
        #[arg(long)]
        note: Option<String>,
    },
    /// Refresh a lease you already hold, so a long task doesn't outlive it.
    Renew {
        /// A path you already hold. Its TTL restarts from now.
        path: String,
    },
    /// Release leases you hold, or all of them with --all.
    Release {
        /// One or more paths, like `acquire`. Unlike `acquire` this is NOT
        /// all-or-nothing: every path is attempted, because on the way out
        /// releasing three of four beats releasing none (pact-mqw.7).
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        path: Vec<String>,
        /// Release even if held by another agent.
        #[arg(long, conflicts_with = "all")]
        force: bool,
        /// Release every lease you hold, in one command.
        #[arg(long)]
        all: bool,
    },
    /// Reclaim holds whose holder is gone, recorded as recovery not as a steal.
    ///
    /// `--steal` overrides a live claim on your word, and writes exactly what
    /// trampling a working peer writes — so the audit cannot tell the two
    /// apart. This reclaims on pact's own evidence and records it: how far past
    /// its TTL the hold was, and how long its holder had been silent.
    ///
    /// By default only holds past their own TTL, which are nobody's by the
    /// lease's own terms. `--suspect` also takes holds still inside their TTL
    /// whose holder has gone quiet for more than half of it — the case that
    /// produced every double-win in one measured fleet run, because a dead
    /// agent's 45-minute lease reads as live for 45 minutes.
    Sweep {
        /// Limit to these paths. Omit to sweep every eligible hold.
        path: Vec<String>,
        /// Also reclaim holds `lease ls` labels SUSPECT, not only lapsed ones.
        #[arg(long)]
        suspect: bool,
    },
    /// List active leases.
    Ls {
        /// Include expired leases in the listing.
        #[arg(long)]
        all: bool,
    },
}

/// `pact context <action>`.
#[derive(Subcommand)]
pub(crate) enum ContextAction {
    /// Record a constraint this run operates under.
    ///
    /// Appends a `context` row to `.pact/events.jsonl`, chain-hashed like every
    /// other event, so the policy a fleet worked under lives in the same log as
    /// what it did. Keys are free-form; docs/fleet-patterns.md carries a starter
    /// vocabulary (`commit-policy`, `scheduler`, `topology-expectation`).
    ///
    /// Setting a key again records the new value and keeps the old row: the
    /// active value is the last one, and the change itself is history.
    Set {
        /// e.g. `commit-policy`. No whitespace and no `=`, so a row always
        /// renders unambiguously as `key=value`.
        key: String,
        /// e.g. `none`, `per-task`, `orchestrator-only`. Free text.
        value: String,
    },
}

/// `pact plan <action>`.
#[derive(Subcommand)]
pub(crate) enum PlanAction {
    /// Lint a plan manifest: intra-wave file overlap, cycles, orphans, hot files.
    ///
    /// The manifest is a JSON array or one JSON object per line, of
    /// `{id, wave, files[], depends_on[]}`. pact does NOT read the Beads store to
    /// build it — the orchestrator exports it, which is what keeps bd off every pact
    /// command's path. docs/plan.md has the schema and a `bd list --json` pipeline
    /// that produces one.
    Lint {
        /// Path to the manifest.
        manifest: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum MsgAction {
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
        /// One-line summary. `msg inbox` gives it its own column; in `pact log`
        /// it is the entry's detail, which falls back to the body without one.
        #[arg(long)]
        subject: Option<String>,
        /// Message body. Mutually exclusive with --body-file.
        #[arg(required_unless_present = "body_file", conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the body from a file, or "-" for stdin. No shell escaping.
        #[arg(long, value_name = "PATH|-")]
        body_file: Option<String>,
        /// Recipient to leave out of this send; repeat for several.
        ///
        /// It existed for replaying a partially-failed multi-recipient send
        /// (pact-m7j.6.5): bd needed one create per recipient, so a send could land
        /// for some and fail for others, and the `--json` error carried an
        /// `already_sent` list to pass back here. A send is one append since 0.9.0
        /// and cannot partially fail, so that shape is gone and this flag means only
        /// what it says.
        #[arg(long, value_name = "AGENT")]
        skip: Vec<String>,
    },
    /// List messages addressed to you, one line each.
    Inbox {
        /// Leave out messages you have already read.
        #[arg(long)]
        unread_only: bool,
        /// Print every body in full instead of one line per message.
        #[arg(long)]
        full: bool,
        /// List `pact watch` notices alongside authored messages, one entry per
        /// path rather than one per delivery.
        #[arg(long, conflicts_with = "watch_only")]
        include_watch: bool,
        /// Only `pact watch` notices — what changed under you while you worked.
        #[arg(long)]
        watch_only: bool,
    },
    /// List messages you sent, newest first, and whether they were read.
    Sent,
    /// Read a message (or its thread) by id.
    Read {
        /// Message id, as `msg inbox` prints it. Reading marks it read for you
        /// — the only thing that tells the sender their message landed.
        id: String,
        /// Envelope, subject and the first few lines of each body — for
        /// deciding what to read in full, on a thread too long to read whole.
        #[arg(long)]
        brief: bool,
    },
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

/// Past this, a suggested correction says how old it is. Below it, the name
/// alone — an age on a peer that acted seconds ago is noise, and the whole
/// point of the annotation is that a borderline suggestion can be judged.
/// Names older than `agents`' suggestion horizon are not offered at all.
const ANNOTATE_SUGGESTION_AGE_SECS: i64 = 15 * 60;

/// clap's verdict, as an exit code plus the argv *shape* that stands in for a
/// subcommand name we never got to parse.
///
/// Only an explicit `--help` / `-V` is a success. Deliberately NOT
/// DisplayHelpOnMissingArgumentOrSubcommand: there, clap prints help *because
/// the invocation was incomplete*, which is a usage error the user did not ask
/// for. Treating it as success made bare `pact` exit 0, so a script whose
/// variable expanded to nothing would read "worked" — the very shape of
/// interpolation bug this code exists to disambiguate.
pub(crate) fn clap_outcome(kind: clap::error::ErrorKind) -> (i32, &'static str) {
    match kind {
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => (0, "help"),
        _ => (USAGE_ERROR, "usage-error"),
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
pub(crate) fn subcommand_name(command: &Command) -> &'static str {
    match command {
        Command::Init { .. } => "init",
        Command::Whoami => "whoami",
        Command::Agents { .. } => "agents",
        Command::Lease { action } => match action {
            LeaseAction::Acquire { .. } => "lease acquire",
            LeaseAction::Renew { .. } => "lease renew",
            LeaseAction::Release { .. } => "lease release",
            LeaseAction::Sweep { .. } => "lease sweep",
            LeaseAction::Ls { .. } => "lease ls",
        },
        Command::Merge { .. } => "merge",
        Command::Msg { action } => match action {
            MsgAction::Send { .. } => "msg send",
            MsgAction::Inbox { .. } => "msg inbox",
            MsgAction::Sent => "msg sent",
            MsgAction::Read { .. } => "msg read",
        },
        Command::Log { .. } => "log",
        Command::Context { action } => match action {
            ContextAction::Set { .. } => "context set",
        },
        Command::Plan { action } => match action {
            PlanAction::Lint { .. } => "plan lint",
        },
        Command::Doctor { .. } => "doctor",
        // The shell is a closed enum clap already validated, so it cannot
        // carry user text into a span name.
        Command::Completion { shell } => match shell {
            clap_complete::Shell::Bash => "completion bash",
            clap_complete::Shell::Zsh => "completion zsh",
            clap_complete::Shell::Fish => "completion fish",
            clap_complete::Shell::Elvish => "completion elvish",
            clap_complete::Shell::PowerShell => "completion powershell",
            _ => "completion other",
        },
        Command::Watch { action } => match action {
            WatchAction::Add { .. } => "watch add",
            WatchAction::Rm { .. } => "watch rm",
            WatchAction::Ls => "watch ls",
        },
        Command::Audit {
            compare: Some(_), ..
        } => "audit compare",
        Command::Audit { check, .. } => match check.as_deref() {
            Some("double-win") => "audit double-win",
            Some("stale-holds") => "audit stale-holds",
            Some("chain-integrity") => "audit chain-integrity",
            Some("commit-correlation") => "audit commit-correlation",
            Some("merge-divergence") => "audit merge-divergence",
            Some("claim-lease-divergence") => "audit claim-lease-divergence",
            Some("retry-storm") => "audit retry-storm",
            Some("silent-contention") => "audit silent-contention",
            Some("topology") => "audit topology",
            // One literal for anything else, including a bad value: the argument
            // is user text and must never reach a span name.
            Some(_) => "audit other",
            None => "audit",
        },
        #[cfg(feature = "ui")]
        Command::Ui => "ui",
        #[cfg(feature = "mcp")]
        Command::Mcp { action } => match action {
            McpAction::Serve => "mcp serve",
        },
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
pub(crate) fn repo_name() -> Option<String> {
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
pub(crate) fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir()?;

    match cli.command {
        // `doctor` reports failure as an exit code rather than an error,
        // because its report *is* the output; everything else succeeds or
        // raises, and `Ok(())` means exit 0.
        Command::Context { action } => match action {
            ContextAction::Set { key, value } => {
                run_context_set(&cwd, cli.json, &key, &value, cli.agent.as_deref()).map(|()| 0)
            }
        },
        Command::Plan { action } => match action {
            PlanAction::Lint { manifest } => run_plan_lint(&cwd, cli.json, &manifest),
        },
        Command::Doctor { fix } => run_doctor(&cwd, cli.json, fix, cli.agent.as_deref()),
        Command::Init {
            print,
            no_commit,
            force,
        } => run_init(
            &cwd,
            print,
            no_commit,
            force,
            cli.agent.as_deref(),
            cli.json,
        )
        .map(|()| 0),
        Command::Whoami => run_whoami(&cwd, cli.agent.as_deref(), cli.json).map(|()| 0),
        Command::Agents { r#for } => run_agents(&cwd, cli.json, r#for.as_deref()).map(|()| 0),
        Command::Lease { action } => {
            run_lease(&cwd, cli.agent.as_deref(), cli.json, action).map(|()| 0)
        }
        Command::Merge {
            branch,
            verify,
            ttl,
            allow_dirty,
        } => run_merge(
            &cwd,
            cli.agent.as_deref(),
            cli.json,
            &branch,
            verify.as_deref(),
            &ttl,
            allow_dirty,
        )
        .map(|()| 0),
        Command::Msg { action } => {
            run_msg(&cwd, cli.agent.as_deref(), cli.json, action).map(|()| 0)
        }
        Command::Log { limit } => run_log(&cwd, cli.json, limit).map(|()| 0),
        Command::Completion { shell } => run_completion(shell).map(|()| 0),
        Command::Watch { action } => {
            run_watch(&cwd, cli.agent.as_deref(), cli.json, action).map(|()| 0)
        }
        Command::Audit {
            check,
            since,
            include_annotated,
            expect,
            allow_main,
            compare,
            export,
        } => run_audit(
            &cwd,
            cli.json,
            AuditArgs {
                check,
                since,
                include_annotated,
                expect,
                allow_main,
                compare,
                export,
            },
        ),
        #[cfg(feature = "ui")]
        Command::Ui => {
            let root = repo::find_repo_root(&cwd)?;
            let agent = identity::resolve_agent(cli.agent.as_deref()).ok();
            tui::run(root, agent).map(|()| 0)
        }
        // No identity resolved and none needed: an observer holds nothing and
        // sends nothing, so there is no agent for it to be. The tools that need
        // one take it as a parameter, because an observer may watch several.
        #[cfg(feature = "mcp")]
        Command::Mcp { action } => match action {
            McpAction::Serve => mcp::serve(repo::find_repo_root(&cwd)?),
        },
    }
}

fn run_msg(cwd: &Path, agent_flag: Option<&str>, json: bool, action: MsgAction) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    // No backend probe, and no conflicting-store warning (pact-as5.3).
    //
    // Both existed because `pact msg` QUERIED a Beads store: the `locate()?` made
    // every message command exit 3 when `bd` was missing, and the warning explained
    // an "inbox empty" that was really a second, shadowed store being read
    // (pact-m7j.10.7). Messages live in `.pact/messages.jsonl` now, so neither fact
    // can affect an inbox, and repeating them here would be telling an agent about a
    // dependency this command no longer has. `pact doctor` still reports both.
    //
    // This is what makes exit 3 unreachable from every `msg` path — see the
    // exit-code table in README.md.
    let agent = identity::resolve_agent(agent_flag)?;
    match action {
        MsgAction::Send {
            to,
            to_owner_of,
            thread,
            subject,
            body,
            body_file,
            skip,
        } => {
            let body = match body_file {
                Some(p) => read_body(&p)?,
                None => body.unwrap_or_default(),
            };
            if body.trim().is_empty() {
                anyhow::bail!("empty message body — nothing to send");
            }
            // Normalized once, here, before anything compares it — pact-m7j.8.6:
            // `to_owner_of` is whatever the caller's own CWD made of it, and
            // both uses below need the SAME canonical spelling `acquire`
            // itself would have produced: the lookup just below (so a path
            // typed from a subdirectory still resolves to its real prior
            // owner) and, further down, the `about-<path>` labels this send
            // is tagged with (so a later `about_path` query — typed from yet
            // another CWD — still finds it). Reassigning rather than reading
            // through a second binding, so nothing downstream can reach the
            // un-normalized spelling by mistake.
            let to_owner_of: Vec<String> = to_owner_of
                .iter()
                .map(|p| lease::normalize_path(&root, p))
                .collect();
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
                // Every `--to-owner-of` path resolved to the sender itself
                // (the self-owner branch above warned and added nothing), and
                // no explicit `--to` was given — clap requires one or the
                // other. Refusing outright used to strand the sender exactly
                // when `--to-owner-of` exists to save them from guessing a
                // name (pact-m7j.10.5, reproduced live: an agent that had
                // just taken over a path had no way to tell its previous
                // co-editor). `msg::send`'s about-<path> tagging attaches to
                // every `to_owner_of` path unconditionally, and
                // `messages_about()` surfaces it to whoever leases that path
                // next regardless of who `to` was — so addressing it to
                // `agents::HUMAN` still delivers it forward through that
                // pipeline instead of losing it.
                output::warn(&format!(
                    "note: every --to-owner-of path resolves to you; addressing to {} so the note \
                     still reaches whoever leases it next",
                    agents::HUMAN
                ));
                to.push(agents::HUMAN.to_string());
            }
            // pact-m7j.6.5: a replay of a partially-failed send names the
            // recipients who already got it (`already_sent` in the previous
            // attempt's `--json` error) so this one does not duplicate
            // delivery to them. Applied after every other recipient source
            // (`--to`, `--to-owner-of`, the HUMAN fallback above) has already
            // built the list, so `--skip` behaves the same regardless of how
            // a name got into `to`.
            if !skip.is_empty() {
                let before = to.len();
                to.retain(|r| !skip.contains(r));
                let skipped = before - to.len();
                if skipped > 0 {
                    output::warn(&format!(
                        "note: {skipped} recipient(s) skipped — already sent to them, not re-sending"
                    ));
                }
            }
            for recipient in &to {
                check_recipient(recipient)?;
            }
            // One registry lookup for all recipients, not one per --to.
            warn_if_unknown(&root, &to);
            let sent = msg::send(
                &root,
                &agent,
                &to,
                msg::Draft {
                    thread: thread.as_deref(),
                    subject: subject.as_deref(),
                    body: &body,
                    about: &to_owner_of,
                    // Always authored. There is deliberately no flag for
                    // "file this under machine noise" — that tag exists so an
                    // agent can trust that what is left IS correspondence.
                    notice: false,
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
            let messages = msg::sent(&root, &agent)?;
            output::emit(json, &messages, |messages: &Vec<msg::Message>| {
                if messages.is_empty() {
                    format!("{agent} has sent nothing yet")
                } else {
                    render_sent(messages)
                }
            });
            Ok(())
        }
        MsgAction::Inbox {
            unread_only,
            full,
            include_watch,
            watch_only,
        } => {
            let view = if watch_only {
                msg::WatchView::Only
            } else if include_watch {
                msg::WatchView::Include
            } else {
                msg::WatchView::Authored
            };
            let messages = msg::inbox(&root, &agent, unread_only)?;
            // `--json` is never coalesced: a machine can group for itself, and
            // collapsing nine deliveries into one entry would cost it their ids.
            // The flags choose which messages it sees and nothing else.
            let picked: Vec<&msg::Message> = messages
                .iter()
                .filter(|m| match view {
                    msg::WatchView::Authored => !m.notice,
                    msg::WatchView::Include => true,
                    msg::WatchView::Only => m.notice,
                })
                .collect();
            let (authored, notices) = msg::split_notices(&messages);
            output::emit(json, &picked, |_: &Vec<&msg::Message>| {
                render_inbox_view(&authored, &notices, view, full)
            });
            Ok(())
        }
        MsgAction::Read { id, brief } => {
            let thread = msg::read_thread(&root, &agent, &id)?;
            // `--brief` is a RENDERING flag: `--json` is pinned shape and stays
            // one object per recipient either way (pact-83r.8).
            output::emit(json, &thread, |thread: &Vec<msg::Message>| {
                let flat = thread.iter().collect::<Vec<_>>();
                if brief {
                    render_brief(&flat)
                } else {
                    render_full(&flat)
                }
            });
            Ok(())
        }
    }
}

/// Ceiling on the stdin read, not a latency target: a legitimate producer may be
/// slow, so this is generous on purpose. `PACT_STDIN_BODY_TIMEOUT_MS` overrides
/// it — the only way a test can prove the bound exists without waiting it out.
fn stdin_body_timeout() -> std::time::Duration {
    match std::env::var("PACT_STDIN_BODY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => std::time::Duration::from_secs(60),
    }
}

/// `--body-file -` must not be able to block forever (pact-83r.5).
///
/// UNCONFIRMED: a fleet reported this hanging past 120 s and wedging the shell,
/// and it does not reproduce on 0.9.4 — but it cannot be reproduced here either,
/// because the report's precondition is a tty on stdin and no test environment
/// has one. Guarded anyway, because of *where* the hang is: `msg send` is the
/// tool an agent uses to report that it is blocked, so a hang there is the one
/// an agent cannot report its way out of.
///
/// Two guards, for the two ways the read never returns. A tty means no producer
/// is attached at all — that is a mistake, not slowness, so it is refused
/// immediately and names the alternative. Everything else gets a bounded read;
/// the reading thread is left blocked, which costs nothing because the process
/// is about to exit either way.
fn read_stdin_body() -> Result<String> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        anyhow::bail!(
            "--body-file - reads the body from stdin, but stdin is a terminal: \
             nothing is feeding it, so the read would never return. Pipe the body \
             in, or write it to a file and use --body-file <path>."
        );
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = tx.send(std::io::stdin().read_to_string(&mut s).map(|_| s));
    });
    let timeout = stdin_body_timeout();
    match rx.recv_timeout(timeout) {
        Ok(r) => r.context("reading message body from stdin"),
        Err(_) => anyhow::bail!(
            "stdin gave no end of input after {}s — whatever is feeding \
             --body-file - is not finishing. Write the body to a file and use \
             --body-file <path>; nothing was sent.",
            timeout.as_secs_f32(),
        ),
    }
}

/// `-` means stdin, so a multi-paragraph body full of quotes and backslashes
/// never has to survive a shell (pact-rnc.3).
fn read_body(path: &str) -> Result<String> {
    let raw = if path == "-" {
        read_stdin_body()?
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
fn warn_if_unknown(root: &Path, to: &[String]) {
    let known = agents::list(root).unwrap_or_default();
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
    // Each suggestion carries how long since that agent last did anything.
    // A correction is a claim about what you meant to type, and "alice-prime,
    // last seen 3h ago" is a claim the reader can judge where a bare name is
    // not. Names older than the suggestion horizon are not offered at all —
    // see agents::is_stale_for_suggestion.
    let hits: Vec<String> = agents::suggest(known, to)
        .into_iter()
        .map(|name| {
            // Only annotate an age worth judging. "tui-dev (last seen 0s ago)"
            // is noise on the common case — a peer that is working right now —
            // and noise is what stops the useful case being read.
            let stale = known
                .iter()
                .find(|a| a.name == name)
                .and_then(|a| age_of(&a.last_seen))
                .filter(|secs| *secs >= ANNOTATE_SUGGESTION_AGE_SECS);
            match stale {
                Some(secs) => format!("{name} (last seen {} ago)", human_secs(secs)),
                None => name,
            }
        })
        .collect();
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
///
/// `WHEN` (pact-m7j.12.3): named in both real fleet retrospectives and
/// `sut-analysis.md` as a missing at-a-glance signal — an agent deciding
/// whether an unread message is worth a context switch had to run `msg read`
/// just to see how stale it was. `Message.created_at` was already carried
/// end to end; this is `pact log`'s own `since()` reused for the same
/// column, not a new mechanism.
/// The inbox an agent actually reads: correspondence, with `pact watch` notices
/// summarised rather than listed (pact-mqw.5).
///
/// The default is authored-only because a notice is a side effect of a peer
/// doing its job, and the queue an agent checks for "does anybody need something
/// from me" must not be dominated by them. In the crucible run it was, 11 to 1,
/// and the one authored message in that window was a warning about six duplicate
/// test functions.
///
/// Notices are never *hidden*: the trailing line always counts them, per path,
/// and names the flag that shows them. A count an agent can see is what makes
/// skipping them a decision instead of an accident.
fn render_inbox_view(
    authored: &[&msg::Message],
    notices: &[msg::NoticeGroup],
    view: msg::WatchView,
    full: bool,
) -> String {
    // Nothing at all keeps the string it has always had: "inbox empty" is what
    // an agent's first command prints on a quiet repo, and "no authored
    // messages" would imply something else is waiting.
    if authored.is_empty() && notices.is_empty() {
        return "inbox empty".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if view != msg::WatchView::Only {
        if authored.is_empty() {
            parts.push("no authored messages".to_string());
        } else if full {
            parts.push(render_full(authored));
        } else {
            parts.push(render_inbox(authored));
        }
    }

    if notices.is_empty() {
        if view == msg::WatchView::Only {
            parts.push("no watch notices".to_string());
        }
    } else if view == msg::WatchView::Authored {
        // The summary, not the notices. One line, because its job is to let an
        // agent decide whether to look — not to be the looking.
        let total: usize = notices.iter().map(|g| g.count).sum();
        let unread: usize = notices.iter().map(|g| g.unread).sum();
        let per_path = notices
            .iter()
            .map(|g| format!("{} ×{}", g.path, g.count))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!(
            "{total} watch notice(s) on {} path(s), {unread} unread: {per_path}\n\
             `pact msg inbox --include-watch` lists them, `--watch-only` shows only them",
            notices.len(),
        ));
    } else {
        // One row per PATH, not per delivery. Nine diffs of one file nine
        // seconds apart answer one question and only the last of them answers
        // it, so the earlier ones are counted and the latest is the one offered
        // to `pact msg read`.
        let mut rows = vec![vec![
            "PATH".to_string(),
            "CHANGES".to_string(),
            "UNREAD".to_string(),
            "LATEST FROM".to_string(),
            "WHEN".to_string(),
            "LATEST ID".to_string(),
        ]];
        rows.extend(notices.iter().map(|g| {
            vec![
                g.path.clone(),
                g.count.to_string(),
                g.unread.to_string(),
                if g.latest_from.is_empty() {
                    "?".to_string()
                } else {
                    g.latest_from.clone()
                },
                since(&g.latest_at),
                g.latest_id.clone(),
            ]
        }));
        // "changed under you" is wrong for a reserved key: nothing changed, a lock
        // was let go (pact-bsf). Only claim a change when at least one group is a
        // real file, so a waiter watching only mutexes is not told its lock moved.
        let any_file = notices.iter().any(|g| !lease::is_mutex(&g.path));
        parts.push(format!(
            "{}\n\n{} path(s) {} — `pact msg read <latest id>` for the newest",
            table(&rows),
            notices.len(),
            if any_file {
                "changed under you"
            } else {
                "were released"
            },
        ));
    }
    parts.join("\n\n")
}

fn render_inbox(messages: &[&msg::Message]) -> String {
    let mut rows = vec![vec![
        "ID".to_string(),
        String::new(), // unread marker; a header would be wider than the column
        "FROM".to_string(),
        "WHEN".to_string(),
        "SUBJECT".to_string(),
        "BODY".to_string(),
    ]];
    rows.extend(messages.iter().map(|m| {
        vec![
            m.id.clone(),
            if m.read { " " } else { "*" }.to_string(),
            if m.from.is_empty() { "?" } else { &m.from }.to_string(),
            since(&m.created_at),
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

/// One stored message, however many recipients it fanned out to (pact-83r.8).
///
/// [`msg::Message`] is one copy PER RECIPIENT so `--json` keeps `to` a single
/// name, and that is not changing — a machine consumer is pinned to it. But a
/// human renderer that walks the fan-out prints the body once per recipient: a
/// 15-recipient broadcast cost ~280 KB to read, and it bit hardest on exactly the
/// messages that mattered, because the run that measured it REQUIRED hot-file
/// changers to broadcast to every dependent. Regrouping here collapses what the
/// API layer legitimately fans out.
///
/// Copies of one message are adjacent in every listing pact builds (fan-out is
/// per record), so this is a linear pass and not a sort.
fn group_by_id<'a>(messages: &[&'a msg::Message]) -> Vec<Vec<&'a msg::Message>> {
    let mut out: Vec<Vec<&msg::Message>> = Vec::new();
    for m in messages {
        match out.last_mut() {
            Some(g) if g[0].id == m.id => g.push(m),
            _ => out.push(vec![m]),
        }
    }
    out
}

/// The recipients, once, split by whether they have acknowledged it.
///
/// The union of the two lists is the recipient list, so nobody is named twice —
/// and "who still owes this a look" is the question a sender actually has, where
/// the old per-copy `(unread)` marker only ever answered it one recipient at a
/// time.
fn roster(group: &[&msg::Message]) -> String {
    let (read, unread): (Vec<&str>, Vec<&str>) = group
        .iter()
        .map(|m| m.to.as_str())
        .partition(|to| group[0].read_by.iter().any(|a| a == to));
    let mut parts = Vec::new();
    if !read.is_empty() {
        parts.push(format!("read by {}", read.join(", ")));
    }
    if !unread.is_empty() {
        parts.push(format!("unread by {}", unread.join(", ")));
    }
    parts.join(" — ")
}

/// Full text with the envelope pact used to throw away: from, to, subject, time
/// (pact-rnc.1). Shared by `msg read` and `msg inbox --full`, so a sender can
/// finally read their own message back with its metadata.
fn render_full(messages: &[&msg::Message]) -> String {
    group_by_id(messages)
        .iter()
        .map(|g| {
            let m = g[0];
            format!(
                "[{}] from: {}  to: {}\nsubject: {}\nat: {}  thread: {}\n\n{}",
                m.id,
                if m.from.is_empty() { "?" } else { &m.from },
                roster(g),
                m.subject.as_deref().unwrap_or("(none)"),
                m.created_at,
                m.thread,
                m.body,
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// How much of a body `--brief` shows. Enough to tell a warning from a status
/// ping, which is the triage decision; the id to read it in full is on the line
/// above either way.
const BRIEF_LINES: usize = 5;

/// `--brief`: envelope, subject and the head of the body, for deciding which of
/// a thread's messages to read in full rather than reading all of them.
fn render_brief(messages: &[&msg::Message]) -> String {
    group_by_id(messages)
        .iter()
        .map(|g| {
            let m = g[0];
            let head: Vec<&str> = m.body.lines().take(BRIEF_LINES).collect();
            let rest = m.body.lines().count().saturating_sub(head.len());
            format!(
                "[{}] from: {}  to: {}\nsubject: {}\nat: {}\n\n{}{}",
                m.id,
                if m.from.is_empty() { "?" } else { &m.from },
                roster(g),
                m.subject.as_deref().unwrap_or("(none)"),
                m.created_at,
                head.join("\n"),
                if rest == 0 {
                    String::new()
                } else {
                    format!(
                        "\n… {rest} more line(s) — `pact msg read {}` for the full text",
                        m.id
                    )
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

#[cfg(test)]
mod tests {
    use super::commands::message;
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

    /// A `pact watch` release notice, subject-shaped the way
    /// `watch::notify_release` builds it so `split_notices` can parse the path
    /// back out.
    fn notice(id: &str, path: &str, holder: &str, read: bool) -> msg::Message {
        msg::Message {
            subject: Some(format!("{path}{}{holder}", msg::NOTICE_SUBJECT_MARKER)),
            from: holder.to_string(),
            notice: true,
            ..message(id, holder, "a diff", read)
        }
    }

    /// pact-rnc.1 + pact-rnc.2: sender, unread marker, one line per message.
    #[test]
    fn inbox_shows_from_and_an_unread_marker_on_one_line_each() {
        let body = "para one\n\npara two with \"quotes\"\n".repeat(40);
        let a = message("pact-wisp-aaa", "msg-fix", &body, false);
        let b = message("pact-wisp-bbb", "lease-fix", "short", true);
        let out = render_inbox(&[&a, &b]);
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

    /// pact-mqw.5: the inbox an agent reads is correspondence, and watch notices
    /// are a trailing count. Reproduces the crucible ratio directly — 11
    /// automatic notices to 1 authored message — and asserts the authored one is
    /// the thing on screen.
    #[test]
    fn the_default_inbox_is_authored_only_with_notices_counted() {
        let mut all: Vec<msg::Message> = (0..9)
            .map(|i| notice("n{i}", "src/ast.rs", &format!("agent-0{i}"), false))
            .collect();
        all.push(notice("n9", "src/eval.rs", "agent-09", false));
        all.push(notice("n10", "src/eval.rs", "agent-09", true));
        let authored_msg = message("a1", "agent-05", "six duplicate test fns", false);
        all.push(authored_msg);

        let (authored, notices) = msg::split_notices(&all);
        let out = render_inbox_view(&authored, &notices, msg::WatchView::Authored, false);

        assert!(
            out.contains("agent-05"),
            "the authored message must show: {out}"
        );
        assert!(
            !out.contains("src/ast.rs ×9\nn0"),
            "notices must not be listed row by row: {out}"
        );
        // Counted, per path, with the flag that reveals them.
        assert!(out.contains("11 watch notice(s) on 2 path(s)"), "{out}");
        assert!(out.contains("src/ast.rs ×9"), "{out}");
        assert!(out.contains("src/eval.rs ×2"), "{out}");
        assert!(out.contains("10 unread"), "{out}");
        assert!(out.contains("--include-watch"), "{out}");
        // And the notice ids are NOT in the default table, or nothing was saved.
        assert!(!out.contains("n0"), "{out}");
    }

    /// `--watch-only` is one row per PATH, not per delivery. Nine diffs of one
    /// file nine seconds apart answer one question and only the last answers it,
    /// so the latest id is what gets offered to `msg read`.
    #[test]
    fn watch_only_coalesces_per_path_and_offers_the_newest_diff() {
        let all: Vec<msg::Message> = vec![
            notice("n0", "src/ast.rs", "agent-01", true),
            notice("n1", "src/ast.rs", "agent-04-r2", false),
            notice("n2", "src/printer.rs", "agent-02", false),
        ];
        let (authored, notices) = msg::split_notices(&all);
        let out = render_inbox_view(&authored, &notices, msg::WatchView::Only, false);

        let rows: Vec<&str> = out.lines().filter(|l| l.contains("src/")).collect();
        assert_eq!(rows.len(), 2, "one row per path: {out}");
        let ast = rows.iter().find(|r| r.contains("src/ast.rs")).unwrap();
        assert!(ast.contains("agent-04-r2"), "the latest releaser: {ast}");
        assert!(ast.contains("n1"), "the latest id: {ast}");
        assert!(
            !out.contains("n0"),
            "a superseded diff is a count, not a row: {out}"
        );
        // No authored section at all in this view.
        assert!(!out.contains("no authored messages"), "{out}");
    }

    /// An inbox with nothing but notices must not read as "inbox empty" — that
    /// was the shape that let a fleet believe nothing had happened.
    #[test]
    fn an_inbox_of_only_notices_says_so_rather_than_looking_empty() {
        let all = vec![notice("n0", "src/ast.rs", "agent-01", false)];
        let (authored, notices) = msg::split_notices(&all);
        let out = render_inbox_view(&authored, &notices, msg::WatchView::Authored, false);
        assert!(out.contains("no authored messages"), "{out}");
        assert!(out.contains("1 watch notice(s)"), "{out}");
    }

    #[test]
    fn full_render_carries_the_envelope() {
        let m = message("pact-wisp-aaa", "msg-fix", "the body", true);
        let out = render_full(&[&m]);
        assert!(out.contains("from: msg-fix"), "{out}");
        assert!(out.contains("to: read by cli-wire"), "{out}");
        assert!(out.contains("subject: a subject"), "{out}");
        assert!(out.contains("the body"), "{out}");
    }

    /// One stored message fanned out to N recipients, the shape `msg read`
    /// hands the renderer.
    fn broadcast(id: &str, body: &str, to: &[&str], read_by: &[&str]) -> Vec<msg::Message> {
        to.iter()
            .map(|t| msg::Message {
                to: t.to_string(),
                read: read_by.contains(t),
                read_by: read_by.iter().map(|a| a.to_string()).collect(),
                ..message(id, "msg-fix", body, false)
            })
            .collect()
    }

    /// pact-83r.8: the renderer used to walk the per-recipient fan-out, so a
    /// 15-recipient broadcast printed the body 15 times — ~280 KB to read one
    /// message, and worst on the broadcasts that mattered most.
    #[test]
    fn a_broadcast_renders_its_body_once_and_its_recipients_once() {
        let to: Vec<String> = (0..15).map(|i| format!("agent-{i:02}")).collect();
        let names: Vec<&str> = to.iter().map(String::as_str).collect();
        let fanned = broadcast("pact-wisp-aaa", "MAX_QUADS moved", &names, &names[..2]);
        let out = render_full(&fanned.iter().collect::<Vec<_>>());

        assert_eq!(out.matches("MAX_QUADS moved").count(), 1, "{out}");
        // One envelope, not fifteen — the id still appears twice, as the
        // message's own and as its thread's.
        assert_eq!(out.matches("subject: a subject").count(), 1, "{out}");
        // Each recipient exactly once, split by who still owes it a look. The
        // union of the two lists IS the recipient list, so naming them plainly
        // as well would be the duplication this bead is about.
        for name in &names {
            assert_eq!(out.matches(name).count(), 1, "{name} twice in {out}");
        }
        assert!(
            out.contains("to: read by agent-00, agent-01 — unread by agent-02"),
            "{out}"
        );
    }

    /// Two distinct messages in one thread still render as two.
    #[test]
    fn grouping_collapses_recipients_not_messages() {
        let mut all = broadcast("pact-wisp-aaa", "the question", &["alpha", "bravo"], &[]);
        all.extend(broadcast("pact-wisp-bbb", "the answer", &["msg-fix"], &[]));
        let out = render_full(&all.iter().collect::<Vec<_>>());
        assert_eq!(out.matches("\n---\n").count(), 1, "{out}");
        assert!(
            out.contains("the question") && out.contains("the answer"),
            "{out}"
        );
    }

    #[test]
    fn brief_shows_the_head_of_the_body_and_how_to_get_the_rest() {
        let body = (0..12).map(|i| format!("line {i}\n")).collect::<String>();
        let fanned = broadcast("pact-wisp-aaa", &body, &["alpha"], &[]);
        let out = render_brief(&fanned.iter().collect::<Vec<_>>());
        assert!(out.contains("subject: a subject"), "{out}");
        assert!(out.contains("from: msg-fix"), "{out}");
        assert!(out.contains("line 4") && !out.contains("line 5"), "{out}");
        assert!(out.contains("… 7 more line(s)"), "{out}");
        assert!(out.contains("pact msg read pact-wisp-aaa"), "{out}");

        // A body that fits is shown whole, with no dangling "more lines" tail.
        let short = broadcast("pact-wisp-bbb", "one line", &["alpha"], &[]);
        let out = render_brief(&short.iter().collect::<Vec<_>>());
        assert!(
            out.contains("one line") && !out.contains("more line(s)"),
            "{out}"
        );
    }

    fn agent_info(name: &str, leases: usize, sent: usize, received: usize) -> agents::AgentInfo {
        agents::AgentInfo {
            name: name.to_string(),
            // Now, not a fixed date: this fixture means "an agent that
            // exists", and a hardcoded stamp silently ages past the
            // suggestion horizon and changes what the test is asserting.
            last_seen: chrono::Utc::now().to_rfc3339(),
            leases_held: leases,
            lease_events: 0,
            messages_sent: sent,
            messages_received: received,
            name_valid: true,
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
                ttl: "900".to_string(),
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
                skip: Vec::new(),
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
        assert_eq!(subcommand_name(&Command::Doctor { fix: false }), "doctor");
    }
}
