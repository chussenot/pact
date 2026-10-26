//! pact-4ez: a linked worktree literally named `main` (worktrunk's bare layout
//! puts the default branch in one) or `outside` must not be stamped with the
//! reserved `invoked_from` literals, or `--check topology --expect worktrees`
//! reports a fleet that ran entirely in worktrees as a mismatch.
//!
//! A separate file from `tests/worktree.rs`, which a sibling bead held when this
//! landed; the helpers are the small subset needed here.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pact(dir: &Path, agent: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(args)
        .current_dir(dir)
        .env("PACT_AGENT", agent)
        .env_remove("PACT_WORKTREE_SCOPE")
        .output()
        .expect("pact")
}

/// Main checkout plus a linked worktree whose directory (and so git's worktree
/// name) is exactly `name`.
fn repo_with_linked(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let main = base.join("checkout");
    std::fs::create_dir(&main).unwrap();
    git_ok(&main, &["init", "--quiet", "--initial-branch=trunk"]);
    git_ok(&main, &["config", "user.email", "tests@pact.invalid"]);
    git_ok(&main, &["config", "user.name", "pact tests"]);
    std::fs::write(main.join("a.txt"), "x\n").unwrap();
    git_ok(&main, &["add", "."]);
    git_ok(&main, &["commit", "--quiet", "-m", "initial"]);
    let wt = base.join("wts").join(name);
    std::fs::create_dir(base.join("wts")).unwrap();
    git_ok(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "feat",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );
    (tmp, main, wt)
}

fn check_reserved_name(name: &str) {
    let (_tmp, main, wt) = repo_with_linked(name);
    let acquired = pact(&wt, "agent-a", &["lease", "acquire", "a.rs"]);
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );

    let log = std::fs::read_to_string(main.join(".pact/events.jsonl")).unwrap();
    let want = format!("\"invoked_from\":\"worktree:{name}\"");
    assert!(log.contains(&want), "expected {want} in {log}");

    let out = pact(
        &main,
        "reader",
        &["audit", "--check", "topology", "--expect", "worktrees"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // And it is not the main checkout either.
    let out = pact(
        &main,
        "reader",
        &["audit", "--check", "topology", "--expect", "main"],
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn a_linked_worktree_named_main_is_a_worktree_not_the_main_checkout() {
    check_reserved_name("main");
}

#[test]
fn a_linked_worktree_named_outside_is_a_worktree_not_outside() {
    check_reserved_name("outside");
}
