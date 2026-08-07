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

/// Is a Beads CLI reachable?
///
/// Needed because exit 3 has two causes and only one of them is about topology.
/// `BeadsCli::locate()` runs before the bare-repo check, so with no backend at all
/// `msg send` refuses with "no Beads CLI found on PATH" — also exit 3, also
/// correct, and not the message the topology assertion is looking for.
///
/// This mattered: the assertion below passed locally for four days and failed on
/// every CI push in that window, because ci.yml installs no Beads CLI and a
/// developer machine has one. An assertion that can only be reached in one
/// environment has to say so.
fn have_beads() -> bool {
    ["bd", "br"].iter().any(|b| {
        Command::new(b)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
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

/// (f) Bare repository plus worktrees: leases anchor inside the common gitdir,
/// and messaging refuses instead of creating a store somewhere nobody will find.
#[test]
fn a_worktree_of_a_bare_repo_anchors_state_and_refuses_messaging() {
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

    // Messaging refuses with exit 3 either way, and that much is asserted
    // unconditionally: a bare worktree cannot message, and neither can a machine
    // with no backend. `--to` a name that does not exist is fine — the refusal
    // comes before any recipient resolution.
    let sent = pact(&wt, "agent-a", &["msg", "send", "--to", "agent-b", "hello"]);
    assert_eq!(
        sent.status.code(),
        Some(3),
        "bare topology must refuse messaging with exit 3; stderr: {}",
        stderr(&sent)
    );
    let why = stderr(&sent);
    if have_beads() {
        // Only reachable with a backend installed: `locate()` runs first, so
        // without one the "no Beads CLI on PATH" refusal wins — a truer answer to
        // a more fundamental problem, and the reason this branch is conditional
        // rather than the assertion being weakened for everyone.
        assert!(why.contains("BARE"), "must explain the topology: {why}");
        assert!(
            why.contains("Leases and the event log work") || why.contains("messaging does not"),
            "must say what still works: {why}"
        );
    } else {
        assert!(
            why.contains("no Beads CLI"),
            "with no backend, exit 3 should be about the missing CLI: {why}"
        );
    }
    // Nothing was created in the worktree as a side effect of refusing.
    assert!(
        !wt.join(".beads").exists(),
        "refusing must not leave a Beads store behind"
    );
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
        ["acquired_at", "agent", "note", "path", "ttl_secs"],
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
