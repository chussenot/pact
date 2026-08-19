//! Integration tests for `pact lease`, driven through the compiled binary
//! (this is a binary-only crate with no lib target, so these shell out to
//! `env!("CARGO_BIN_EXE_pact")` rather than reaching into `src::lease`
//! directly).

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// `find_repo_root` only checks for a `.git` entry's existence, so a bare
/// directory is enough — no need to shell out to real `git init`.
fn init_repo() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    tmp
}

fn pact(repo: &Path, agent: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pact"));
    cmd.args(args).current_dir(repo).env("PACT_AGENT", agent);
    // The refusal prose these tests read now carries the holder's harness and
    // model when either is known (pact-c3y): `held by agent-0 [claude-code] (0s
    // old, …)`. Cleared so the string under test is the same one a maintainer,
    // CI and an agent running inside a harness all see — the alternative is
    // assertions that pass or fail depending on who invoked cargo.
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
    cmd.output().expect("failed to run pact binary")
}

fn json_stdout(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn acquire_conflict_from_second_agent_exits_2() {
    let tmp = init_repo();

    let first = pact(tmp.path(), "agent-a", &["lease", "acquire", "shared.txt"]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = pact(tmp.path(), "agent-b", &["lease", "acquire", "shared.txt"]);
    assert_eq!(second.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("agent-a"),
        "stderr should name the holder: {stderr}"
    );
}

#[test]
fn expiry_then_steal_reports_stolen() {
    let tmp = init_repo();

    let first = pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "f.txt", "--ttl", "10", "--json"],
    );
    assert!(first.status.success());

    // Fabricate an already-expired lock instead of sleeping past ttl+grace.
    let lock_path = tmp.path().join(".pact/leases/f.txt.lock");
    let mut lease: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    let stale = chrono::Utc::now() - chrono::Duration::seconds(1000);
    lease["acquired_at"] = serde_json::Value::String(stale.to_rfc3339());
    std::fs::write(&lock_path, serde_json::to_string(&lease).unwrap()).unwrap();

    let second = pact(
        tmp.path(),
        "agent-b",
        &["lease", "acquire", "f.txt", "--json"],
    );
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let v = json_stdout(&second);
    assert_eq!(v["stolen"], true);
    assert_eq!(v["lease"]["agent"], "agent-b");
}

#[test]
fn reentrant_acquire_by_same_agent_refreshes_without_error() {
    let tmp = init_repo();

    let first = pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "g.txt", "--json"],
    );
    assert!(first.status.success());
    let t1 = json_stdout(&first)["lease"]["acquired_at"]
        .as_str()
        .unwrap()
        .to_string();

    std::thread::sleep(std::time::Duration::from_secs(1));

    let second = pact(
        tmp.path(),
        "agent-a",
        &["lease", "acquire", "g.txt", "--json"],
    );
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let v = json_stdout(&second);
    assert_eq!(v["stolen"], false);
    let t2 = v["lease"]["acquired_at"].as_str().unwrap().to_string();
    assert_ne!(t1, t2, "re-entrant acquire should refresh acquired_at");
}

/// Exactly one of two racers wins a concurrent expired-lease steal.
///
/// Used to assert only `successes >= 1`: `verify_own_lease` alone narrowed the
/// race from "time since read" to "between rename and re-read" without
/// closing it, and research against the compiled binary
/// (antithesis/scratchbook/properties/lease-double-win-reachable.md) showed
/// that residual window was not a hypothetical — ordinary CLI-level races
/// produced double-wins in real rounds, including at this test's own N=2
/// shape. `WriteGuard` (src/lease.rs) now serializes the whole
/// read-decide-write sequence, so the invariant is back to the one the name
/// promises:
///   1. Exactly one process exits 0.
///   2. The agent recorded on disk after both exit is the one that exited 0.
///   3. The other process exits exactly 2.
#[test]
fn concurrent_steal_of_expired_lease_has_consistent_outcome() {
    let tmp = init_repo();

    // Plant an already-expired lease (fabricate stale acquired_at directly).
    let lock_path = tmp.path().join(".pact/leases/contested.txt.lock");
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

    // Spawn both processes without waiting — true parallelism.
    let mut child_a = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["lease", "acquire", "contested.txt"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "agent-a")
        .spawn()
        .expect("failed to spawn agent-a");

    let mut child_b = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["lease", "acquire", "contested.txt"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "agent-b")
        .spawn()
        .expect("failed to spawn agent-b");

    let status_a = child_a.wait().expect("agent-a wait failed");
    let status_b = child_b.wait().expect("agent-b wait failed");

    let code_a = status_a.code().unwrap_or(-1);
    let code_b = status_b.code().unwrap_or(-1);

    // Invariant 1: exactly one process exits 0.
    let successes = [&status_a, &status_b]
        .iter()
        .filter(|s| s.success())
        .count();
    assert_eq!(
        successes, 1,
        "exactly one process must win the concurrent expired-lease steal; \
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
}

/// N-way regression guard for the exact reachability finding that motivated
/// `WriteGuard`: real CLI-process races on one pre-expired lock produced
/// double-wins in ~20-30% of rounds at N=6..10 and even triple-wins at N=6/N=8
/// (antithesis/scratchbook/properties/lease-double-win-reachable.md,
/// n-way-worktree-double-win-scaling.md), using this exact method — real
/// `pact lease acquire` subprocesses, no thread racing, no forced scheduling.
/// Several rounds so the assertion cannot pass by dodging the one round in
/// several that used to reproduce it.
#[test]
fn concurrent_nway_steal_of_expired_lease_has_exactly_one_winner() {
    const N: usize = 8;
    const ROUNDS: usize = 5;

    for round in 0..ROUNDS {
        let tmp = init_repo();
        let lock_path = tmp.path().join(".pact/leases/contested.txt.lock");
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

        let children: Vec<_> = (0..N)
            .map(|i| {
                Command::new(env!("CARGO_BIN_EXE_pact"))
                    .args(["lease", "acquire", "contested.txt"])
                    .current_dir(tmp.path())
                    .env("PACT_AGENT", format!("agent-{i}"))
                    .spawn()
                    .expect("failed to spawn racer")
            })
            .collect();

        let codes: Vec<i32> = children
            .into_iter()
            .map(|mut c| c.wait().expect("racer wait failed").code().unwrap_or(-1))
            .collect();

        let successes = codes.iter().filter(|&&c| c == 0).count();
        assert_eq!(
            successes, 1,
            "round {round}: exactly one of {N} racers must win; exit codes: {codes:?}"
        );
        for &code in &codes {
            if code != 0 {
                assert_eq!(
                    code, 2,
                    "round {round}: loser exited {code}, expected 0 or 2"
                );
            }
        }

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        let disk_agent = on_disk["agent"].as_str().unwrap();
        let winner_index: usize = disk_agent.strip_prefix("agent-").unwrap().parse().unwrap();
        assert_eq!(
            codes[winner_index], 0,
            "round {round}: disk names {disk_agent} but it did not exit 0; codes: {codes:?}"
        );
    }
}

#[test]
fn release_of_nonexistent_lease_is_idempotent() {
    let tmp = init_repo();

    let out = pact(
        tmp.path(),
        "agent-a",
        &["lease", "release", "never-leased.txt"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// (pact-m7j.5.3) `lease::peek` (behind `pact agents`) and `lease::list`'s
/// sweep (behind `pact lease ls`) share one `scan` (src/lease.rs) and differ
/// only in whether `collect_expired` runs. This is a regression fence, not a
/// bug fix: no `src/` change accompanies it. Real concurrent processes — not
/// sequenced by the test harness — race a batch of peek-backed readers against
/// the one process whose documented job is to sweep, across many trials, and
/// assert three things every trial: (1) a peek-backed reader never fails while
/// a sweep races it, (2) a lease that has NOT expired is untouched byte-for-
/// byte by either side, and (3) every removed lock has exactly one "expired"
/// event naming its OWN dead holder — never the reader, never doubled by a
/// racing peek.
#[test]
fn peek_never_mutates_while_list_sweeps_expired_locks_concurrently() {
    const TRIALS: usize = 15;
    const READERS: usize = 6;

    for trial in 0..TRIALS {
        let tmp = init_repo();
        let leases_dir = tmp.path().join(".pact/leases");
        std::fs::create_dir_all(&leases_dir).unwrap();

        let stale = chrono::Utc::now() - chrono::Duration::seconds(2000);
        let fresh = chrono::Utc::now();
        let write_lease = |name: &str, agent: &str, at: chrono::DateTime<chrono::Utc>| {
            let path = leases_dir.join(format!("{name}.lock"));
            let lease = serde_json::json!({
                "agent": agent,
                "path": name,
                "acquired_at": at.to_rfc3339(),
                "ttl_secs": 900,
                "note": null
            });
            std::fs::write(&path, serde_json::to_string(&lease).unwrap()).unwrap();
            path
        };
        // Two expired locks (must be swept) and one still-valid lock (must
        // survive both the readers and the sweep, unmodified).
        write_lease("dead0.txt", "agent-dead-0", stale);
        write_lease("dead1.txt", "agent-dead-1", stale);
        let valid_path = write_lease("alive.txt", "agent-alive", fresh);
        let valid_before = std::fs::read_to_string(&valid_path).unwrap();

        // Peek-backed readers and the one list-backed sweeper, spawned without
        // waiting between them — genuine concurrency, the same idiom as
        // `concurrent_steal_of_expired_lease_has_consistent_outcome` above.
        let mut readers: Vec<_> = (0..READERS)
            .map(|_| {
                Command::new(env!("CARGO_BIN_EXE_pact"))
                    .args(["agents", "--json"])
                    .current_dir(tmp.path())
                    .env("PACT_AGENT", "agent-reader")
                    .spawn()
                    .expect("failed to spawn reader")
            })
            .collect();
        let mut writer = Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["lease", "ls", "--json"])
            .current_dir(tmp.path())
            .env("PACT_AGENT", "agent-writer")
            .spawn()
            .expect("failed to spawn sweeper");

        for r in &mut readers {
            let status = r.wait().expect("reader wait failed");
            assert!(
                status.success(),
                "trial {trial}: a peek-backed reader must never fail while a sweep races it"
            );
        }
        assert!(
            writer.wait().expect("sweeper wait failed").success(),
            "trial {trial}: the sweeper itself must succeed"
        );

        // The expired locks are gone...
        assert!(
            !leases_dir.join("dead0.txt.lock").exists(),
            "trial {trial}: dead0 should have been swept"
        );
        assert!(
            !leases_dir.join("dead1.txt.lock").exists(),
            "trial {trial}: dead1 should have been swept"
        );
        // ...the still-valid one is untouched, byte for byte, by either side.
        assert_eq!(
            std::fs::read_to_string(&valid_path).unwrap(),
            valid_before,
            "trial {trial}: neither peek nor list may touch a lease that has not expired"
        );

        // Every disappearance is accounted for by exactly one "expired" event,
        // naming the dead lease's own holder.
        let events_raw =
            std::fs::read_to_string(tmp.path().join(".pact/events.jsonl")).unwrap_or_default();
        let mut expired_agents: Vec<String> = events_raw
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|v| v["kind"] == "expired")
            .map(|v| v["agent"].as_str().unwrap().to_string())
            .collect();
        expired_agents.sort();
        assert_eq!(
            expired_agents,
            vec!["agent-dead-0".to_string(), "agent-dead-1".to_string()],
            "trial {trial}: exactly one expired event per swept lock, naming its own dead \
             holder — never the reader, never doubled"
        );
    }
}

/// (pact-m7j.5.4) `repo::pact_dir` (creates `.pact/`) and `repo::pact_dir_path`
/// (never creates anything) are already a complete split per an exhaustive
/// grep of every call site. Regression fence, not a bug fix: no `src/` change
/// accompanies this test.
///
/// Concurrent read-only commands, with no writer anywhere in the picture, must
/// never bring `.pact/` into existence — no matter how many of them race each
/// other for the same nonexistent directory.
#[test]
fn concurrent_read_only_commands_never_create_pact_dir_without_a_writer() {
    const TRIALS: usize = 15;
    const READERS: usize = 6;

    for trial in 0..TRIALS {
        let tmp = init_repo();

        let mut readers: Vec<_> = (0..READERS)
            .map(|i| {
                // Two different peek-backed surfaces, alternated, so the fence
                // covers more than one caller.
                let args: &[&str] = if i % 2 == 0 {
                    &["agents", "--json"]
                } else {
                    &["doctor"]
                };
                Command::new(env!("CARGO_BIN_EXE_pact"))
                    .args(args)
                    .current_dir(tmp.path())
                    .env("PACT_AGENT", "agent-reader")
                    .spawn()
                    .expect("failed to spawn reader")
            })
            .collect();

        for r in &mut readers {
            // `doctor` exits 1 on a fresh repo (missing AGENTS.md etc.) — that
            // is expected and unrelated to this fence, so only a crash
            // (missing exit code) would be worth failing on. What matters is
            // what's on disk afterwards, asserted below.
            r.wait().expect("reader wait failed");
        }

        assert!(
            !tmp.path().join(".pact").exists(),
            "trial {trial}: concurrent read-only commands must never create .pact/ on their own"
        );
    }
}

/// (pact-m7j.5.4, continued) The other half: when a genuine first-time writer
/// IS racing the readers for the same nonexistent `.pact/`, the readers must
/// still never fail (the missing directory must not be a mid-race error for a
/// read path), and the directory that appears afterwards is the writer's,
/// never something a reader's own race with another reader produced.
#[test]
fn pact_dir_creation_survives_concurrent_readers_racing_a_genuine_writer() {
    const TRIALS: usize = 15;
    const READERS: usize = 6;

    for trial in 0..TRIALS {
        let tmp = init_repo();

        let mut readers: Vec<_> = (0..READERS)
            .map(|i| {
                let args: &[&str] = if i % 2 == 0 {
                    &["agents", "--json"]
                } else {
                    &["doctor"]
                };
                Command::new(env!("CARGO_BIN_EXE_pact"))
                    .args(args)
                    .current_dir(tmp.path())
                    .env("PACT_AGENT", "agent-reader")
                    .spawn()
                    .expect("failed to spawn reader")
            })
            .collect();
        // The one process in this trial allowed to create `.pact/`.
        let mut writer = Command::new(env!("CARGO_BIN_EXE_pact"))
            .args(["lease", "acquire", "f.txt"])
            .current_dir(tmp.path())
            .env("PACT_AGENT", "agent-writer")
            .spawn()
            .expect("failed to spawn writer");

        for r in &mut readers {
            r.wait().expect("reader wait failed");
        }
        assert!(
            writer.wait().expect("writer wait failed").success(),
            "trial {trial}: the genuine first-time writer must succeed"
        );

        assert!(
            tmp.path().join(".pact/leases").is_dir(),
            "trial {trial}: the writer must have created .pact/leases"
        );
    }
}
