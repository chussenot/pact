//! One identity in two linked worktrees (pact-3tg), driven through the compiled binary
//! against real `git worktree add` layouts. Kept beside tests/worktree.rs, whose helpers it copies.
//!
//! The unit tests in `src/repo.rs` build the `.git`/`commondir` chain by hand,
//! which is right for parsing but proves nothing about what `git` actually
//! writes — the two-hop layout, the relative `commondir`, and the fact that a
//! worktree's `.git` is a file at all are all git's choices, not pact's. These
//! tests let git make them.
//!
//! What is being proven is one claim: a lease is advisory, so its entire value
//! is that a peer can see it. Two worktrees of one repository that cannot see
//! each other's leases have advisory locks that advise nobody — both agents are
//! told they succeeded, which is strictly worse than no lock at all.
//!
//! Skipped, not failed, when `git` is absent: this suite says nothing about pact
//! on a machine with no git, and a test that fails for a missing tool trains
//! people to ignore red.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        // A worktree is created with a commit, so git needs an identity and must
        // not read the developer's own config (a global `commit.gpgsign` would
        // otherwise make these tests depend on a signing key being present).
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

/// A real repository with one commit, plus a linked worktree on its own branch.
/// Returns (tempdir, main worktree, linked worktree).
fn repo_with_worktree(branch: &str, wt_name: &str) -> (TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let main = base.join("main");
    std::fs::create_dir(&main).unwrap();

    git_ok(&main, &["init", "--quiet", "--initial-branch=main"]);
    // A LOCAL identity, in this throwaway repo only. Without it `git commit`
    // falls back to whatever the machine can auto-detect, which works on a
    // developer box and is refused on a CI runner — git will not accept an
    // auto-detected `runner@host.(none)` with no domain. That is why this
    // failed only in CI, and only once the chaos fix let cargo reach this
    // target at all (pact-h5f).
    git_ok(&main, &["config", "user.email", "tests@pact.invalid"]);
    git_ok(&main, &["config", "user.name", "pact tests"]);
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/api.ts"), "export const x = 1;\n").unwrap();
    git_ok(&main, &["add", "."]);
    git_ok(&main, &["commit", "--quiet", "-m", "initial"]);

    let wt = base.join(wt_name);
    git_ok(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );
    (tmp, main, wt)
}

fn pact(dir: &Path, agent: &str, args: &[&str]) -> Output {
    pact_scoped(dir, agent, args, None)
}

fn pact_scoped(dir: &Path, agent: &str, args: &[&str], scope: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pact"));
    cmd.args(args).current_dir(dir).env("PACT_AGENT", agent);
    // Cleared for the same reason PACT_WORKTREE_SCOPE is below, and with the same
    // consequence if it is not (pact-c3y). The lock-payload assertions in this
    // file pin an exact key SET, and `harness`/`model` are `skip_serializing_if`
    // — so an agent running the suite from inside a harness would see keys a
    // maintainer running it from a plain shell does not. That is a test whose
    // result depends on who ran it, which is worse than one that fails.
    //
    // Note what is NOT being claimed: `harness`/`model` are deliberately not
    // gated on worktree topology, unlike `branch`/`worktree`. They say WHAT is
    // holding the path, which is as true in one checkout as in twenty. They are
    // absent from those assertions because nothing declared them here, not
    // because a repo without worktrees suppresses them.
    for var in [
        "PACT_HARNESS",
        "PACT_MODEL",
        "PACT_HARNESS_SESSION",
        "PACT_HARNESS_SUBAGENT",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
    ] {
        cmd.env_remove(var);
    }
    match scope {
        Some(s) => cmd.env("PACT_WORKTREE_SCOPE", s),
        // Explicitly cleared: a developer with this exported in their shell must
        // not silently turn the sharing tests into isolation tests.
        None => cmd.env_remove("PACT_WORKTREE_SCOPE"),
    };
    cmd.output().expect("failed to run pact binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// One identity, two worktrees (pact-3tg)
// ---------------------------------------------------------------------------

/// The one lock file under `.pact/leases/`, parsed.
fn only_lock(main: &Path) -> (PathBuf, serde_json::Value) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(main.join(".pact/leases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "lock" || e == "json") || p.is_file())
        .collect();
    assert_eq!(entries.len(), 1, "expected one lock: {entries:?}");
    let p = entries.remove(0);
    let v = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    (p, v)
}

/// The same `PACT_AGENT` asking for a live lease from a worktree other than the
/// one stamped on it is two processes, not one agent resuming: refused, exit 2,
/// both worktrees named, and the lock is left as the holder wrote it.
#[test]
fn a_same_identity_refresh_from_another_worktree_is_refused() {
    if !have_git() {
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");
    let first = pact(&main, "parent", &["lease", "acquire", "src/api.ts"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let (lock, before) = only_lock(&main);
    let bytes = std::fs::read(&lock).unwrap();

    let second = pact(&wt, "parent", &["lease", "acquire", "src/api.ts"]);
    assert_eq!(second.status.code(), Some(2), "{}", stderr(&second));
    let why = stderr(&second);
    assert!(
        why.contains("worktree main"),
        "must name the holder's worktree: {why}"
    );
    assert!(
        why.contains("worktree wt-auth"),
        "must name the caller's worktree: {why}"
    );
    assert!(why.contains("PACT_AGENT"), "must say how to fix it: {why}");
    assert_eq!(
        std::fs::read(&lock).unwrap(),
        bytes,
        "refusal must not rewrite the lock"
    );
    assert_eq!(before["worktree"], "main");

    // `renew` is the same refresh and gets the same answer.
    let renew = pact(&wt, "parent", &["lease", "renew", "src/api.ts"]);
    assert_eq!(renew.status.code(), Some(2), "{}", stderr(&renew));
    assert_eq!(std::fs::read(&lock).unwrap(), bytes);

    // --steal stays the deliberate override.
    let steal = pact(
        &wt,
        "parent",
        &["lease", "acquire", "src/api.ts", "--steal"],
    );
    assert!(steal.status.success(), "{}", stderr(&steal));
    assert_eq!(only_lock(&main).1["worktree"], "wt-auth");
}

/// The legitimate refresh is untouched: same identity, same worktree.
#[test]
fn a_same_identity_refresh_from_the_same_worktree_still_succeeds() {
    if !have_git() {
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");
    for dir in [&main, &wt] {
        let a = pact(dir, "solo", &["lease", "acquire", "src/api.ts"]);
        assert!(a.status.success(), "{}", stderr(&a));
        let b = pact(dir, "solo", &["lease", "acquire", "src/api.ts"]);
        assert!(b.status.success(), "{}", stderr(&b));
        let r = pact(dir, "solo", &["lease", "renew", "src/api.ts"]);
        assert!(r.status.success(), "{}", stderr(&r));
        let rel = pact(dir, "solo", &["lease", "release", "src/api.ts"]);
        assert!(rel.status.success(), "{}", stderr(&rel));
    }
}

/// A lock with no worktree stamp (an older pact wrote it) has nothing to
/// compare, and an expired one is a takeover rather than a refresh: neither refuses.
#[test]
fn an_unstamped_or_expired_lock_is_not_refused() {
    if !have_git() {
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");
    let first = pact(&main, "parent", &["lease", "acquire", "src/api.ts"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let (lock, mut v) = only_lock(&main);

    // Unstamped.
    let obj = v.as_object_mut().unwrap();
    obj.remove("worktree");
    obj.remove("branch");
    obj.remove("invoked_from");
    std::fs::write(&lock, serde_json::to_string(&v).unwrap()).unwrap();
    let r = pact(&wt, "parent", &["lease", "acquire", "src/api.ts"]);
    assert!(r.status.success(), "{}", stderr(&r));
    assert_eq!(only_lock(&main).1["worktree"], "wt-auth");

    // Expired, stamped with the other worktree.
    let (lock, mut v) = only_lock(&main);
    v["worktree"] = serde_json::json!("main");
    v["acquired_at"] = serde_json::json!("2000-01-01T00:00:00+00:00");
    std::fs::write(&lock, serde_json::to_string(&v).unwrap()).unwrap();
    let r = pact(&main, "parent", &["lease", "acquire", "src/api.ts"]);
    assert!(r.status.success(), "{}", stderr(&r));
}

/// Without linked worktrees nothing is stamped and nothing is compared: the
/// lock stays free of the keys, and a refresh is the plain one.
#[test]
fn a_repo_without_worktrees_refreshes_as_before() {
    if !have_git() {
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");
    git_ok(
        &main,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    let a = pact(&main, "solo", &["lease", "acquire", "src/api.ts"]);
    assert!(a.status.success(), "{}", stderr(&a));
    let b = pact(&main, "solo", &["lease", "acquire", "src/api.ts"]);
    assert!(b.status.success(), "{}", stderr(&b));
    let (_, v) = only_lock(&main);
    assert!(v.get("worktree").is_none(), "{v}");
}
