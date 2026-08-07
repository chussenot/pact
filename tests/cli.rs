//! End-to-end tests for the `pact` CLI surface, driven through the compiled
//! binary (`env!("CARGO_BIN_EXE_pact")`) exactly like `tests/lease.rs`.
//!
//! Why this file exists (pact-rnc.22): the library logic is unit-tested, but the
//! *wiring* — clap parsing, exit codes, what lands on stdout vs stderr, and what
//! survives a closed pipe — is only reachable through a real process. Every P3
//! in this round (release --all reporting expired leases, --body-file eating
//! trailing blank lines, timestamps sorted as strings) lived in that layer and
//! had to be found by hand.
//!
//! House rules for anything added here:
//!   * one `TempDir` per test, never a shared one — `cargo test` runs these
//!     concurrently and they all write `.pact/`;
//!   * never touch the repo the tests are running in;
//!   * assert on `Output.status` of the *pact child*, never on a shell's `$?`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// `find_repo_root` only checks that a `.git` entry exists, so a bare directory
/// is a good enough repo (same shortcut as `tests/lease.rs`).
fn init_repo() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    tmp
}

fn pact(repo: &Path, agent: &str, args: &[&str]) -> Output {
    pact_cmd(repo, args)
        .env("PACT_AGENT", agent)
        .output()
        .expect("failed to run pact binary")
}

/// The binary with **no** identity in the environment. `env_remove` is not
/// optional: these tests are run by agents who export `PACT_AGENT` themselves,
/// and the child would inherit it.
fn pact_cmd(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pact"));
    cmd.args(args).current_dir(repo).env_remove("PACT_AGENT");
    cmd
}

/// Like [`pact`], but run from `cwd` instead of the repo root — for tests
/// that need a path spelled relative to a subdirectory, the exact case
/// `normalize_path` exists to resolve consistently either way.
fn pact_from(cwd: &Path, agent: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pact"));
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("PACT_AGENT")
        .env("PACT_AGENT", agent);
    cmd.output().expect("failed to run pact binary")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json_stdout(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\nstdout: {}", stdout_of(out)))
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0\nstdout: {}\nstderr: {}",
        stdout_of(out),
        stderr_of(out)
    );
}

/// Where `lease acquire <path>` puts its lock (`/` -> `__`).
fn lock_path(repo: &Path, path: &str) -> PathBuf {
    repo.join(".pact/leases")
        .join(format!("{}.lock", path.replace('/', "__")))
}

/// Move a lock's `acquired_at` back by `secs`, so a test can reach ttl/grace
/// boundaries without sleeping (same trick as `tests/lease.rs`).
fn backdate(repo: &Path, path: &str, secs: i64) {
    let lock = lock_path(repo, path);
    let mut lease: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    let then = chrono::Utc::now() - chrono::Duration::seconds(secs);
    lease["acquired_at"] = serde_json::Value::String(then.to_rfc3339());
    std::fs::write(&lock, serde_json::to_string(&lease).unwrap()).unwrap();
}

fn age_secs(rfc3339: &str) -> i64 {
    let t = chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap_or_else(|e| panic!("unparsable timestamp {rfc3339:?}: {e}"));
    (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()
}

// ---------------------------------------------------------------- whoami

/// `whoami` is what you run *because* something is broken (pact-rnc.12), so a
/// missing identity must not make it fail — it must still print the paths that
/// answer "which repo/.pact am I even talking to".
#[test]
fn whoami_without_an_identity_exits_0_and_still_reports_paths() {
    let tmp = init_repo();
    let out = pact_cmd(tmp.path(), &["whoami"])
        .output()
        .expect("failed to run pact binary");

    assert_ok(&out);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("(none)"),
        "should say the identity is unset: {stdout}"
    );
    assert!(
        stdout.contains("no agent identity"),
        "should report the missing identity as a problem: {stdout}"
    );
    let root = tmp.path().canonicalize().unwrap();
    for label in ["repo root", "pact dir"] {
        assert!(stdout.contains(label), "missing {label} row: {stdout}");
    }
    assert!(
        stdout.contains(root.to_str().unwrap()),
        "should print the resolved repo root: {stdout}"
    );
    // Read-only question: it must not have created `.pact/`.
    assert!(!tmp.path().join(".pact").exists(), "whoami created .pact/");
}

// ---------------------------------------------------------- lease acquire

#[test]
fn acquire_multi_path_takes_every_path() {
    let tmp = init_repo();
    let out = pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "p1.txt", "p2.txt", "--json"],
    );

    assert_ok(&out);
    let v = json_stdout(&out);
    let arr = v.as_array().expect("several paths serialize as an array");
    assert_eq!(arr.len(), 2, "{v}");
    assert!(lock_path(tmp.path(), "p1.txt").exists());
    assert!(lock_path(tmp.path(), "p2.txt").exists());
}

/// A one-path acquire must keep serializing as an OBJECT even though the
/// command learned to batch — `lease acquire f --json | jq .lease.path` is a
/// documented script shape.
#[test]
fn acquire_single_path_json_stays_an_object() {
    let tmp = init_repo();
    let out = pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "f.txt", "--json"],
    );

    assert_ok(&out);
    let v = json_stdout(&out);
    assert!(v.is_object(), "expected an object, got: {v}");
    assert_eq!(v["lease"]["path"], "f.txt");
}

/// All-or-nothing (pact-rnc.21): the conflict is reported, the peer's lock is
/// untouched, and — the part a unit test cannot see — the lock that *was*
/// already written before the conflict is rolled back off disk.
#[test]
fn acquire_multi_path_conflict_rolls_back_leaving_no_stray_lock() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "q1.txt"],
    ));

    let out = pact(
        tmp.path(),
        "agent-b",
        &["lease", "acquire", "q2.txt", "q1.txt"],
    );

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("q1.txt") && stderr.contains("agent-a"),
        "error should name the contended path and its holder: {stderr}"
    );
    assert!(
        !lock_path(tmp.path(), "q2.txt").exists(),
        "q2.txt lock was not rolled back"
    );
    let held: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lock_path(tmp.path(), "q1.txt")).unwrap())
            .unwrap();
    assert_eq!(held["agent"], "agent-a", "peer's lock was modified");
}

/// pact-m7j.5.1: AGENTS.md tells every agent to prefer `--json` over parsing
/// human-formatted text and to branch on the exit code, not the message — but
/// `main`'s single error handler printed only plain text to stderr on EVERY
/// failure, `--json` or not, so a `--json` caller got an empty stdout on the
/// single most routine non-zero outcome two agents contending on a file will
/// ever produce. This must fail against the pre-fix binary (empty stdout).
#[test]
fn a_json_acquire_conflict_still_reports_a_parseable_error_on_stdout() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "contended.txt"],
    ));

    let out = pact(
        tmp.path(),
        "agent-b",
        &["lease", "acquire", "contended.txt", "--json"],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    let err = json_stdout(&out);
    assert_eq!(err["exit_code"], 2, "{err}");
    assert!(
        err["error"]
            .as_str()
            .is_some_and(|s| s.contains("contended.txt") && s.contains("agent-a")),
        "error text should still name the path and holder: {err}"
    );
}

/// Same gap, `renew`'s conflict branch.
#[test]
fn a_json_renew_conflict_still_reports_a_parseable_error_on_stdout() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "s2.txt"],
    ));

    let out = pact(
        tmp.path(),
        "agent-b",
        &["lease", "renew", "s2.txt", "--json"],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    let err = json_stdout(&out);
    assert_eq!(err["exit_code"], 2, "{err}");
    assert!(
        err["error"].as_str().is_some_and(|s| s.contains("agent-a")),
        "error text should still name the real holder: {err}"
    );
}

/// Same gap, `release`'s conflict branch (a specific path held by another
/// agent, no `--force`).
#[test]
fn a_json_release_conflict_still_reports_a_parseable_error_on_stdout() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "s3.txt"],
    ));

    let out = pact(
        tmp.path(),
        "agent-b",
        &["lease", "release", "s3.txt", "--json"],
    );
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    let err = json_stdout(&out);
    assert_eq!(err["exit_code"], 2, "{err}");
    assert!(
        err["error"]
            .as_str()
            .is_some_and(|s| s.contains("agent-a") && s.contains("--force")),
        "error text should still name the holder and the override: {err}"
    );
}

/// Same gap, `find_repo_root`'s exit-4 path — shared by every subcommand, so
/// `doctor` stands in for all of them.
#[test]
fn a_json_command_outside_any_git_repo_still_reports_a_parseable_error_on_stdout() {
    let tmp = tempfile::tempdir().unwrap(); // deliberately no .git anywhere in it
    let out = pact(tmp.path(), "agent-a", &["doctor", "--json"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr_of(&out));
    let err = json_stdout(&out);
    assert_eq!(err["exit_code"], 4, "{err}");
    assert!(
        err["error"]
            .as_str()
            .is_some_and(|s| s.contains("not in a git repository")),
        "{err}"
    );
}

// ------------------------------------------------------------ lease renew

#[test]
fn renew_refreshes_your_own_lease() {
    let tmp = init_repo();
    assert_ok(&pact(tmp.path(), "agent-a", &["lease", "acquire", "r.txt"]));
    backdate(tmp.path(), "r.txt", 600);

    let out = pact(
        tmp.path(),
        "agent-a",
        &["lease", "renew", "r.txt", "--json"],
    );

    assert_ok(&out);
    let v = json_stdout(&out);
    assert_eq!(v["path"], "r.txt");
    assert!(
        age_secs(v["acquired_at"].as_str().unwrap()) < 60,
        "renew should reset acquired_at, got {v}"
    );
    // ...and persist it, not just report it.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lock_path(tmp.path(), "r.txt")).unwrap())
            .unwrap();
    assert!(age_secs(on_disk["acquired_at"].as_str().unwrap()) < 60);
}

/// A typo'd path must not silently claim something new.
#[test]
fn renew_of_an_unleased_path_errors_without_creating_one() {
    let tmp = init_repo();
    let out = pact(tmp.path(), "agent-a", &["lease", "renew", "typo.txt"]);

    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("no lease on typo.txt") && stderr.contains("acquire"),
        "error should say there is nothing to renew, and what to run: {stderr}"
    );
    assert!(
        !lock_path(tmp.path(), "typo.txt").exists(),
        "renew created a lease it was supposed to refuse"
    );
    let ls = pact(tmp.path(), "agent-a", &["lease", "ls"]);
    assert!(
        stdout_of(&ls).contains("no active leases"),
        "{}",
        stdout_of(&ls)
    );
}

#[test]
fn renew_of_another_agents_lease_exits_2() {
    let tmp = init_repo();
    assert_ok(&pact(tmp.path(), "agent-a", &["lease", "acquire", "s.txt"]));

    let out = pact(tmp.path(), "agent-b", &["lease", "renew", "s.txt"]);

    assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout_of(&out));
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("agent-a"),
        "error should name the real holder: {stderr}"
    );
    let held: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lock_path(tmp.path(), "s.txt")).unwrap())
            .unwrap();
    assert_eq!(held["agent"], "agent-a", "renew stole the lease");
}

// ---------------------------------------------------------- lease release

#[test]
fn release_all_releases_yours_and_leaves_another_agents_alone() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "a1.txt", "a2.txt"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "agent-b",
        &["lease", "acquire", "b1.txt"],
    ));

    let out = pact(
        tmp.path(),
        "agent-a",
        &["lease", "release", "--all", "--json"],
    );

    assert_ok(&out);
    assert_eq!(json_stdout(&out), serde_json::json!(["a1.txt", "a2.txt"]));
    assert!(!lock_path(tmp.path(), "a1.txt").exists());
    assert!(!lock_path(tmp.path(), "a2.txt").exists());
    assert!(
        lock_path(tmp.path(), "b1.txt").exists(),
        "release --all took a peer's lease"
    );

    let human = pact(tmp.path(), "agent-a", &["lease", "release", "--all"]);
    assert_ok(&human);
    assert!(
        stdout_of(&human).contains("held no leases"),
        "second --all should be honest about holding nothing: {}",
        stdout_of(&human)
    );
}

/// pact-rnc.24: an expired lease is nobody's claim any more, so reporting it as
/// "released" overstates what happened. It gets swept silently; only the live
/// one is reported. Deliberately no `lease ls` before the release — `ls` GCs
/// expired locks itself and would hide the bug.
#[test]
fn release_all_does_not_report_an_expired_lease_as_released() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "live.txt", "gone.txt"],
    ));
    backdate(tmp.path(), "gone.txt", 5000); // past ttl + grace

    let out = pact(
        tmp.path(),
        "agent-a",
        &["lease", "release", "--all", "--json"],
    );

    assert_ok(&out);
    assert_eq!(
        json_stdout(&out),
        serde_json::json!(["live.txt"]),
        "expired lease must not be reported as released"
    );
    assert!(
        !lock_path(tmp.path(), "gone.txt").exists(),
        "expired lock should still be swept off disk"
    );
}

#[test]
fn release_all_of_only_expired_leases_says_it_held_none() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "old.txt"],
    ));
    backdate(tmp.path(), "old.txt", 5000);

    let out = pact(tmp.path(), "agent-a", &["lease", "release", "--all"]);

    assert_ok(&out);
    assert!(
        stdout_of(&out).contains("agent-a held no leases"),
        "stdout: {}",
        stdout_of(&out)
    );
    assert!(!lock_path(tmp.path(), "old.txt").exists());
}

// ------------------------------------------------ corrupt lock recovery

/// pact-m7j.7.2: docs/leases.md documents `--steal`/`--force` as advisory
/// overrides with no authorization check and no automatic notification to
/// the displaced agent — "nothing stops an agent from editing a file it
/// hasn't leased, the same way nothing stops you from `git push --force` over
/// a coworker's branch." This pins that contract against a future well-
/// intentioned patch (an age check, an auto-notify) silently changing it:
/// both overrides must still succeed against a live, non-expired,
/// just-acquired lease with no authorization gate, must still warn on
/// stderr, and must never send a message as an automatic side effect.
#[test]
fn steal_and_force_release_have_no_auth_check_and_send_no_automatic_notification() {
    let Some(tmp) = bd_repo("steal_and_force_have_no_auth_or_notify") else {
        return;
    };

    // A live, non-expired lease held by agent-a — acquired moments ago, the
    // least sympathetic case for an authorization gate to wave through.
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "guarded.rs"],
    ));

    let stolen = pact(
        tmp.path(),
        "agent-b",
        &["lease", "acquire", "guarded.rs", "--steal", "--json"],
    );
    assert_ok(&stolen);
    assert_eq!(
        json_stdout(&stolen)["stolen"],
        true,
        "no authorization check must block a live, non-expired steal"
    );
    assert!(
        stderr_of(&stolen).contains("stealing non-expired lease"),
        "the override must still warn on stderr: {}",
        stderr_of(&stolen)
    );

    let forced = pact(
        tmp.path(),
        "agent-c",
        &["lease", "release", "guarded.rs", "--force"],
    );
    assert_ok(&forced);
    let force_stderr = stderr_of(&forced);
    assert!(
        force_stderr.contains("destroyed agent-b's live claim")
            && force_stderr.contains("not notified"),
        "release --force must still warn, and say it did not notify: {force_stderr}"
    );

    // Neither override sent a message as an automatic side effect: nobody's
    // inbox has anything in it.
    for agent in ["agent-a", "agent-b", "agent-c"] {
        let inbox = pact(tmp.path(), agent, &["msg", "inbox", "--json"]);
        assert_ok(&inbox);
        assert_eq!(
            json_stdout(&inbox).as_array().map(Vec::len),
            Some(0),
            "{agent}'s inbox must stay empty — no automatic notification on override"
        );
    }
}

/// pact-m7j.4.2: a lock file whose JSON cannot be parsed used to make
/// `read_lease`'s `?` propagate a raw parse error from EVERY acquire attempt
/// on that path — `--steal` included, even though overriding a problematic
/// existing claim is the entire reason `--steal` exists. Before the fix this
/// exited 1 with a `serde_json` parse error instead of recovering the lease.
///
/// pact-m7j.4.8: a plain (non-`--steal`) acquire against that same corrupt
/// lock used to exit 1 (generic) instead of 2 ("this path is not available"),
/// the code AGENTS.md tells every agent to branch on instead of message text.
#[test]
fn steal_recovers_a_lease_behind_a_corrupt_lock_file() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "corrupt.rs"],
    ));

    // Hand-corrupt the lock file's bytes: no longer valid JSON.
    std::fs::write(lock_path(tmp.path(), "corrupt.rs"), b"not json at all").unwrap();

    // Without --steal, a corrupt lock must still block a plain acquire —
    // ownership cannot be verified, so this must not become a silent
    // takeover of whatever is sitting behind unreadable bytes. It must exit
    // 2, the same code a confirmed live claim exits, since the remediation
    // (--steal) is identical either way.
    let plain = pact(tmp.path(), "agent-b", &["lease", "acquire", "corrupt.rs"]);
    assert_eq!(
        plain.status.code(),
        Some(2),
        "a corrupt lock must exit 2 like any other unavailable path: {}",
        stderr_of(&plain)
    );

    let out = pact(
        tmp.path(),
        "agent-b",
        &["lease", "acquire", "corrupt.rs", "--steal", "--json"],
    );
    assert_ok(&out);
    let v = json_stdout(&out);
    assert_eq!(v["lease"]["agent"], "agent-b");
    assert_eq!(v["stolen"], true);
}

/// Same fix, applied to `renew` for consistency: ownership can't be checked
/// against unparsable content, so renewing must fail — but with a message
/// that names the recovery path instead of echoing a raw parse error. Also
/// exits 2, not 1 (pact-m7j.4.8): the original fix set this to 1, inconsistent
/// with `release_fs`'s corrupt-lock branch, which was already 2.
#[test]
fn renew_of_a_corrupt_lock_points_at_steal_instead_of_a_raw_parse_error() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "corrupt2.rs"],
    ));
    std::fs::write(lock_path(tmp.path(), "corrupt2.rs"), b"not json at all").unwrap();

    let out = pact(tmp.path(), "agent-a", &["lease", "renew", "corrupt2.rs"]);
    let stderr = stderr_of(&out);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("--steal"),
        "must point at the recovery path: {stderr}"
    );
}

/// And on `release`: ownership can't be verified from unparsable content
/// either, so a plain release must refuse, and only `--force` removes it.
#[test]
fn release_of_a_corrupt_lock_requires_force() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "corrupt3.rs"],
    ));
    std::fs::write(lock_path(tmp.path(), "corrupt3.rs"), b"not json at all").unwrap();

    let plain = pact(tmp.path(), "agent-a", &["lease", "release", "corrupt3.rs"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(
        lock_path(tmp.path(), "corrupt3.rs").exists(),
        "a plain release must refuse, not silently remove a corrupt lock"
    );
    assert!(
        stderr_of(&plain).contains("--force"),
        "{}",
        stderr_of(&plain)
    );

    let forced = pact(
        tmp.path(),
        "agent-a",
        &["lease", "release", "corrupt3.rs", "--force"],
    );
    assert_ok(&forced);
    assert!(!lock_path(tmp.path(), "corrupt3.rs").exists());
}

// ------------------------------------------ unresolved prior claim (9.1)

/// pact-m7j.9.1: an empty `.pact/leases/` — a fresh clone, or the manual
/// reset `pact doctor` itself prescribes for a corrupt lock — looks, locally,
/// exactly like a path nobody has ever touched. But the SHARED
/// `events.jsonl` can still carry an unmatched "acquired" for it, with no
/// later release/expiry/steal. Before the fix, a fresh acquire from a
/// different agent against that shape succeeded with zero mention of the
/// unresolved prior claim.
#[test]
fn acquiring_a_path_with_an_unresolved_prior_acquire_in_the_shared_log_warns() {
    let tmp = init_repo();
    let pact_dir = tmp.path().join(".pact");
    std::fs::create_dir_all(&pact_dir).unwrap();
    // No `.pact/leases/` at all: the fresh-clone/reset shape this bug is
    // about. The log alone remembers agent-a's claim, and nothing ever
    // closed it out.
    std::fs::write(
        pact_dir.join("events.jsonl"),
        "{\"at\":\"2026-08-01T00:00:00+00:00\",\"agent\":\"agent-a\",\"kind\":\"acquired\",\
         \"path\":\"shared.rs\",\"detail\":null}\n",
    )
    .unwrap();

    let out = pact(tmp.path(), "agent-b", &["lease", "acquire", "shared.rs"]);
    assert_ok(&out);
    assert!(
        lock_path(tmp.path(), "shared.rs").exists(),
        "the acquire must still succeed, not just warn"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("agent-a"),
        "must name the unresolved prior holder: {stderr}"
    );
    assert!(
        stderr.contains("never closed out") || stderr.contains("unresolved"),
        "must say the prior claim was never resolved: {stderr}"
    );
}

/// The negative control: when the log's last word on a path IS a resolution
/// (released, here), a fresh acquire against an empty `.pact/leases/` is the
/// ordinary case and must stay quiet — nothing was left unresolved.
#[test]
fn acquiring_a_path_whose_prior_claim_was_released_stays_quiet() {
    let tmp = init_repo();
    let pact_dir = tmp.path().join(".pact");
    std::fs::create_dir_all(&pact_dir).unwrap();
    std::fs::write(
        pact_dir.join("events.jsonl"),
        "{\"at\":\"2026-08-01T00:00:00+00:00\",\"agent\":\"agent-a\",\"kind\":\"acquired\",\
         \"path\":\"shared.rs\",\"detail\":null}\n\
         {\"at\":\"2026-08-01T00:05:00+00:00\",\"agent\":\"agent-a\",\"kind\":\"released\",\
         \"path\":\"shared.rs\",\"detail\":null}\n",
    )
    .unwrap();

    let out = pact(tmp.path(), "agent-b", &["lease", "acquire", "shared.rs"]);
    assert_ok(&out);
    // A different, pre-existing advisory ("last released by agent-a...") is
    // expected here and is not what this test is about; only the new
    // unresolved-claim wording must stay silent for a path that WAS resolved.
    let stderr = stderr_of(&out);
    assert!(
        !stderr.contains("never closed out") && !stderr.contains("unresolved acquire"),
        "a properly released prior claim must not be reported as unresolved: {stderr}"
    );
}

// --------------------------------------------------------------- lease ls

/// The whitespace-separated cells of the row for `path`. Column *positions* are
/// asserted, not just substrings, so "age" and "state" cannot quietly swap or
/// vanish into the note column.
fn row(stdout: &str, path: &str) -> Vec<String> {
    stdout
        .lines()
        .find(|l| l.split_whitespace().next() == Some(path))
        .unwrap_or_else(|| panic!("no row for {path} in:\n{stdout}"))
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// pact-rnc.10: the listing leads with an age and a state word, because
/// "remaining seconds" alone reads as long-held on a lease seconds old.
#[test]
fn lease_ls_shows_age_and_a_state_label_per_lease() {
    let tmp = init_repo();
    // Explicit `--ttl`, not the default. This test is about the active → stale →
    // expired state machine, and the backdates below are chosen relative to the
    // TTL; pinning it means recalibrating DEFAULT_TTL_SECS from telemetry cannot
    // turn this red for a reason that has nothing to do with what it asserts.
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &[
            "lease",
            "acquire",
            "fresh.txt",
            "stale.txt",
            "dead.txt",
            "--ttl",
            "900",
        ],
    ));
    backdate(tmp.path(), "stale.txt", 910); // past ttl, inside the grace window
    backdate(tmp.path(), "dead.txt", 5000); // past ttl + grace

    let out = pact(tmp.path(), "agent-a", &["lease", "ls", "--all"]);
    assert_ok(&out);
    let stdout = stdout_of(&out);

    let header = row(&stdout, "PATH");
    assert_eq!(header[..4], ["PATH", "AGENT", "HELD", "STATE"], "{stdout}");

    // HELD is a compact human duration (`0s`, `15m10s`), never a bare second
    // count, and it comes before the state word.
    let fresh = row(&stdout, "fresh.txt");
    assert!(
        fresh[2].ends_with('s') && fresh[2].len() <= 3,
        "a seconds-old lease should read as seconds: {:?}",
        fresh
    );
    assert_eq!(fresh[3], "active", "{fresh:?}");

    let stale = row(&stdout, "stale.txt");
    assert!(
        stale[2].starts_with("15m") && stale[2].ends_with('s'),
        "15m-old lease should read as minutes and seconds: {stale:?}"
    );
    assert_eq!(stale[3], "stale", "{stale:?}");
    assert!(
        stale.join(" ").contains("stale (reclaimable in"),
        "a stale lease must say when it becomes reclaimable: {stale:?}"
    );

    let dead = row(&stdout, "dead.txt");
    assert!(dead[2].starts_with("1h"), "{dead:?}");
    assert_eq!(dead[3], "expired", "{dead:?}");

    // `--all` is what shows an expired lease; the default listing GCs it and
    // hides it. (`dead.txt` is already gone — the listing above swept it — so
    // expire a fresh one to check the default view.)
    backdate(tmp.path(), "fresh.txt", 5000);
    let plain = pact(tmp.path(), "agent-a", &["lease", "ls"]);
    assert_ok(&plain);
    assert!(
        !stdout_of(&plain).contains("fresh.txt"),
        "default listing should hide expired leases: {}",
        stdout_of(&plain)
    );
    assert!(!lock_path(tmp.path(), "fresh.txt").exists(), "not GC'd");
}

// -------------------------------------------------------------------- log

/// pact-rnc.13, the crashed-agent case and the reason the feed exists: an agent
/// dies holding a file, its TTL lapses, a peer runs `lease ls` and the lock is
/// collected. That is precisely when someone runs `pact log` — so if the lapse is
/// not recorded, the freshest news in the fleet is "alice acquired gone.txt" for
/// a lock that is already gone, and the operator is told a dead agent still holds
/// the file. A false trace is worse than the missing one this was filed for.
#[test]
fn log_reports_a_collected_lease_as_expired_against_the_agent_whose_claim_lapsed() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "alice",
        &["lease", "acquire", "gone.txt", "--ttl", "60"],
    ));
    backdate(tmp.path(), "gone.txt", 600); // past ttl + grace

    // A *different* agent notices, which is the normal case: alice is gone.
    let ls = pact(tmp.path(), "carol", &["lease", "ls"]);
    assert_ok(&ls);
    assert!(
        !lock_path(tmp.path(), "gone.txt").exists(),
        "`lease ls` should have collected the expired lock: {}",
        stdout_of(&ls)
    );

    let log = pact(tmp.path(), "carol", &["log", "--json"]);
    assert_ok(&log);
    let feed = json_stdout(&log);
    // The bd half is unavailable in a bare tempdir (it warns and carries on);
    // only the lease half is asserted here.
    let lease_rows: Vec<&serde_json::Value> = feed
        .as_array()
        .expect("feed is an array")
        .iter()
        .filter(|e| e["kind"] != "message")
        .collect();
    let kinds: Vec<&str> = lease_rows
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        kinds,
        ["acquired", "expired"],
        "the lapse must be the feed's last word on gone.txt: {feed}"
    );
    let expiry = lease_rows[1];
    assert_eq!(
        expiry["agent"], "alice",
        "the event belongs to the holder whose lease lapsed, not to whoever ran \
         the command that collected it: {expiry}"
    );
    assert_eq!(expiry["target"], "gone.txt", "{expiry}");

    // And it is visible in the human feed an operator actually reads.
    let human = pact(tmp.path(), "carol", &["log"]);
    assert_ok(&human);
    let rendered = stdout_of(&human);
    assert!(
        rendered.contains("expired") && rendered.contains("alice"),
        "{rendered}"
    );
}

// ------------------------------------------------------------------ EPIPE

/// Enough lock files that `lease ls` writes more than any pipe buffer can hold.
/// That is what makes the EPIPE test below deterministic rather than a race:
/// whichever of the two processes gets there first, the write cannot complete.
fn seed_leases(repo: &Path, agent: &str, count: usize) {
    let dir = repo.join(".pact/leases");
    std::fs::create_dir_all(&dir).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let note = "n".repeat(60);
    for i in 0..count {
        let path = format!("src/some/deeply/nested/module/file-{i:04}.rs");
        let lease = serde_json::json!({
            "agent": agent,
            "path": path,
            "acquired_at": now,
            "ttl_secs": 900,
            "note": note,
        });
        std::fs::write(
            dir.join(format!("{}.lock", path.replace('/', "__"))),
            serde_json::to_string(&lease).unwrap(),
        )
        .unwrap();
    }
}

/// pact-rnc.26, and the most valuable test in this file: `pact … | head -0`
/// must not panic and must not invent an exit status. The side effect has
/// already landed by the time anything is printed, so a non-zero status here
/// reads as "your command failed" and the caller retries — which is the
/// duplicate-message bug this was reported for.
///
/// Reintroduce a bare `println!`/`eprintln!` anywhere on this path and the child
/// dies at 101 with `failed printing to stdout: Broken pipe`, failing here.
#[test]
fn output_to_a_reader_that_closed_does_not_panic_or_change_the_exit_status() {
    let tmp = init_repo();
    seed_leases(tmp.path(), "agent-a", 1000);

    let mut child = pact_cmd(tmp.path(), &["lease", "ls"])
        .env("PACT_AGENT", "agent-a")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn pact binary");
    // The reader walks away without reading a byte, like `head -0`.
    drop(child.stdout.take().expect("stdout was piped"));
    let out = child.wait_with_output().expect("pact never exited");

    assert_eq!(
        out.status.code(),
        Some(0),
        "a closed reader must not change the exit status (stderr: {})",
        stderr_of(&out)
    );
    let stderr = stderr_of(&out);
    assert!(
        !stderr.contains("panic"),
        "pact panicked on a closed pipe: {stderr}"
    );
    assert!(
        stderr.is_empty(),
        "a closed reader is not worth a word on stderr: {stderr}"
    );
}

/// The `2>&1 | head -0` shape: both streams gone, including the one the
/// warning would go to. Still exit 0 — and the work still landed, which is the
/// whole argument for not failing.
#[test]
fn closed_stdout_and_stderr_still_exit_0_and_the_work_still_lands() {
    let tmp = init_repo();
    assert_ok(&pact(tmp.path(), "agent-a", &["lease", "acquire", "w.txt"]));

    // `release --force` on someone else's lease: writes to stdout AND stderr.
    let mut child = pact_cmd(tmp.path(), &["lease", "release", "w.txt", "--force"])
        .env("PACT_AGENT", "agent-b")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn pact binary");
    drop(child.stdout.take().expect("stdout was piped"));
    drop(child.stderr.take().expect("stderr was piped"));
    let status = child.wait().expect("pact never exited");

    assert_eq!(status.code(), Some(0), "status: {status:?}");
    assert!(
        !lock_path(tmp.path(), "w.txt").exists(),
        "the release itself must happen even when nobody is listening"
    );
}

// ------------------------------------------------- msg (requires bd on PATH)

/// `pact msg` shells out to `bd`, so these tests need a bd-initialised repo.
/// CI may not have bd; when it doesn't they print why and pass, rather than
/// making the whole file conditional. Everything above runs unconditionally.
fn bd_repo(test: &str) -> Option<TempDir> {
    beads_repo(test, "bd")
}

/// The br twin of [`bd_repo`] (pact-l94). `BeadsCli::locate()` picks the
/// backend that can actually read the workspace, so a `br init` repo is the
/// only thing that selects the br code paths at all — without one they are
/// unreachable from the CLI and every br-specific divergence in `msg.rs` is
/// covered by unit tests alone.
///
/// It needs no separate body: `br init` takes the same argv shape and, though
/// it commits nothing (unlike `bd init`), the git identity is harmless. What
/// it must NOT be handed is `--db <path>` — br ignores it and initialises in
/// the cwd regardless, which is how a stray beads store landed in this repo
/// once. The cwd *is* the tempdir here, so that hazard cannot fire.
fn br_repo(test: &str) -> Option<TempDir> {
    beads_repo(test, "br")
}

fn beads_repo(test: &str, tool: &str) -> Option<TempDir> {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false);
    if !on_path {
        eprintln!("SKIP {test}: {tool} not found on PATH — `pact msg` cannot be tested here");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    // A real repo this time: `bd init` commits the files it creates, and it
    // needs an identity to commit with.
    let setup: [&[&str]; 4] = [
        &["git", "init", "-q", "."],
        &["git", "config", "user.email", "tests@pact.invalid"],
        &["git", "config", "user.name", "pact tests"],
        &[tool, "init"],
    ];
    for cmd in setup {
        match Command::new(cmd[0])
            .args(&cmd[1..])
            .current_dir(tmp.path())
            .output()
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                eprintln!(
                    "SKIP {test}: `{}` failed ({}): {}",
                    cmd.join(" "),
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                return None;
            }
            Err(e) => {
                eprintln!("SKIP {test}: cannot run `{}`: {e}", cmd[0]);
                return None;
            }
        }
    }
    Some(tmp)
}

/// pact-rnc.25: `--body-file` promises byte fidelity, so a body that ends in
/// deliberate blank lines (an ASCII table, an indented block) must arrive with
/// them. Exactly one trailing newline is dropped — that is the file's
/// punctuation, not content. Quotes and backslashes never meet a shell.
#[test]
fn msg_send_body_file_preserves_trailing_blank_lines_and_punctuation() {
    let Some(tmp) = bd_repo("msg_send_body_file_preserves_trailing_blank_lines_and_punctuation")
    else {
        return;
    };
    let body_file = tmp.path().join("body.txt");
    let on_disk = "first line\n\"quoted\" and a \\backslash\n    indented\n\n\n";
    std::fs::write(&body_file, on_disk).unwrap();

    let out = pact(
        tmp.path(),
        "sender-agent",
        &[
            "msg",
            "send",
            "--to",
            "reader-agent",
            "--subject",
            "fidelity",
            "--body-file",
            body_file.to_str().unwrap(),
        ],
    );
    assert_ok(&out);
    assert!(
        stdout_of(&out).contains("thread"),
        "send should report the thread: {}",
        stdout_of(&out)
    );

    let inbox = pact(tmp.path(), "reader-agent", &["msg", "inbox", "--json"]);
    assert_ok(&inbox);
    let messages = json_stdout(&inbox);
    assert_eq!(messages.as_array().map(Vec::len), Some(1), "{messages}");
    assert_eq!(
        messages[0]["body"].as_str().unwrap(),
        "first line\n\"quoted\" and a \\backslash\n    indented\n\n",
        "exactly one trailing newline should be dropped, nothing else"
    );
}

fn inbox_json(repo: &Path, agent: &str) -> serde_json::Value {
    let out = pact(repo, agent, &["msg", "inbox", "--json"]);
    assert_ok(&out);
    json_stdout(&out)
}

/// pact-rnc.4, the half that only a live bd can prove: one send, several
/// recipients, ONE thread — and every surface agreeing on its id.
///
/// The trap this pins: recipients 2..N get their own child bead, and `msg read`
/// used to report *that* id as the thread. `msg inbox`'s human output prints no
/// thread column, so `msg read` is where a recipient learns the id — and a reply
/// parented on a child becomes a grandchild that `msg read <root>` (direct
/// children only) never shows. The shared decision fragments again, one hop later,
/// for exactly the agents multi-recipient send was built for.
#[test]
fn every_recipient_of_a_fan_out_sees_one_thread_and_can_reply_into_it() {
    let Some(tmp) = bd_repo("every_recipient_of_a_fan_out_sees_one_thread_and_can_reply_into_it")
    else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "alpha",
        &[
            "msg",
            "send",
            "--to",
            "bravo",
            "--to",
            "charlie",
            "--subject",
            "shared decision",
            "friday?",
        ],
    ));

    let bravo = inbox_json(tmp.path(), "bravo");
    let root = bravo[0]["id"]
        .as_str()
        .expect("bravo got a message")
        .to_string();
    assert_eq!(bravo[0]["thread"], serde_json::json!(root));

    let charlie = inbox_json(tmp.path(), "charlie");
    let charlie_id = charlie[0]["id"]
        .as_str()
        .expect("charlie got a message")
        .to_string();
    assert_ne!(charlie_id, root, "recipient 2 has its own bead");
    assert_eq!(
        charlie[0]["thread"],
        serde_json::json!(root),
        "inbox reports the root: {charlie}"
    );

    // The surface a recipient actually learns the thread id from must say the
    // same thing — and reading her own copy must give her the conversation, not
    // a one-message "thread".
    let read = pact(
        tmp.path(),
        "charlie",
        &["msg", "read", &charlie_id, "--json"],
    );
    assert_ok(&read);
    let thread = json_stdout(&read);
    assert_eq!(
        thread.as_array().map(Vec::len),
        Some(2),
        "reading a non-root member must return the whole thread: {thread}"
    );
    for m in thread.as_array().unwrap() {
        assert_eq!(
            m["thread"],
            serde_json::json!(root),
            "two pact commands must not disagree about the thread id: {m}"
        );
    }
    // Human output too, since that is what an agent reads by default.
    let human = pact(tmp.path(), "charlie", &["msg", "read", &charlie_id]);
    assert_ok(&human);
    assert!(
        stdout_of(&human).contains(&format!("thread: {root}")),
        "{}",
        stdout_of(&human)
    );

    // Now follow the CLI's own output: reply with the id charlie was handed.
    let reply = pact(
        tmp.path(),
        "charlie",
        &[
            "msg",
            "send",
            "--to",
            "alpha",
            "--thread",
            &charlie_id,
            "--subject",
            "re: shared decision",
            "charlie acks",
        ],
    );
    assert_ok(&reply);
    assert!(
        stdout_of(&reply).contains(&format!("thread {root}")),
        "a reply from any member belongs to the root thread: {}",
        stdout_of(&reply)
    );

    let as_alpha = pact(tmp.path(), "alpha", &["msg", "read", &root, "--json"]);
    assert_ok(&as_alpha);
    let full = json_stdout(&as_alpha);
    let bodies: Vec<&str> = full
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap_or_default())
        .collect();
    assert!(
        bodies.contains(&"charlie acks"),
        "the ack must be in the thread everyone reads, not an invisible \
         grandchild: {full}"
    );
}

/// pact-rnc.17: read state is shared (bd `read-by-<agent>` labels), not a local
/// per-agent file — so the SENDER can see whether the agent they told has looked.
/// Both directions in one pass, because the risk in that migration was a reader
/// and a writer disagreeing about the label.
#[test]
fn reading_a_message_clears_the_readers_unread_and_shows_the_sender_it_landed() {
    let Some(tmp) =
        bd_repo("reading_a_message_clears_the_readers_unread_and_shows_the_sender_it_landed")
    else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "sender-agent",
        &[
            "msg",
            "send",
            "--to",
            "reader-agent",
            "--subject",
            "ack me",
            "please ack",
        ],
    ));

    let unread = pact(
        tmp.path(),
        "reader-agent",
        &["msg", "inbox", "--unread-only", "--json"],
    );
    assert_ok(&unread);
    let pending = json_stdout(&unread);
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{pending}");
    let id = pending[0]["id"].as_str().unwrap().to_string();

    let before = pact(tmp.path(), "sender-agent", &["msg", "sent"]);
    assert_ok(&before);
    assert!(
        stdout_of(&before).contains("1 not read yet"),
        "the sender must see it as unread until the recipient reads it: {}",
        stdout_of(&before)
    );

    assert_ok(&pact(tmp.path(), "reader-agent", &["msg", "read", &id]));

    let after = pact(
        tmp.path(),
        "reader-agent",
        &["msg", "inbox", "--unread-only", "--json"],
    );
    assert_ok(&after);
    assert_eq!(
        json_stdout(&after).as_array().map(Vec::len),
        Some(0),
        "reading it must clear the reader's unread: {}",
        stdout_of(&after)
    );

    let confirmed = pact(tmp.path(), "sender-agent", &["msg", "sent"]);
    assert_ok(&confirmed);
    assert!(
        stdout_of(&confirmed).contains("0 not read yet"),
        "the recipient's read must be visible to the sender: {}",
        stdout_of(&confirmed)
    );
}

#[test]
fn msg_send_rejects_an_all_whitespace_body_file() {
    let Some(tmp) = bd_repo("msg_send_rejects_an_all_whitespace_body_file") else {
        return;
    };
    let body_file = tmp.path().join("blank.txt");
    std::fs::write(&body_file, "   \n\t\n \n").unwrap();

    let out = pact(
        tmp.path(),
        "sender-agent",
        &[
            "msg",
            "send",
            "--to",
            "reader-agent",
            "--body-file",
            body_file.to_str().unwrap(),
        ],
    );

    assert_eq!(out.status.code(), Some(1), "stdout: {}", stdout_of(&out));
    assert!(
        stderr_of(&out).contains("empty message body"),
        "stderr: {}",
        stderr_of(&out)
    );
    let sent = pact(tmp.path(), "sender-agent", &["msg", "sent"]);
    assert_ok(&sent);
    assert!(
        stdout_of(&sent).contains("has sent nothing yet"),
        "a rejected body must not have been sent: {}",
        stdout_of(&sent)
    );
}

/// A message body is free text from another agent, printed verbatim by design
/// (byte fidelity, pact-rnc.25) — but a raw terminal escape sequence in that
/// text is not "content", it's a command aimed at whoever's terminal renders
/// it: clear the screen, ring the bell, rewrite what's on screen next. This
/// pins the fix at the only place that matters — the actual bytes written to
/// stdout, not the `String` `render_*` builds before `output::line` ever sees
/// it — across every human-rendered surface that showed a gap: the one-line
/// inbox row (`render_inbox`, truncated through `one_line`) and the untruncated
/// thread view (`render_full`, `msg read`).
#[test]
fn escape_sequences_in_a_message_body_never_reach_the_terminal() {
    let Some(tmp) = bd_repo("escape_sequences_in_a_message_body_never_reach_the_terminal") else {
        return;
    };

    // ESC CSI clear-screen + home, then BEL, then enough filler to push the
    // inbox row past one_line's 60-char cap so its truncation path also runs
    // over the attack bytes rather than just the render_full path.
    let attack_body = format!("clear\x1b[2Jscreen\x07BEL-{}", "x".repeat(80));

    let send = pact(
        tmp.path(),
        "sender-agent",
        &["msg", "send", "--to", "reader-agent", &attack_body],
    );
    assert_ok(&send);

    // render_inbox: one line per message, body truncated through one_line.
    let inbox = pact(tmp.path(), "reader-agent", &["msg", "inbox"]);
    assert_ok(&inbox);
    assert!(
        !inbox.stdout.contains(&0x1b),
        "ESC byte leaked into `msg inbox` stdout: {:?}",
        inbox.stdout
    );
    assert!(
        !inbox.stdout.contains(&0x07),
        "BEL byte leaked into `msg inbox` stdout: {:?}",
        inbox.stdout
    );
    assert!(
        stdout_of(&inbox).contains("clear"),
        "surrounding printable text should survive sanitization: {}",
        stdout_of(&inbox)
    );

    // render_full via `msg read <id>`: the untruncated thread view.
    let id = inbox_json(tmp.path(), "reader-agent")[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let read = pact(tmp.path(), "reader-agent", &["msg", "read", &id]);
    assert_ok(&read);
    assert!(
        !read.stdout.contains(&0x1b),
        "ESC byte leaked into `msg read` stdout: {:?}",
        read.stdout
    );
    assert!(
        !read.stdout.contains(&0x07),
        "BEL byte leaked into `msg read` stdout: {:?}",
        read.stdout
    );
    let read_out = stdout_of(&read);
    assert!(
        read_out.contains("clear") && read_out.contains("screen") && read_out.contains("BEL-x"),
        "surrounding printable text should survive sanitization, untruncated: {read_out}"
    );
}

/// `-V` is the machine-facing form and must stay the bare `pact <semver>` line;
/// `--version` carries the build stamp. Guarding both together is the point —
/// the value of the stamp is answering "is the binary on PATH the one I built?",
/// and the value of `-V` is that a script grepping it never sees the stamp.
#[test]
fn short_version_stays_bare_and_long_version_carries_the_build_stamp() {
    let tmp = init_repo();

    let short = pact(tmp.path(), "version-agent", &["-V"]);
    assert_ok(&short);
    assert_eq!(
        stdout_of(&short).trim(),
        format!("pact {}", env!("CARGO_PKG_VERSION")),
        "-V must stay a single greppable line"
    );

    let long = pact(tmp.path(), "version-agent", &["--version"]);
    assert_ok(&long);
    let out = stdout_of(&long);
    for field in [
        "commit:",
        "built:",
        "rustc:",
        "target:",
        "profile:",
        "features:",
    ] {
        assert!(out.contains(field), "--version missing {field}:\n{out}");
    }
    // Not a blanket "unknown" scan: the target triple legitimately contains it
    // (`x86_64-unknown-linux-gnu`). The commit is the field that silently
    // degrades, since `build.rs` falls back rather than failing a tarball build.
    let commit = out
        .lines()
        .find_map(|l| l.strip_prefix("commit:"))
        .expect("commit line")
        .trim();
    assert!(
        commit
            .trim_end_matches("-dirty")
            .chars()
            .all(|c| c.is_ascii_hexdigit()),
        "commit should be a git sha, got {commit:?}"
    );
}

// ------------------------------------------------------------------- init

/// The bug this guards (found by bootstrapping a fresh repo): Claude Code
/// loads CLAUDE.md, `.claude/CLAUDE.md`, CLAUDE.local.md and `.claude/rules/`
/// — never AGENTS.md. `pact init` wrote only AGENTS.md, so a Claude-driven
/// fleet read no protocol at all and silently skipped leases and messaging.
#[test]
fn init_makes_the_protocol_reachable_from_claude_md_without_touching_prior_content() {
    let tmp = init_repo();
    std::fs::write(
        tmp.path().join("CLAUDE.md"),
        "# House rules\n\nKeep it lazy.\n",
    )
    .unwrap();

    assert_ok(&pact(tmp.path(), "init-agent", &["init"]));

    let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
    assert!(
        claude.contains("@AGENTS.md"),
        "CLAUDE.md must import AGENTS.md:\n{claude}"
    );
    assert!(
        claude.starts_with("# House rules\n\nKeep it lazy.\n"),
        "pre-existing content must survive verbatim:\n{claude}"
    );
    // A pointer, not a second copy — two copies would drift apart.
    assert!(
        !claude.contains("PACT_AGENT"),
        "CLAUDE.md should import the protocol, not duplicate it:\n{claude}"
    );

    // Re-running must be a zero-diff no-op, like the AGENTS.md block.
    assert_ok(&pact(tmp.path(), "init-agent", &["init"]));
    let again = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
    assert_eq!(claude, again, "second init changed CLAUDE.md");

    let doc = pact(tmp.path(), "init-agent", &["doctor", "--json"]);
    let checks = json_stdout(&doc);
    let check = checks["checks"]
        .as_array()
        .expect("doctor emits a checks array")
        .iter()
        .find(|c| c["name"] == "CLAUDE.md reaches the protocol")
        .expect("doctor should report Claude reachability");
    assert_eq!(check["ok"], true, "{check}");
}

/// The symlink layout (`CLAUDE.md -> AGENTS.md`, which bd's own guidance
/// suggests): the protocol is already in the file Claude loads, and writing
/// `@AGENTS.md` into AGENTS.md would make it import itself.
#[cfg(unix)]
#[test]
fn init_does_not_write_a_self_import_when_claude_md_symlinks_to_agents_md() {
    let tmp = init_repo();
    assert_ok(&pact(tmp.path(), "init-agent", &["init"]));
    std::fs::remove_file(tmp.path().join("CLAUDE.md")).unwrap();
    std::os::unix::fs::symlink("AGENTS.md", tmp.path().join("CLAUDE.md")).unwrap();

    let out = pact(tmp.path(), "init-agent", &["init"]);
    assert_ok(&out);

    let agents = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
    assert!(
        !agents.contains("@AGENTS.md"),
        "AGENTS.md must never import itself:\n{agents}"
    );
    assert!(
        agents.contains("PACT_AGENT"),
        "the protocol itself must still be there:\n{agents}"
    );
    // Already reachable: doctor must not nag about a layout that works.
    let doc = pact(tmp.path(), "init-agent", &["doctor", "--json"]);
    let checks = json_stdout(&doc);
    let check = checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "CLAUDE.md reaches the protocol")
        .unwrap();
    assert_eq!(check["ok"], true, "{check}");
}

/// A real git repo, since `git check-ignore` needs one (the bare `.git`
/// directory `init_repo` fakes is not enough). Skips rather than fails if git
/// is unavailable, matching `bd_repo`.
fn git_repo(test: &str) -> Option<TempDir> {
    let tmp = tempfile::tempdir().unwrap();
    match Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .output()
    {
        Ok(o) if o.status.success() => Some(tmp),
        Ok(o) => {
            eprintln!("SKIP {test}: `git init` failed ({})", o.status);
            None
        }
        Err(e) => {
            eprintln!("SKIP {test}: cannot run git: {e}");
            None
        }
    }
}

fn reach_check(repo: &Path) -> serde_json::Value {
    let out = pact(repo, "reach-agent", &["doctor", "--json"]);
    json_stdout(&out)["checks"]
        .as_array()
        .expect("doctor emits a checks array")
        .iter()
        .find(|c| c["name"] == "protocol files reach a clone")
        .cloned()
        .expect("doctor should report whether the protocol files reach a clone")
}

/// pact's own repo shipped its entire history this way: a global `~/.gitignore`
/// rule meant `AGENTS.md` was written by every `pact init`, silently refused by
/// `git add`, and never once committed. Every other check was green — the block
/// really was current, it just reached nobody who cloned.
#[test]
fn doctor_flags_a_gitignored_agents_md_that_no_clone_will_ever_see() {
    let Some(tmp) = git_repo("doctor_flags_a_gitignored_agents_md") else {
        return;
    };
    std::fs::write(tmp.path().join(".gitignore"), "AGENTS.md\n").unwrap();
    assert_ok(&pact(tmp.path(), "reach-agent", &["init"]));

    let check = reach_check(tmp.path());
    // Warns, does not fail (pact-1q0): ignoring AGENTS.md can be deliberate, so
    // pact says so out loud rather than deciding the repo is broken.
    assert_eq!(check["warn"], true, "{check}");
    assert_eq!(
        check["ok"], true,
        "a warning must not fail the check: {check}"
    );
    let detail = check["detail"].as_str().unwrap();
    assert!(detail.contains("AGENTS.md"), "{detail}");
    // The actionable half: *which* ignore rule to go fix.
    assert!(detail.contains(".gitignore"), "{detail}");

    // Against a CONTROL, not against a hardcoded 0. The first version of this
    // asserted `Some(0)` and encoded the author's machine: CI has no Beads CLI,
    // so that check fails there and doctor exits 1 for a reason that has
    // nothing to do with this warning. What the test actually means is "the
    // warning does not change the code", so it compares the same repo with and
    // without the gitignore rule.
    let out = pact(tmp.path(), "reach-agent", &["doctor"]);
    let control = git_repo("doctor_flags_control").expect("git was available a moment ago");
    assert_ok(&pact(control.path(), "reach-agent", &["init"]));
    let baseline = pact(control.path(), "reach-agent", &["doctor"]);
    assert_eq!(
        out.status.code(),
        baseline.status.code(),
        "a warning must not change the exit code (baseline stderr: {})",
        stderr_of(&baseline)
    );

    let stdout = stdout_of(&out);
    // Visible without reading every line: a distinct glyph, and a count in the
    // summary so a `!` scrolled off the top is still reported.
    assert!(
        stdout.contains("! protocol files reach a clone"),
        "warnings render `!`, not `✓`: {stdout}"
    );
    assert!(
        stdout.contains("1 warning"),
        "the summary must count warnings: {stdout}"
    );
}

/// The positive control, and the reason the check tests tracking too: straight
/// after `pact init` the files are untracked but perfectly committable. Warning
/// there would fire on every fresh repo and train people to ignore the check.
#[test]
fn doctor_does_not_warn_about_untracked_but_committable_protocol_files() {
    let Some(tmp) = git_repo("doctor_does_not_warn_about_untracked") else {
        return;
    };
    assert_ok(&pact(tmp.path(), "reach-agent", &["init"]));

    let check = reach_check(tmp.path());
    assert_eq!(check["ok"], true, "{check}");
}

/// Committing is on by default, and `init` must be safe to re-run: the second
/// call finds nothing to commit rather than piling up empty commits.
#[test]
fn init_commits_what_it_wrote_and_a_re_run_adds_no_second_commit() {
    let Some(tmp) = git_repo("init_commits_what_it_wrote") else {
        return;
    };
    git_identity(tmp.path());

    assert_ok(&pact(tmp.path(), "commit-agent", &["init"]));
    let committed = git_out(tmp.path(), &["show", "--name-only", "--format=", "HEAD"]);
    for f in ["AGENTS.md", "CLAUDE.md", ".gitignore"] {
        assert!(
            committed.contains(f),
            "{f} missing from the commit: {committed}"
        );
    }
    // Conventional Commits: `bd init`'s non-conventional subject is what broke
    // `cog bump` over the whole history. pact must not repeat it.
    let subject = git_out(tmp.path(), &["log", "-1", "--format=%s"]);
    assert!(
        subject.starts_with("chore(pact): "),
        "subject must be a conventional commit, got {subject:?}"
    );

    assert_ok(&pact(tmp.path(), "commit-agent", &["init"]));
    assert_eq!(
        git_out(tmp.path(), &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "re-running init must not create a second commit"
    );
}

/// The property that makes committing-by-default acceptable: `init` commits its
/// own three files and nothing else, so a user's in-flight staged work is still
/// staged afterwards instead of being swept into a commit pact authored.
#[test]
fn init_does_not_sweep_unrelated_staged_work_into_its_commit() {
    let Some(tmp) = git_repo("init_does_not_sweep_unrelated_staged_work") else {
        return;
    };
    git_identity(tmp.path());
    std::fs::write(tmp.path().join("README"), "base\n").unwrap();
    run_git(tmp.path(), &["add", "README"]);
    run_git(tmp.path(), &["commit", "-qm", "chore: base"]);

    std::fs::write(tmp.path().join("wip.txt"), "half-finished\n").unwrap();
    run_git(tmp.path(), &["add", "wip.txt"]);

    assert_ok(&pact(tmp.path(), "commit-agent", &["init"]));

    let committed = git_out(tmp.path(), &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        !committed.contains("wip.txt"),
        "pact committed someone else's staged work: {committed}"
    );
    assert!(
        git_out(tmp.path(), &["diff", "--cached", "--name-only"]).contains("wip.txt"),
        "wip.txt must still be staged, waiting for its own commit"
    );
}

/// `--no-commit` writes the files and stops. Also the escape hatch for anyone
/// who wants pact nowhere near their history.
#[test]
fn init_no_commit_writes_the_files_and_creates_no_commit() {
    let Some(tmp) = git_repo("init_no_commit") else {
        return;
    };
    git_identity(tmp.path());

    assert_ok(&pact(tmp.path(), "commit-agent", &["init", "--no-commit"]));

    assert!(tmp.path().join("AGENTS.md").exists());
    assert_eq!(
        git_out(tmp.path(), &["rev-list", "--count", "--all"]).trim(),
        "0",
        "--no-commit must leave the history alone"
    );
}

/// A commit pact cannot make must never look like an init that failed: the
/// files are on disk and correct, so the exit status stays 0 and the reason
/// goes to stderr — the same rule the broken-pipe fix established.
#[test]
fn init_still_succeeds_when_the_commit_cannot_be_made() {
    let Some(tmp) = git_repo("init_still_succeeds_when_commit_fails") else {
        return;
    };
    git_identity(tmp.path());
    std::fs::write(tmp.path().join(".gitignore"), "AGENTS.md\n").unwrap();

    let out = pact(tmp.path(), "commit-agent", &["init"]);
    assert_ok(&out);
    assert!(
        tmp.path().join("AGENTS.md").exists(),
        "the file must still be written"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("not committed") && stderr.contains(".gitignore"),
        "stderr must say what wasn't committed and why: {stderr}"
    );
    assert_eq!(
        git_out(tmp.path(), &["rev-list", "--count", "--all"]).trim(),
        "0"
    );
}

/// `AGENTS.md` itself tells every agent to lease everything it writes, but
/// `init` used to rewrite AGENTS.md/CLAUDE.md with zero lease check at all —
/// not a narrowed race, a total absence (pact-m7j.9.3). A held lease on
/// either file must refuse the whole init, not just warn.
#[test]
fn init_refuses_to_write_through_a_live_lease_on_a_managed_file() {
    let Some(tmp) = git_repo("init_refuses_live_lease") else {
        return;
    };
    git_identity(tmp.path());

    assert_ok(&pact(
        tmp.path(),
        "alice",
        &[
            "lease",
            "acquire",
            "AGENTS.md",
            "--note",
            "editing the intro",
        ],
    ));

    let out = pact(tmp.path(), "bob", &["init", "--no-commit"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    let stderr = stderr_of(&out);
    assert!(stderr.contains("alice"), "must name the holder: {stderr}");
    assert!(
        !tmp.path().join("AGENTS.md").exists(),
        "must not write anything when refusing"
    );

    // --force overrides, exactly like every other takeover in pact.
    assert_ok(&pact(
        tmp.path(),
        "bob",
        &["init", "--no-commit", "--force"],
    ));
    assert!(tmp.path().join("AGENTS.md").exists());
}

/// The agent that leased `AGENTS.md` itself is not a peer to refuse against:
/// re-entrant `init` over your own hold must succeed without `--force`.
#[test]
fn init_writes_through_its_own_holders_lease() {
    let Some(tmp) = git_repo("init_writes_through_own_lease") else {
        return;
    };
    git_identity(tmp.path());

    assert_ok(&pact(
        tmp.path(),
        "alice",
        &["lease", "acquire", "AGENTS.md"],
    ));
    let out = pact(tmp.path(), "alice", &["init", "--no-commit"]);
    assert_ok(&out);
    assert!(tmp.path().join("AGENTS.md").exists());
}

/// `init` also writes `.gitignore`/`.gitattributes`; a live lease on either
/// must refuse the run exactly like one on `AGENTS.md`/`CLAUDE.md`.
#[test]
fn init_refuses_to_write_through_a_live_lease_on_gitignore() {
    let Some(tmp) = git_repo("init_refuses_live_lease_gitignore") else {
        return;
    };
    git_identity(tmp.path());

    assert_ok(&pact(
        tmp.path(),
        "alice",
        &["lease", "acquire", ".gitignore"],
    ));
    let out = pact(tmp.path(), "bob", &["init", "--no-commit"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr_of(&out));
    assert!(!tmp.path().join("AGENTS.md").exists());
}

fn git_identity(repo: &Path) {
    run_git(repo, &["config", "user.email", "tests@pact.invalid"]);
    run_git(repo, &["config", "user.name", "pact tests"]);
}

fn run_git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    stdout_of(&out)
}

// ------------------------------------------- init: other instruction files

/// pact-4zx, requested by init-dev in thread pact-wisp-6m2. `pact init` points
/// the agent-instruction files a repo ALREADY has back at AGENTS.md, and
/// invents none.
///
/// Both halves are load-bearing and only a real process shows them. Creating
/// `.cursorrules` in a repo that never used Cursor is pact writing config for a
/// tool nobody runs; and a *copy* of the protocol in five files is five copies
/// to drift, which `is_current()` can only police in one. So the assertion is
/// "these two files, pointing, and nothing else on disk".
#[test]
fn init_points_existing_instruction_files_at_agents_md_and_creates_none() {
    let tmp = init_repo();
    std::fs::write(tmp.path().join("GEMINI.md"), "# Gemini\n").unwrap();
    std::fs::create_dir(tmp.path().join(".github")).unwrap();
    std::fs::write(
        tmp.path().join(".github/copilot-instructions.md"),
        "# Copilot\n",
    )
    .unwrap();

    let out = pact(tmp.path(), "init-agent", &["init", "--no-commit", "--json"]);
    assert_ok(&out);
    let report = json_stdout(&out);
    let managed = report["instruction_files"]
        .as_array()
        .unwrap_or_else(|| panic!("init --json must report instruction_files: {report}"));
    assert_eq!(
        managed.len(),
        2,
        "only the files that already existed: {report}"
    );
    for want in ["GEMINI.md", "copilot-instructions.md"] {
        assert!(
            managed
                .iter()
                .any(|p| p.as_str().unwrap_or_default().ends_with(want)),
            "{want} missing from instruction_files: {report}"
        );
    }

    let gemini = std::fs::read_to_string(tmp.path().join("GEMINI.md")).unwrap();
    assert!(
        gemini.starts_with("# Gemini\n"),
        "pre-existing content must survive verbatim:\n{gemini}"
    );
    assert!(
        gemini.contains("<!-- pact:begin -->") && gemini.contains("@AGENTS.md"),
        "GEMINI.md must carry a managed block importing AGENTS.md:\n{gemini}"
    );
    assert!(
        !gemini.contains("PACT_AGENT"),
        "a pointer, never a second copy of the protocol:\n{gemini}"
    );

    for invented in [".windsurfrules", ".cursorrules", ".clinerules"] {
        assert!(
            !tmp.path().join(invented).exists(),
            "init created {invented} in a repo that never had one"
        );
    }

    // Idempotent, like the AGENTS.md and CLAUDE.md blocks before it.
    assert_ok(&pact(tmp.path(), "init-agent", &["init", "--no-commit"]));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("GEMINI.md")).unwrap(),
        gemini,
        "a second init changed GEMINI.md"
    );
}

/// pact-m7j.9.5. The AGENTS.md-alias filter in `present_targets` only ever
/// excluded a candidate that canonicalizes to AGENTS.md's OWN path — nothing
/// caught two OTHER candidates resolving to each other (a dotfiles-style
/// setup: `GEMINI.md` and `.cursorrules` both symlinked to one shared file).
/// Before the fix, `init` spliced both nominal targets independently; since
/// they are the same underlying file, the second write clobbers the first's
/// block, and `doctor` reports one of them stale immediately after `init`
/// claimed to update it. The fix skips the later alias instead of writing it.
#[cfg(unix)]
#[test]
fn init_skips_the_later_of_two_instruction_targets_aliased_to_each_other() {
    let tmp = init_repo();
    let root = tmp.path();

    // A shared file outside the managed filename list, symlinked at by two
    // different `INSTRUCTION_TARGETS` names — the dotfiles-style layout the
    // design note calls "likely intentional, not broken".
    std::fs::write(root.join("shared-instructions.md"), "# shared\n").unwrap();
    std::os::unix::fs::symlink("shared-instructions.md", root.join("GEMINI.md")).unwrap();
    std::os::unix::fs::symlink("shared-instructions.md", root.join(".cursorrules")).unwrap();

    let out = pact(root, "init-agent", &["init", "--no-commit", "--json"]);
    assert_ok(&out);
    let report = json_stdout(&out);
    let managed = report["instruction_files"]
        .as_array()
        .unwrap_or_else(|| panic!("init --json must report instruction_files: {report}"));
    assert_eq!(
        managed.len(),
        1,
        "the two aliases must count as one managed file: {report}"
    );
    assert!(
        managed[0]
            .as_str()
            .unwrap_or_default()
            .ends_with("GEMINI.md"),
        "the earlier-listed alias (GEMINI.md) should be the one written: {report}"
    );

    let shared = std::fs::read_to_string(root.join("shared-instructions.md")).unwrap();
    assert_eq!(
        shared.matches("<!-- pact:begin -->").count(),
        1,
        "the shared file must carry exactly one managed block, not one per alias:\n{shared}"
    );

    // The acceptance check: doctor must not report anything stale right after
    // an init that claimed to update it. Not `assert_ok`: overall health also
    // depends on unrelated checks (e.g. no Beads CLI on PATH in one `mise run
    // test` leg), so only this one check's `ok` is asserted.
    let doc = pact(root, "init-agent", &["doctor", "--json"]);
    let checks = json_stdout(&doc);
    let check = checks["checks"]
        .as_array()
        .expect("doctor emits a checks array")
        .iter()
        .find(|c| c["name"] == "other instruction files current")
        .expect("doctor should have an opinion about instruction files");
    assert_eq!(
        check["ok"], true,
        "doctor must not flag a file init just wrote as stale: {check}"
    );
}

// ------------------------------------------- msg over the br backend (pact-l94)

/// pact-l94, requested by br-dev in thread pact-wisp-0ma. The plain round trip
/// on br, which is a sharper guard than it looks: br rejects
/// `--no-inherit-labels` with `error: unexpected argument`, so a regression in
/// the create argv is a hard failure on EVERY send rather than a subtle one.
/// Read state rides along because `read-by-<agent>` labels are the one part of
/// the messaging model that needed no br-specific code — and "needed no change"
/// is exactly the claim that rots without a test.
#[test]
fn br_backed_msg_round_trip_carries_read_state_the_way_bd_does() {
    let Some(tmp) = br_repo("br_backed_msg_round_trip_carries_read_state_the_way_bd_does") else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "sender-agent",
        &[
            "msg",
            "send",
            "--to",
            "reader-agent",
            "--subject",
            "ack me",
            "please ack",
        ],
    ));

    let inbox = inbox_json(tmp.path(), "reader-agent");
    assert_eq!(inbox.as_array().map(Vec::len), Some(1), "{inbox}");
    assert_eq!(inbox[0]["from"], serde_json::json!("sender-agent"));
    assert_eq!(inbox[0]["to"], serde_json::json!("reader-agent"));
    assert_eq!(inbox[0]["read"], serde_json::json!(false));
    assert_eq!(inbox[0]["body"], serde_json::json!("please ack"));
    let id = inbox[0]["id"].as_str().unwrap().to_string();

    let before = pact(tmp.path(), "sender-agent", &["msg", "sent"]);
    assert_ok(&before);
    assert!(
        stdout_of(&before).contains("1 not read yet"),
        "{}",
        stdout_of(&before)
    );

    assert_ok(&pact(tmp.path(), "reader-agent", &["msg", "read", &id]));

    let after = pact(
        tmp.path(),
        "reader-agent",
        &["msg", "inbox", "--unread-only", "--json"],
    );
    assert_ok(&after);
    assert_eq!(
        json_stdout(&after).as_array().map(Vec::len),
        Some(0),
        "reading must clear the reader's unread on br too: {}",
        stdout_of(&after)
    );
    let confirmed = pact(tmp.path(), "sender-agent", &["msg", "sent"]);
    assert_ok(&confirmed);
    assert!(
        stdout_of(&confirmed).contains("0 not read yet"),
        "the read must be visible to the sender: {}",
        stdout_of(&confirmed)
    );
}

/// The br twin of `every_recipient_of_a_fan_out_sees_one_thread_and_can_reply_into_it`,
/// and the one br-dev asked not to skip.
///
/// bd answers "which messages are in this thread" with `list --parent <id>`.
/// br has no such filter and its `list --json` omits `parent` entirely, so on br
/// the answer is reconstructed from the ROOT's `parent-child` dependents out of
/// `show --json`. A naive port passes every unit test and still reports each
/// reply as its own one-message thread — the shared decision fragmenting exactly
/// as pact-rnc.4 described, one backend later. Only a live br proves the walk.
#[test]
fn br_backed_fan_out_shares_one_thread_and_a_reply_reaches_the_root() {
    let Some(tmp) = br_repo("br_backed_fan_out_shares_one_thread_and_a_reply_reaches_the_root")
    else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "alpha",
        &[
            "msg",
            "send",
            "--to",
            "bravo",
            "--to",
            "charlie",
            "--subject",
            "fan out",
            "friday?",
        ],
    ));

    let bravo = inbox_json(tmp.path(), "bravo");
    let root = bravo[0]["id"].as_str().expect("bravo got it").to_string();
    assert_eq!(bravo[0]["thread"], serde_json::json!(root));

    let charlie = inbox_json(tmp.path(), "charlie");
    let charlie_id = charlie[0]["id"]
        .as_str()
        .expect("charlie got it")
        .to_string();
    assert_ne!(charlie_id, root, "recipient 2 has its own bead");
    assert_eq!(
        charlie[0]["thread"],
        serde_json::json!(root),
        "both recipients must report the same thread: {charlie}"
    );

    // Reading a non-root member gives the whole conversation, not a stub.
    let read = pact(
        tmp.path(),
        "charlie",
        &["msg", "read", &charlie_id, "--json"],
    );
    assert_ok(&read);
    assert_eq!(
        json_stdout(&read).as_array().map(Vec::len),
        Some(2),
        "{}",
        stdout_of(&read)
    );

    assert_ok(&pact(
        tmp.path(),
        "charlie",
        &[
            "msg",
            "send",
            "--to",
            "alpha",
            "--thread",
            &charlie_id,
            "--subject",
            "re: fan out",
            "charlie acks",
        ],
    ));

    let as_alpha = pact(tmp.path(), "alpha", &["msg", "read", &root, "--json"]);
    assert_ok(&as_alpha);
    let full = json_stdout(&as_alpha);
    assert_eq!(
        full.as_array().map(Vec::len),
        Some(3),
        "a reply parented on a child must still land in the root thread: {full}"
    );
    let bodies: Vec<&str> = full
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap_or_default())
        .collect();
    assert!(bodies.contains(&"charlie acks"), "{full}");
}

// ------------------------------------------------ --json shapes (pact-er0)

/// pact-er0. The `--json` shapes are an API nobody wrote down, and they have
/// already broken twice in silence: `pact init --json` emitted a bare path
/// string, became an object with three keys, then gained three more the same
/// day; `lease release --json` went string -> object earlier. Both were
/// breaking for anything reading them and neither was noticed, because nothing
/// in the tree contradicted the change.
///
/// The protocol block in AGENTS.md tells every agent to prefer `--json` over
/// parsing human output, so each of these shapes is something a peer parses.
/// Pinning the top-level keys does not make a shape *good* — several below are
/// frankly inconsistent, see the comments — it makes changing one a deliberate
/// act that edits this file, which is the failure mode that actually happened.
///
/// Adding a key breaks these tests too, on purpose: an added key is how both
/// historical breakages arrived, and a reviewer seeing this file in the diff is
/// the whole mechanism.
fn assert_object_keys(what: &str, v: &serde_json::Value, expected: &[&str]) {
    let mut got: Vec<&str> = v
        .as_object()
        .unwrap_or_else(|| panic!("`{what} --json` must be a JSON object, got: {v}"))
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "`{what} --json` changed shape. This is an API agents parse (pact-er0): \
         update the expectation here in the same commit, and say so in the \
         changelog.\nfull payload: {v}"
    );
}

/// Array-ness is asserted separately because it is invisible in a key
/// comparison, and it is precisely what changed under `lease release --json`.
fn assert_array_of(what: &str, v: &serde_json::Value, expected: &[&str]) {
    let arr = v
        .as_array()
        .unwrap_or_else(|| panic!("`{what} --json` must be a JSON array, got: {v}"));
    let first = arr.first().unwrap_or_else(|| {
        panic!("`{what} --json` needs a non-empty sample here to pin its element shape")
    });
    assert_object_keys(&format!("{what}[]"), first, expected);
}

/// Every `--json` shape that does not need a Beads backend. One test, because
/// the value is in the list being exhaustive: a command missing from it is a
/// shape nobody is guarding.
#[test]
fn json_shapes_of_every_command_that_needs_no_beads_backend() {
    let tmp = init_repo();
    let repo = tmp.path();
    let run = |args: &[&str]| {
        let out = pact(repo, "shape-agent", args);
        assert_ok(&out);
        json_stdout(&out)
    };

    assert_object_keys(
        "init",
        &run(&["init", "--no-commit", "--json"]),
        &[
            "agents_md",
            "claude_md",
            "claude_md_status",
            "instruction_files",
            "commit",
            "committed_files",
            "commit_status",
        ],
    );

    assert_object_keys(
        "whoami",
        &run(&["whoami", "--json"]),
        &[
            "agent",
            "agent_source",
            "pact_binary",
            "repo_root",
            "pact_dir",
            "bd_binary",
            "bd_version",
            "problems",
        ],
    );

    // A single-path acquire stays an OBJECT while a multi-path one is an ARRAY
    // of the same element — already pinned by
    // `acquire_single_path_json_stays_an_object`, repeated here so the two
    // shapes sit next to each other in the contract.
    const LEASE: &[&str] = &["path", "agent", "acquired_at", "ttl_secs", "note"];
    let one = run(&["lease", "acquire", "a.rs", "--json"]);
    assert_object_keys("lease acquire <one>", &one, &["lease", "stolen"]);
    assert_object_keys("lease acquire <one>.lease", &one["lease"], LEASE);
    assert_array_of(
        "lease acquire <many>",
        &run(&["lease", "acquire", "b.rs", "c.rs", "--json"]),
        &["lease", "stolen"],
    );

    // `renew` returns the bare lease, not the `{lease, stolen}` envelope
    // `acquire` returns. Inconsistent, and deliberately pinned as-is: this test
    // records the contract, it does not get to quietly improve it.
    assert_object_keys(
        "lease renew",
        &run(&["lease", "renew", "a.rs", "--json"]),
        LEASE,
    );

    let ls = run(&["lease", "ls", "--json"]);
    assert_array_of(
        "lease ls",
        &ls,
        &["lease", "age_secs", "remaining_secs", "expired"],
    );
    assert_object_keys("lease ls[].lease", &ls[0]["lease"], LEASE);

    // pact-3dz: `--print --json` used to emit raw markdown at exit 0, so a
    // script piping to jq failed to parse while pact reported success.
    assert_object_keys(
        "init --print",
        &run(&["init", "--print", "--json"]),
        &["block"],
    );

    assert_array_of(
        "agents",
        &run(&["agents", "--json"]),
        &[
            "name",
            "last_seen",
            "leases_held",
            // Added by pact-6sx: lease events from the log, so an agent that
            // released its last lease is still a known agent.
            "lease_events",
            "messages_sent",
            "messages_received",
        ],
    );

    assert_array_of(
        "log",
        &run(&["log", "--json"]),
        &["at", "kind", "agent", "target", "detail"],
    );

    assert_object_keys(
        "lease release <one>",
        &run(&["lease", "release", "a.rs", "--json"]),
        &["path", "displaced"],
    );

    // `release --all` is an array of bare path STRINGS, not of the
    // `{path, displaced}` objects the single-path form emits. Whoever fixes
    // that will have to come here first, which is the entire point.
    let all = run(&["lease", "release", "--all", "--json"]);
    let paths = all
        .as_array()
        .unwrap_or_else(|| panic!("`lease release --all --json` must be an array: {all}"));
    assert!(
        paths.iter().all(serde_json::Value::is_string),
        "`lease release --all --json` is an array of path strings: {all}"
    );

    // `doctor` is the one command whose exit code tracks its findings, so it is
    // run without `assert_ok` — the shape must hold whether or not the repo is
    // healthy, and a tempdir with no bd is not.
    let doctor = json_stdout(&pact(repo, "shape-agent", &["doctor", "--json"]));
    assert_object_keys("doctor", &doctor, &["healthy", "checks"]);
    assert_array_of(
        "doctor.checks",
        &doctor["checks"],
        &["name", "ok", "warn", "detail"],
    );
}

/// The other half of pact-er0: everything `pact msg` emits. All four commands
/// return an ARRAY of the same message record — including `msg send`, which
/// returns one element per recipient, and `msg read`, which returns the thread
/// rather than the one message you asked for.
#[test]
fn json_shapes_of_every_msg_command() {
    let Some(tmp) = bd_repo("json_shapes_of_every_msg_command") else {
        return;
    };
    const MESSAGE: &[&str] = &[
        "id",
        "thread",
        "from",
        "to",
        "subject",
        "body",
        "created_at",
        "read",
        "read_by",
    ];

    let sent = pact(
        tmp.path(),
        "alpha",
        &[
            "msg",
            "send",
            "--to",
            "bravo",
            "--subject",
            "shapes",
            "--json",
            "body",
        ],
    );
    assert_ok(&sent);
    assert_array_of("msg send", &json_stdout(&sent), MESSAGE);

    let inbox = inbox_json(tmp.path(), "bravo");
    assert_array_of("msg inbox", &inbox, MESSAGE);
    let id = inbox[0]["id"].as_str().unwrap().to_string();

    let read = pact(tmp.path(), "bravo", &["msg", "read", &id, "--json"]);
    assert_ok(&read);
    assert_array_of("msg read", &json_stdout(&read), MESSAGE);

    let outbox = pact(tmp.path(), "alpha", &["msg", "sent", "--json"]);
    assert_ok(&outbox);
    assert_array_of("msg sent", &json_stdout(&outbox), MESSAGE);
}

// -------------------------------------------------------------- exit codes

/// Exit 2 is documented as "lease held by another agent", and the protocol
/// block tells agents to branch on the code rather than the message text. clap
/// also exits 2 for any usage error, so that instruction was unfollowable: two
/// agents hit the collision in one fleet run (an unrecognized subcommand, and a
/// `--thread` left valueless by shell word-splitting), and a wrapper branching
/// on 2 reads a typo as a lease conflict. Usage errors are 5 now (pact-8ou).
#[test]
fn usage_errors_exit_5_so_that_2_still_means_only_a_held_lease() {
    let tmp = init_repo();

    for args in [
        vec!["nosuchcommand"],
        vec!["lease"],                      // subcommand required
        vec!["lease", "acquire"],           // <PATHS> required
        vec!["msg", "send", "--to"],        // flag with no value
        vec!["lease", "acquire", "--nope"], // unknown flag
    ] {
        let out = pact(tmp.path(), "usage-agent", &args);
        assert_eq!(
            out.status.code(),
            Some(5),
            "`pact {}` should be a usage error, not exit {:?}\nstderr: {}",
            args.join(" "),
            out.status.code(),
            stderr_of(&out)
        );
    }

    // The whole point: a real lease conflict keeps 2 to itself.
    assert_ok(&pact(tmp.path(), "agent-a", &["lease", "acquire", "x.txt"]));
    let conflict = pact(tmp.path(), "agent-b", &["lease", "acquire", "x.txt"]);
    assert_eq!(conflict.status.code(), Some(2), "{}", stderr_of(&conflict));
}

/// `--help` and `-V` travel clap's error path but are not errors. Bare `pact`
/// does NOT get that treatment: clap prints help there because the invocation
/// was incomplete, and exiting 0 would let a script whose variable expanded to
/// nothing read it as success — the very interpolation bug 5 disambiguates.
#[test]
fn help_and_version_exit_0_but_a_bare_invocation_does_not() {
    let tmp = init_repo();

    for args in [
        vec!["--help"],
        vec!["-V"],
        vec!["--version"],
        vec!["lease", "--help"],
    ] {
        let out = pact(tmp.path(), "usage-agent", &args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "`pact {}` is a request, not an error",
            args.join(" ")
        );
        assert!(
            !stdout_of(&out).is_empty(),
            "help/version must go to stdout"
        );
    }

    let bare = pact(tmp.path(), "usage-agent", &[]);
    assert_eq!(
        bare.status.code(),
        Some(5),
        "bare `pact` is an incomplete invocation, not a successful one"
    );
}

// ----------------------------------------------------------------- ownership

/// pact modelled who HOLDS a path and nothing else, so a released path became
/// indistinguishable from one nobody had ever opened. In a nine-agent run that
/// cost `src/doctor.rs` blocking two agents in sequence, and one word-fix being
/// routed by three agents then nearly applied twice (pact-o38).
#[test]
fn a_released_path_still_remembers_who_worked_on_it() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-one",
        &[
            "lease",
            "acquire",
            "src/doctor.rs",
            "--note",
            "half of the bead lives here",
        ],
    ));
    assert_ok(&pact(
        tmp.path(),
        "agent-one",
        &["lease", "release", "--all"],
    ));

    // `lease ls` is still only about locks held right now.
    let plain = stdout_of(&pact(tmp.path(), "agent-two", &["lease", "ls"]));
    assert!(!plain.contains("src/doctor.rs"), "{plain}");

    // `--all` is "everything pact knows about paths", which now includes this.
    let all = stdout_of(&pact(tmp.path(), "agent-two", &["lease", "ls", "--all"]));
    assert!(
        all.contains("src/doctor.rs") && all.contains("agent-one"),
        "{all}"
    );

    // The scriptable answer, and the reason it is not folded into `ls --json`:
    // a released path has no lock, so it cannot honestly be a LeaseEntry.
    let out = pact(
        tmp.path(),
        "agent-two",
        &["agents", "--for", "src/doctor.rs", "--json"],
    );
    assert_ok(&out);
    let v = json_stdout(&out);
    assert_eq!(v["agent"], "agent-one");
    assert_eq!(v["last"], "released");
    assert_eq!(v["note"], "half of the bead lives here");
}

/// Advisory, never blocking: the acquire succeeds, and the note goes to stderr
/// *after* the success line so the reader sees what happened before what they
/// should know.
#[test]
fn acquiring_a_recently_released_path_warns_but_succeeds() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-one",
        &["lease", "acquire", "f.txt"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "agent-one",
        &["lease", "release", "--all"],
    ));

    let out = pact(tmp.path(), "agent-two", &["lease", "acquire", "f.txt"]);
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("agent-one"),
        "must name who to ask: {stderr}"
    );
    assert!(
        stderr.contains("--to-owner-of"),
        "must say how to reach them: {stderr}"
    );
    assert!(
        stdout_of(&out).contains("acquired lease"),
        "advisory must not block"
    );
}

/// Your own history is not news. A long task that renews or re-acquires its own
/// path would otherwise warn about itself on every call, which is how a warning
/// becomes noise people filter out.
#[test]
fn re_acquiring_your_own_path_says_nothing() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "agent-one",
        &["lease", "acquire", "f.txt"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "agent-one",
        &["lease", "release", "--all"],
    ));

    let again = pact(tmp.path(), "agent-one", &["lease", "acquire", "f.txt"]);
    assert_ok(&again);
    assert!(
        !stderr_of(&again).contains("last"),
        "should not warn about itself: {}",
        stderr_of(&again)
    );
}

#[test]
fn agents_for_an_untouched_path_answers_rather_than_failing() {
    let tmp = init_repo();
    let out = pact(
        tmp.path(),
        "agent-one",
        &["agents", "--for", "never/seen.rs", "--json"],
    );
    assert_ok(&out); // "no owner" is an answer, like whoami's missing identity
    assert_eq!(json_stdout(&out)["agent"], serde_json::Value::Null);
}

/// The acceptance criterion: a handoff addressed to a FILE reaches whoever
/// worked on it, after its author has exited. 51 of 59 messages in one fleet
/// run were never read because they were addressed to processes, not to work.
#[test]
fn a_message_addressed_to_a_path_reaches_the_agent_who_last_held_it() {
    let Some(tmp) = bd_repo("a_message_addressed_to_a_path") else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "acquire", "src/api.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "release", "--all"],
    ));

    // second-agent never learns "first-owner" — it addresses the file.
    let sent = pact(
        tmp.path(),
        "second-agent",
        &[
            "msg",
            "send",
            "--to-owner-of",
            "src/api.rs",
            "--subject",
            "handoff",
            "found a bug in your change",
        ],
    );
    assert_ok(&sent);

    let inbox = pact(tmp.path(), "first-owner", &["msg", "inbox"]);
    assert_ok(&inbox);
    assert!(
        stdout_of(&inbox).contains("handoff"),
        "the path's owner should have received it: {}",
        stdout_of(&inbox)
    );
}

/// pact-m7j.10.5, reproduced in a real fleet run: when every `--to-owner-of`
/// path resolves to the sender itself and no `--to` was given, the command
/// used to bail with "no recipients resolved" — stranding an agent that had
/// just taken over a path with no way to leave a note for whoever it took it
/// from, the exact case `--to-owner-of` exists to save them from having to
/// guess a name for. It now addresses `human` instead, and the note still
/// follows the file to whoever leases it next via the same about-<path>
/// pipeline `--to` never controlled in the first place.
#[test]
fn to_owner_of_self_resolution_addresses_human_instead_of_dropping_the_message() {
    let Some(tmp) = bd_repo("to_owner_of_self_resolution") else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "src/x.rs"],
    ));

    let sent = pact(
        tmp.path(),
        "agent-a",
        &[
            "msg",
            "send",
            "--to-owner-of",
            "src/x.rs",
            "note for whoever's next",
        ],
    );
    assert_ok(&sent);
    assert!(
        stderr_of(&sent).contains("human"),
        "must say it fell back to human: {}",
        stderr_of(&sent)
    );

    assert_ok(&pact(tmp.path(), "agent-a", &["lease", "release", "--all"]));

    // Whoever leases the path next sees it, exactly like an ordinary
    // about-<path> message — the fallback recipient never gated that pipeline.
    let out = pact(tmp.path(), "agent-b", &["lease", "acquire", "src/x.rs"]);
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("unread message") && stderr.contains("note for whoever's next"),
        "the note must still follow the file: {stderr}"
    );
}

#[test]
fn to_owner_of_an_untouched_path_is_an_error_not_a_silent_no_op() {
    let Some(tmp) = bd_repo("to_owner_of_an_untouched_path") else {
        return;
    };
    let out = pact(
        tmp.path(),
        "sender",
        &["msg", "send", "--to-owner-of", "nobody/touched.rs", "body"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    assert!(stderr_of(&out).contains("no owner"), "{}", stderr_of(&out));
}

/// pact leases file paths; the Beads store its own messaging lives in is shared
/// mutable state no lease covered and no check guarded. `br --db /elsewhere.db
/// init` ignored its own --db and initialised in the cwd of this live repo at
/// exit 0, leaving a second database next to the real one. classify_workspace
/// resolved it correctly — Dolt first, because that is where the data is — and
/// that correct tiebreak is exactly what made it invisible (pact-nv4).
#[test]
fn doctor_warns_when_two_beads_stores_share_one_directory() {
    let tmp = init_repo();
    let beads = tmp.path().join(".beads");
    std::fs::create_dir_all(beads.join("embeddeddolt")).unwrap();

    let check = |repo: &Path| -> serde_json::Value {
        json_stdout(&pact(repo, "store-agent", &["doctor", "--json"]))["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "one Beads store")
            .cloned()
            .unwrap()
    };

    let clean = check(tmp.path());
    assert_eq!(clean["warn"], false, "one store is not a problem: {clean}");

    // The stray br store, plus the -wal/-shm siblings it writes alongside.
    for f in ["beads.db", "beads.db-wal", "beads.db-shm"] {
        std::fs::write(beads.join(f), "").unwrap();
    }

    let conflicted = check(tmp.path());
    assert_eq!(conflicted["warn"], true, "{conflicted}");
    assert_eq!(
        conflicted["ok"], true,
        "warns, never fails: two stores can coexist mid-migration, and pact still \
         resolves correctly — {conflicted}"
    );
    let detail = conflicted["detail"].as_str().unwrap();
    // Naming both halves is the whole value: which one wins, which is ignored.
    assert!(
        detail.contains("embeddeddolt"),
        "must name the store in use: {detail}"
    );
    assert!(
        detail.contains("beads.db"),
        "must name the ignored store: {detail}"
    );
}

/// pact-m7j.8.5: `PACT_STATE_DIR` had no collision detection at all. Point two
/// UNRELATED checkouts at the same override directory — the mistake it exists
/// to make possible for tests, the fleet harness and demos — and their leases
/// silently shared one space with no signal that anything was wrong: before
/// this fix, `pact doctor` from the second repo reported `Placement::
/// StateDirOverride` with no check that noticed the other repo's lease
/// sitting right there.
///
/// Two independently-created repos, not a submodule and its superproject (the
/// bead's other suggested shape): the simpler way to get two checkouts that
/// share nothing at all — no worktree relationship, no common ancestor
/// directory, nothing but the environment variable in common.
#[test]
fn doctor_flags_a_state_dir_shared_with_an_unrelated_repository() {
    let repo_a = init_repo();
    let repo_b = init_repo();
    let shared_state = tempfile::tempdir().unwrap();

    // A lease from repo A, landing in the directory both repos are about to
    // share. Its path's parent (`only-in-a/`) exists under neither bare test
    // repo, but that's fine — nothing here checks repo A against itself.
    let acquire = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["lease", "acquire", "only-in-a/marker.rs"])
        .current_dir(repo_a.path())
        .env_remove("PACT_AGENT")
        .env("PACT_AGENT", "agent-a")
        .env("PACT_STATE_DIR", shared_state.path())
        .output()
        .expect("failed to run pact binary");
    assert_ok(&acquire);

    let check = |repo: &Path| -> serde_json::Value {
        let out = Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["doctor", "--json"])
            .current_dir(repo)
            .env_remove("PACT_AGENT")
            .env("PACT_AGENT", "agent-b")
            .env("PACT_STATE_DIR", shared_state.path())
            .output()
            .expect("failed to run pact binary");
        json_stdout(&out)["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "state dir isolation")
            .cloned()
            .unwrap_or_else(|| panic!("no `state dir isolation` check in {out:?}"))
    };

    let from_b = check(repo_b.path());
    assert_eq!(
        from_b["warn"], true,
        "a lease from an unrelated repo sharing this PACT_STATE_DIR must be flagged: {from_b}"
    );
    assert_eq!(
        from_b["ok"], true,
        "warns, never fails — the same contract as `one Beads store`: {from_b}"
    );
    let detail = from_b["detail"].as_str().unwrap();
    assert!(
        detail.contains("only-in-a/marker.rs") && detail.contains("agent-a"),
        "must name the foreign lease and its holder: {detail}"
    );
}

/// pact-m7j.10.7: `conflicting_stores` had exactly one call site in the whole
/// binary — `doctor.rs`. Reproduced live: with the identical on-disk conflict
/// the test above exercises, `pact doctor` warned correctly and, seconds
/// later, `pact msg inbox` against the SAME repo reported "inbox empty", exit
/// 0, and said nothing about the second store it was silently ignoring.
/// `msg.rs`, `run_msg` and the MCP message tools had no side channel to report
/// it. This is the plumbing fix: `run_msg` now calls the same
/// `beads::conflict_warning` doctor.rs does, so the fact reaches stderr on
/// every msg invocation where it is true.
#[test]
fn msg_inbox_names_a_conflicting_store_on_its_own_stderr() {
    let Some(tmp) = bd_repo("msg_inbox_names_a_conflicting_store_on_its_own_stderr") else {
        return;
    };
    // The stray br store beside the real bd one `bd_repo` just created — the
    // forward-order half of pact-nv4's own incident, reused here rather than
    // invented fresh so this test is provably the same conflict doctor sees.
    std::fs::write(tmp.path().join(".beads").join("beads.db"), "").unwrap();

    let out = pact(tmp.path(), "someone", &["msg", "inbox"]);
    assert_ok(&out);
    assert_eq!(
        stdout_of(&out).trim(),
        "inbox empty",
        "the resolution itself is unchanged by this fix — bd is still queried"
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("two stores in .beads/") && stderr.contains("beads.db"),
        "msg inbox must name the conflict pact doctor already sees, not stay \
         silent about it: {stderr}"
    );
}

/// pact-m7j.10.6, the reverse-order half of the same incident, reproduced
/// exactly: `br init`, then a REAL message (`br create` with a live
/// `--assignee`), then `bd init` in the same `.beads/`. `bd init` succeeds at
/// exit 0 with no warning of its own and leaves its empty `embeddeddolt/` as
/// what `BeadsCli::locate()` reads from then on — confirmed live via `bd list
/// --json` returning `[]` while `br list --json` still shows the record.
/// `PACT_AGENT=someone pact msg inbox`, the exact command AGENTS.md tells
/// every agent to run first, used to answer "inbox empty" with no hint that
/// its own recipient's real message was sitting one directory entry away.
///
/// Fixing the tiebreak itself (data-aware `classify_workspace`, or refusing
/// `bd init` outright over a non-empty store) is a declined-for-now
/// architecture question — see `beads::conflict_warning`'s doc comment. What
/// this test proves is the mitigation: the silence is gone even though the
/// resolution is not.
#[test]
fn msg_inbox_after_bd_init_over_a_live_br_store_still_warns() {
    let Some(tmp) = br_repo("msg_inbox_after_bd_init_over_a_live_br_store_still_warns_br_half")
    else {
        return;
    };

    let create = Command::new("br")
        .args([
            "create",
            "--type=message",
            "--title=hello",
            "--description=a real message, about to be shadowed",
            "--assignee=someone",
            "--actor=sender",
            "--json",
        ])
        .current_dir(tmp.path())
        .output()
        .expect("br create");
    assert!(create.status.success(), "br create: {}", stderr_of(&create));

    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("bd").is_file()))
        .unwrap_or(false);
    if !on_path {
        eprintln!(
            "SKIP msg_inbox_after_bd_init_over_a_live_br_store_still_warns: \
             bd not found on PATH"
        );
        return;
    }
    let bd_init = Command::new("bd")
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .expect("bd init");
    assert!(
        bd_init.status.success(),
        "bd init atop a live br store must still succeed (that half of the \
         behaviour is unchanged by this fix): {}",
        stderr_of(&bd_init)
    );

    let out = pact(tmp.path(), "someone", &["msg", "inbox"]);
    assert_ok(&out);
    // (a) the underlying resolution is unchanged by design: bd's freshly
    // created, empty store is still what answers, so the real message stays
    // invisible here — that is the declined tiebreak change, not a bug this
    // fix introduces.
    assert_eq!(
        stdout_of(&out).trim(),
        "inbox empty",
        "bd's own store has no messages, and that tiebreak is not what this fix changes"
    );
    // (b) but the mitigation fires in exactly this reverse-order scenario,
    // not only the forward one the previous test covers.
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("two stores in .beads/"),
        "must name the conflict in the br-then-bd order this bug reproduced: {stderr}"
    );
}

// ------------------------------------------------------- telemetry (pact-aw7)

/// A collector that completes the TCP handshake and then never says another
/// word: a listener nothing ever accepts from, so the kernel's backlog answers
/// the SYN and no byte ever comes back. That is what a *wedged* collector looks
/// like, and it is the state that decided this epic's transport — the
/// `opentelemetry-otlp` prototype spent 1031 ms per command in it, twenty times
/// pact's exit budget (otel-core, pact-aw7.1). A merely *closed* port fails
/// fast and would prove nothing, which is why this is a real listener and not
/// an unused port number.
fn blackholed_collector() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a blackhole");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    // Non-blocking so the test can ask "did anything connect?" without hanging
    // when the answer is no — which is the expected answer in a default build.
    listener.set_nonblocking(true).unwrap();
    (listener, url)
}

/// Run pact with telemetry either off or pointed at `endpoint`, and time it.
///
/// The `env_remove` list is not defensive padding. The machine this was written
/// on exports Claude Code to a gRPC collector via `OTEL_EXPORTER_OTLP_PROTOCOL`,
/// and `cargo test` inherits the developer's environment: leave those variables
/// to chance and the "telemetry on" run silently exports nothing, so the test
/// passes by measuring nothing at all.
fn timed_pact(
    repo: &Path,
    agent: &str,
    args: &[&str],
    endpoint: Option<&str>,
) -> (Output, std::time::Duration) {
    let mut cmd = pact_cmd(repo, args);
    cmd.env("PACT_AGENT", agent);
    for key in [
        "OTEL_SDK_DISABLED",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
        "OTEL_SERVICE_NAME",
    ] {
        cmd.env_remove(key);
    }
    if let Some(url) = endpoint {
        cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", url)
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json");
    }
    let start = std::time::Instant::now();
    let out = cmd.output().expect("failed to run pact binary");
    (out, start.elapsed())
}

/// The invariant the whole telemetry epic hangs on, asked for independently by
/// otel-core (pact-wisp-hvo), lease-metrics (pact-wisp-rcm) and msg-metrics
/// (pact-wisp-ldu): a collector that never answers must change neither exit
/// code, nor stdout, nor how long pact takes to get out of the way.
///
/// Both documented lease codes are exercised, because they fail differently: 0
/// runs the whole happy path with an export at the end, and 2 is the one an
/// agent's script branches on. Exit codes are API (README), so a telemetry
/// layer that can move one is a bug in every consumer at once.
///
/// The test is deliberately NOT `#[cfg(feature = "otel")]`. In the default
/// build it asserts something just as load-bearing — that the same variables
/// are inert, and that nothing dials out of a binary built without the feature.
#[test]
fn a_wedged_collector_changes_neither_exit_code_nor_stdout_nor_exit_latency() {
    let (blackhole, endpoint) = blackholed_collector();

    // Two repos so the runs see identical state rather than each other's leases.
    let quiet = init_repo();
    let traced = init_repo();

    let scenario = |repo: &Path, ep: Option<&str>| {
        let free = timed_pact(repo, "agent-a", &["lease", "acquire", "src/x.rs"], ep);
        let held = timed_pact(repo, "agent-b", &["lease", "acquire", "src/x.rs"], ep);
        (free, held)
    };

    let ((q_free, q_free_t), (q_held, q_held_t)) = scenario(quiet.path(), None);
    let ((t_free, t_free_t), (t_held, t_held_t)) = scenario(traced.path(), Some(&endpoint));

    assert_ok(&q_free);
    assert_ok(&t_free);
    assert_eq!(q_held.status.code(), Some(2), "{}", stderr_of(&q_held));
    assert_eq!(
        t_held.status.code(),
        Some(2),
        "a wedged collector must not turn `2 = held by another agent` into \
         anything else\nstdout: {}\nstderr: {}",
        stdout_of(&t_held),
        stderr_of(&t_held)
    );

    // Byte-identical stdout, not merely "looks right". stderr is excluded on
    // purpose: the conflict message carries the lease's age, which legitimately
    // differs between two runs.
    assert_eq!(
        stdout_of(&t_free),
        stdout_of(&q_free),
        "telemetry leaked into stdout on the success path"
    );
    assert_eq!(
        stdout_of(&t_held),
        stdout_of(&q_held),
        "telemetry leaked into stdout on the conflict path"
    );

    // Latency is measured as a delta against the same commands with telemetry
    // off, so a slow machine moves both sides. 500 ms of slack is ~10x the
    // measured cost of this state (+31.7 ms, bounded by otel::EXIT_BUDGET_MS)
    // and still an order of magnitude under the 1031 ms regression it guards.
    let quiet_total = q_free_t + q_held_t;
    let traced_total = t_free_t + t_held_t;
    assert!(
        traced_total < quiet_total + std::time::Duration::from_millis(500),
        "a collector that never answers delayed exit: {traced_total:?} against \
         {quiet_total:?} with telemetry off"
    );

    // Everything above would also pass if pact had simply not tried, so ask the
    // blackhole whether anyone knocked. This is what keeps the assertions honest
    // when someone changes how the endpoint is resolved.
    let connected = blackhole.accept().is_ok();
    if cfg!(feature = "otel") {
        assert!(
            connected,
            "nothing reached the collector, so this test proved nothing — \
             check how OTEL_EXPORTER_OTLP_ENDPOINT is resolved"
        );
    } else {
        assert!(
            !connected,
            "a build without the `otel` feature must ignore OTEL_* entirely; \
             something opened a socket"
        );
    }
}

/// `pact doctor` on an unhealthy repo must report and exit 1 — not exit behind
/// main's back. It used to end in `std::process::exit(1)`, which skips every
/// destructor: the failing doctor run, the only one anybody troubleshoots,
/// flushed no telemetry at all (span-dev, pact-aw7.2). The fix is invisible
/// from the outside, which is exactly why it needs a test — the next person to
/// reach for `process::exit` here will pass every other test in this file.
#[test]
fn an_unhealthy_doctor_reports_on_stdout_and_exits_1_without_short_circuiting() {
    let tmp = init_repo();
    let out = pact(tmp.path(), "doctor-agent", &["doctor"]);

    assert_eq!(
        out.status.code(),
        Some(1),
        "a bare repo has no AGENTS.md and is not healthy\nstdout: {}\nstderr: {}",
        stdout_of(&out),
        stderr_of(&out)
    );
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("AGENTS.md"),
        "the report belongs on stdout: {stdout}"
    );
    assert!(
        !stderr_of(&out).starts_with("error:"),
        "an unhealthy repo is a finding, not a command failure: {}",
        stderr_of(&out)
    );
}

/// The finding pact-4tj measured: `--to-owner-of` fixed ADDRESSING, and
/// addressing was never the failure. 30 of 44 agent-to-agent messages in one
/// fleet run went to agents who had already exited and none were ever read,
/// while every message to a live agent WAS read. Every one of the 30 was about
/// a file, sent to the agent who had just released it — so the moment someone
/// leases that file is the moment the message becomes useful again.
#[test]
fn a_message_about_a_path_is_delivered_to_whoever_leases_it_next() {
    let Some(tmp) = bd_repo("a_message_about_a_path_is_delivered") else {
        return;
    };
    // first-owner works the file, then exits.
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "acquire", "src/otel.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "release", "--all"],
    ));

    // second-agent addresses the FILE; it never learns "first-owner".
    assert_ok(&pact(
        tmp.path(),
        "second-agent",
        &[
            "msg",
            "send",
            "--to-owner-of",
            "src/otel.rs",
            "--subject",
            "BLOCKER: flush",
            "spans never sent",
        ],
    ));

    // third-agent has read nothing and is not the addressee, but takes the path.
    let out = pact(
        tmp.path(),
        "third-agent",
        &["lease", "acquire", "src/otel.rs"],
    );
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("unread message") && stderr.contains("BLOCKER: flush"),
        "the message must follow the file to its next holder: {stderr}"
    );
    assert!(
        stderr.contains("second-agent"),
        "and name who sent it: {stderr}"
    );
}

/// br twin of the test above (pact-m7j.10.1). Doubles as the end-to-end proof
/// for `encode_path`'s charset fix: `src/otel.rs` contains a `.`, which a real
/// br 0.2.19 store rejects outright in a label ("invalid characters", exit 4)
/// — before that fix AND before 10.1's atomic `--labels` (which turns a
/// rejected label from a swallowed warning into a hard `msg send` failure),
/// this exact scenario either silently failed to tag the bead or failed to
/// send at all. If it did, `third-agent`'s acquire below would see nothing.
#[test]
fn a_message_about_a_path_is_delivered_to_whoever_leases_it_next_on_br() {
    let Some(tmp) = br_repo("a_message_about_a_path_is_delivered_on_br") else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "acquire", "src/otel.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "release", "--all"],
    ));

    let sent = pact(
        tmp.path(),
        "second-agent",
        &[
            "msg",
            "send",
            "--to-owner-of",
            "src/otel.rs",
            "--subject",
            "BLOCKER: flush",
            "spans never sent",
        ],
    );
    assert_ok(&sent);
    assert!(
        !stderr_of(&sent).contains("could not tag"),
        "the label must land in the same create call, not a separate one that \
         can fail: {}",
        stderr_of(&sent)
    );

    let out = pact(
        tmp.path(),
        "third-agent",
        &["lease", "acquire", "src/otel.rs"],
    );
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("unread message") && stderr.contains("BLOCKER: flush"),
        "the message must follow the file to its next holder on br too: {stderr}"
    );
}

/// pact-m7j.4.7: `about_path` used to fetch every message bead in the repo and
/// filter client-side for the one label it wanted. Proves the end behaviour
/// survives a store with plenty of unrelated traffic: the one message about
/// the target path is still the only one surfaced, regardless of how much
/// noise sits alongside it.
#[test]
fn about_path_surfaces_only_the_message_about_the_target_path_among_unrelated_traffic() {
    let Some(tmp) = bd_repo("about_path_surfaces_only_the_message_about_the_target_path") else {
        return;
    };

    // N messages with nothing to do with the target path.
    for i in 0..5 {
        assert_ok(&pact(
            tmp.path(),
            "chatter",
            &[
                "msg",
                "send",
                "--to",
                &format!("bystander-{i}"),
                &format!("unrelated chatter number {i}"),
            ],
        ));
    }

    // The one message that IS about the target path.
    assert_ok(&pact(
        tmp.path(),
        "owner-agent",
        &["lease", "acquire", "src/target.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "owner-agent",
        &["lease", "release", "--all"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "second-agent",
        &[
            "msg",
            "send",
            "--to-owner-of",
            "src/target.rs",
            "--subject",
            "the one that matters",
            "body",
        ],
    ));

    let out = pact(
        tmp.path(),
        "third-agent",
        &["lease", "acquire", "src/target.rs"],
    );
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("1 unread message") && stderr.contains("the one that matters"),
        "must surface exactly the one message about this path, not the unrelated noise: {stderr}"
    );
}

/// Advisory, and quiet when it has nothing to say: a path with no messages
/// about it must not grow a line, or the signal is lost in ceremony.
#[test]
fn leasing_a_path_with_no_messages_about_it_says_nothing_extra() {
    let Some(tmp) = bd_repo("leasing_a_path_with_no_messages") else {
        return;
    };
    let out = pact(tmp.path(), "solo", &["lease", "acquire", "quiet.rs"]);
    assert_ok(&out);
    assert!(
        !stderr_of(&out).contains("unread message"),
        "{}",
        stderr_of(&out)
    );
}

/// pact-m7j.10.3: with no Beads CLI reachable at all, the check that would
/// have surfaced a genuine unread message must say it could not run — not
/// print nothing, which was byte-identical to the genuinely clean case above.
#[test]
fn no_backend_on_path_reports_a_check_failure_not_silence() {
    let Some(tmp) = bd_repo("no_backend_on_path_reports_a_check_failure") else {
        return;
    };
    // A genuine unread message about src/x.rs, sent while bd is still
    // reachable, so the ONLY thing missing for the acquire below is the
    // backend to check it with.
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "acquire", "src/x.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "release", "--all"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "sender",
        &["msg", "send", "--to-owner-of", "src/x.rs", "body"],
    ));

    // Acquire again with PATH pointing at an empty directory: no bd, no br.
    let empty_path = tmp.path().join("no-backend-here");
    std::fs::create_dir(&empty_path).unwrap();
    let mut cmd = pact_cmd(tmp.path(), &["lease", "acquire", "src/x.rs"]);
    cmd.env("PACT_AGENT", "no-backend-agent")
        .env("PATH", &empty_path);
    let broken = cmd.output().expect("failed to run pact binary");
    assert_ok(&broken);
    let broken_stderr = stderr_of(&broken);
    assert!(
        broken_stderr.contains("could not check for pending messages"),
        "a missing backend must say so, not look clean: {broken_stderr}"
    );

    // The genuinely clean comparison: healthy backend, a path with nothing to
    // report. Before this fix these two cases were byte-identical (both
    // printed nothing extra); now only this one is quiet.
    let clean = pact(
        tmp.path(),
        "no-backend-agent",
        &["lease", "acquire", "quiet.rs"],
    );
    assert_ok(&clean);
    let clean_stderr = stderr_of(&clean);
    assert!(
        !clean_stderr.contains("could not check"),
        "a genuinely healthy, clean check must not also claim it could not check: {clean_stderr}"
    );
    assert_ne!(
        broken_stderr, clean_stderr,
        "a missing backend and a genuinely clean path must not read the same"
    );
}

/// A repo that has never run `bd`/`br init` at all — no `.beads/`, ever — has
/// given no signal it intends to use messaging. `lease acquire` has never
/// depended on the messaging backend for anything else, so surfacing "could
/// not check for pending messages" on EVERY acquire, forever, for a
/// lease-only repo would be exactly the noise AGENTS.md's own messaging
/// discipline warns against — for a check that could not possibly have found
/// anything anyway. This must stay quiet even with no Beads CLI on PATH,
/// unlike the sibling test above where `.beads/` genuinely exists.
#[test]
fn no_beads_directory_at_all_stays_quiet_even_with_no_backend_on_path() {
    let tmp = init_repo();
    assert!(
        !tmp.path().join(".beads").exists(),
        "test setup: a plain repo must not have .beads/"
    );

    let empty_path = tmp.path().join("no-backend-here");
    std::fs::create_dir(&empty_path).unwrap();
    let mut cmd = pact_cmd(tmp.path(), &["lease", "acquire", "src/x.rs"]);
    cmd.env("PACT_AGENT", "lease-only-agent")
        .env("PATH", &empty_path);
    let out = cmd.output().expect("failed to run pact binary");
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        !stderr.contains("could not check"),
        "a repo with no .beads/ at all must not warn about messaging it never set up: {stderr}"
    );
}

/// `msg send --to-owner-of` says who the path resolved to. A resolved name
/// looks like a delivered message and is not — one agent worked around this by
/// hand-adding `--to human` to every send, and was the only one who thought of
/// it.
#[test]
fn to_owner_of_reports_who_it_resolved_to() {
    let Some(tmp) = bd_repo("to_owner_of_reports_who") else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "owner-agent",
        &["lease", "acquire", "src/x.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "owner-agent",
        &["lease", "release", "--all"],
    ));

    let out = pact(
        tmp.path(),
        "sender",
        &["msg", "send", "--to-owner-of", "src/x.rs", "body"],
    );
    assert_ok(&out);
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("resolved to owner-agent") && stderr.contains("last seen"),
        "must name the resolution and its age: {stderr}"
    );
}

/// pact-m7j.8.6: `normalize_path` fixed one-file-one-lock, but `prior_owners`
/// and `--to-owner-of`'s resolution each did their own raw string comparison
/// against un-normalized input — so the same real file, spelled relative to a
/// subdirectory instead of the repo root, looked like a file nobody had ever
/// touched on both surfaces, even though the lock itself resolved correctly.
/// Reproduced live in the original bug report against a real bd 1.1.2 store;
/// this pins it against this repo's own real backend.
#[test]
fn prior_owner_and_to_owner_of_agree_across_cwd_relative_spellings() {
    let Some(tmp) = bd_repo("prior_owner_agrees_across_cwd_spellings") else {
        return;
    };
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();

    assert_ok(&pact(
        tmp.path(),
        "owner-agent",
        &["lease", "acquire", "src/shared.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "owner-agent",
        &["lease", "release", "--all"],
    ));

    // Re-acquire the SAME file from src/, spelled relative to it rather than
    // the canonical src/shared.rs.
    let reacquired = pact_from(
        &tmp.path().join("src"),
        "second-agent",
        &["lease", "acquire", "shared.rs"],
    );
    assert_ok(&reacquired);
    let stderr = stderr_of(&reacquired);
    assert!(
        stderr.contains("owner-agent") && stderr.contains("was last"),
        "the CWD-relative spelling must still surface the prior owner: {stderr}"
    );

    // --to-owner-of, also typed from src/, must resolve to the agent who now
    // holds it (second-agent) instead of erroring "no agent has ever leased
    // it" — the exact false assertion the original bug report reproduced.
    let sent = pact_from(
        &tmp.path().join("src"),
        "third-agent",
        &["msg", "send", "--to-owner-of", "shared.rs", "body"],
    );
    assert_ok(&sent);
    assert!(
        stderr_of(&sent).contains("resolved to second-agent"),
        "--to-owner-of must resolve the CWD-relative spelling to the real owner: {}",
        stderr_of(&sent)
    );

    // The send above also tagged the message about-<path> — from src/, using
    // the CWD-relative spelling. A FOURTH agent, checking the canonical
    // spelling from the repo root, must still see the "unread message about"
    // advisory: the write-side tag and the read-side query must land on the
    // same label however each command line spelled the path.
    let checked = pact(
        tmp.path(),
        "fourth-agent",
        &["lease", "acquire", "src/shared.rs", "--steal"],
    );
    assert_ok(&checked);
    assert!(
        stderr_of(&checked).contains("unread message"),
        "a message tagged from a CWD-relative spelling must be found by a query \
         from the canonical spelling: {}",
        stderr_of(&checked)
    );
}

/// A refused acquire named the victim and never the holder, so the
/// who-blocks-whom edge existed only in `pact log`. The holder belongs on the
/// SPAN and not on a metric — an agent name is unbounded and would mint a
/// metric series per fleet member — but "click through, don't group by" only
/// works if the span actually carries it (pact-ebe).
#[test]
fn a_refused_acquire_still_reports_the_holder_and_exits_2() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "holder-agent",
        &["lease", "acquire", "src/x.rs"],
    ));

    let out = pact(
        tmp.path(),
        "blocked-agent",
        &["lease", "acquire", "src/x.rs"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("holder-agent"),
        "the human-facing error must name the holder too: {}",
        stderr_of(&out)
    );
}

/// An agent that acquires a lease, does the work and releases it — the correct
/// behaviour — used to vanish from `pact agents` the moment its last lock file
/// was deleted. `msg send` then warned "no agent named X has acted in this
/// repo" one line after the resolver said "last seen 0s ago": two sources of
/// truth, and the one behind the warning was the one that forgets (pact-6sx).
#[test]
fn an_agent_that_released_its_leases_is_still_a_known_agent() {
    let tmp = init_repo();
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "acquire", "src/otel.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "first-owner",
        &["lease", "release", "--all"],
    ));

    let out = pact(tmp.path(), "onlooker", &["agents", "--json"]);
    assert_ok(&out);
    let found = json_stdout(&out);
    let row = found
        .as_array()
        .expect("agents emits an array")
        .iter()
        .find(|a| a["name"] == "first-owner")
        .cloned()
        .expect("an agent that released its lease must still be known");
    assert_eq!(row["leases_held"], 0, "it really did release: {row}");
    assert!(
        row["lease_events"].as_u64().unwrap_or(0) > 0,
        "and the event log is why it is still here: {row}"
    );
}

/// The warning this evidence feeds must keep catching what it exists for: a
/// name nobody has ever answered to is still a typo, and widening the roster
/// must not have widened that.
#[test]
fn a_name_nobody_ever_used_is_still_reported_as_unknown() {
    let Some(tmp) = bd_repo("a_name_nobody_ever_used") else {
        return;
    };
    assert_ok(&pact(
        tmp.path(),
        "real-agent",
        &["lease", "acquire", "f.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "real-agent",
        &["lease", "release", "--all"],
    ));

    let typo = pact(
        tmp.path(),
        "sender",
        &["msg", "send", "--to", "raal-agent", "body"],
    );
    assert_ok(&typo); // warns, still sends
    assert!(
        stderr_of(&typo).contains("has acted in this repo"),
        "a typo must still warn: {}",
        stderr_of(&typo)
    );

    let real = pact(
        tmp.path(),
        "sender",
        &["msg", "send", "--to", "real-agent", "body"],
    );
    assert_ok(&real);
    assert!(
        !stderr_of(&real).contains("has acted in this repo"),
        "a real, exited agent must not: {}",
        stderr_of(&real)
    );
}

/// A path used to mean whatever the CWD made it mean. Two agents each took a
/// lease on the SAME physical file — one from the repo root, one from the
/// subdirectory it lives in — and both were told they held it, which is the one
/// outcome the lease surface exists to prevent (pact-r2s.1).
#[test]
fn one_file_is_one_lease_however_the_agent_spells_the_path() {
    let tmp = init_repo();
    std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
    std::fs::write(tmp.path().join("src/deep/foo.rs"), "x").unwrap();

    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "src/deep/foo.rs"],
    ));

    // Same file, named from inside its own directory.
    let out = pact_cmd(
        &tmp.path().join("src/deep"),
        &["lease", "acquire", "foo.rs"],
    )
    .env("PACT_AGENT", "agent-b")
    .output()
    .expect("pact");
    assert_eq!(
        out.status.code(),
        Some(2),
        "a second spelling of one file must conflict, not mint a second lease: {}",
        stderr_of(&out)
    );

    let locks = std::fs::read_dir(tmp.path().join(".pact/leases"))
        .unwrap()
        .count();
    assert_eq!(locks, 1, "one file, one lock file");
}

/// The inverse, which must keep working: two genuinely different files that
/// happen to share a basename used to collapse into one lock, so an agent was
/// told to negotiate over a file nobody shared.
#[test]
fn two_different_files_sharing_a_basename_are_two_leases() {
    let tmp = init_repo();
    std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
    std::fs::write(tmp.path().join("src/deep/foo.rs"), "x").unwrap();
    std::fs::write(tmp.path().join("foo.rs"), "y").unwrap();

    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "src/deep/foo.rs"],
    ));
    assert_ok(&pact(
        tmp.path(),
        "agent-c",
        &["lease", "acquire", "foo.rs"],
    ));

    let locks = std::fs::read_dir(tmp.path().join(".pact/leases"))
        .unwrap()
        .count();
    assert_eq!(locks, 2, "different files must not share a lock");
}

/// `..` is folded lexically, never by `canonicalize()` — leasing a file that
/// does not exist yet is a documented workflow (docs/leases.md), and
/// canonicalize fails on a missing path.
#[test]
fn a_dot_dot_path_and_a_lease_on_a_file_that_does_not_exist_both_resolve() {
    let tmp = init_repo();
    std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
    std::fs::write(tmp.path().join("foo.rs"), "y").unwrap();

    assert_ok(&pact(
        tmp.path(),
        "agent-c",
        &["lease", "acquire", "foo.rs"],
    ));
    let out = pact_cmd(
        &tmp.path().join("src/deep"),
        &["lease", "acquire", "../../foo.rs"],
    )
    .env("PACT_AGENT", "agent-d")
    .output()
    .expect("pact");
    assert_eq!(out.status.code(), Some(2), "`..` must reach the same lock");

    // The new-file case the lexical fold exists to protect.
    assert_ok(&pact(
        tmp.path(),
        "agent-e",
        &["lease", "acquire", "src/deep/not_yet.rs"],
    ));
}

/// `--to a --to a` used to create two beads in one thread and deliver the same
/// message to one inbox twice. pact has no uniqueness constraint to trip over,
/// so it succeeded silently — Agent Mail hit the same case and got a
/// composite-PK IntegrityError instead (c66e54f #190). The realistic caller is
/// a recipient list built from `pact agents --json` or a template, not a human
/// typing the flag twice (pact-r2s.2).
#[test]
fn a_repeated_recipient_is_delivered_once() {
    let Some(tmp) = bd_repo("a_repeated_recipient_is_delivered_once") else {
        return;
    };
    let out = pact(
        tmp.path(),
        "sender",
        &[
            "msg",
            "send",
            "--to",
            "dupe-target",
            "--to",
            "dupe-target",
            "--subject",
            "once",
            "body",
        ],
    );
    assert_ok(&out);
    assert!(
        stderr_of(&out).contains("duplicate recipient"),
        "the collapse must be reported, not silent: {}",
        stderr_of(&out)
    );

    let inbox = pact(tmp.path(), "dupe-target", &["msg", "inbox", "--json"]);
    assert_ok(&inbox);
    assert_eq!(
        json_stdout(&inbox).as_array().map(Vec::len),
        Some(1),
        "one message, not two: {}",
        stdout_of(&inbox)
    );
}

/// pact-m7j.6.4, reproduced in production: a sender that could not confirm a
/// send re-sends it (`sent()`'s own documented policy), and without an
/// idempotency key that retry mints a second, near-identical bead. `bd`'s
/// `--id`/`--force` upsert now makes an identical retry land on the same
/// bead. bd-only: `br` has no equivalent primitive, so this is `bd_repo`,
/// not both backends.
#[test]
fn a_retried_identical_send_does_not_duplicate_on_bd() {
    let Some(tmp) = bd_repo("a_retried_identical_send_does_not_duplicate") else {
        return;
    };
    let send = || {
        pact(
            tmp.path(),
            "sender",
            &[
                "msg",
                "send",
                "--to",
                "recipient",
                "--subject",
                "long send",
                "the harness dropped stdout before I saw the exit code",
            ],
        )
    };
    assert_ok(&send());
    // The retry: same agent, same recipient, same subject and body — exactly
    // what a sender unsure whether the first one landed would re-run.
    assert_ok(&send());

    let inbox = pact(tmp.path(), "recipient", &["msg", "inbox", "--json"]);
    assert_ok(&inbox);
    assert_eq!(
        json_stdout(&inbox).as_array().map(Vec::len),
        Some(1),
        "a retried identical send must land on one bead, not two: {}",
        stdout_of(&inbox)
    );
}

/// pact-m7j.6.7: `bd create --id= --force` echoes ITS OWN call's wall-clock
/// time as `created_at` even when the id already existed and nothing was
/// actually created — verified against a real scratch bd store. Before the
/// fix, the retry's own `--json` response reported a `created_at` that
/// disagreed with what `bd show`/`msg inbox` reported moments later for the
/// same bead. bd-only, same reason as the sibling test above.
#[test]
fn a_retried_send_reports_the_original_created_at_not_the_retrys() {
    let Some(tmp) = bd_repo("a_retried_send_reports_the_original_created_at") else {
        return;
    };
    let send = || {
        pact(
            tmp.path(),
            "sender",
            &[
                "msg",
                "send",
                "--to",
                "recipient",
                "--subject",
                "long send",
                "--json",
                "the harness dropped stdout before I saw the exit code",
            ],
        )
    };
    let first = send();
    assert_ok(&first);
    let first_created_at = json_stdout(&first)[0]["created_at"].clone();

    // The retry: same agent, same recipient, same subject and body. bd's
    // upsert lands on the same bead (a_retried_identical_send_does_not_
    // duplicate_on_bd covers that); this test is about what the retry's OWN
    // response says about it.
    let retry = send();
    assert_ok(&retry);
    let retry_created_at = json_stdout(&retry)[0]["created_at"].clone();

    let inbox = pact(tmp.path(), "recipient", &["msg", "inbox", "--json"]);
    assert_ok(&inbox);
    let stored_created_at = json_stdout(&inbox)[0]["created_at"].clone();

    assert_eq!(
        retry_created_at, stored_created_at,
        "the retry's own --json response must report the persisted created_at, \
         not its own call's wall-clock time"
    );
    assert_eq!(
        first_created_at, retry_created_at,
        "the original and the retry must agree: it is the same bead"
    );
}

/// A stand-in `bd` that fails outright when asked to create a bead for
/// `fail_for`, and forwards to the real `bd` (resolved once, from the current
/// PATH, before this directory is ever prepended to one) for everything else.
/// Lets a test force a partial fan-out failure at a specific recipient
/// against a REAL backend (pact-m7j.6.5), instead of guessing at a bd
/// argument shape that happens to error.
fn bd_wrapper_that_fails_for(fail_for: &str) -> TempDir {
    let real_bd = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join("bd"))
        .find(|p| p.is_file())
        .expect("real bd must be on PATH for this test");
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("bd");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  \
             if [ \"$a\" = \"--assignee={fail_for}\" ]; then\n    \
             echo 'synthetic failure for testing' >&2\n    exit 7\n  fi\n\
             done\nexec {real} \"$@\"\n",
            fail_for = fail_for,
            real = real_bd.display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    dir
}

/// Like [`pact`], but with `path_prefix` searched before the rest of `PATH` —
/// so a wrapper binary placed there shadows the real one for this call only,
/// without touching any other test's environment.
fn pact_with_path_prefix(repo: &Path, agent: &str, args: &[&str], path_prefix: &Path) -> Output {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let new_path = std::env::join_paths(
        std::iter::once(path_prefix.to_path_buf()).chain(std::env::split_paths(&existing)),
    )
    .unwrap();
    pact_cmd(repo, args)
        .env("PACT_AGENT", agent)
        .env("PATH", new_path)
        .output()
        .expect("failed to run pact binary")
}

/// pact-m7j.6.5: replaying a partially-failed multi-recipient send with
/// `--skip` for the recipients an earlier attempt's `--json` error already
/// named as sent must not duplicate delivery to them, and must still attempt
/// the recipient that actually failed.
#[test]
fn a_partially_failed_send_replays_safely_with_skip() {
    let Some(tmp) = bd_repo("a_partially_failed_send_replays_safely_with_skip") else {
        return;
    };
    let wrapper = bd_wrapper_that_fails_for("agent-d");

    // agent-b (recipient 1, thread root) and agent-c (recipient 2) succeed;
    // agent-d (recipient 3) fails.
    let failed = pact_with_path_prefix(
        tmp.path(),
        "sender",
        &[
            "msg",
            "send",
            "--to",
            "agent-b",
            "--to",
            "agent-c",
            "--to",
            "agent-d",
            "--subject",
            "shared decision",
            "--json",
            "friday?",
        ],
        wrapper.path(),
    );
    assert_eq!(
        failed.status.code(),
        Some(1),
        "a partial failure is still a failure: {}",
        stderr_of(&failed)
    );
    // pact-m7j.5.1: this JSON object is on stdout now, not stderr — every
    // --json failure gets its structured shape on the same stream a
    // successful --json run uses.
    let err = json_stdout(&failed);
    let already_sent: Vec<&str> = err["already_sent"]
        .as_array()
        .unwrap_or_else(|| panic!("already_sent missing or not an array: {err}"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(already_sent, ["agent-b", "agent-c"], "{err}");
    assert_eq!(err["failed_at"], "agent-d", "{err}");

    // Replay with --skip for the two recipients the failed attempt's JSON
    // already said got it. No wrapper this time: agent-d succeeds.
    assert_ok(&pact(
        tmp.path(),
        "sender",
        &[
            "msg",
            "send",
            "--to",
            "agent-b",
            "--to",
            "agent-c",
            "--to",
            "agent-d",
            "--subject",
            "shared decision",
            "--skip",
            "agent-b",
            "--skip",
            "agent-c",
            "friday?",
        ],
    ));

    for agent in ["agent-b", "agent-c", "agent-d"] {
        let inbox = inbox_json(tmp.path(), agent);
        assert_eq!(
            inbox.as_array().map(Vec::len),
            Some(1),
            "{agent} must end up with exactly one bead: {inbox}"
        );
    }
}

/// The other half of pact-m7j.6.5: a naive identical replay (no `--skip`) is
/// deliberately UNCHANGED by this fix. bd's own `--id`/`--force` upsert
/// (pact-m7j.6.4) already protects the thread ROOT — the first `--to` — from
/// duplicating on an identical retry; that upsert key is only computed when
/// there is no `--parent`, so recipients 2..N of a fan-out carry no such
/// protection and still duplicate. Confirmed against real bd 1.1.2: this is
/// the actual pre-existing behavior, not a hypothetical the fix leaves alone.
#[test]
fn a_naive_replay_without_skip_still_duplicates_non_root_recipients() {
    let Some(tmp) = bd_repo("a_naive_replay_without_skip_still_duplicates_non_root_recipients")
    else {
        return;
    };
    let wrapper = bd_wrapper_that_fails_for("agent-d");
    let send_args: &[&str] = &[
        "msg",
        "send",
        "--to",
        "agent-b",
        "--to",
        "agent-c",
        "--to",
        "agent-d",
        "--subject",
        "shared decision",
        "friday?",
    ];

    let failed = pact_with_path_prefix(tmp.path(), "sender", send_args, wrapper.path());
    assert_eq!(failed.status.code(), Some(1), "{}", stderr_of(&failed));

    // Naive identical replay: no --skip, no wrapper (agent-d succeeds now).
    assert_ok(&pact(tmp.path(), "sender", send_args));

    assert_eq!(
        inbox_json(tmp.path(), "agent-b").as_array().map(Vec::len),
        Some(1),
        "the thread root is already protected by pact-m7j.6.4's --id upsert"
    );
    assert_eq!(
        inbox_json(tmp.path(), "agent-c").as_array().map(Vec::len),
        Some(2),
        "recipient 2 carries no --id and duplicates on a naive replay — \
         the gap --skip exists to let a sender avoid, left unchanged here"
    );
    assert_eq!(
        inbox_json(tmp.path(), "agent-d").as_array().map(Vec::len),
        Some(1),
        "agent-d only ever succeeded once, on the replay"
    );
}

/// Deduping must not reorder a genuine fan-out: the thread root is the first
/// distinct recipient, and dropping a later duplicate must not move it.
#[test]
fn deduping_preserves_the_order_of_distinct_recipients() {
    let Some(tmp) = bd_repo("deduping_preserves_the_order") else {
        return;
    };
    let out = pact(
        tmp.path(),
        "sender",
        &[
            "msg",
            "send",
            "--to",
            "alpha-one",
            "--to",
            "beta-two",
            "--to",
            "alpha-one",
            "body",
        ],
    );
    assert_ok(&out);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("2 message(s)"),
        "two distinct recipients: {stdout}"
    );
    let a = stdout.find("alpha-one").expect("alpha-one listed");
    let b = stdout.find("beta-two").expect("beta-two listed");
    assert!(a < b, "first-seen order must survive the dedupe: {stdout}");
}

/// A lock file's name must never exist before its contents do.
///
/// Acquiring a free path used to be create_new() then write_all(): the file
/// existed and was empty in between. A reader in that window got
/// `EOF while parsing a value`, and `pact doctor` reported "1 unreadable lock
/// file (remove manually from .pact/leases/)" — advice that, followed during
/// the window, deletes a live agent's lock. The claim is a hard_link now, which
/// is atomic AND fails if the destination exists, so the name and the bytes
/// appear together (pact-r2s.3).
///
/// Hammers acquire/release while a reader polls, and asserts the reader never
/// observes a lock file that exists but does not parse.
#[test]
fn a_lock_file_is_never_visible_before_its_contents() {
    let tmp = init_repo();
    let leases = tmp.path().join(".pact/leases");
    let root = tmp.path().to_path_buf();

    let writer = std::thread::spawn(move || {
        for _ in 0..300 {
            let _ = std::process::Command::new(env!("CARGO_BIN_EXE_pact"))
                .args(["lease", "acquire", "hot.rs"])
                .current_dir(&root)
                .env("PACT_AGENT", "writer-agent")
                .output();
            let _ = std::process::Command::new(env!("CARGO_BIN_EXE_pact"))
                .args(["lease", "release", "--all"])
                .current_dir(&root)
                .env("PACT_AGENT", "writer-agent")
                .output();
        }
    });

    let mut empty_seen = 0usize;
    while !writer.is_finished() {
        if let Ok(entries) = std::fs::read_dir(&leases) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "lock") {
                    // A lock that exists must parse. Zero bytes is the failure.
                    if let Ok(body) = std::fs::read_to_string(&p) {
                        if body.trim().is_empty() {
                            empty_seen += 1;
                        }
                    }
                }
            }
        }
    }
    writer.join().unwrap();
    assert_eq!(empty_seen, 0, "observed {empty_seen} zero-byte lock files");
}

/// `pact init` must never leave a user's own file truncated.
///
/// AGENTS.md, CLAUDE.md and .gitignore are mostly written by the human, and the
/// module promises never to touch content outside its markers. A plain
/// `fs::write` truncates before it writes, so a crash in between broke that
/// promise completely rather than partially — and on a first init the file is
/// not committed yet, so there is nothing to recover from (pact-703.2).
///
/// Runs init repeatedly while a reader watches AGENTS.md, and asserts the file
/// is never observed empty or missing its own markers.
#[test]
fn init_never_exposes_a_truncated_agents_md() {
    let tmp = init_repo();
    assert_ok(&pact(tmp.path(), "setup-agent", &["init", "--no-commit"]));
    // Content outside the markers, which is the thing at risk.
    let agents_md = tmp.path().join("AGENTS.md");
    let seeded = format!(
        "# House rules\n\nkeep it lazy.\n\n{}",
        std::fs::read_to_string(&agents_md).unwrap()
    );
    std::fs::write(&agents_md, &seeded).unwrap();

    let root = tmp.path().to_path_buf();
    let writer = std::thread::spawn(move || {
        for _ in 0..40 {
            let _ = std::process::Command::new(env!("CARGO_BIN_EXE_pact"))
                .args(["init", "--no-commit"])
                .current_dir(&root)
                .env("PACT_AGENT", "writer-agent")
                .output();
        }
    });

    let mut bad = 0usize;
    while !writer.is_finished() {
        match std::fs::read_to_string(&agents_md) {
            // Either spelling of the failure: gone, empty, or missing the
            // user's own text that lives outside the managed block.
            Ok(body) if body.is_empty() || !body.contains("# House rules") => bad += 1,
            Err(_) => bad += 1,
            Ok(_) => {}
        }
    }
    writer.join().unwrap();
    assert_eq!(
        bad, 0,
        "observed {bad} truncated or missing reads of AGENTS.md"
    );

    // And no litter beside the file it replaced.
    let strays = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".pact-write"))
        .count();
    assert_eq!(strays, 0, "temp files left next to the user's file");
}

/// Sibling of `init_never_exposes_a_truncated_agents_md`, same harness
/// (concurrent hammer against the real binary), a different failure mode:
/// that test's racer left AGENTS.md's own content static, so it could only
/// catch truncation, never a lost update. This one races a thread that keeps
/// EDITING AGENTS.md's user-owned prefix — bumping a `revision N` line, via a
/// temp file + rename so the injected edit is itself a fully atomic write —
/// concurrently with `pact init`.
///
/// `splice_block`'s old read-modify-write read the file once, computed its
/// replacement in memory, and wrote it back with no lock and no version
/// check. Pausing between that read and its commit-moment rename (reproduced
/// live with strace delay injection) let a concurrent write land in between;
/// the delayed rename then silently overwrote it with the stale copy — a
/// lost update, made visible here as the revision counter going backwards.
///
/// The PRECISE version of that property — a single injected race, zero
/// timing dependence — is proven deterministically by
/// `agents_md::tests::write_atomic_cas_never_commits_over_a_write_that_landed_mid_call`,
/// which is the authoritative acceptance test for this bug (reverting the
/// fix makes it fail every time; restoring it makes it pass every time).
/// Reproducing that same precision via wall-clock racing against the
/// compiled binary was tried at length: `write_atomic_cas`'s own residual
/// window (a couple of syscalls immediately before the rename — an
/// acknowledged, inherent property of "recheck, then bounded retry" without
/// a lock, not a bug) turned out close enough in scale to ordinary
/// scheduling jitter that no zero-tolerance threshold reliably separated
/// "the fix is present" from "it never shipped" without flaking — confirmed
/// empirically, more than once, while writing this test. So the assertion
/// below is deliberately generous: it treats a couple of distinct
/// regressions as the fix's known residual, not a failure, while still
/// catching the unfixed behavior, which reliably produced far more under
/// this exact harness. This test's real job is the same as its sibling's:
/// `init`, raced against a stream of real concurrent edits, must never
/// crash, truncate the file, or duplicate/garble the managed block.
#[test]
fn init_survives_a_concurrently_mutating_agents_md() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let tmp = init_repo();
    assert_ok(&pact(tmp.path(), "setup-agent", &["init", "--no-commit"]));
    let agents_md = tmp.path().join("AGENTS.md");

    // The user-owned prefix carries a monotonically increasing revision
    // instead of static text, so the racer is genuinely mutating the file on
    // every write, not just re-saving the same bytes `init` already knows.
    fn with_revision(n: u32, block_onward: &str) -> String {
        format!("# House rules\n\nrevision {n}\n\nkeep it lazy.\n\n{block_onward}")
    }
    fn revision_of(body: &str) -> Option<u32> {
        body.lines()
            .find_map(|l| l.strip_prefix("revision ")?.trim().parse().ok())
    }
    fn block_onward_of(body: &str) -> String {
        body.find("<!-- pact:begin -->")
            .map(|i| body[i..].to_string())
            .unwrap_or_default()
    }

    let seeded = std::fs::read_to_string(&agents_md).unwrap();
    std::fs::write(&agents_md, with_revision(0, &block_onward_of(&seeded))).unwrap();

    let done = Arc::new(AtomicBool::new(false));

    let root = tmp.path().to_path_buf();
    let initter = std::thread::spawn(move || {
        let mut successes = 0u32;
        for _ in 0..40 {
            if let Ok(out) = std::process::Command::new(env!("CARGO_BIN_EXE_pact"))
                .args(["init", "--no-commit"])
                .current_dir(&root)
                .env("PACT_AGENT", "writer-agent")
                .output()
            {
                if out.status.success() {
                    successes += 1;
                }
            }
        }
        successes
    });

    let mutator_root = tmp.path().to_path_buf();
    let mutator_done = Arc::clone(&done);
    let mutator = std::thread::spawn(move || {
        let agents_md = mutator_root.join("AGENTS.md");
        let mut n = 0u32;
        while !mutator_done.load(Ordering::Relaxed) {
            n += 1;
            if let Ok(current) = std::fs::read_to_string(&agents_md) {
                let block_onward = block_onward_of(&current);
                let tmp_path = mutator_root.join(format!(".mutator-tmp-{n}"));
                if std::fs::write(&tmp_path, with_revision(n, &block_onward)).is_ok() {
                    let _ = std::fs::rename(&tmp_path, &agents_md);
                }
            }
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
    });

    // Count DISTINCT lost-update incidents (the revision going backwards),
    // not every poll that observes one: a reverted value can sit on disk for
    // several poll iterations before the mutator's next write corrects it,
    // and counting every such poll would conflate "one incident, observed
    // repeatedly" with "many incidents" — very different signals.
    let mut bad = 0usize;
    let mut max_seen = 0u32;
    let mut last_seen = 0u32;
    let mut distinct_regressions = 0usize;
    while !initter.is_finished() {
        match std::fs::read_to_string(&agents_md) {
            // Truncated, or the user's own prefix is gone — same failure
            // shapes `init_never_exposes_a_truncated_agents_md` checks for,
            // now under a racer that is actively rewriting the file instead
            // of leaving it untouched.
            Ok(body) if body.is_empty() || !body.contains("# House rules") => bad += 1,
            // Exactly one begin/end marker pair: a duplicated or malformed
            // block would mean `init`'s "no markers found" branch fired
            // against a torn or half-updated read instead of the real thing.
            Ok(body)
                if body.matches("<!-- pact:begin -->").count() != 1
                    || body.matches("<!-- pact:end -->").count() != 1 =>
            {
                bad += 1;
            }
            Ok(body) => {
                if let Some(rev) = revision_of(&body) {
                    if rev < max_seen && rev != last_seen {
                        distinct_regressions += 1;
                    }
                    last_seen = rev;
                    max_seen = max_seen.max(rev);
                }
            }
            Err(_) => bad += 1,
        }
    }
    done.store(true, Ordering::Relaxed);
    let successes = initter.join().unwrap();
    mutator.join().unwrap();

    assert_eq!(
        bad, 0,
        "observed {bad} truncated, malformed, or missing reads of AGENTS.md while it was \
         being concurrently edited"
    );
    // Zero distinct regressions is the design's goal, but the mechanism is a
    // compare-and-swap check immediately before the rename, not a lock: a
    // write that lands in that syscall-scale gap AFTER the check but BEFORE
    // the rename is a real, acknowledged residual (see write_atomic_cas's
    // own doc comment, and the deterministic, timing-free proof of the same
    // property in `agents_md::tests`,
    // `write_atomic_cas_never_commits_over_a_write_that_landed_mid_call`,
    // which is the authoritative test for this mechanism). This harness
    // hammers far harder than any real concurrent edit ever would, and the
    // sliver widens under CPU contention: 15 standalone runs stayed at 0-1,
    // but running inside the full `mise run check` suite (every other test
    // in this binary competing for scheduling) observed 4 in one run. 8
    // gives real headroom over that observation while still failing hard on
    // the unfixed read-modify-write, which reliably showed far more than
    // this under the identical harness.
    assert!(
        distinct_regressions <= 8,
        "AGENTS.md's revision went backwards {distinct_regressions} distinct time(s) — more \
         than the fix's small, acknowledged residual window should produce: `pact init` is \
         clobbering concurrent edits with a stale read"
    );
    assert!(
        successes > 0,
        "every `pact init` failed under contention — the retry budget or backoff needs a look"
    );

    // And no litter beside the file it replaced, matching the sibling test.
    let strays = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".pact-write"))
        .count();
    assert_eq!(strays, 0, "temp files left next to the user's file");
}

/// The narrow fix for a real past incident (AGENTS.md destroyed via a
/// symlinked instruction file) only ever excluded a candidate that
/// canonicalizes to AGENTS.md's OWN path. Nothing distinguished an
/// intentional symlink (`CLAUDE.md` -> a dotfiles repo, deliberately outside
/// `repo_root`) from an accidental one (a bad merge, a restored backup, a
/// stray `ln -s`) pointing anywhere else the process can write — both have
/// the identical shape. The maintainer call is warn, not refuse, so the
/// write still lands (refusing would break the legitimate case), but a
/// warning naming both the symlink and its resolved target must appear.
#[cfg(unix)]
#[test]
fn init_warns_when_a_write_target_symlinks_outside_the_repo() {
    let tmp = init_repo();
    let root = tmp.path().canonicalize().unwrap();

    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().canonicalize().unwrap().join("victim.md");
    std::fs::write(&sentinel, "the victim's own content\n").unwrap();

    std::os::unix::fs::symlink(&sentinel, root.join("AGENTS.md")).unwrap();

    let out = pact(tmp.path(), "setup-agent", &["init", "--no-commit"]);
    // Warn, not block: the design deliberately does not refuse this write,
    // so the command still succeeds and the sentinel is still touched.
    assert_ok(&out);

    let sentinel_after = std::fs::read_to_string(&sentinel).unwrap();
    assert!(
        sentinel_after.contains("pact coordination protocol"),
        "the write must still go through the symlink (warn, not block):\n{sentinel_after}"
    );

    let stderr = stderr_of(&out);
    let agents_md = root.join("AGENTS.md");
    assert!(
        stderr.contains(&agents_md.display().to_string()),
        "warning must name the symlinked path itself:\n{stderr}"
    );
    assert!(
        stderr.contains(&sentinel.display().to_string()),
        "warning must name the resolved outside-repo target:\n{stderr}"
    );
}
