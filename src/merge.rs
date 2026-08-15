//! `pact merge` — take the merge mutex, merge, prove it, release (pact-08h).
//!
//! ## Why this is a subcommand and not four lines of prose
//!
//! A fleet with no orchestrator has to serialize its own writes to the shared
//! branch. The convention that emerged — and it WORKS; the millrace run's
//! `--check double-win` is clean over 63 events with eight self-merges — is a
//! reserved lease key held across merge-then-test:
//!
//! ```text
//! pact lease acquire .pact/internal/merge-to-master --note "..."
//! git merge --no-ff <branch>
//! cargo test --release          # on master, AFTER the merge
//! # red: revert, KEEP the mutex until green, file a bead
//! pact lease release .pact/internal/merge-to-master
//! ```
//!
//! Three defects follow from that being prose rather than code, and none of
//! them is that agents got it wrong:
//!
//! 1. **The merge commit is the one commit nobody can sign.** `git merge` has
//!    `--signoff` and no `--trailer` — checked against git 2.53 — so an agent
//!    told to run `git commit --trailer Pact-Agent=$PACT_AGENT` signs every
//!    commit it authors and cannot sign the merge. In the millrace run all 13
//!    work commits carried the trailer, across five identities, and all six
//!    merge commits did not. `pact audit --check commit-correlation` exits 1 on
//!    exactly that. The commit that changed the shared branch, under a mutex,
//!    that the audit most wants to attribute, is structurally the one that
//!    could not be attributed. This module merges with `--no-commit` and then
//!    commits, which is the only way to get a trailer onto a merge.
//!
//! 2. **The mutex is held for the length of a test run.** Measured holds:
//!    25-64s, median ~37s. That is not lock overhead, it is the oracle. Nothing
//!    in the prose told a waiting agent that what it waits behind is a test
//!    suite, so the wait looked like a stuck peer.
//!
//! 3. **The red path is the least-exercised and most dangerous.** "Revert, and
//!    KEEP the mutex until green" never fired in the pilot — untested prose an
//!    agent must execute correctly while the shared branch is broken and peers
//!    are blocked behind it. Here it is code, with a test.
//!
//! ## What this deliberately does not do
//!
//! Checkpoint rotation (committing `.pact/events.jsonl` and
//! `.pact/messages.jsonl` when they are stale) is a chore the annex gives to
//! whoever holds the mutex, and this is the natural home for it. It is left out
//! on purpose: committing files the caller did not name, inside a command whose
//! job is one merge, is a surprise — and a surprise inside the one command that
//! runs while a shared branch is half-written is the wrong place to be clever.
//! It belongs in its own command that this one could call.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::{lease, output, repo};

/// The reserved key that serializes merges into `into`.
///
/// Derived from the target branch rather than hardcoded to `master`, so a repo
/// on `main` gets its own key and two branches can be merged into
/// independently. On `master` it produces exactly the key the millrace annex
/// names by hand, so existing logs stay comparable.
pub fn mutex_key(into: &str) -> String {
    format!("{}merge-to-{into}", lease::MUTEX_PREFIX)
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeOutcome {
    pub agent: String,
    /// The branch that was merged in.
    pub branch: String,
    /// The branch it was merged into.
    pub into: String,
    /// The reserved key held across the operation.
    pub mutex: String,
    /// The merge commit, absent when there was nothing to merge.
    pub merge_commit: Option<String>,
    pub already_up_to_date: bool,
    /// The merge landed on a branch that was ALREADY failing, having added no
    /// new failure of its own. Distinct from `verified: Some(false)` alone,
    /// which would read as "this merge broke it".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub landed_on_red: bool,
    /// `None` when no `--verify` command was given — which is not the same as
    /// passing, and is why this is a three-state field rather than a bool.
    pub verified: Option<bool>,
}

/// Which failures keep the mutex, and which give it back.
///
/// The distinction is the whole point of the red path: a broken shared branch
/// must stay locked so no peer merges on top of it, while a merge that never
/// landed must not leave the fleet blocked behind a lock protecting nothing.
enum Failure {
    /// The branch is broken and the caller still holds the mutex.
    HoldingBroken(anyhow::Error),
    /// Nothing landed; release and get out of the way.
    Clean(anyhow::Error),
}

/// Run a git command, capturing output.
fn git(repo_root: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("could not run git {}", args.join(" ")))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Tracked changes in the working tree, ignoring untracked files.
///
/// The red path resets `--hard`, which would take uncommitted work with it. A
/// merge onto a dirty tree is refused rather than risked: git itself would
/// refuse a conflicting merge, but it would happily merge around unrelated
/// dirty files that the later reset then destroys. Untracked files are exempt
/// because `reset --hard` does not touch them.
fn dirty_tracked(repo_root: &Path) -> Result<Vec<String>> {
    let (_, stdout, _) = git(
        repo_root,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    Ok(stdout.lines().map(str::to_string).collect())
}

/// One run of the verification command.
struct Verified {
    passed: bool,
    /// The names of the tests that failed, when the output was in a shape this
    /// recognises. Empty for a failure it could not attribute — which is why
    /// `passed` is carried separately and never inferred from emptiness.
    failures: BTreeSet<String>,
}

/// Run the verification command, echoing its output and remembering what failed.
///
/// Output is captured rather than inherited so the failing-test set can be
/// compared against the pre-merge base, then printed in full — an agent watching
/// a long suite still sees everything, just at the end rather than streaming.
fn run_verify(repo_root: &Path, verify: &str) -> Result<Verified> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(verify)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run the verification command: {verify}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    output::line(&text);
    Ok(Verified {
        passed: out.status.success(),
        failures: failing_tests(&text),
    })
}

/// Test names from a libtest-style run: `test some::name ... FAILED`.
///
/// Deliberately narrow. A verification command can be anything, and guessing at
/// an unfamiliar format would produce a *wrong* failure set — which, compared
/// against a baseline, decides whether an agent's work is reverted. An
/// unrecognised format yields an empty set, and an empty set never satisfies the
/// subset test below unless the base failed identically, so the conservative
/// path is the one taken when this cannot read the output.
fn failing_tests(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let name = l.strip_prefix("test ")?.strip_suffix(" ... FAILED")?;
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// Redo a merge that was reverted for somebody else's breakage.
fn reapply(repo_root: &Path, branch: &str, into: &str, agent: &str) -> Result<String> {
    let (ok, _, stderr) = git(repo_root, &["merge", "--no-ff", "--no-commit", branch])?;
    if !ok {
        let _ = git(repo_root, &["merge", "--abort"]);
        bail!("could not re-apply the merge after the baseline check: {stderr}");
    }
    let (ok, _, stderr) = git(
        repo_root,
        &[
            "commit",
            "-m",
            &format!("Merge {branch} into {into}"),
            "--trailer",
            &format!("Pact-Agent={agent}"),
        ],
    )?;
    if !ok {
        let _ = git(repo_root, &["merge", "--abort"]);
        bail!("could not commit the re-applied merge: {stderr}");
    }
    head(repo_root)
}

fn head(repo_root: &Path) -> Result<String> {
    let (ok, stdout, stderr) = git(repo_root, &["rev-parse", "HEAD"])?;
    if !ok {
        bail!("could not resolve HEAD: {}", stderr.trim());
    }
    Ok(stdout.trim().to_string())
}

/// Merge `branch` into the current branch under the merge mutex, prove it, and
/// release.
///
/// On a failing verification the merge is reverted and **the mutex is kept**:
/// the error says so, and the caller is expected to fix the branch and release
/// it by hand. Every other failure releases.
pub fn merge(
    repo_root: &Path,
    agent: &str,
    branch: &str,
    verify: Option<&str>,
    ttl_secs: u64,
) -> Result<MergeOutcome> {
    let ctx = repo::RepoContext::resolve(repo_root);
    let into = ctx.branch().context(
        "HEAD is detached, so there is no branch to merge into. \
         `pact merge` serializes writes to a shared branch; check one out first.",
    )?;

    // Before the mutex, not after: refusing here costs nobody a wait, while
    // refusing while holding would block every peer for the length of the
    // diagnosis.
    let dirty = dirty_tracked(repo_root)?;
    if !dirty.is_empty() {
        bail!(
            "the working tree has uncommitted tracked changes, and a failed \
             verification resets --hard:\n{}\n\ncommit or stash them first — \
             `pact merge` will not risk work it did not create.",
            dirty.join("\n")
        );
    }

    let key = mutex_key(&into);
    let before = head(repo_root)?;

    // Contention raises with pact's own exit 2 and its holder/remaining
    // reporting, so a refusal here is indistinguishable from `lease acquire`'s
    // — the same message, the same code, the same advice to subscribe.
    lease::acquire(
        repo_root,
        agent,
        &key,
        ttl_secs,
        false,
        Some(format!("merging {branch} into {into}")),
    )?;

    match merge_while_held(repo_root, agent, branch, &into, verify, &before) {
        Ok(outcome) => {
            let mut outcome = outcome;
            outcome.mutex = key.clone();
            // Release only on the way out of a GREEN merge. The lease's own
            // release path writes the event and notifies watchers, which since
            // pact-bsf includes telling anyone waiting on this key that it is
            // free.
            let _ = lease::release(repo_root, agent, &key, false);
            Ok(outcome)
        }
        Err(Failure::Clean(e)) => {
            let _ = lease::release(repo_root, agent, &key, false);
            Err(e)
        }
        Err(Failure::HoldingBroken(e)) => Err(e),
    }
}

/// The body that runs with the mutex held. Split out so that every exit from it
/// is forced to say, by its `Failure` variant, what happens to the lock.
fn merge_while_held(
    repo_root: &Path,
    agent: &str,
    branch: &str,
    into: &str,
    verify: Option<&str>,
    before: &str,
) -> std::result::Result<MergeOutcome, Failure> {
    let mut outcome = MergeOutcome {
        agent: agent.to_string(),
        branch: branch.to_string(),
        into: into.to_string(),
        mutex: String::new(),
        merge_commit: None,
        already_up_to_date: false,
        landed_on_red: false,
        verified: None,
    };

    // `--no-commit` is not a style choice: it is the only way a trailer reaches
    // a merge commit, because `git merge` has no `--trailer`. `--no-ff` keeps
    // the merge commit itself, which is what carries the attribution.
    let (ok, _, stderr) =
        git(repo_root, &["merge", "--no-ff", "--no-commit", branch]).map_err(Failure::Clean)?;
    if !ok {
        // Leave no half-merged tree behind for the next agent to find.
        let _ = git(repo_root, &["merge", "--abort"]);
        return Err(Failure::Clean(anyhow::anyhow!(
            "merging {branch} into {into} failed, and the merge was aborted:\n{}",
            stderr.trim()
        )));
    }

    // Robust against locale: a real merge leaves MERGE_HEAD, an up-to-date one
    // does not. Matching git's English output would break under LANG.
    let (has_merge_head, _, _) =
        git(repo_root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).map_err(Failure::Clean)?;
    if !has_merge_head {
        outcome.already_up_to_date = true;
        return Ok(outcome);
    }

    let (ok, _, stderr) = git(
        repo_root,
        &[
            "commit",
            "-m",
            &format!("Merge {branch} into {into}"),
            "--trailer",
            &format!("Pact-Agent={agent}"),
        ],
    )
    .map_err(Failure::Clean)?;
    if !ok {
        let _ = git(repo_root, &["merge", "--abort"]);
        return Err(Failure::Clean(anyhow::anyhow!(
            "the merge staged cleanly but the commit failed:\n{}",
            stderr.trim()
        )));
    }
    outcome.merge_commit = head(repo_root).ok();

    let Some(verify) = verify else {
        return Ok(outcome);
    };

    let post = match run_verify(repo_root, verify) {
        Ok(v) => v,
        Err(e) => {
            // The command could not even start, so nothing was proved. Undo the
            // merge and give the lock back: this is not a broken branch.
            let _ = git(repo_root, &["reset", "--hard", before]);
            return Err(Failure::Clean(e));
        }
    };

    if post.passed {
        outcome.verified = Some(true);
        return Ok(outcome);
    }

    // Red — but is it red because of THIS merge?
    //
    // The first version of this gate asked "is the branch green?", which is the
    // right question only when the branch was green to begin with. Under
    // sustained upstream breakage it inverts: an agent merging unrelated work
    // onto an already-red master fails a verification it did not break, has its
    // good work reverted, and is handed a mutex it did not earn. Since the fixes
    // FOR the breakage are themselves merges, and merges are gated on green,
    // master can never recover. Measured on a live fleet: three saboteur
    // regressions inside 15 minutes, then 25 minutes in which not one agent
    // merge landed.
    //
    // So ask the question that was always meant: did I make it worse? Re-run the
    // verification on the pre-merge base and compare which tests failed. The
    // baseline run costs a second test cycle, but only on the red path — a green
    // merge, which is the common case, still runs the suite exactly once.
    let baseline = match git(repo_root, &["reset", "--hard", before]) {
        Ok((true, _, _)) => run_verify(repo_root, verify).ok(),
        _ => None,
    };

    if let Some(base) = &baseline {
        // `!post.failures.is_empty()` is load-bearing: the empty set is a subset
        // of everything, so without it a verification whose output this cannot
        // attribute — `exit 1` with no libtest report — would satisfy the check
        // trivially and land on a red branch. Unattributable means unproven, and
        // unproven takes the conservative path.
        if !base.passed && !post.failures.is_empty() && post.failures.is_subset(&base.failures) {
            // Not this agent's doing. Re-apply the merge and get out of the way:
            // blocking every unrelated change behind somebody else's breakage is
            // the livelock this branch exists to prevent.
            match reapply(repo_root, branch, into, agent) {
                Ok(commit) => {
                    outcome.merge_commit = Some(commit);
                    outcome.verified = Some(false);
                    outcome.landed_on_red = true;
                    output::warn(&format!(
                        "note: {into} was ALREADY failing before this merge, and this merge \
                         added no new failure, so it was landed rather than reverted. \
                         {} test(s) are failing and none of them are yours. The mutex is \
                         released. Somebody still has to fix {into} — check whether a bead \
                         already exists for it before filing another.",
                        base.failures.len()
                    ));
                    return Ok(outcome);
                }
                Err(e) => return Err(Failure::HoldingBroken(e)),
            }
        }
    }

    // Either the base was green, or this merge added failures the base did not
    // have. Both mean the merger owns it. Undo, and KEEP the mutex: until
    // somebody proves this branch green again, no peer should merge on top.
    let new_failures: Vec<&str> = match &baseline {
        Some(base) => post
            .failures
            .difference(&base.failures)
            .map(String::as_str)
            .collect(),
        None => Vec::new(),
    };
    let added = if new_failures.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nFailures this merge ADDED, which the base did not have:\n  {}\n",
            new_failures.join("\n  ")
        )
    };
    let (reset_ok, _, reset_err) = match git(repo_root, &["reset", "--hard", before]) {
        Ok(t) => t,
        Err(e) => return Err(Failure::HoldingBroken(e)),
    };
    let reverted = if reset_ok {
        format!("the merge was reverted; {into} is back at {before}.")
    } else {
        format!(
            "AND THE REVERT FAILED ({}). {into} may still carry the merge — \
             fix it by hand before releasing.",
            reset_err.trim()
        )
    };

    Err(Failure::HoldingBroken(anyhow::anyhow!(
        "verification failed after merging {branch} into {into}, so {reverted}{added}\n\
         \n\
         YOU STILL HOLD THE MERGE MUTEX, deliberately: a peer merging onto a \
         branch that has just failed its own oracle would bury the cause. Fix \
         the branch, or file a bead describing what it broke, then release with:\n\
         \n\
         \x20   pact lease release {}\n",
        mutex_key(into)
    )))
}

/// Human rendering, shared by the CLI so `--json` and the plain output cannot
/// describe different things.
pub fn describe(o: &MergeOutcome) -> String {
    if o.already_up_to_date {
        return format!("{} was already up to date with {}", o.into, o.branch);
    }
    let commit = o.merge_commit.as_deref().unwrap_or("(unknown)");
    let short = commit.get(..12).unwrap_or(commit);
    let proof = match (o.verified, o.landed_on_red) {
        (Some(true), _) => "verified".to_string(),
        (Some(false), true) => {
            format!(
                "{} was already failing; this merge added nothing new",
                o.into
            )
        }
        (Some(false), false) => "NOT verified".to_string(),
        (None, _) => "unverified (no --verify given)".to_string(),
    };
    format!(
        "merged {} into {} as {short}, signed Pact-Agent={} — {proof}",
        o.branch, o.into, o.agent
    )
}

/// Warn when a merge landed on a shared branch with nothing proving it.
pub fn warn_if_unproven(o: &MergeOutcome) {
    if o.verified.is_none() && !o.already_up_to_date {
        output::warn(
            "note: nothing verified this merge. The mutex exists so the branch can be \
             proved green while it is held — pass --verify '<command>' (for example \
             --verify 'cargo test --release') so a red merge is caught here rather \
             than by the next agent.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repository, because every one of these paths is a git operation
    /// and a fake would only prove the mock agrees with itself.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for args in [
            vec!["init", "-q", "-b", "master"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            git(root, &args).unwrap();
        }
        std::fs::write(root.join("f.txt"), "base\n").unwrap();
        // `pact init` writes a `.pact/*` rule for exactly this reason, and
        // without it the `git add -A` below tracks pact's own runtime state —
        // locks and event logs — onto a feature branch, which then collides
        // with the untracked copies on master. These tests are about merge
        // mechanics, so the whole directory is ignored rather than mirroring
        // init's re-inclusion of the two committable logs.
        std::fs::write(root.join(".gitignore"), ".pact/\n").unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", "base"]).unwrap();
        tmp
    }

    /// A branch off HEAD that changes `f.txt`.
    fn branch_with_change(root: &Path, name: &str, content: &str) {
        git(root, &["checkout", "-q", "-b", name]).unwrap();
        std::fs::write(root.join("f.txt"), content).unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", &format!("{name} change")]).unwrap();
        git(root, &["checkout", "-q", "master"]).unwrap();
    }

    fn subject_and_trailer(root: &Path, rev: &str) -> String {
        let (_, out, _) = git(
            root,
            &[
                "log",
                "-1",
                "--format=%s|%(trailers:key=Pact-Agent,valueonly)",
                rev,
            ],
        )
        .unwrap();
        out.trim().to_string()
    }

    #[test]
    fn the_key_is_derived_from_the_target_branch() {
        assert_eq!(mutex_key("master"), ".pact/internal/merge-to-master");
        assert_eq!(mutex_key("main"), ".pact/internal/merge-to-main");
        // And it lands in the reserved namespace the audit recognises.
        assert!(lease::is_mutex(&mutex_key("master")));
    }

    /// The defect this module exists for: the merge commit must carry the
    /// trailer, which `git merge` alone cannot produce.
    #[test]
    fn a_green_merge_signs_its_merge_commit_and_releases_the_mutex() {
        let tmp = repo();
        let root = tmp.path();
        branch_with_change(root, "feature", "changed\n");

        let out = merge(root, "wheelwright", "feature", Some("true"), 600).unwrap();
        assert_eq!(out.verified, Some(true));
        assert!(!out.already_up_to_date);

        let commit = out.merge_commit.clone().unwrap();
        let line = subject_and_trailer(root, &commit);
        assert_eq!(line, "Merge feature into master|wheelwright", "{line}");

        // The mutex is not held any more.
        let held = lease::list(root, true).unwrap();
        assert!(
            !held.iter().any(|l| l.lease.path == mutex_key("master")),
            "a green merge must release: {held:?}"
        );
    }

    /// The red path, which was untested prose. The merge is undone and the lock
    /// is deliberately NOT given back.
    #[test]
    fn a_failed_verification_reverts_and_keeps_the_mutex() {
        let tmp = repo();
        let root = tmp.path();
        branch_with_change(root, "bad", "broken\n");
        let before = head(root).unwrap();

        let err = merge(root, "sluice", "bad", Some("false"), 600).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("YOU STILL HOLD THE MERGE MUTEX"), "{msg}");
        assert!(msg.contains("pact lease release"), "{msg}");

        // The branch is back where it started.
        assert_eq!(head(root).unwrap(), before, "the merge must be undone");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "base\n"
        );

        // And the lock is still held, so no peer merges onto a broken branch.
        let held = lease::list(root, true).unwrap();
        assert!(
            held.iter().any(|l| l.lease.path == mutex_key("master")),
            "a red merge must KEEP the mutex: {held:?}"
        );
    }

    /// Nothing to merge is a success, not a failure, and must not leave a lock
    /// behind protecting a merge that never happened.
    #[test]
    fn an_up_to_date_merge_says_so_and_releases() {
        let tmp = repo();
        let root = tmp.path();
        git(root, &["branch", "stale"]).unwrap();

        let out = merge(root, "fuller", "stale", Some("true"), 600).unwrap();
        assert!(out.already_up_to_date);
        assert!(out.merge_commit.is_none());
        assert!(describe(&out).contains("already up to date"), "{out:?}");

        let held = lease::list(root, true).unwrap();
        assert!(!held.iter().any(|l| l.lease.path == mutex_key("master")));
    }

    /// A dirty tree is refused BEFORE the mutex is taken, because the red path
    /// resets --hard and would take the uncommitted work with it.
    #[test]
    fn a_dirty_tree_is_refused_without_ever_taking_the_lock() {
        let tmp = repo();
        let root = tmp.path();
        branch_with_change(root, "feature", "changed\n");
        std::fs::write(root.join("f.txt"), "local edit not committed\n").unwrap();

        let err = merge(root, "millwright", "feature", Some("true"), 600).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("uncommitted tracked changes"), "{msg}");

        // Untouched, and no lock taken.
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "local edit not committed\n"
        );
        assert!(lease::list(root, true).unwrap().is_empty());
    }

    /// A second agent cannot merge while the first holds the key — the property
    /// the whole convention exists for.
    #[test]
    fn a_peer_is_refused_while_the_mutex_is_held() {
        let tmp = repo();
        let root = tmp.path();
        branch_with_change(root, "bad", "broken\n");

        // Leaves the mutex held, on purpose.
        merge(root, "sluice", "bad", Some("false"), 600).unwrap_err();

        branch_with_change(root, "other", "other\n");
        let err = merge(root, "fuller", "other", Some("true"), 600).unwrap_err();
        assert_eq!(
            output::code_for(&err),
            2,
            "contention is exit 2, as it is for `lease acquire`: {err:#}"
        );
    }

    /// A verification whose output this understands: it prints a libtest-style
    /// report from a tracked file and fails if anything in it FAILED. Lets a
    /// test set the base's failures and the branch's independently.
    const VERIFY: &str = "cat report.txt; ! grep -q FAILED report.txt";

    fn with_report(root: &Path, contents: &str, msg: &str) {
        std::fs::write(root.join("report.txt"), contents).unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", msg]).unwrap();
    }

    /// The livelock this gate was rewritten for. Master is already failing when
    /// the agent arrives; its merge adds no new failure; blocking it would mean
    /// nothing can ever land, INCLUDING the fixes for the failure.
    #[test]
    fn a_merge_that_adds_no_new_failure_lands_on_an_already_red_branch() {
        let tmp = repo();
        let root = tmp.path();
        with_report(
            root,
            "test a ... FAILED\n",
            "master is broken by somebody else",
        );

        git(root, &["checkout", "-q", "-b", "unrelated"]).unwrap();
        std::fs::write(root.join("f.txt"), "unrelated work\n").unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", "unrelated"]).unwrap();
        git(root, &["checkout", "-q", "master"]).unwrap();

        let out = merge(root, "penstock", "unrelated", Some(VERIFY), 600).unwrap();
        assert!(out.landed_on_red, "it must land: {out:?}");
        assert_eq!(out.verified, Some(false), "and must not claim to be proved");
        assert!(out.merge_commit.is_some());

        // The work is really on master, and signed.
        let line = subject_and_trailer(root, "HEAD");
        assert_eq!(line, "Merge unrelated into master|penstock", "{line}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "unrelated work\n"
        );

        // And the mutex is released — holding it would block the fleet behind
        // a breakage this agent did not cause.
        let held = lease::list(root, true).unwrap();
        assert!(
            !held.iter().any(|l| l.lease.path == mutex_key("master")),
            "must not hold the mutex for somebody else's breakage: {held:?}"
        );
    }

    /// The other half: a red base does not become a licence to add failures.
    #[test]
    fn a_merge_that_adds_a_new_failure_is_still_reverted_and_still_holds() {
        let tmp = repo();
        let root = tmp.path();
        with_report(
            root,
            "test a ... FAILED\n",
            "master is broken by somebody else",
        );
        let before = head(root).unwrap();

        git(root, &["checkout", "-q", "-b", "worse"]).unwrap();
        std::fs::write(
            root.join("report.txt"),
            "test a ... FAILED\ntest b ... FAILED\n",
        )
        .unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", "adds a failure"]).unwrap();
        git(root, &["checkout", "-q", "master"]).unwrap();

        let err = merge(root, "pitwheel", "worse", Some(VERIFY), 600).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("YOU STILL HOLD THE MERGE MUTEX"), "{msg}");
        assert!(
            msg.contains("Failures this merge ADDED") && msg.contains("b"),
            "it must name what this merge broke: {msg}"
        );
        assert_eq!(head(root).unwrap(), before, "the merge must be undone");
        assert!(lease::list(root, true)
            .unwrap()
            .iter()
            .any(|l| l.lease.path == mutex_key("master")));
    }

    #[test]
    fn failing_test_names_are_read_from_a_libtest_report() {
        let text = "running 3 tests\ntest alpha ... ok\ntest beta::gamma ... FAILED\n\
                    test delta ... FAILED\n\ntest result: FAILED. 1 passed; 2 failed;\n";
        let got = failing_tests(text);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got.contains("beta::gamma") && got.contains("delta"),
            "{got:?}"
        );
    }

    /// An unreadable report must not be treated as "no failures" — that would
    /// make every unrecognised red base look identical to a green one and land
    /// merges that should have been held.
    #[test]
    fn an_unrecognised_verification_format_yields_no_names_and_still_holds() {
        assert!(failing_tests("make: *** [test] Error 1\n").is_empty());

        let tmp = repo();
        let root = tmp.path();
        branch_with_change(root, "feature", "changed\n");
        // Fails, but says nothing this can attribute, on both base and merge.
        let err = merge(
            root,
            "a",
            "feature",
            Some("echo opaque failure; exit 1"),
            600,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("YOU STILL HOLD THE MERGE MUTEX"),
            "an unattributable failure must take the conservative path"
        );
    }

    /// No `--verify` is a distinct state from a passing one, and must be
    /// visible as such rather than reading like proof.
    #[test]
    fn an_unverified_merge_records_none_rather_than_true() {
        let tmp = repo();
        let root = tmp.path();
        branch_with_change(root, "feature", "changed\n");

        let out = merge(root, "a", "feature", None, 600).unwrap();
        assert_eq!(out.verified, None);
        assert!(describe(&out).contains("unverified"), "{}", describe(&out));
    }
}
