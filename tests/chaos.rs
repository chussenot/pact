//! Tests for `scripts/chaos.sh` — the rails, not the faults.
//!
//! chaos.sh kills processes and mutates files it did not create, so what is
//! under test here is almost entirely the blast radius: that it refuses to run
//! unarmed, that it never signals a PID it was not given, that it re-checks a
//! PID still lives where it was told before signalling it, that a hidden binary
//! comes back even when chaos is killed, and that a seed reproduces a run.
//!
//! Every test builds a disposable fleet repo in a tempdir and points chaos at
//! it. `HOME` is overridden to that tempdir wherever the `$HOME`-prefix rail is
//! involved, so the backend-outage path can be exercised without a real binary
//! anywhere near a real prefix.
//!
//! Skipped rather than failed when `bash` or `jq` is missing: this file says
//! nothing about pact on a machine without them, and a test that fails for a
//! missing tool trains people to ignore red.

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
        .join("chaos.sh")
}

/// A repo that satisfies both markers, plus the pids file chaos requires.
fn armed_repo() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".pact/leases")).unwrap();
    std::fs::write(tmp.path().join(".chaos-armed"), "").unwrap();
    std::fs::write(tmp.path().join("pids"), "").unwrap();
    tmp
}

fn chaos(repo: &Path, extra: &[&str]) -> Output {
    let mut cmd = Command::new("bash");
    cmd.arg(script())
        .arg("--repo")
        .arg(repo)
        .arg("--pids")
        .arg(repo.join("pids"))
        .args(extra)
        // pact must be findable for a non-dry run; harmless for a dry one.
        .env(
            "PATH",
            format!(
                "{}:{}",
                Path::new(env!("CARGO_BIN_EXE_pact"))
                    .parent()
                    .unwrap()
                    .display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    cmd.output().expect("failed to run chaos.sh")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The decision sequence, run-start/run-end excluded — that is what a seed is
/// supposed to reproduce.
fn decisions(repo: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(repo.join("chaos-log.jsonl")).unwrap_or_default();
    log.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| !v["action"].as_str().unwrap_or("").starts_with("run-"))
        .map(|v| {
            format!(
                "{}|{}|{}",
                v["action"].as_str().unwrap_or(""),
                v["target"].as_str().unwrap_or(""),
                v["detail"].as_str().unwrap_or("")
            )
        })
        .collect()
}

// ------------------------------------------------------------------- rail 1

/// The marker that makes the difference between a disposable fleet repo and
/// somebody's work. `.pact/` alone must not be enough — every repository pact
/// has ever touched has one.
#[test]
fn refuses_to_run_without_the_chaos_armed_marker() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    std::fs::remove_file(tmp.path().join(".chaos-armed")).unwrap();

    let out = chaos(tmp.path(), &["--dry-run"]);
    assert!(!out.status.success(), "must refuse an unarmed repo");
    let err = stderr(&out);
    assert!(err.contains("REFUSING TO RUN"), "{err}");
    assert!(err.contains(".chaos-armed"), "must name the marker: {err}");
    assert!(
        !tmp.path().join("chaos-log.jsonl").exists(),
        "a refused run must not even create the log"
    );
}

#[test]
fn refuses_a_repo_with_no_pact_directory() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    std::fs::remove_dir_all(tmp.path().join(".pact")).unwrap();
    let out = chaos(tmp.path(), &["--dry-run"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains(".pact/"), "{}", stderr(&out));
}

// ------------------------------------------------------------------- rail 2

/// Pointed at pact's own checkout, which has both a `.pact/` and — if somebody
/// armed it — the marker. The content check is what has to catch this.
#[test]
fn refuses_pacts_own_checkout_even_when_armed() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let pact_repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Not armed for real — arming pact's own repo is exactly what must never be
    // needed. The canonical-path rail fires first and is what is asserted.
    let out = Command::new("bash")
        .arg(script())
        .arg("--repo")
        .arg(pact_repo)
        .arg("--pids")
        .arg("/dev/null")
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(!out.status.success(), "must never accept its own repo");
    let err = stderr(&out);
    assert!(err.contains("REFUSING TO RUN"), "{err}");
    assert!(
        err.contains("pact") && (err.contains("own checkout") || err.contains("source checkout")),
        "the refusal must say why: {err}"
    );
}

// ------------------------------------------------------------------- rail 3

/// The allowlist and the cwd re-check, proven by survival rather than by
/// reading the log alone: a decoy process registered in `--pids` but running
/// OUTSIDE the repo must still be alive afterwards.
#[test]
fn never_kills_a_registered_pid_whose_cwd_is_outside_the_repo() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let outside = tempfile::tempdir().unwrap();

    // A decoy that is registered under an agent name chaos will see as a lease
    // holder, but whose cwd is somewhere else entirely.
    let mut decoy = Command::new("sleep")
        .arg("300")
        .current_dir(outside.path())
        .spawn()
        .expect("spawn decoy");
    let decoy_pid = decoy.id();

    std::fs::write(
        tmp.path().join("pids"),
        format!("{decoy_pid}\tdecoy-agent\n"),
    )
    .unwrap();
    // A lease held by that agent, so kill-holder has a reason to pick it.
    std::fs::write(
        tmp.path().join(".pact/leases/src__decoy.rs.lock"),
        r#"{"agent":"decoy-agent","path":"src/decoy.rs","acquired_at":"2099-01-01T00:00:00Z","ttl_secs":2700,"note":null}"#,
    )
    .unwrap();

    let out = chaos(
        tmp.path(),
        &[
            "--seed",
            "1",
            "--duration",
            "6",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "kill-holder",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // The decoy is untouched.
    assert!(
        matches!(decoy.try_wait(), Ok(None)),
        "a pid whose cwd is outside --repo must never be signalled"
    );
    let _ = decoy.kill();

    // And the rail said so, rather than firing silently.
    let d = decisions(tmp.path()).join("\n");
    assert!(
        d.contains("SKIPPED") && d.contains("kill-holder"),
        "the refusal must be in the log: {d}"
    );
}

/// A PID that is alive and inside the repo but was never registered is not a
/// candidate at all.
#[test]
fn never_kills_a_pid_absent_from_the_pids_file() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let mut inside = Command::new("sleep")
        .arg("300")
        .current_dir(tmp.path())
        .spawn()
        .expect("spawn");

    // Registered under a DIFFERENT pid than the one running, so the agent maps
    // to nothing killable.
    std::fs::write(tmp.path().join("pids"), "999999\tghost-agent\n").unwrap();
    std::fs::write(
        tmp.path().join(".pact/leases/src__ghost.rs.lock"),
        r#"{"agent":"ghost-agent","path":"src/ghost.rs","acquired_at":"2099-01-01T00:00:00Z","ttl_secs":2700,"note":null}"#,
    )
    .unwrap();

    let out = chaos(
        tmp.path(),
        &[
            "--seed",
            "3",
            "--duration",
            "6",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "kill-holder",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        matches!(inside.try_wait(), Ok(None)),
        "an unregistered process inside the repo must survive"
    );
    let _ = inside.kill();
}

// ------------------------------------------------------------------- rail 4

/// An outage must never outlive chaos. Killed mid-outage, the trap has to put
/// the binary back — otherwise a crashed fault injector leaves the machine
/// without its Beads CLI.
#[test]
fn a_hidden_backend_is_restored_even_when_chaos_is_killed() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    // The $HOME-prefix rail means the fake binary has to live under HOME, so
    // HOME becomes the tempdir for this test.
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_bd = bin_dir.join("bd");
    std::fs::write(&fake_bd, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bd, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = Command::new("bash")
        .arg(script())
        .arg("--repo")
        .arg(tmp.path())
        .arg("--pids")
        .arg(tmp.path().join("pids"))
        .args([
            "--seed",
            "5",
            "--duration",
            "10",
            "--time-unit",
            "sec",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "backend-outage",
            "--outage-secs",
            "120",
        ])
        .env("HOME", tmp.path())
        // The real PATH is kept, not replaced: chaos needs jq and
        // sha256sum, and a narrowed PATH makes it die at its own
        // dependency check instead of reaching the rail under test.
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chaos");

    // Wait for the outage to actually start — the first fault is a minute in,
    // so poll for the rename rather than sleeping a fixed amount.
    let hidden = bin_dir.join("bd.chaos-hidden");
    let mut saw_outage = false;
    for _ in 0..900 {
        if hidden.exists() {
            saw_outage = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(saw_outage, "the outage never started; nothing to restore");

    // SIGTERM, not SIGKILL, and the difference is the whole point: Rust's
    // `Child::kill()` sends SIGKILL, which the kernel delivers without running
    // any handler, so a trap CANNOT fire. That is a real limitation of the rail
    // and is documented as one — chaos guarantees restore against every signal
    // a process can catch, and against nothing else. Testing it with SIGKILL
    // would assert an impossibility.
    let pid = child.id().to_string();
    let _ = Command::new("kill").args(["-TERM", &pid]).status();
    let _ = child.wait();

    // The trap runs in chaos's own process, so give it a moment to finish.
    let mut restored = false;
    for _ in 0..100 {
        if fake_bd.exists() && !hidden.exists() {
            restored = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        restored,
        "the trap must restore the binary: bd={} hidden={}",
        fake_bd.exists(),
        hidden.exists()
    );
}

/// Rail 5: a binary outside $HOME is a system path and is refused rather than
/// renamed.
#[test]
fn refuses_to_hide_a_backend_outside_home() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let bin_dir = tmp.path().join("sysbin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_bd = bin_dir.join("bd");
    std::fs::write(&fake_bd, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bd, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // HOME points somewhere the binary is NOT under.
    let elsewhere = tempfile::tempdir().unwrap();
    let out = Command::new("bash")
        .arg(script())
        .arg("--repo")
        .arg(tmp.path())
        .arg("--pids")
        .arg(tmp.path().join("pids"))
        .args([
            "--seed",
            "5",
            "--duration",
            "3",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "backend-outage",
        ])
        .env("HOME", elsewhere.path())
        // The real PATH is kept, not replaced: chaos needs jq and
        // sha256sum, and a narrowed PATH makes it die at its own
        // dependency check instead of reaching the rail under test.
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(fake_bd.exists(), "the binary must not have been renamed");
    let d = decisions(tmp.path()).join("\n");
    assert!(
        d.contains("SKIPPED") && d.contains("system path"),
        "the refusal must name the reason: {d}"
    );
}

// --------------------------------------------------------------- determinism

/// A seed has to reproduce a run, or a failure found by chaos cannot be
/// replayed. Two dry passes, byte-identical decision sequences.
#[test]
fn the_same_seed_produces_the_same_decision_sequence() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let a = armed_repo();
    let b = armed_repo();
    for repo in [a.path(), b.path()] {
        let out = chaos(repo, &["--seed", "12345", "--duration", "60", "--dry-run"]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let first = decisions(a.path());
    let second = decisions(b.path());
    assert!(
        first.len() >= 4,
        "too few decisions to prove anything: {first:?}"
    );
    assert_eq!(first, second, "same seed must replay identically");

    // And a different seed must NOT match, or "deterministic" would just mean
    // "constant" — which is exactly what the first cut of the PRNG was.
    let c = armed_repo();
    let out = chaos(
        c.path(),
        &["--seed", "999", "--duration", "60", "--dry-run"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_ne!(
        first,
        decisions(c.path()),
        "a different seed must produce a different plan"
    );
}

/// `--dry-run` is the CI self-test, so it must be provably side-effect free
/// beyond its own log.
#[test]
fn a_dry_run_touches_nothing_but_its_log() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let lock = tmp.path().join(".pact/leases/src__a.rs.lock");
    let body = r#"{"agent":"a","path":"src/a.rs","acquired_at":"2099-01-01T00:00:00Z","ttl_secs":2700,"note":null}"#;
    std::fs::write(&lock, body).unwrap();

    let out = chaos(
        tmp.path(),
        &["--seed", "8", "--duration", "60", "--dry-run"],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        body,
        "a dry run must not have touched a lock"
    );
    let d = decisions(tmp.path()).join("\n");
    assert!(d.contains("would"), "dry decisions read as intentions: {d}");
    let log = std::fs::read_to_string(tmp.path().join("chaos-log.jsonl")).unwrap();
    for line in log.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["dry"], true, "every line of a dry run is marked: {line}");
    }
}

// ------------------------------------------------------- faults, for real

/// lock-vandal must produce exactly the shape pact documents as corrupt, and
/// pact must then behave the way docs/leases.md says it does: a plain acquire
/// refuses with exit 2, and `--steal` gets through.
#[test]
fn lock_vandal_produces_the_documented_corrupt_lease_path() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    // A real lock, written by pact itself.
    let pact = env!("CARGO_BIN_EXE_pact");
    assert!(Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(pact)
        .args(["lease", "acquire", "src/victim.rs"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "holder")
        .status()
        .unwrap()
        .success());

    let out = chaos(
        tmp.path(),
        &[
            "--seed",
            "2",
            "--duration",
            "6",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "lock-vandal",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let lock = tmp.path().join(".pact/leases/src__victim.rs.lock");
    assert_eq!(
        std::fs::metadata(&lock).unwrap().len(),
        0,
        "vandal truncates to 0 bytes"
    );
    // The log must carry what was there before, or the fault is unexplainable
    // after the fact.
    let d = decisions(tmp.path()).join("\n");
    assert!(d.contains("was:") && d.contains("holder"), "{d}");

    // Six slots were planned and lock-vandal is once-per-run, so the five after
    // the one that fired must say so rather than truncating again. This is the
    // half of pact-mqw.8 that moved the once-per-run gate from plan_build to
    // dispatch: the plan may now offer an action several slots, and the gate
    // spends the budget on the first one that actually EXECUTES.
    assert_eq!(
        d.matches("truncated to 0 bytes").count(),
        1,
        "lock-vandal must fire exactly once however many slots it is offered: {d}"
    );
    assert!(
        d.contains("once-per-run and already executed this run"),
        "the slots after the fault must be logged as spent, not silently dropped: {d}"
    );

    // pact's documented behaviour on a corrupt lock: refuse with exit 2.
    let refused = Command::new(pact)
        .args(["lease", "acquire", "src/victim.rs"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "newcomer")
        .output()
        .unwrap();
    assert_eq!(
        refused.status.code(),
        Some(2),
        "a corrupt lock must refuse with exit 2, got {:?}: {}",
        refused.status.code(),
        stderr(&refused)
    );
    // And --steal is the documented way through.
    let stolen = Command::new(pact)
        .args(["lease", "acquire", "src/victim.rs", "--steal"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "newcomer")
        .output()
        .unwrap();
    assert!(
        stolen.status.success(),
        "--steal must recover a corrupt lock: {}",
        stderr(&stolen)
    );
}

/// stale-lock must plant a lease pact reads as expired — and it must refuse to
/// write anything at all if the lock shape has drifted, rather than guessing.
#[test]
fn stale_lock_plants_a_lease_pact_treats_as_expired() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let pact = env!("CARGO_BIN_EXE_pact");
    assert!(Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .status()
        .unwrap()
        .success());
    let hint = tmp.path().join("paths-hint");
    std::fs::write(&hint, "src/contested.rs\n").unwrap();

    let out = chaos(
        tmp.path(),
        &[
            "--seed",
            "4",
            "--duration",
            "6",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "stale-lock",
            "--paths-hint",
            hint.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let d = decisions(tmp.path()).join("\n");
    assert!(
        !d.contains("SHAPE DRIFTED"),
        "the lock shape drifted and chaos correctly refused — update chaos.sh: {d}"
    );
    assert!(d.contains("backdated"), "{d}");

    // pact must see it as expired and hand it over without --steal, which is
    // the takeover path this fault exists to exercise.
    let listed = Command::new(pact)
        .args(["lease", "ls", "--all", "--json"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&listed.stdout).unwrap_or(serde_json::Value::Null);
    let expired = json
        .as_array()
        .map(|a| {
            a.iter()
                .any(|e| e["lease"]["path"] == "src/contested.rs" && e["expired"] == true)
        })
        .unwrap_or(false);
    assert!(expired, "the planted lease must read as expired: {json}");

    let took = Command::new(pact)
        .args(["lease", "acquire", "src/contested.rs"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "taker")
        .output()
        .unwrap();
    assert!(
        took.status.success(),
        "an expired lease must be acquirable with no --steal: {}",
        stderr(&took)
    );
}

/// A held hint path must cost one attempt, not the whole action.
///
/// pact-mqw.8: `do_stale_lock` drew ONE path from `--paths-hint` and gave up if
/// the acquire failed. In crucible that meant one attempt against a file a live
/// agent held, one logged skip, and zero stale leases planted in an 85-minute
/// run — for the only fault that exercises expired-lease takeover against a lock
/// pact itself wrote. Every path worth listing in a hint file is a hot path, so
/// the busier the fleet the likelier the highest-value fault no-opped.
#[test]
fn stale_lock_moves_on_to_a_free_path_when_the_first_one_is_held() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let pact = env!("CARGO_BIN_EXE_pact");
    assert!(Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .status()
        .unwrap()
        .success());
    // One of the two hint paths is held by a live agent for a long TTL, so no
    // acquire of it can succeed for the duration of this test.
    assert!(Command::new(pact)
        .args(["lease", "acquire", "src/held.rs", "--ttl", "3600"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "live-holder")
        .status()
        .unwrap()
        .success());
    let hint = tmp.path().join("paths-hint");
    std::fs::write(&hint, "src/held.rs\nsrc/free.rs\n").unwrap();

    let out = chaos(
        tmp.path(),
        &[
            "--seed",
            "11",
            "--duration",
            "1",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "stale-lock",
            "--paths-hint",
            hint.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // Order-independent: the shuffle may try either path first, but the free one
    // is the only one that can be planted, and it must be.
    let d = decisions(tmp.path()).join("\n");
    assert!(
        !d.contains("SHAPE DRIFTED"),
        "the lock shape drifted and chaos correctly refused — update chaos.sh: {d}"
    );
    assert!(
        d.contains("stale-lock|src/free.rs|backdated"),
        "the free hint path must be planted even though another was held: {d}"
    );
    // And the live holder's lock is untouched: chaos plants, it does not displace.
    let held = std::fs::read_to_string(tmp.path().join(".pact/leases/src__held.rs.lock")).unwrap();
    assert!(
        held.contains("live-holder"),
        "a held path must be left to its holder: {held}"
    );
}

/// Exhausting the hint list must be a stated conclusion, not one silent skip.
///
/// This is the deterministic half of the pact-mqw.8 regression: with every hint
/// path held there is no shuffle order in which the old code and the new one
/// agree. Old: one skip. New: one skip per path plus the verdict.
#[test]
fn stale_lock_reports_the_whole_hint_list_as_held_when_it_is() {
    if !have("bash") || !have("jq") {
        eprintln!("SKIP: bash or jq missing");
        return;
    }
    let tmp = armed_repo();
    let pact = env!("CARGO_BIN_EXE_pact");
    assert!(Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(tmp.path())
        .status()
        .unwrap()
        .success());
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        assert!(Command::new(pact)
            .args(["lease", "acquire", path, "--ttl", "3600"])
            .current_dir(tmp.path())
            .env("PACT_AGENT", "live-holder")
            .status()
            .unwrap()
            .success());
    }
    let hint = tmp.path().join("paths-hint");
    std::fs::write(&hint, "src/a.rs\nsrc/b.rs\nsrc/c.rs\n").unwrap();

    let out = chaos(
        tmp.path(),
        &[
            "--seed",
            "12",
            "--duration",
            "1",
            "--time-unit",
            "sec",
            "--interval-min",
            "1",
            "--interval-max",
            "1",
            "--actions",
            "stale-lock",
            "--paths-hint",
            hint.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let d = decisions(tmp.path()).join("\n");
    assert_eq!(
        d.matches("pact lease acquire failed").count(),
        3,
        "every hint path must be tried and each failure logged: {d}"
    );
    assert!(
        d.contains("every hint path is held by a live agent; no stale lease planted"),
        "exhausting the list must be stated, or 'chaos did nothing' is unreadable: {d}"
    );
    assert!(!d.contains("backdated"), "nothing was plantable: {d}");
}
