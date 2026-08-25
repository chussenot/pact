use crate::{audit, lease};

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

mod commands;
mod util;

#[cfg(feature = "mcp")]
use commands::run_mcp;
#[cfg(feature = "ui")]
use commands::run_ui;
use commands::{
    run_agents, run_audit, run_completion, run_context_set, run_doctor, run_handoff, run_init,
    run_lease, run_log, run_merge, run_msg, run_plan_lint, run_watch, run_whoami, AuditArgs,
};

/// `tui` renders the same activity feed `pact log` does and reaches for these
/// two through the crate root; main.rs re-exports them from here, so the path it
/// already used still resolves.
#[cfg(feature = "ui")]
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

/// How much weight the next agent should give a handoff.
///
/// **The same three tiers `recount` reports for testimony**, and that is a
/// contract rather than a coincidence. A fleet that reads `confidence: medium` on
/// an inherited finding and `confidence: medium` on a joined transcript should be
/// reading one scale, not two it has to convert between in its head. If recount's
/// vocabulary moves, this moves with it.
///
/// Rendered in words wherever it is shown. A number would invite arithmetic on
/// something nobody measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Confidence {
    /// Verified: tested, reproduced, or read directly from the source.
    High,
    /// Consistent with what was seen, not independently confirmed.
    Medium,
    /// A lead worth checking, and worth doubting.
    Low,
}

impl Confidence {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
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
    /// The sequence a fleet has to run by hand, as one auditable command: take
    /// the reserved key, merge with `--no-ff`, sign the merge commit with
    /// `Pact-Agent` (which `git merge` cannot do on its own), run `--verify`, and
    /// release.
    ///
    /// `--verify` asks whether YOUR merge ADDED a failure, not whether the branch
    /// is green. Arriving to a branch that is already failing for somebody else's
    /// reason, it lands your work anyway, says so, and releases the mutex — **a
    /// red shared branch is never a reason to hold a finished merge.** Only a
    /// failure your merge introduced is reverted, and only then is the mutex
    /// DELIBERATELY kept, so no peer merges onto a branch that has just failed its
    /// own oracle.
    ///
    /// It merges into the branch you are CURRENTLY on, which decides who can run
    /// it. Under the one-worktree-per-agent topology pact recommends, somebody is
    /// sitting in the main checkout on the shared branch, and git refuses any
    /// other worktree a checkout of it — so an agent in a worktree cannot get
    /// onto the target branch and cannot self-merge. There, the merges are the
    /// orchestrator's, from the main checkout. Self-merge is for fleets where
    /// nobody holds the shared branch. See docs/fleet-patterns.md.
    Merge {
        /// The branches to merge into the current one.
        ///
        /// Several are merged under ONE mutex and proved by ONE `--verify` run,
        /// because a pair can be atomically coupled: `plan lint` guarantees
        /// intra-wave FILE disjointness, and one agent adding a struct field
        /// while another constructs that struct in a test is file-disjoint and
        /// cannot land apart. On failure all of them are reverted together.
        #[arg(required = true, num_args = 1..)]
        branches: Vec<String>,
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
    /// Leave findings for whoever picks up the beads that depend on this one.
    ///
    /// Resolves `<bead>`'s dependents from the graph `pact plan lint` snapshotted
    /// and posts one message per dependent onto that dependent's own thread —
    /// addressed to the WORK, because the agent who inherits it frequently does
    /// not exist yet. They read it with `pact msg thread bead:<their-id>`.
    ///
    /// Never blocks and never gates a close: this is inheritance, not ceremony. A
    /// bead with nothing to say sends nothing, and `pact audit --check
    /// handoff-coverage` is where that shows up.
    Handoff {
        /// The bead you are finishing.
        bead: String,
        /// How much weight the next agent should give this.
        ///
        /// The same three tiers `recount` reports for testimony, deliberately, so
        /// a fleet's evidence and its inheritance are graded on one scale rather
        /// than two that have to be mentally converted.
        #[arg(long, value_enum)]
        confidence: Confidence,
        /// What you found. `@path` reads a file — quotes, backslashes and aligned
        /// tables do not survive a shell, and a handoff is exactly that kind of
        /// content.
        #[arg(long)]
        findings: String,
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
        /// Count `--check gate-order` violations toward the exit code.
        ///
        /// For CI, where somebody has decided this fleet's declared gates ARE a
        /// rule. pact does not decide that on their behalf: a gate is a
        /// declaration a plan made about itself, and a violated one is as often a
        /// finding about the plan as about the agent — work that ran early and
        /// turned out fine means the gate was declared over something that did not
        /// depend on it. So by default it reports and exits 0.
        #[arg(long)]
        strict: bool,
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
        ///
        /// The bead this hold is for. Recorded as its own field on the lease
        /// event, and placed at the front of the note so `lease ls`, `pact log`
        /// and `agents --for` show it exactly as a hand-written one.
        ///
        /// `--bead x-4xh --note "split the audit table"` is the same lease as
        /// `--note "x-4xh: split the audit table"`, which is what every fleet
        /// types by hand. The flag makes the convention un-misspellable.
        #[arg(long)]
        bead: Option<String>,
        /// Wait up to this long for a path somebody else is holding, instead of
        /// exiting 2 immediately. Same duration grammar as `--ttl`.
        ///
        /// The wait happens INSIDE this command, which is the whole point. A
        /// subagent's process is its turn loop: ending the turn to wait for a
        /// notification is operationally identical to exiting, and nothing can
        /// re-enter it. Measured on one 12-agent fleet, seven agents parked on
        /// "subscribe and pick up other work", four never resumed, and three
        /// resumed nine hours later only because a human woke the parent session.
        /// Blocking here keeps the turn alive.
        #[arg(long)]
        wait: Option<String>,
        /// Start it with the bead id and a colon — `--note "pact-4xh.7: split
        /// the audit table"`. There is no `--bead` flag; pact reads that id out
        /// of the note when it writes the event and records it as its own field,
        /// which is what `audit --check claim-lease-divergence` cross-checks
        /// against bd. See docs/fleet-patterns.md.
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
    /// Lint a plan manifest: intra-wave file overlap, dependency ordering, cycles,
    /// orphans, hot files. A wave must be a dependency-free set — every entry's
    /// `depends_on` has to live in a STRICTLY earlier wave — and `wave` is an
    /// integer. See docs/plan.md.
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
    /// Read a whole thread by its key, including messages addressed to nobody.
    ///
    /// The route to an inheritance. `pact handoff` posts onto `bead:<id>` with no
    /// recipient, because the agent who will pick that bead up may not exist yet —
    /// so `inbox` cannot show it and only the thread key reaches it.
    Thread {
        /// `bead:<id>`, or any thread key.
        key: String,
    },
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
        Command::Handoff { .. } => "handoff",
        Command::Msg { action } => match action {
            MsgAction::Send { .. } => "msg send",
            MsgAction::Inbox { .. } => "msg inbox",
            MsgAction::Sent => "msg sent",
            MsgAction::Read { .. } => "msg read",
            MsgAction::Thread { .. } => "msg thread",
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
        Command::Handoff {
            bead,
            confidence,
            findings,
        } => run_handoff(
            &cwd,
            cli.agent.as_deref(),
            cli.json,
            &bead,
            confidence,
            &findings,
        )
        .map(|()| 0),
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
            branches,
            verify,
            ttl,
            allow_dirty,
        } => run_merge(
            &cwd,
            cli.agent.as_deref(),
            cli.json,
            &branches,
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
            strict,
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
                strict,
            },
        ),
        #[cfg(feature = "ui")]
        Command::Ui => run_ui(&cwd, cli.agent.as_deref()),
        #[cfg(feature = "mcp")]
        Command::Mcp { action } => run_mcp(&cwd, action),
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
                bead: None,
                wait: None,
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
