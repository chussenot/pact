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
    assert_ok(&pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "fresh.txt", "stale.txt", "dead.txt"],
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
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("bd").is_file()))
        .unwrap_or(false);
    if !on_path {
        eprintln!("SKIP {test}: bd not found on PATH — `pact msg` cannot be tested here");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    // A real repo this time: `bd init` commits the files it creates, and it
    // needs an identity to commit with.
    let setup: [&[&str]; 4] = [
        &["git", "init", "-q", "."],
        &["git", "config", "user.email", "tests@pact.invalid"],
        &["git", "config", "user.name", "pact tests"],
        &["bd", "init"],
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
    assert_eq!(check["ok"], false, "{check}");
    let detail = check["detail"].as_str().unwrap();
    assert!(detail.contains("AGENTS.md"), "{detail}");
    // The actionable half: *which* ignore rule to go fix.
    assert!(detail.contains(".gitignore"), "{detail}");

    assert_eq!(
        pact(tmp.path(), "reach-agent", &["doctor"]).status.code(),
        Some(1),
        "a failing check must exit 1"
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
