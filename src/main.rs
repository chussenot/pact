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
    },
    /// Show what pact resolved: identity, paths, and the bd it will use.
    Whoami,
    /// List the agent identities seen in this repo's leases and messages.
    Agents,
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
        #[arg(long, required = true)]
        to: Vec<String>,
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

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        output::warn(&format!("error: {e:#}"));
        std::process::exit(output::code_for(&e));
    }
}

fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;

    match cli.command {
        Command::Init { print } => run_init(&cwd, print, cli.json),
        Command::Whoami => run_whoami(&cwd, cli.agent.as_deref(), cli.json),
        Command::Agents => run_agents(&cwd, cli.json),
        Command::Lease { action } => run_lease(&cwd, cli.agent.as_deref(), cli.json, action),
        Command::Msg { action } => run_msg(&cwd, cli.agent.as_deref(), cli.json, action),
        Command::Log { limit } => run_log(&cwd, cli.json, limit),
        Command::Doctor => run_doctor(&cwd, cli.json),
        #[cfg(feature = "ui")]
        Command::Ui => {
            let root = repo::find_repo_root(&cwd)?;
            let agent = identity::resolve_agent(cli.agent.as_deref()).ok();
            tui::run(root, agent)
        }
    }
}

fn run_init(cwd: &Path, print: bool, json: bool) -> Result<()> {
    if print {
        // Through output::line like everything else, so `init --print | head`
        // cannot panic on a closed pipe (pact-rnc.26). trim_end because line()
        // supplies the trailing newline the block already ends with.
        output::line(agents_md::managed_block().trim_end());
        return Ok(());
    }
    let root = repo::find_repo_root(cwd)?;
    repo::pact_dir(&root)?;
    let path = agents_md::apply(&root)?;
    // Claude Code never loads AGENTS.md, so writing only that file left a
    // Claude-driven fleet with no protocol at all (see `ensure_claude_md`).
    let claude = agents_md::ensure_claude_md(&root)?;
    agents_md::ensure_gitignore(&root)?;

    #[derive(serde::Serialize)]
    struct InitReport {
        agents_md: PathBuf,
        /// `null` when `CLAUDE.md` is `AGENTS.md` under another name.
        claude_md: Option<PathBuf>,
        claude_md_status: &'static str,
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

    let report = InitReport {
        agents_md: path,
        claude_md,
        claude_md_status,
    };
    output::emit(json, &report, |r: &InitReport| {
        format!("updated {}\n{claude_line}", r.agents_md.display())
    });
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
fn bd_health(bd: &beads::BeadsCli, root: &Path) -> (Option<String>, Vec<String>) {
    let mut problems = Vec::new();
    let version = match bd.version(root) {
        Ok(v) => Some(v),
        Err(e) => {
            problems.push(format!("bd found but not runnable: {e:#}"));
            None
        }
    };
    if let Err(e) = bd.run(root, &["list", "--include-infra", "--json"]) {
        problems.push(format!(
            "bd cannot read this repo's beads database, so `pact msg` and the message \
             half of `pact agents` will fail: {e:#}"
        ));
    }
    (version, problems)
}

fn run_agents(cwd: &Path, json: bool) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
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
            output::emit(json, &entries, |entries: &Vec<lease::LeaseEntry>| {
                render_leases(entries)
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
                thread.as_deref(),
                subject.as_deref(),
                &body,
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

fn run_doctor(cwd: &Path, json: bool) -> Result<()> {
    // Without a repo root none of the other checks mean anything, so this one
    // is a hard prerequisite rather than a soft check: propagate its exit
    // code (4) straight through instead of folding it into the report.
    let root = repo::find_repo_root(cwd)?;
    let report = doctor::checks(&root);

    output::emit(json, &report, |r| {
        let mut lines: Vec<String> = r
            .checks
            .iter()
            .map(|c| format!("{} {}: {}", if c.ok { "✓" } else { "✗" }, c.name, c.detail))
            .collect();
        lines.push(String::new());
        lines.push(if r.healthy {
            "all checks passed".to_string()
        } else {
            "some checks failed".to_string()
        });
        lines.join("\n")
    });

    if report.healthy {
        Ok(())
    } else {
        std::process::exit(1);
    }
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

    #[test]
    fn table_pads_all_but_the_last_column() {
        let out = table(&[
            vec!["a".into(), "x".into()],
            vec!["longer".into(), "y".into()],
        ]);
        assert_eq!(out, "a       x\nlonger  y");
    }
}
