use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::{agents_md, identity, lease, otel, output, repo};

/// Conventional Commits on purpose. `bd init` writes `bd init: initialize …`,
/// which is not a valid conventional subject and makes `cog bump` fail on the
/// whole history — pact must not hand anyone that problem.
const INIT_COMMIT_MESSAGE: &str = "chore(pact): sync the coordination protocol block

Written by `pact init`: the managed block in AGENTS.md, the pointer back to it
in every agent-instruction file this repo already has (CLAUDE.md, GEMINI.md,
copilot-instructions.md, …), and the .pact/ line in .gitignore.";

/// Refuse to write through a live lease another agent holds on one of the
/// files `init` is about to rewrite (pact-m7j.9.3): `AGENTS.md` itself tells
/// every agent to lease everything it writes, and `init` rewrites exactly
/// that kind of shared, multi-writer file without ever having checked before
/// this existed. A peek, not an acquire: init does not need to hold a lease
/// to do a bounded rewrite-and-exit, only to refuse when someone else already
/// holds one — the same asymmetry `lease::peek` exists for elsewhere.
pub(super) fn refuse_if_a_target_is_leased(
    root: &Path,
    targets: &[PathBuf],
    agent_flag: Option<&str>,
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }
    let held = lease::peek(root, false)?;
    if held.is_empty() {
        return Ok(());
    }
    // Resolved lazily, and its absence is not itself an error: the common
    // bootstrap case (no PACT_AGENT set, nothing leased on any target) must
    // keep working exactly as before. Only reached at all when something IS
    // held, and even then only matters for telling "my own re-entrant hold"
    // apart from "someone else's" below.
    let self_agent = identity::resolve_agent(agent_flag).ok();
    for target in targets {
        let Ok(relative) = target.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy();
        // Compared as lock keys, not raw strings: a lease taken as `agents.md`
        // on a case-insensitive filesystem is the same lock as `AGENTS.md`,
        // and comparing the raw `LeaseInfo.path` spellings would miss it.
        let key = lease::encode_path(&relative);
        let Some(entry) = held
            .iter()
            .find(|e| lease::encode_path(&e.lease.path) == key)
        else {
            continue;
        };
        // Our own lease, re-entrant: an agent that followed the protocol and
        // leased the file it is about to `init` over must not be refused for
        // doing exactly that. Unresolved identity does NOT get this pass —
        // it cannot be proven to be the holder, so it is treated as a peer.
        if self_agent.as_deref() == Some(entry.lease.agent.as_str()) {
            continue;
        }
        return Err(output::exit_with(
            2,
            format!(
                "lease on {relative} is held by {}; refusing to let `pact init` write through it \
                 (use --force to override)",
                entry.lease.agent
            ),
        ));
    }
    Ok(())
}

pub(in crate::cli) fn run_init(
    cwd: &Path,
    print: bool,
    no_commit: bool,
    force: bool,
    agent_flag: Option<&str>,
    json: bool,
) -> Result<()> {
    if print {
        // `--json` has to be honoured here too. It used to fall through and emit
        // raw markdown at exit 0, so `pact init --print --json | jq` failed to
        // parse while pact reported success — the same shape as the closed-pipe
        // println! bug the house rules exist for: the side effect looks fine and
        // the report lies (pact-3dz).
        //
        // Through output::line either way, so `init --print | head` cannot panic
        // on a closed pipe (pact-rnc.26). trim_end because line() supplies the
        // trailing newline the block already ends with.
        let block = agents_md::managed_block();
        #[derive(serde::Serialize)]
        struct PrintReport<'a> {
            block: &'a str,
        }
        output::emit(
            json,
            &PrintReport {
                block: block.trim_end(),
            },
            |r: &PrintReport| r.block.to_string(),
        );
        return Ok(());
    }
    let root = repo::find_repo_root(cwd)?;
    repo::pact_dir(&root)?;

    // Checked before any write, not per-file inside `apply`/`ensure_*`: a
    // partial rewrite (AGENTS.md updated, then refused on CLAUDE.md) would be
    // worse than the all-or-nothing this mirrors from `acquire_many`. Covers
    // every file `run_init` writes below, not just the two the bug report
    // named: `.gitignore`/`.gitattributes` are managed writes too.
    let mut candidates = vec![
        root.join("AGENTS.md"),
        root.join("CLAUDE.md"),
        root.join(".gitignore"),
        root.join(".gitattributes"),
    ];
    candidates.extend(agents_md::managed_instruction_files(&root));
    refuse_if_a_target_is_leased(&root, &candidates, agent_flag, force)?;

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
        // Paired with the narrowed ignore above: `events.jsonl` is now committed,
        // and an append-only file that git merges line-by-line conflicts on every
        // branch that touched it.
        agents_md::ensure_gitattributes(&root)?;
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
