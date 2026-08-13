//! Coordination across linked git worktrees, driven through the compiled binary
//! against real `git worktree add` layouts.
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// (a) and the core of the whole feature: the same relative path, claimed from
/// two different checkouts, is one lock. And (b): releasing in A frees it for B.
#[test]
fn the_same_path_contends_across_worktrees_and_frees_again() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");

    let first = pact(
        &main,
        "agent-a",
        &["lease", "acquire", "src/api.ts", "--note", "rewriting"],
    );
    assert!(first.status.success(), "{}", stderr(&first));

    // One lock file, under the MAIN worktree, for a path claimed from either.
    let lock_dir = main.join(".pact/leases");
    assert_eq!(
        std::fs::read_dir(&lock_dir).unwrap().count(),
        1,
        "expected exactly one lock under {}",
        lock_dir.display()
    );
    assert!(
        !wt.join(".pact").exists(),
        "the linked worktree must not have its own .pact/: {}",
        wt.join(".pact").display()
    );

    // (a) The second worktree loses, with exit 2 — the documented code, so a
    // wrapper can branch on it exactly as it would within one checkout.
    let second = pact(&wt, "agent-b", &["lease", "acquire", "src/api.ts"]);
    assert_eq!(
        second.status.code(),
        Some(2),
        "cross-worktree contention must exit 2; stderr: {}",
        stderr(&second)
    );

    // The message has to explain WHY the loser cannot see the file changing:
    // the holder is editing a different checkout of it.
    let why = stderr(&second);
    assert!(why.contains("agent-a"), "must name the holder: {why}");
    assert!(
        why.contains("branch main"),
        "must name the holder's branch: {why}"
    );
    assert!(
        why.contains("worktree main"),
        "must name the holder's worktree: {why}"
    );

    // (b) Released in A, acquirable in B.
    let released = pact(&main, "agent-a", &["lease", "release", "src/api.ts"]);
    assert!(released.status.success(), "{}", stderr(&released));
    let retry = pact(&wt, "agent-b", &["lease", "acquire", "src/api.ts"]);
    assert!(
        retry.status.success(),
        "B must acquire after A releases: {}",
        stderr(&retry)
    );

    // And now the holder is in the linked worktree, on its branch.
    let listed = stdout(&pact(&main, "agent-a", &["lease", "ls"]));
    assert!(listed.contains("agent-b"), "{listed}");
    assert!(listed.contains("feat/auth"), "{listed}");
    assert!(listed.contains("wt-auth"), "{listed}");
}

/// An absolute path spelled from the MAIN worktree's root, typed from a
/// LINKED worktree, must still resolve to the same lock as the relative
/// spelling — not a second, disjoint lock file for the same real file
/// (pact-m7j.8.2). This is exactly the shape of copying an absolute path out
/// of `pact lease ls`'s own WHERE output or a peer's message and pasting it
/// from a different checkout.
#[test]
fn an_absolute_path_from_a_sibling_worktree_is_the_same_lock_as_the_relative_spelling() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");

    let first = pact(
        &main,
        "agent-a",
        &[
            "lease",
            "acquire",
            "src/api.ts",
            "--note",
            "editing from main",
        ],
    );
    assert!(first.status.success(), "{}", stderr(&first));

    let absolute = main.join("src/api.ts");
    let second = pact(
        &wt,
        "agent-b",
        &["lease", "acquire", absolute.to_str().unwrap()],
    );
    assert_eq!(
        second.status.code(),
        Some(2),
        "an absolute path rooted in a sibling worktree must alias the same \
         lock as the relative spelling, not open a second one; stderr: {}",
        stderr(&second)
    );

    // Exactly one lock file on disk, not two.
    let lock_dir = main.join(".pact/leases");
    assert_eq!(
        std::fs::read_dir(&lock_dir).unwrap().count(),
        1,
        "expected exactly one lock under {}, got a split-brain",
        lock_dir.display()
    );
}

/// The same alias has to hold for a THIRD, non-main linked worktree — not
/// just the main root (pact-m7j.8.7). Running from `wt_b`, a path spelled
/// absolute from `wt_c` matches neither of the two candidates `wt_b`'s own
/// resolution already had (its own root, the shared/main root), so it needs
/// `linked_worktree_roots` enumerating every sibling to recover the alias.
#[test]
fn an_absolute_path_from_a_third_non_main_worktree_is_the_same_lock() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt_b) = repo_with_worktree("feat/auth", "wt-b");
    let wt_c = main.parent().unwrap().join("wt-c");
    git_ok(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "feat/other",
            wt_c.to_str().unwrap(),
            "HEAD",
        ],
    );

    let first = pact(
        &main,
        "agent-a",
        &[
            "lease",
            "acquire",
            "src/api.ts",
            "--note",
            "editing from main",
        ],
    );
    assert!(first.status.success(), "{}", stderr(&first));

    // pact runs FROM wt_b, but the path is spelled absolute rooted in wt_c —
    // the third worktree, neither wt_b's own root nor the main/shared root.
    let absolute = wt_c.join("src/api.ts");
    let second = pact(
        &wt_b,
        "agent-b",
        &["lease", "acquire", absolute.to_str().unwrap()],
    );
    assert_eq!(
        second.status.code(),
        Some(2),
        "an absolute path rooted in a third sibling worktree must alias the \
         same lock as the relative spelling, not open a second one; stderr: {}",
        stderr(&second)
    );

    // Exactly one lock file on disk, not two.
    let lock_dir = main.join(".pact/leases");
    assert_eq!(
        std::fs::read_dir(&lock_dir).unwrap().count(),
        1,
        "expected exactly one lock under {}, got a split-brain",
        lock_dir.display()
    );
}

/// The `..`-relative variant of the same bug: `cwd.join()` resolves this to
/// an absolute path rooted in the sibling worktree before the failing
/// `strip_prefix` ever runs, so it is the identical code path, not a
/// separate case (confirmed in the property research's Investigation Log).
#[test]
fn a_dotdot_relative_escape_into_a_sibling_worktree_is_the_same_lock() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/auth", "wt-auth");

    let first = pact(&main, "agent-a", &["lease", "acquire", "src/api.ts"]);
    assert!(first.status.success(), "{}", stderr(&first));

    // wt and main are siblings under the same base dir: "../main/src/api.ts"
    // typed from wt lexically resolves to main's src/api.ts.
    let escape = format!(
        "../{}/src/api.ts",
        main.file_name().unwrap().to_str().unwrap()
    );
    let second = pact(&wt, "agent-b", &["lease", "acquire", &escape]);
    assert_eq!(
        second.status.code(),
        Some(2),
        "a `..`-escape into a sibling worktree must alias the same lock; stderr: {}",
        stderr(&second)
    );
}

/// (d) The same set of leases, seen from either checkout. A dashboard run in the
/// wrong directory showing an empty board is the failure this prevents.
#[test]
fn lease_ls_agrees_from_either_worktree() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/two", "wt-two");

    assert!(pact(&main, "agent-a", &["lease", "acquire", "src/api.ts"])
        .status
        .success());
    assert!(pact(&wt, "agent-b", &["lease", "acquire", "README.md"])
        .status
        .success());

    let from_main: serde_json::Value =
        serde_json::from_str(&stdout(&pact(&main, "x", &["lease", "ls", "--json"]))).unwrap();
    let from_wt: serde_json::Value =
        serde_json::from_str(&stdout(&pact(&wt, "x", &["lease", "ls", "--json"]))).unwrap();

    let names = |v: &serde_json::Value| {
        let mut paths: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["lease"]["path"].as_str().unwrap().to_string())
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(names(&from_main), ["README.md", "src/api.ts"]);
    assert_eq!(
        names(&from_main),
        names(&from_wt),
        "both worktrees must see one board"
    );

    // The payload carries where each holder is, so a reader can tell the two
    // apart without asking anybody.
    let by_path = |v: &serde_json::Value, p: &str| -> serde_json::Value {
        v.as_array()
            .unwrap()
            .iter()
            .find(|e| e["lease"]["path"] == p)
            .unwrap()
            .clone()
    };
    assert_eq!(by_path(&from_main, "src/api.ts")["lease"]["branch"], "main");
    assert_eq!(
        by_path(&from_main, "README.md")["lease"]["branch"],
        "feat/two"
    );
    assert_eq!(
        by_path(&from_main, "README.md")["lease"]["worktree"],
        "wt-two"
    );
}

/// (c) The two-process race, across two worktrees, with the invariants from
/// tests/lease.rs's `concurrent_steal_of_expired_lease_has_consistent_outcome`
/// reused unchanged. Sharing state must not turn a clean race into a corrupt one.
#[test]
fn concurrent_acquire_across_worktrees_has_consistent_outcome() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/race", "wt-race");

    // Plant an already-expired lease, exactly as the single-checkout race test
    // does, so both processes have something to steal.
    let lock_path = main.join(".pact/leases/contested.txt.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let stale = chrono::Utc::now() - chrono::Duration::seconds(2000);
    let expired_lease = serde_json::json!({
        "agent": "agent-dead",
        "path": "contested.txt",
        "acquired_at": stale.to_rfc3339(),
        "ttl_secs": 900,
        "note": null
    });
    std::fs::write(&lock_path, serde_json::to_string(&expired_lease).unwrap()).unwrap();

    let spawn = |dir: &Path, agent: &'static str| {
        Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["lease", "acquire", "contested.txt"])
            .current_dir(dir)
            .env("PACT_AGENT", agent)
            .env_remove("PACT_WORKTREE_SCOPE")
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {agent}: {e}"))
    };
    // One per worktree — the difference from the original test.
    let mut child_a = spawn(&main, "agent-a");
    let mut child_b = spawn(&wt, "agent-b");

    let status_a = child_a.wait().expect("agent-a wait failed");
    let status_b = child_b.wait().expect("agent-b wait failed");
    let code_a = status_a.code().unwrap_or(-1);
    let code_b = status_b.code().unwrap_or(-1);

    // Invariant 1: at least one process exits 0.
    let successes = [&status_a, &status_b]
        .iter()
        .filter(|s| s.success())
        .count();
    assert!(
        successes >= 1,
        "at least one process must win the concurrent expired-lease steal; \
         agent-a={code_a}, agent-b={code_b}",
    );

    // Invariant 2: the agent on disk after both exit is one that exited 0.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    let disk_agent = on_disk["agent"].as_str().unwrap();
    match disk_agent {
        "agent-a" => assert!(status_a.success(), "agent-a is on disk but exited {code_a}"),
        "agent-b" => assert!(status_b.success(), "agent-b is on disk but exited {code_b}"),
        other => panic!("unexpected agent on disk: {other}"),
    }

    // Invariant 3: any non-zero exit must be exactly 2 (lease held by another).
    for (name, code) in [("agent-a", code_a), ("agent-b", code_b)] {
        if code != 0 {
            assert_eq!(code, 2, "{name} exited {code}, expected 0 or 2");
        }
    }

    // Invariant 4, specific to sharing: exactly ONE lock file exists. Two would
    // mean the two worktrees raced on different files and both "won".
    assert_eq!(
        std::fs::read_dir(main.join(".pact/leases"))
            .unwrap()
            .count(),
        1
    );
}

/// (e) The escape hatch does what it says — and what it says is "your leases now
/// advise nobody", which doctor has to state out loud.
#[test]
fn scope_local_restores_per_worktree_isolation() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/local", "wt-local");

    let a = pact_scoped(
        &main,
        "agent-a",
        &["lease", "acquire", "src/api.ts"],
        Some("local"),
    );
    let b = pact_scoped(
        &wt,
        "agent-b",
        &["lease", "acquire", "src/api.ts"],
        Some("local"),
    );
    assert!(a.status.success(), "{}", stderr(&a));
    assert!(
        b.status.success(),
        "with scope=local both must succeed — that is the isolation being asked for: {}",
        stderr(&b)
    );
    assert!(
        wt.join(".pact/leases").is_dir(),
        "worktree keeps its own state"
    );

    // And doctor must not let that pass quietly.
    let doc = stdout(&pact_scoped(&wt, "agent-b", &["doctor"], Some("local")));
    assert!(
        doc.contains("PACT_WORKTREE_SCOPE=local"),
        "doctor must report the scope in effect: {doc}"
    );
    assert!(
        doc.contains("INVISIBLE"),
        "doctor must say the leases are invisible to siblings: {doc}"
    );
}

/// pact-m7j.9.6: `RepoContext::resolve` re-derives the state directory from
/// scratch on every invocation, with nothing to detect that
/// `PACT_WORKTREE_SCOPE` differs between the call that acquired a lease and a
/// later call trying to release it. Before this fix, `release_fs` treated the
/// resulting miss as ordinary idempotent "nothing to release" —
/// indistinguishable from having genuinely already released it, while the
/// real lock sat live in the other, un-probed directory until its TTL lapsed.
#[test]
fn release_after_a_scope_change_names_the_other_candidate_directory() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/drift", "wt-drift");

    // Acquired from the linked worktree with the default (shared) scope:
    // state lands under the MAIN worktree.
    let acquired = pact(&wt, "agent-a", &["lease", "acquire", "src/api.ts"]);
    assert!(acquired.status.success(), "{}", stderr(&acquired));
    let shared_leases = main.join(".pact/leases");
    assert_eq!(std::fs::read_dir(&shared_leases).unwrap().count(), 1);

    // Released from the SAME worktree and path, but with scope=local: resolves
    // to wt/.pact instead, which has never held anything.
    let released = pact_scoped(
        &wt,
        "agent-a",
        &["lease", "release", "src/api.ts"],
        Some("local"),
    );
    assert!(
        released.status.success(),
        "release stays idempotent-success, not a hard failure: {}",
        stderr(&released)
    );
    let warning = stderr(&released);
    assert!(
        warning.contains(main.join(".pact").to_str().unwrap()),
        "must name the other candidate directory where the real lock is sitting: {warning}"
    );
    assert!(
        warning.contains("PACT_WORKTREE_SCOPE"),
        "must point at the mechanism that could have changed: {warning}"
    );

    // And the real lock is still there, live — the whole point of the bug:
    // a naive fix must not go on to actually delete it either.
    assert_eq!(
        std::fs::read_dir(&shared_leases).unwrap().count(),
        1,
        "the real lock must not have been silently dropped"
    );
}

/// (f) Bare repository plus worktrees: leases anchor inside the common gitdir,
/// and messaging refuses instead of creating a store somewhere nobody will find.
#[test]
fn a_worktree_of_a_bare_repo_anchors_state_and_can_message() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();

    // A seed checkout to make a commit, then a bare clone with a worktree.
    let seed = base.join("seed");
    std::fs::create_dir(&seed).unwrap();
    git_ok(&seed, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(seed.join("f.txt"), "x\n").unwrap();
    git_ok(&seed, &["add", "."]);
    git_ok(&seed, &["commit", "--quiet", "-m", "initial"]);

    let bare = base.join("repo.git");
    git_ok(
        &base,
        &[
            "clone",
            "--bare",
            "--quiet",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let wt = base.join("wt-bare");
    git_ok(
        &bare,
        &["worktree", "add", "--quiet", wt.to_str().unwrap(), "main"],
    );

    let acquired = pact(&wt, "agent-a", &["lease", "acquire", "f.txt"]);
    assert!(acquired.status.success(), "{}", stderr(&acquired));

    // State inside the common gitdir — `pact/`, not `.pact/`, because it is not
    // sitting beside a working tree.
    assert!(
        bare.join("pact/leases").is_dir(),
        "expected leases under {}",
        bare.join("pact").display()
    );
    assert!(
        !wt.join(".pact").exists(),
        "must not fall back to worktree-local"
    );

    // doctor explains the placement rather than leaving it to be discovered.
    let doc = stdout(&pact(&wt, "agent-a", &["doctor"]));
    assert!(doc.contains("common-gitdir"), "{doc}");
    assert!(doc.contains("BARE"), "{doc}");

    // Messaging WORKS here now, and that is the change worth pinning.
    //
    // This used to assert exit 3 unconditionally: a bare worktree could not message,
    // because `bd` needs a working tree and `locate()` ran before anything else. So
    // the topology that most needs coordination — several worktrees off one bare
    // clone — was the one topology where agents could not talk to each other.
    // `.pact/messages.jsonl` lives under the resolved shared root, which for a bare
    // repo is the common gitdir, so a send is just an append there.
    let sent = pact(&wt, "agent-a", &["msg", "send", "--to", "agent-b", "hello"]);
    assert_eq!(
        sent.status.code(),
        Some(0),
        "a bare worktree can message now; stderr: {}",
        stderr(&sent)
    );
    // Anchored beside the leases, in the common gitdir, not in the worktree.
    assert!(
        bare.join("pact/messages.jsonl").is_file(),
        "expected the store at {}",
        bare.join("pact/messages.jsonl").display()
    );
    assert!(
        !wt.join(".pact").exists(),
        "still no worktree-local fallback"
    );

    // And it is readable from the worktree that sent it.
    let inbox = pact(&wt, "agent-b", &["msg", "inbox"]);
    assert!(inbox.status.success(), "{}", stderr(&inbox));
    assert!(stdout(&inbox).contains("hello"), "{}", stdout(&inbox));
}

/// (g) A `.git` file pact cannot follow: local fallback, a doctor warning, and no
/// panic. A broken worktree pointer is a reason to coordinate less, never a
/// reason for `lease acquire` to abort in the middle of a fleet.
#[test]
fn a_malformed_git_file_falls_back_locally_with_a_warning() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, _main, wt) = repo_with_worktree("feat/broken", "wt-broken");

    // Corrupt the pointer git wrote.
    std::fs::write(wt.join(".git"), "gitdir: /nowhere/at/all\n").unwrap();

    let acquired = pact(&wt, "agent-a", &["lease", "acquire", "src/api.ts"]);
    assert!(
        acquired.status.success(),
        "a broken .git must degrade, not fail: {}",
        stderr(&acquired)
    );
    assert!(
        wt.join(".pact/leases").is_dir(),
        "state should fall back into the worktree"
    );

    let doc = stdout(&pact(&wt, "agent-a", &["doctor"]));
    assert!(doc.contains("local-fallback"), "{doc}");
    assert!(
        doc.contains("fell back"),
        "doctor must say resolution fell back: {doc}"
    );
}

/// (h) The bug this covers: a `LocalFallback` degradation used to surface only
/// through a separately-run `pact doctor`. `lease acquire` is the command that
/// actually hit the split brain — two worktrees silently NOT sharing leases —
/// so it must say so on its own stderr, not leave the operator to think to run
/// `doctor`.
#[test]
fn lease_acquire_prints_the_fallback_warning_itself() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/degraded", "wt-degraded");

    // The exact trigger named in `RepoContext::resolve_topology`'s
    // `LocalFallback` arm: an unreadable `commondir` in the linked worktree's
    // own gitdir. The `.git` pointer itself is untouched and still resolves.
    let gitdir = main.join(".git/worktrees/wt-degraded");
    std::fs::remove_file(gitdir.join("commondir"))
        .expect("commondir must exist right after `git worktree add`");

    let acquired = pact(&wt, "agent-a", &["lease", "acquire", "src/api.ts"]);
    assert!(
        acquired.status.success(),
        "a missing commondir must degrade, not fail: {}",
        stderr(&acquired)
    );
    let err = stderr(&acquired);
    assert!(
        err.contains("commondir") && err.contains("NOT be shared"),
        "the acquire call itself must print the fallback warning, not only a \
         separately-run `pact doctor`: {err}"
    );
}

/// The warning must print exactly once per process no matter how many times a
/// single command internally re-resolves the topology. `lease acquire` is a
/// natural case for this: `lock_file_path` resolves it via `pact_dir`, and
/// `worktree_stamp` resolves it again independently — both inside one
/// invocation.
#[test]
fn the_fallback_warning_prints_only_once_per_invocation() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/degraded-once", "wt-degraded-once");

    let gitdir = main.join(".git/worktrees/wt-degraded-once");
    std::fs::remove_file(gitdir.join("commondir")).unwrap();

    let acquired = pact(&wt, "agent-a", &["lease", "acquire", "src/api.ts"]);
    assert!(acquired.status.success(), "{}", stderr(&acquired));
    let err = stderr(&acquired);
    let occurrences = err.matches("NOT be shared").count();
    assert_eq!(
        occurrences, 1,
        "printed {occurrences} times even though RepoContext::resolve runs more \
         than once inside one `lease acquire` call: {err}"
    );
}

/// The zero-change claim, from the outside: an ordinary checkout reports no
/// worktree, keeps state at `<root>/.pact`, and writes lock files with no
/// `branch`/`worktree` keys at all — not null, absent.
#[test]
fn an_ordinary_checkout_is_untouched_by_any_of_this() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let root = tmp.path();

    assert!(pact(root, "agent-a", &["lease", "acquire", "src/api.ts"])
        .status
        .success());

    // The encoding is `encode_path`'s, asserted by name so a change to it shows
    // up here rather than as a mysteriously absent file.
    let lock = root.join(".pact/leases/src__api.ts.lock");
    let raw = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("no lock at {}: {e}", lock.display()));
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // Sorted, because serde_json's map is a BTreeMap and key order is its
    // choice, not the payload's. The claim is about the key SET: `branch` and
    // `worktree` must be absent rather than serialized as null.
    let mut keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["acquired_at", "agent", "note", "path", "ttl_secs"],
        "a repo without worktrees must write the pre-worktree payload exactly"
    );

    let doc = stdout(&pact(root, "agent-a", &["doctor"]));
    assert!(doc.contains("not a worktree"), "{doc}");
    // And `lease ls` gains no column when there is nothing to put in it.
    let listed = stdout(&pact(root, "agent-a", &["lease", "ls"]));
    assert!(!listed.contains("WHERE"), "{listed}");
}

/// A superproject with one submodule at `vendor/lib`, both with a commit.
/// Returns (tempdir, superproject, submodule checkout).
fn repo_with_submodule() -> (TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();

    let lib = base.join("lib-origin");
    std::fs::create_dir(&lib).unwrap();
    git_ok(&lib, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(lib.join("lib.rs"), "pub fn x() {}\n").unwrap();
    git_ok(&lib, &["add", "."]);
    git_ok(&lib, &["commit", "--quiet", "-m", "initial"]);

    let main = base.join("super");
    std::fs::create_dir(&main).unwrap();
    git_ok(&main, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::create_dir_all(main.join("src")).unwrap();
    // The SAME relative path as the submodule will have, which is the point of
    // the isolation: `lib.rs` here and `lib.rs` in the submodule are different
    // files in different repositories and must not contend.
    std::fs::write(main.join("src/lib.rs"), "fn main() {}\n").unwrap();
    git_ok(&main, &["add", "."]);
    git_ok(&main, &["commit", "--quiet", "-m", "initial"]);

    // `protocol.file.allow` because git refuses file:// submodules by default
    // since the CVE-2022-39253 fix.
    git_ok(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            lib.to_str().unwrap(),
            "vendor/lib",
        ],
    );
    git_ok(&main, &["commit", "--quiet", "-m", "add submodule"]);

    (tmp, main.clone(), main.join("vendor/lib"))
}

/// A submodule's `.git` is a file and its gitdir has no `commondir`, because
/// `commondir` is worktree-specific. That used to land in the broken-worktree
/// fallback: `doctor` warned about siblings that do not exist, and every lock
/// file in every submodule gained `branch`/`worktree` keys — breaking exactly the
/// byte-compatibility `has_worktrees` exists to protect.
#[test]
fn a_submodule_writes_the_same_lock_file_as_a_plain_checkout() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, sub) = repo_with_submodule();

    assert!(pact(&sub, "agent-a", &["lease", "acquire", "lib.rs"])
        .status
        .success());

    // State beside the submodule, not in the superproject.
    let lock = sub.join(".pact/leases/lib.rs.lock");
    let raw = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("no lock at {}: {e}", lock.display()));
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let mut keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        // `content_hash` is present in every repository, worktrees or not
        // (pact-8qu) — it is what `pact watch` diffs a release against, and it
        // is gated on the file existing rather than on topology. It is
        // therefore NOT a counterexample to what this test protects, which is
        // that `branch`/`worktree` stay absent: those two are gated on
        // `has_worktrees`, and a submodule wrongly classified as having
        // worktrees is the bug (its `commondir` looks worktree-shaped). The
        // exact key set is kept rather than loosened to "does not contain
        // branch" so a future field cannot appear here unnoticed.
        [
            "acquired_at",
            "agent",
            "content_hash",
            "note",
            "path",
            "ttl_secs"
        ],
        "a submodule must write the plain payload — no branch/worktree stamps"
    );
    assert!(
        !main.join(".pact").exists(),
        "the submodule's lease must not land in the superproject"
    );

    // And doctor calls it what it is, without warning: this is a healthy
    // topology, and a `!` here would train people to ignore warnings.
    let doc = stdout(&pact(&sub, "agent-a", &["doctor"]));
    assert!(doc.contains("submodule"), "{doc}");
    assert!(
        !doc.contains("will NOT be shared with sibling worktrees"),
        "the sibling-worktree warning is nonsense for a submodule: {doc}"
    );
    let worktree_line = doc
        .lines()
        .find(|l| l.contains("worktree:"))
        .expect("a worktree line");
    assert!(
        worktree_line.starts_with('✓'),
        "submodule placement must not warn: {worktree_line}"
    );
}

/// The same relative path in the superproject and in its submodule are different
/// files, so they must not contend — while a linked worktree of the superproject
/// still shares with it. Both halves in one test, because it is the combination
/// that is easy to get wrong.
#[test]
fn a_submodule_stays_scoped_while_superproject_worktrees_still_share() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (tmp, main, sub) = repo_with_submodule();
    let wt = tmp.path().canonicalize().unwrap().join("super-wt");
    git_ok(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "feat/x",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );

    // Same path string, claimed in the superproject and in the submodule.
    assert!(pact(&main, "agent-super", &["lease", "acquire", "lib.rs"])
        .status
        .success());
    let in_sub = pact(&sub, "agent-sub", &["lease", "acquire", "lib.rs"]);
    assert!(
        in_sub.status.success(),
        "the submodule's lib.rs is a different file and must not contend: {}",
        stderr(&in_sub)
    );

    // The superproject's linked worktree DOES contend with the superproject.
    let in_wt = pact(&wt, "agent-wt", &["lease", "acquire", "lib.rs"]);
    assert_eq!(
        in_wt.status.code(),
        Some(2),
        "a superproject worktree must still share state: {}",
        stderr(&in_wt)
    );

    // Two separate boards, with one lease each.
    let board = |dir: &Path| -> usize {
        let out = stdout(&pact(dir, "x", &["lease", "ls", "--json"]));
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap()
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(board(&main), 1, "superproject board");
    assert_eq!(board(&sub), 1, "submodule board");
    assert_eq!(
        board(&wt),
        board(&main),
        "the worktree sees the superproject's"
    );
}

/// A linked worktree OF a submodule itself (row 3 of `classify_git_dir`'s
/// table: `.../modules/vendor/lib/worktrees/wt`) must share state with the
/// submodule's own checkout, not be stranded in the common gitdir as if the
/// submodule were a bare repository (pact-m7j.8.3). Before the fix, both
/// sides reported a healthy topology and each independently "won" a lease on
/// the same file — a split-brain `doctor` never even warned about.
#[test]
fn a_worktree_of_a_submodule_shares_state_with_the_submodules_own_checkout() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (tmp, _main, sub) = repo_with_submodule();
    let lib_wt = tmp.path().canonicalize().unwrap().join("lib-wt");
    git_ok(
        &sub,
        &[
            "worktree",
            "add",
            "-b",
            "feat/lib-y",
            lib_wt.to_str().unwrap(),
            "HEAD",
        ],
    );

    // The submodule's own checkout claims lib.rs first.
    let first = pact(&sub, "agent-a", &["lease", "acquire", "lib.rs"]);
    assert!(first.status.success(), "{}", stderr(&first));

    // The worktree OF that submodule must see the same lease, not open a
    // second, disjoint one — this is the split-brain the bug reproduced.
    let second = pact(&lib_wt, "agent-b", &["lease", "acquire", "lib.rs"]);
    assert_eq!(
        second.status.code(),
        Some(2),
        "a worktree of a submodule must share the submodule's own coordination \
         state, not open a second board; stderr: {}",
        stderr(&second)
    );

    // doctor must call this what it is, not "worktree of a bare repository".
    let doc = stdout(&pact(&lib_wt, "agent-b", &["doctor"]));
    assert!(
        doc.contains("submodule-worktree"),
        "expected submodule-worktree placement: {doc}"
    );
    assert!(
        !doc.contains("BARE"),
        "must not describe a submodule's worktree as bare: {doc}"
    );

    // One board, one lease, seen from either side.
    let board = |dir: &Path| -> usize {
        let out = stdout(&pact(dir, "x", &["lease", "ls", "--json"]));
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap()
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(board(&sub), 1, "submodule's own board");
    assert_eq!(board(&lib_wt), 1, "the worktree sees the submodule's board");
    assert!(
        !lib_wt.join(".pact").exists(),
        "the worktree of the submodule must not have its own .pact/"
    );
}

/// (pact-m7j.8.4) `a_submodule_stays_scoped_while_superproject_worktrees_still_share`
/// above proves the isolation with the two `pact lease acquire` calls
/// sequenced by the test harness (superproject first, submodule second).
/// This proves the same isolation holds when the two race for the filesystem
/// with true parallelism — real `.spawn()` on both sides, no waiting in
/// between, same idiom as `concurrent_acquire_across_worktrees_has_consistent_outcome`.
/// Regression fence, not a bug fix: no `src/` change accompanies this test.
#[test]
fn submodule_and_superproject_leases_stay_isolated_under_concurrent_contention() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }

    const ROUNDS: usize = 5;
    for round in 0..ROUNDS {
        let (_tmp, main, sub) = repo_with_submodule();

        // Same relative path string, claimed from the superproject root and
        // from the submodule's own checkout root, spawned together.
        let mut super_child = Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["lease", "acquire", "lib.rs"])
            .current_dir(&main)
            .env("PACT_AGENT", "agent-super")
            .env_remove("PACT_WORKTREE_SCOPE")
            .spawn()
            .expect("failed to spawn superproject acquire");
        let mut sub_child = Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["lease", "acquire", "lib.rs"])
            .current_dir(&sub)
            .env("PACT_AGENT", "agent-sub")
            .env_remove("PACT_WORKTREE_SCOPE")
            .spawn()
            .expect("failed to spawn submodule acquire");

        let super_status = super_child.wait().expect("superproject wait failed");
        let sub_status = sub_child.wait().expect("submodule wait failed");

        assert!(
            super_status.success(),
            "round {round}: superproject acquire must not contend with the submodule's \
             separate coordination space"
        );
        assert!(
            sub_status.success(),
            "round {round}: submodule acquire must not contend with the superproject's \
             separate coordination space"
        );

        // Each root's board must list only its own lease, never the other
        // root's — the point of the isolation under real concurrent pressure.
        let board = |dir: &Path| -> Vec<serde_json::Value> {
            let out = stdout(&pact(dir, "x", &["lease", "ls", "--json"]));
            serde_json::from_str::<serde_json::Value>(&out)
                .unwrap()
                .as_array()
                .unwrap()
                .clone()
        };
        let super_board = board(&main);
        let sub_board = board(&sub);
        assert_eq!(
            super_board.len(),
            1,
            "round {round}: superproject board must hold exactly its own lease: {super_board:?}"
        );
        assert_eq!(
            sub_board.len(),
            1,
            "round {round}: submodule board must hold exactly its own lease: {sub_board:?}"
        );
        assert_eq!(super_board[0]["lease"]["agent"], "agent-super");
        assert_eq!(sub_board[0]["lease"]["agent"], "agent-sub");
    }
}

/// pact-ler.1: the megablast shape, end to end. A fleet that edits in linked
/// worktrees but runs `pact` from the main checkout produced 62 events that
/// were indistinguishable from a plain single-checkout run, because a lease
/// event had never carried any topology at all — and the one place it WAS
/// recorded (the lock file's `branch`/`worktree`) is deleted on release and
/// gitignored, so the run's topology was unrecoverable by construction.
///
/// Asserts what the log can now answer: which worktree each event came from,
/// under the real binary, with a real linked worktree.
#[test]
fn every_event_records_which_worktree_pact_was_invoked_from() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/ctx", "wt-ctx");

    // Same repo, same shared coordination space, two different invocation
    // points — exactly the ambiguity the field run could not resolve.
    assert!(pact(&main, "agent-main", &["lease", "acquire", "a.rs"])
        .status
        .success());
    assert!(pact(&wt, "agent-wt", &["lease", "acquire", "b.rs"])
        .status
        .success());

    // Read the committed artifact itself, not `pact log --json` (which emits a
    // projected feed struct merging leases and messages): the whole bead is
    // about what survives IN `.pact/events.jsonl`.
    let feed = std::fs::read_to_string(main.join(".pact/events.jsonl")).unwrap();
    let events: Vec<serde_json::Value> = feed
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let by_agent = |who: &str| -> serde_json::Value {
        events
            .iter()
            .find(|e| e["agent"] == who && e["kind"] == "acquired")
            .unwrap_or_else(|| panic!("no acquired event for {who} in {feed}"))
            .clone()
    };

    // The literal "main", not the main worktree's directory name: the field
    // has to be comparable across repositories.
    assert_eq!(by_agent("agent-main")["invoked_from"], "main");
    assert_eq!(by_agent("agent-wt")["invoked_from"], "wt-ctx");

    // Unconditional means unconditional: present on every event, with the
    // scope actually in force and the version that wrote the line.
    for e in &events {
        assert!(
            e["invoked_from"].is_string(),
            "every event carries invoked_from: {e}"
        );
        assert_eq!(e["scope"], "shared", "{e}");
        assert_eq!(e["pact_version"], env!("CARGO_PKG_VERSION"), "{e}");
    }

    // The context fields are inside what the chain attests to, so a forged
    // line cannot strip or rewrite them and still verify.
    let audit = stdout(&pact(
        &main,
        "reader",
        &["audit", "--check", "chain-integrity", "--json"],
    ));
    let report: serde_json::Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(
        report["chain_breaks"].as_array().unwrap().len(),
        0,
        "{audit}"
    );
    assert!(report["chain_tracked"].as_u64().unwrap() >= 2, "{audit}");
}

/// `local` scope is a different coordination space, and the log has to say so
/// — otherwise two logs that cannot see each other's leases look identical.
#[test]
fn the_effective_scope_is_recorded_not_the_raw_env_var() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, _wt) = repo_with_worktree("feat/scope", "wt-scope");

    assert!(pact_scoped(
        &main,
        "agent-local",
        &["lease", "acquire", "a.rs"],
        Some("local")
    )
    .status
    .success());
    // An unrecognised value behaves as shared, so that is what must be
    // recorded — logging the raw string would put a value in the log that
    // pact never honoured.
    assert!(pact_scoped(
        &main,
        "agent-bogus",
        &["lease", "acquire", "b.rs"],
        Some("locale")
    )
    .status
    .success());

    // `local` scope puts state in a different directory, so each lease's event
    // lands in its own log — read whichever file actually holds it.
    let scope_of = |who: &str| -> String {
        for dir in [&main, &_wt] {
            let Ok(feed) = std::fs::read_to_string(dir.join(".pact/events.jsonl")) else {
                continue;
            };
            for line in feed.lines().filter(|l| !l.trim().is_empty()) {
                let e: serde_json::Value = serde_json::from_str(line).unwrap();
                if e["agent"] == who {
                    return e["scope"].as_str().unwrap().to_string();
                }
            }
        }
        panic!("no event for {who}");
    };
    assert_eq!(scope_of("agent-local"), "local");
    assert_eq!(scope_of("agent-bogus"), "shared");
}

/// pact-ler.2: the megablast shape as a diagnosis. A repo that HAS linked
/// worktrees, where every lease was nonetheless taken from the main checkout,
/// is the case where the lease/edit binding rests on convention — so the
/// summary says so out loud instead of leaving it to be inferred.
#[test]
fn audit_flags_worktrees_that_no_lease_was_ever_taken_from() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/topo", "wt-topo");

    // Every lease from main, while a linked worktree exists.
    assert!(pact(&main, "agent-a", &["lease", "acquire", "a.rs"])
        .status
        .success());
    assert!(pact(&main, "agent-a", &["lease", "release", "a.rs"])
        .status
        .success());

    let report = |dir: &Path| -> serde_json::Value {
        serde_json::from_str(&stdout(&pact(dir, "reader", &["audit", "--json"]))).unwrap()
    };

    let s = report(&main);
    assert_eq!(s["by_invoked_from"]["main"], 2, "{s}");
    let note = s["topology_note"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a topology note: {s}"));
    assert!(note.contains("no event was invoked from one"), "{note}");
    assert!(
        stdout(&pact(&main, "reader", &["audit"])).contains("cannot be verified"),
        "the note must reach the human-readable summary too"
    );

    // One lease actually taken from the worktree, and the hint retires — it
    // is evidence-driven, not a permanent nag at any repo with worktrees.
    assert!(pact(&wt, "agent-b", &["lease", "acquire", "b.rs"])
        .status
        .success());
    let s = report(&main);
    assert_eq!(s["by_invoked_from"]["wt-topo"], 1, "{s}");
    assert!(
        s["topology_note"].is_null(),
        "a worktree-invoked event retires the hint: {s}"
    );
}

/// pact-ler.5: the CI assertion, end to end — "did the fleet run where I told
/// it to". Exercises the megablast shape: a repo with a linked worktree where
/// every lease was nonetheless taken from the main checkout.
#[test]
fn audit_check_topology_fails_when_the_run_contradicts_expect() {
    if !have_git() {
        eprintln!("SKIP: no git on PATH");
        return;
    }
    let (_tmp, main, wt) = repo_with_worktree("feat/expect", "wt-expect");

    assert!(pact(&main, "agent-a", &["lease", "acquire", "a.rs"])
        .status
        .success());

    // Asked for worktrees, got main: exit 1, with forensics naming the
    // invocation point rather than only a count.
    let failed = pact(
        &main,
        "reader",
        &["audit", "--check", "topology", "--expect", "worktrees"],
    );
    assert_eq!(failed.status.code(), Some(1), "{}", stdout(&failed));
    let text = stdout(&failed);
    assert!(text.contains("TOPOLOGY MISMATCH"), "{text}");
    assert!(
        text.contains("main"),
        "must name where it actually ran: {text}"
    );

    // The same log against the expectation it actually satisfies: exit 0.
    let passed = pact(
        &main,
        "reader",
        &["audit", "--check", "topology", "--expect", "main"],
    );
    assert_eq!(passed.status.code(), Some(0), "{}", stdout(&passed));

    // Once a lease really is taken from the worktree, the run is mixed and
    // satisfies neither — all-or-nothing, with no proportion threshold.
    assert!(pact(&wt, "agent-b", &["lease", "acquire", "b.rs"])
        .status
        .success());
    for expect in ["worktrees", "main"] {
        let mixed = pact(
            &main,
            "reader",
            &["audit", "--check", "topology", "--expect", expect],
        );
        assert_eq!(
            mixed.status.code(),
            Some(1),
            "a mixed run satisfies neither --expect {expect}: {}",
            stdout(&mixed)
        );
    }
}
