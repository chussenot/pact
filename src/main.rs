mod agents_md;
mod beads;
mod identity;
mod lease;
mod msg;
mod output;
mod repo;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// pact: a dependency-light CLI that coordinates multiple coding agents
/// working on the same repository (onboarding, messaging, leases).
#[derive(Parser)]
#[command(name = "pact", version, about)]
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
    /// Check that pact, AGENTS.md, and the Beads CLI are all in a healthy state.
    Doctor,
}

#[derive(Subcommand)]
enum LeaseAction {
    /// Acquire a lease on a path.
    Acquire {
        path: String,
        #[arg(long, default_value_t = lease::DEFAULT_TTL_SECS)]
        ttl: u64,
        /// Force takeover of a non-expired lease.
        #[arg(long)]
        steal: bool,
        #[arg(long)]
        note: Option<String>,
    },
    /// Release a lease you hold.
    Release {
        path: String,
        /// Release even if held by another agent.
        #[arg(long)]
        force: bool,
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
    /// Send a message to another agent.
    Send {
        #[arg(long)]
        to: String,
        /// Reply within an existing thread; omit to start a new one.
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        body: String,
    },
    /// List messages addressed to you.
    Inbox {
        #[arg(long)]
        unread_only: bool,
    },
    /// Read a message (or its thread) by id.
    Read { id: String },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(output::code_for(&e));
    }
}

fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;

    match cli.command {
        Command::Init { print } => run_init(&cwd, print, cli.json),
        Command::Lease { action } => run_lease(&cwd, cli.agent.as_deref(), cli.json, action),
        Command::Msg { action } => run_msg(&cwd, cli.agent.as_deref(), cli.json, action),
        Command::Doctor => run_doctor(&cwd, cli.json),
    }
}

fn run_init(cwd: &std::path::Path, print: bool, json: bool) -> Result<()> {
    if print {
        print!("{}", agents_md::managed_block());
        return Ok(());
    }
    let root = repo::find_repo_root(cwd)?;
    repo::pact_dir(&root)?;
    let path = agents_md::apply(&root)?;
    agents_md::ensure_gitignore(&root)?;
    output::emit(
        json,
        &path,
        |p: &PathBuf| format!("updated {}", p.display()),
    );
    Ok(())
}

fn run_lease(
    cwd: &std::path::Path,
    agent_flag: Option<&str>,
    json: bool,
    action: LeaseAction,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    match action {
        LeaseAction::Acquire {
            path,
            ttl,
            steal,
            note,
        } => {
            let agent = identity::resolve_agent(agent_flag)?;
            let outcome = lease::acquire(&root, &agent, &path, ttl, steal, note)?;
            output::emit(json, &outcome, |o| {
                if o.stolen {
                    format!("stolen lease on {} for {}", o.lease.path, o.lease.agent)
                } else {
                    format!("acquired lease on {} for {}", o.lease.path, o.lease.agent)
                }
            });
            Ok(())
        }
        LeaseAction::Release { path, force } => {
            let agent = identity::resolve_agent(agent_flag)?;
            lease::release(&root, &agent, &path, force)?;
            output::emit(json, &path, |p: &String| format!("released lease on {p}"));
            Ok(())
        }
        LeaseAction::Ls { all } => {
            let entries = lease::list(&root, all)?;
            output::emit(json, &entries, |entries| {
                if entries.is_empty() {
                    return "no active leases".to_string();
                }
                entries
                    .iter()
                    .map(|e| {
                        format!(
                            "{}\t{}\t{}s old\t{}s left{}",
                            e.lease.path,
                            e.lease.agent,
                            e.age_secs,
                            e.remaining_secs,
                            if e.expired { "\t(expired)" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(())
        }
    }
}

fn run_msg(
    cwd: &std::path::Path,
    agent_flag: Option<&str>,
    json: bool,
    action: MsgAction,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    let cli = beads::BeadsCli::locate()?;
    let agent = identity::resolve_agent(agent_flag)?;
    match action {
        MsgAction::Send {
            to,
            thread,
            subject,
            body,
        } => {
            let m = msg::send(
                &cli,
                &root,
                &agent,
                &to,
                thread.as_deref(),
                subject.as_deref(),
                &body,
            )?;
            output::emit(json, &m, |m| format!("sent {} (thread {})", m.id, m.thread));
            Ok(())
        }
        MsgAction::Inbox { unread_only } => {
            let messages = msg::inbox(&cli, &root, &agent, unread_only)?;
            output::emit(json, &messages, |messages| {
                if messages.is_empty() {
                    return "inbox empty".to_string();
                }
                messages
                    .iter()
                    .map(|m| format!("{}\t{}\t{}", m.id, m.subject.as_deref().unwrap_or(""), m.body))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(())
        }
        MsgAction::Read { id } => {
            let thread = msg::read_thread(&cli, &root, &agent, &id)?;
            output::emit(json, &thread, |thread| {
                thread
                    .iter()
                    .map(|m| format!("[{}] {}", m.id, m.body))
                    .collect::<Vec<_>>()
                    .join("\n---\n")
            });
            Ok(())
        }
    }
}

fn run_doctor(_cwd: &std::path::Path, _json: bool) -> Result<()> {
    todo!("checks: git repo, .pact/ present, AGENTS.md block current, bd found+version, stale lock count")
}
