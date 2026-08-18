//! The event log must survive a clone, and the runtime state must not.
//!
//! Driven through real `git`, because the claim being tested is git's verdict and
//! not pact's: "tracked" means `git ls-files` lists it, and "never committable"
//! means `git add` refuses. A test that only inspected `.gitignore` text would
//! pass on a rule that git interprets differently than the author expected —
//! which is the entire class of bug `pact doctor`'s gitignore check exists for.
//!
//! Skipped rather than failed without `git`, on the same reasoning as
//! tests/worktree.rs: a test that goes red for a missing tool teaches people to
//! ignore red.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        // The developer's own config must not decide the outcome — a global
        // core.excludesFile with `.pact/` in it would make "is it ignored?" a
        // question about this machine rather than about what pact wrote.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "pact tests")
        .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
        .env("GIT_COMMITTER_NAME", "pact tests")
        .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
        .output()
        .expect("failed to run git")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn pact(dir: &Path, agent: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(args)
        .current_dir(dir)
        .env("PACT_AGENT", agent)
        .output()
        .expect("failed to run pact binary")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A real git repo with one commit, so `git ls-files` and `git add` mean
/// something.
fn repo() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git_ok(tmp.path(), &["init", "--quiet", "--initial-branch=main"]);
    // A LOCAL identity, in this throwaway repo only. Without it `git commit`
    // falls back to whatever the machine can auto-detect, which works on a
    // developer box and is refused on a CI runner — git will not accept an
    // auto-detected `runner@host.(none)` with no domain. That is why this
    // failed only in CI, and only once the chaos fix let cargo reach this
    // target at all (pact-h5f).
    git_ok(tmp.path(), &["config", "user.email", "tests@pact.invalid"]);
    git_ok(tmp.path(), &["config", "user.name", "pact tests"]);
    std::fs::write(tmp.path().join("README.md"), "# test\n").unwrap();
    git_ok(tmp.path(), &["add", "."]);
    git_ok(tmp.path(), &["commit", "--quiet", "-m", "initial"]);
    tmp
}

fn tracked(repo: &Path, rel: &str) -> bool {
    git(repo, &["ls-files", "--error-unmatch", "--", rel])
        .status
        .success()
}

fn ignored(repo: &Path, rel: &str) -> bool {
    git(repo, &["check-ignore", "-q", "--", rel])
        .status
        .success()
}

/// `pact init` on a fresh repo must leave the event log committable and the
/// runtime state ignored. Asserted through git, and asserted in both directions:
/// "events.jsonl is tracked" says nothing on its own about whether a lock file
/// slipped in beside it.
#[test]
fn init_commits_the_two_append_only_logs_and_leaves_runtime_state_ignored() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let tmp = repo();
    let root = tmp.path();

    assert!(pact(root, "agent-a", &["init", "--no-commit"])
        .status
        .success());
    // Something in the log, and something in leases/, so both questions are about
    // files that exist.
    assert!(pact(root, "agent-a", &["lease", "acquire", "src/api.rs"])
        .status
        .success());

    assert!(
        root.join(".pact/events.jsonl").is_file(),
        "no event log to test"
    );
    assert!(
        !ignored(root, ".pact/events.jsonl"),
        "the event log is ignored"
    );
    assert!(
        ignored(root, ".pact/leases/src__api.rs.lock"),
        "a lock file is NOT ignored — runtime state would get committed"
    );

    // All THREE fates, because 0.9.0 made the message store committed and a fleet
    // orchestrator follows the .gitignore, not the design note. `read/` is the line
    // that matters most: a read position is per-machine, so committing it would have
    // every clone inherit its peers' inboxes.
    assert!(
        pact(root, "agent-a", &["msg", "send", "--to", "agent-b", "hi"])
            .status
            .success()
    );
    assert!(
        root.join(".pact/messages.jsonl").is_file(),
        "no message store to test"
    );
    assert!(
        !ignored(root, ".pact/messages.jsonl"),
        "the message store is ignored — the fleet's communication record dies at the \
         next clone"
    );
    assert!(pact(root, "agent-b", &["msg", "inbox"]).status.success());
    assert!(
        ignored(root, ".pact/read/agent-b.json"),
        "a read cursor is NOT ignored — every clone would inherit peers' read state"
    );

    // Committed AND append-only means it needs a union merge driver, or the
    // per-worktree fleet pattern conflicts on every wave.
    let attrs = std::fs::read_to_string(root.join(".gitattributes")).unwrap_or_default();
    for path in [".pact/events.jsonl", ".pact/messages.jsonl"] {
        assert!(
            attrs
                .lines()
                .any(|l| l.split_whitespace().next() == Some(path) && l.contains("merge=union")),
            "{path} is committed but has no union merge rule:\n{attrs}"
        );
    }

    // And it really does get committed, which is the property that matters.
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "--quiet", "-m", "with pact history"]);
    assert!(
        tracked(root, ".pact/events.jsonl"),
        "the event log did not survive a commit"
    );
    assert!(
        tracked(root, ".pact/messages.jsonl"),
        "the message store did not survive a commit"
    );
    // Exactly TWO since 0.9.0, and the list is asserted whole rather than by
    // membership: a new file quietly becoming committable is the thing to catch,
    // because `.pact/` is where evidence logs and once a live SIGNOZ_API_KEY have
    // been written by agents.
    let committed =
        String::from_utf8_lossy(&git(root, &["ls-files", "--", ".pact/"]).stdout).into_owned();
    let mut listed: Vec<&str> = committed.lines().collect();
    listed.sort_unstable();
    assert_eq!(
        listed,
        [".pact/events.jsonl", ".pact/messages.jsonl"],
        "exactly these belong in git, got: {committed:?}"
    );

    // A clone is the whole point: history has to travel.
    let clone = tmp.path().parent().unwrap().join("clone-of-pact-test");
    let out = git(
        root,
        &[
            "clone",
            "--quiet",
            root.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    if out.status.success() {
        assert!(
            clone.join(".pact/events.jsonl").is_file(),
            "the clone has no coordination history"
        );
        let _ = std::fs::remove_dir_all(&clone);
    }
}

/// The migration. A repo initialised by an older pact has a broad `.pact/` rule,
/// and a re-run must narrow it — otherwise the history stays lost for exactly the
/// repositories that have accumulated the most of it.
#[test]
fn re_init_narrows_an_older_pacts_broad_ignore_rule() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let tmp = repo();
    let root = tmp.path();

    // Exactly what an older `pact init` left behind, plus an unrelated rule that
    // must survive untouched.
    std::fs::write(root.join(".gitignore"), "/target\n.pact/\n*.tmp\n").unwrap();
    assert!(pact(root, "agent-a", &["lease", "acquire", "f.rs"])
        .status
        .success());
    assert!(
        ignored(root, ".pact/events.jsonl"),
        "precondition: the old rule should be hiding the log"
    );

    assert!(pact(root, "agent-a", &["init", "--no-commit"])
        .status
        .success());

    assert!(
        !ignored(root, ".pact/events.jsonl"),
        "re-running init did not un-ignore the event log:\n{}",
        std::fs::read_to_string(root.join(".gitignore")).unwrap()
    );
    assert!(
        ignored(root, ".pact/leases/f.rs.lock"),
        "narrowing must not un-ignore the runtime state"
    );
    // The upgrade path a sentinel check gets wrong: a repo initialised before 0.9.0
    // has an ignore rule that predates the message store entirely, and re-running
    // init is the only thing that will ever fix it.
    assert!(
        pact(root, "agent-a", &["msg", "send", "--to", "peer", "hi"])
            .status
            .success()
    );
    assert!(
        !ignored(root, ".pact/messages.jsonl"),
        "re-running init did not un-ignore the message store:\n{}",
        std::fs::read_to_string(root.join(".gitignore")).unwrap()
    );
    assert!(
        ignored(root, ".pact/read/agent-a.json"),
        "narrowing must not un-ignore read cursors"
    );
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
    for kept in ["/target", "*.tmp"] {
        assert!(
            gitignore.lines().any(|l| l == kept),
            "lost the unrelated rule {kept}:\n{gitignore}"
        );
    }

    // Idempotent: a third run changes nothing.
    let after_first = gitignore;
    assert!(pact(root, "agent-a", &["init", "--no-commit"])
        .status
        .success());
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        after_first,
        "a second re-init rewrote the file"
    );
}

/// `merge=union` is what makes committing an append-only log tolerable. Asserted
/// by actually merging two branches that both appended, because the whole reason
/// `.pact/` was ignored wholesale was the conflict this avoids.
#[test]
fn two_branches_appending_events_merge_without_conflict() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let tmp = repo();
    let root = tmp.path();
    assert!(pact(root, "agent-a", &["init", "--no-commit"])
        .status
        .success());
    assert!(pact(root, "agent-a", &["lease", "acquire", "base.rs"])
        .status
        .success());
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "--quiet", "-m", "base"]);

    git_ok(root, &["checkout", "--quiet", "-b", "branch-a"]);
    assert!(pact(root, "agent-a", &["lease", "acquire", "a.rs"])
        .status
        .success());
    git_ok(root, &["commit", "--quiet", "-am", "a appends"]);

    git_ok(root, &["checkout", "--quiet", "main"]);
    git_ok(root, &["checkout", "--quiet", "-b", "branch-b"]);
    assert!(pact(root, "agent-b", &["lease", "acquire", "b.rs"])
        .status
        .success());
    git_ok(root, &["commit", "--quiet", "-am", "b appends"]);

    let merge = git(root, &["merge", "--no-edit", "branch-a"]);
    assert!(
        merge.status.success(),
        "an append-only log must merge cleanly with merge=union; git said:\n{}\n{}",
        stdout_of(&merge),
        String::from_utf8_lossy(&merge.stderr)
    );

    // Both agents' events are present — union keeps both sides rather than
    // picking one, which is the point.
    let log = std::fs::read_to_string(root.join(".pact/events.jsonl")).unwrap();
    for needle in ["a.rs", "b.rs", "base.rs"] {
        assert!(log.contains(needle), "merge lost {needle}:\n{log}");
    }
}

/// The same proof for the MESSAGE store, which is the half that was missing.
///
/// 0.9.0 made `.pact/messages.jsonl` committed but wrote no merge rule for it, so
/// the per-worktree fleet pattern docs/fleet-patterns.md recommends — one worktree
/// per agent, merged at wave end — would have hit a conflict on every wave, in the
/// file agents use to warn each other. This is the regression test for that.
#[test]
fn two_branches_sending_messages_merge_without_conflict() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let tmp = repo();
    let root = tmp.path();
    assert!(pact(root, "agent-a", &["init", "--no-commit"])
        .status
        .success());
    assert!(
        pact(root, "agent-a", &["msg", "send", "--to", "human", "base"])
            .status
            .success()
    );
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "--quiet", "-m", "base"]);

    git_ok(root, &["checkout", "--quiet", "-b", "branch-a"]);
    assert!(
        pact(root, "agent-a", &["msg", "send", "--to", "human", "from a"])
            .status
            .success()
    );
    git_ok(root, &["commit", "--quiet", "-am", "a sends"]);

    git_ok(root, &["checkout", "--quiet", "main"]);
    git_ok(root, &["checkout", "--quiet", "-b", "branch-b"]);
    assert!(
        pact(root, "agent-b", &["msg", "send", "--to", "human", "from b"])
            .status
            .success()
    );
    git_ok(root, &["commit", "--quiet", "-am", "b sends"]);

    let merge = git(root, &["merge", "--no-edit", "branch-a"]);
    assert!(
        merge.status.success(),
        "the message store must merge cleanly with merge=union; git said:\n{}\n{}",
        stdout_of(&merge),
        String::from_utf8_lossy(&merge.stderr)
    );

    // Neither agent's mail is lost, which is the whole reason to keep both sides.
    let store = std::fs::read_to_string(root.join(".pact/messages.jsonl")).unwrap();
    for needle in ["base", "from a", "from b"] {
        assert!(store.contains(needle), "merge lost {needle:?}:\n{store}");
    }
    // And the merged store is still readable BY PACT, not merely by grep — a union
    // merge that interleaved two half-lines would satisfy the check above.
    let inbox = pact(root, "human", &["msg", "inbox", "--json"]);
    assert!(
        inbox.status.success(),
        "{}",
        String::from_utf8_lossy(&inbox.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(&inbox)).expect("inbox JSON");
    assert_eq!(
        parsed.as_array().map(Vec::len),
        Some(3),
        "pact must read all three messages back after the merge: {parsed}"
    );
}

/// doctor has to say so when the log is ignored, and warn rather than fail:
/// nothing is broken, but the repository is going to lose its history at the next
/// clone and the reader can reverse that with one command.
#[test]
fn doctor_warns_when_the_event_log_is_ignored_and_is_quiet_when_it_is_not() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let tmp = repo();
    let root = tmp.path();
    std::fs::write(root.join(".gitignore"), ".pact/\n").unwrap();
    assert!(pact(root, "agent-a", &["lease", "acquire", "f.rs"])
        .status
        .success());

    let check = |root: &Path| -> serde_json::Value {
        let out = pact(root, "agent-a", &["doctor", "--json"]);
        let report: serde_json::Value = serde_json::from_str(&stdout_of(&out)).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "event log survives a clone")
            .expect("no event-log check in the report")
            .clone()
    };

    let ignored_check = check(root);
    assert_eq!(ignored_check["warn"], true, "{ignored_check}");
    assert_eq!(
        ignored_check["ok"], true,
        "an ignored log is a warning, never a failure: {ignored_check}"
    );
    let detail = ignored_check["detail"].as_str().unwrap();
    assert!(
        detail.contains("pact init"),
        "must say how to fix it: {detail}"
    );
    assert!(
        detail.contains("clone"),
        "must say what is actually lost: {detail}"
    );

    // After init narrows the rule, the warning goes away.
    assert!(pact(root, "agent-a", &["init", "--no-commit"])
        .status
        .success());
    let fixed = check(root);
    assert_eq!(fixed["warn"], false, "still warning after init: {fixed}");
}
