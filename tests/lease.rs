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
