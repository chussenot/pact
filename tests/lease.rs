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
    Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(args)
        .current_dir(repo)
        .env("PACT_AGENT", agent)
        .output()
        .expect("failed to run pact binary")
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
