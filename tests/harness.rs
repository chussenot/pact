//! Tests for `scripts/pw` — the fleet instrumentation wrapper.
//!
//! What is under test is the log line, because the log line is the whole point:
//! run 6 overturned two of run 5's findings only because 875 calls had been
//! recorded with argv, wall-time and exit code each. Two of those fields were
//! wrong at some point in run 6 itself — a `--reason` containing newlines tore
//! one record across many lines, and a caller-side `| head` was recorded as bd
//! failing — so both failures get a test here rather than only a comment.
//!
//! Skipped rather than failed when `bash` or `git` is missing, matching
//! `tests/chaos.rs`: this file says nothing about pact on a machine without
//! them, and a test that fails for a missing tool trains people to ignore red.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("pw")
}

/// Run `pw <args>` with its log directory pinned to `logdir`.
fn pw(logdir: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script())
        .args(args)
        .env("PACT_HARNESS_DIR", logdir)
        .env("PACT_AGENT", "tester")
        .output()
        .expect("failed to run pw")
}

/// The one line pw appended for `agent`, parsed. Asserts there is exactly one:
/// a torn record shows up here as a count, before it shows up as a parse error.
fn one_record(logdir: &Path, agent: &str) -> serde_json::Value {
    let raw = std::fs::read_to_string(logdir.join(format!("{agent}.jsonl")))
        .unwrap_or_else(|e| panic!("no log for {agent}: {e}"));
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "one call must append one line, got: {raw:?}"
    );
    serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("unparseable record {:?}: {e}", lines[0]))
}

/// One call, one parseable line, carrying argv, wall-time and exit code.
#[test]
fn one_call_logs_one_record() {
    if !have("bash") {
        eprintln!("skipping: no bash");
        return;
    }
    let tmp = TempDir::new().unwrap();

    // `sleep` because a wall-time field that is always 0.000 would pass a test
    // that only checked the key exists — and that is exactly what a `date` with
    // no %N would produce.
    let out = pw(tmp.path(), &["sleep", "0.2"]);
    assert!(out.status.success());

    let rec = one_record(tmp.path(), "tester");
    assert_eq!(rec["agent"], "tester");
    assert_eq!(rec["tool"], "sleep");
    assert_eq!(rec["argv"], "sleep 0.2");
    assert_eq!(rec["exit"], 0);
    assert_eq!(rec["sigpipe"], false);
    let secs = rec["secs"].as_f64().expect("secs must be a JSON number");
    assert!(
        (0.15..10.0).contains(&secs),
        "wall-time must be measured, got {secs}"
    );
}

/// A non-zero exit is recorded as itself and passed through to the caller.
#[test]
fn exit_code_is_recorded_and_propagated() {
    if !have("bash") {
        eprintln!("skipping: no bash");
        return;
    }
    let tmp = TempDir::new().unwrap();

    let out = pw(tmp.path(), &["bash", "-c", "echo boom >&2; exit 7"]);
    assert_eq!(out.status.code(), Some(7), "pw must exit as its child did");
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));

    let rec = one_record(tmp.path(), "tester");
    assert_eq!(rec["exit"], 7);
    assert_eq!(rec["sigpipe"], false);
    // The excerpt is read after the tee, not racing it.
    assert!(
        rec["stderr"].as_str().unwrap().contains("boom"),
        "stderr excerpt lost: {rec}"
    );
}

/// A `--reason` full of newlines must not tear one record into many. This is
/// the failure that made 11 of run 6's first 141 records unparseable.
#[test]
fn multiline_argument_stays_one_line() {
    if !have("bash") {
        eprintln!("skipping: no bash");
        return;
    }
    let tmp = TempDir::new().unwrap();

    pw(
        tmp.path(),
        &[
            "true",
            "--reason",
            "line one\nline two\ttabbed\r\nline three",
        ],
    );

    // one_record already fails on a torn line; this pins the flattening too, so
    // a future filter that drops a field silently cannot pass.
    let rec = one_record(tmp.path(), "tester");
    let argv = rec["argv"].as_str().unwrap();
    assert!(!argv.contains('\n') && !argv.contains('\t') && !argv.contains('\r'));
    assert!(argv.contains("line one line two tabbed"), "argv: {argv}");
    assert_eq!(rec["exit"], 0);
}

/// `pw … | head` kills the child with SIGPIPE. Exit 141 is the caller's doing,
/// so it is flagged rather than reported as the tool falling over — and the log
/// line still gets written, because pw itself writes nothing to stdout.
#[test]
fn sigpipe_from_the_caller_is_flagged() {
    if !have("bash") {
        eprintln!("skipping: no bash");
        return;
    }
    let tmp = TempDir::new().unwrap();

    let out = Command::new("bash")
        .arg("-c")
        .arg(format!("{} yes pact | head -1", shell_quote(&script())))
        .env("PACT_HARNESS_DIR", tmp.path())
        .env("PACT_AGENT", "tester")
        .output()
        .expect("failed to run pw under head");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("pact"));

    let rec = one_record(tmp.path(), "tester");
    assert_eq!(rec["exit"], 141, "128+SIGPIPE, kept raw: {rec}");
    assert_eq!(rec["sigpipe"], true);
    assert_eq!(rec["tool"], "yes");
}

/// Two agents, two files: what stops 50 of them contending on one log.
#[test]
fn each_agent_writes_its_own_log() {
    if !have("bash") {
        eprintln!("skipping: no bash");
        return;
    }
    let tmp = TempDir::new().unwrap();

    for agent in ["alpha", "beta"] {
        Command::new("bash")
            .arg(script())
            .args(["true", agent])
            .env("PACT_HARNESS_DIR", tmp.path())
            .env("PACT_AGENT", agent)
            .output()
            .unwrap();
    }
    assert_eq!(one_record(tmp.path(), "alpha")["argv"], "true alpha");
    assert_eq!(one_record(tmp.path(), "beta")["argv"], "true beta");
}

/// With no `PACT_HARNESS_DIR`, a call from a linked worktree logs to the MAIN
/// checkout — the same root pact resolves its `.pact/` against. Otherwise a
/// 20-worktree fleet produces 20 unrelated logs and the run cannot be read.
#[test]
fn worktree_logs_to_the_main_checkout() {
    if !have("bash") || !have("git") {
        eprintln!("skipping: no bash or no git");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let main = base.join("main");
    std::fs::create_dir(&main).unwrap();

    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "pact tests")
            .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
            .env("GIT_COMMITTER_NAME", "pact tests")
            .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
            .output()
            .expect("failed to run git");
        assert!(out.status.success(), "git {args:?}: {:?}", out.stderr);
    };
    git(&main, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(main.join("a.txt"), "x\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "--quiet", "-m", "initial"]);
    let wt = base.join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "side",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );

    Command::new("bash")
        .arg(script())
        .args(["true", "from-worktree"])
        .current_dir(&wt)
        .env_remove("PACT_HARNESS_DIR")
        .env("PACT_AGENT", "tester")
        .output()
        .unwrap();

    assert!(
        !wt.join(".harness").exists(),
        "the worktree must not grow its own log directory"
    );
    assert_eq!(
        one_record(&main.join(".harness"), "tester")["argv"],
        "true from-worktree"
    );
}

/// The script is in the gate's blind spot: `mise run lint-scripts` globs
/// `scripts/*.sh`, and pw has no extension because it is typed at a prompt all
/// day. Shellcheck-clean is a property something enforces, so it is enforced
/// here instead.
#[test]
fn pw_is_shellcheck_clean() {
    if !have("shellcheck") {
        eprintln!("skipping: no shellcheck");
        return;
    }
    let out = Command::new("shellcheck")
        .args(["--severity=warning".as_ref(), script().as_os_str()])
        .output()
        .expect("failed to run shellcheck");
    assert!(
        out.status.success(),
        "shellcheck: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}
