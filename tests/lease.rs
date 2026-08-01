//! Integration tests for `pact lease`, driven through the compiled binary
//! (this is a binary-only crate with no lib target, so these shell out to
//! `env!("CARGO_BIN_EXE_pact")` rather than reaching into `src::lease`
//! directly — see docs/pact-scaffolding-prompt.md).

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

#[test]
fn concurrent_steal_of_expired_lease_only_one_process_wins() {
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

    let successes = [&status_a, &status_b]
        .iter()
        .filter(|s| s.success())
        .count();
    assert_eq!(
        successes,
        1,
        "exactly one process must win the concurrent expired-lease steal; \
         agent-a={}, agent-b={}",
        status_a.code().unwrap_or(-1),
        status_b.code().unwrap_or(-1),
    );

    // The loser must exit 2 (lease held by another agent), not any other code.
    let loser_code = if status_a.success() {
        status_b.code()
    } else {
        status_a.code()
    };
    assert_eq!(loser_code, Some(2), "loser must exit 2, got {loser_code:?}");
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
