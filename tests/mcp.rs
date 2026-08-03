//! Integration tests for `pact mcp serve`, driven through the compiled binary
//! over real pipes (this is a binary-only crate with no lib target, so these
//! shell out to `env!("CARGO_BIN_EXE_pact")` rather than reaching into
//! `src::mcp`).
//!
//! The unit tests in `src/mcp.rs` cover the JSON-RPC framing in-process. What
//! only a subprocess can prove is the part that matters most: that a client
//! spawning this server and calling every tool leaves the repository's
//! coordination state **byte-identical**. That is the read-only claim, and a
//! claim of that shape is worth a test rather than a comment — the module docs
//! name two ways it was nearly broken (`msg::read_thread` writes a read-by
//! label, `lease::list` sweeps expired locks), and both would pass every other
//! test in this repo.

// Gated as a whole: without the feature there is no `pact mcp serve` to drive.
// The complementary assertion — that a default build does not have the
// subcommand at all — is in tests/mcp_absent.rs, which is gated the other way.
#![cfg(feature = "mcp")]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::SystemTime;

use serde_json::Value;
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

/// Every file under `dir`, by relative path, with its bytes and its mtime.
///
/// Content *and* mtime, because content alone would miss a rewrite that
/// happened to produce the same bytes — and "rewrote the file with what was
/// already there" is exactly what an atomic temp-then-rename write looks like
/// from outside. A missing directory snapshots as empty rather than failing, so
/// the same helper works before `.pact/` exists.
fn snapshot(dir: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, (Vec<u8>, SystemTime)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = entry.metadata().expect("metadata");
            if meta.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .expect("under base")
                    .to_string_lossy()
                    .into_owned();
                let bytes = std::fs::read(&path).expect("read file");
                out.insert(rel, (bytes, meta.modified().expect("mtime")));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Everything `pact mcp serve` could conceivably write to: its own state
/// directory and the Beads store.
fn coordination_state(repo: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    let mut all = snapshot(&repo.join(".pact"));
    for (k, v) in snapshot(&repo.join(".beads")) {
        all.insert(format!(".beads/{k}"), v);
    }
    all
}

/// Feed `requests` to `pact mcp serve` on stdin, close stdin, and return one
/// parsed response per line of stdout.
///
/// Closing stdin is half the test: the spec's only portable shutdown signal is
/// EOF, so a server that needed a kill would hang here rather than fail.
fn serve(repo: &Path, env: &[(&str, &str)], requests: &[Value]) -> (Vec<Value>, i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["mcp", "serve"])
        .current_dir(repo)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pact mcp serve");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write request");
        }
    }
    // Dropped with the child's stdin handle taken above, which closes it.
    child.stdin.take();

    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let responses = stdout
        .lines()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("stdout line is not JSON: {e}\n{l}"))
        })
        .collect();
    (
        responses,
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn initialize() -> Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "pact-integration-test", "version": "0"},
        },
    })
}

fn call(id: u32, name: &str, arguments: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
}

/// The whole point: a real client, a real lease, every tool that does not need
/// `bd`, and not one byte of coordination state different afterwards.
#[test]
fn every_tool_call_leaves_the_repository_byte_identical() {
    let tmp = init_repo();
    let repo = tmp.path();

    // A lease there is something to see, and an event log to tail.
    let acquired = pact(
        repo,
        "worker-a",
        &[
            "lease",
            "acquire",
            "src/api.rs",
            "--note",
            "rewriting the response shape",
        ],
    );
    assert!(acquired.status.success(), "setup: acquire failed");

    // A decoy Beads store, so "did not write to .beads/" is a claim about a
    // directory that exists rather than one that does not.
    std::fs::create_dir(repo.join(".beads")).unwrap();
    std::fs::write(repo.join(".beads/issues.jsonl"), b"{\"id\":\"decoy\"}\n").unwrap();

    let before = coordination_state(repo);
    assert!(
        before.keys().any(|k| k.contains("api.rs")),
        "setup: expected a lock file for src/api.rs, got {:?}",
        before.keys().collect::<Vec<_>>()
    );

    let (responses, code, _stderr) = serve(
        repo,
        &[("PACT_AGENT", "observer")],
        &[
            initialize(),
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            call(3, "pact_lease_list", serde_json::json!({})),
            call(
                4,
                "pact_lease_list",
                serde_json::json!({"include_expired": true}),
            ),
            call(5, "pact_doctor", serde_json::json!({})),
            call(6, "pact_events_tail", serde_json::json!({"limit": 100})),
        ],
    );

    assert_eq!(code, 0, "stdin EOF must be a clean exit");
    // Six requests, one notification: six responses.
    assert_eq!(responses.len(), 6, "got {responses:#?}");

    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no response with id {id}"))
    };

    assert_eq!(by_id(1)["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "pact");
    assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 5);

    // The lease we planted, seen through MCP: holder, path and note.
    let leases = &by_id(3)["result"]["structuredContent"];
    assert_eq!(leases.as_array().map(Vec::len), Some(1), "{leases:#?}");
    assert_eq!(leases[0]["lease"]["agent"], "worker-a");
    assert_eq!(leases[0]["lease"]["path"], "src/api.rs");
    assert_eq!(leases[0]["lease"]["note"], "rewriting the response shape");
    assert_eq!(leases[0]["expired"], false);
    assert_eq!(by_id(3)["result"]["isError"], false);

    // doctor reports checks, and the event log has the acquire in it.
    assert!(by_id(5)["result"]["structuredContent"]["checks"]
        .as_array()
        .is_some_and(|c| !c.is_empty()));
    let events = &by_id(6)["result"]["structuredContent"];
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["path"] == "src/api.rs"),
        "the acquire should be in the event log: {events:#?}"
    );

    // THE assertion this file exists for.
    let after = coordination_state(repo);
    assert_eq!(
        before, after,
        "pact mcp serve modified coordination state; it must be strictly read-only"
    );
}

/// `pact lease ls` sweeps expired lock files as it lists them — that is its
/// documented job. The tool that mirrors it must not, or a client polling for a
/// dashboard would be silently reclaiming other agents' paths.
#[test]
fn listing_an_expired_lease_does_not_sweep_it() {
    let tmp = init_repo();
    let repo = tmp.path();

    // A one-second TTL, then wait past it *and* past the grace window.
    let acquired = pact(
        repo,
        "worker-b",
        &["lease", "acquire", "doomed.rs", "--ttl", "1"],
    );
    assert!(acquired.status.success(), "setup: acquire failed");
    std::thread::sleep(std::time::Duration::from_secs(2));

    let before = coordination_state(repo);
    let (responses, code, _) = serve(
        repo,
        &[("PACT_AGENT", "observer")],
        &[
            initialize(),
            call(
                2,
                "pact_lease_list",
                serde_json::json!({"include_expired": true}),
            ),
        ],
    );
    assert_eq!(code, 0);

    let leases = &responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("tool response")["result"]["structuredContent"];
    assert_eq!(
        leases.as_array().map(Vec::len),
        Some(1),
        "the expired lease should still be reported: {leases:#?}"
    );
    assert_eq!(leases[0]["lease"]["path"], "doomed.rs");

    let after = coordination_state(repo);
    assert_eq!(
        before, after,
        "the expired lock file was swept; pact_lease_list must use lease::peek, not lease::list"
    );
}

/// With no Beads CLI reachable, the two message tools must fail the way the
/// CLI's exit code 3 means, keep serving, and still write nothing.
#[test]
fn a_missing_beads_cli_is_a_tool_error_not_a_crash() {
    let tmp = init_repo();
    let repo = tmp.path();
    let before = coordination_state(repo);

    // An empty PATH is what makes this deterministic: the machine running the
    // test may well have `bd` installed, and a test that passes only where it
    // is absent proves nothing on the machine that matters.
    let (responses, code, _) = serve(
        repo,
        &[("PACT_AGENT", "observer"), ("PATH", "")],
        &[
            initialize(),
            call(
                2,
                "pact_msg_inbox",
                serde_json::json!({"agent": "worker-a"}),
            ),
            call(3, "pact_msg_thread", serde_json::json!({"id": "pact-abc"})),
            // After two failures: still answering, so nothing crashed.
            serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        ],
    );

    assert_eq!(code, 0, "a tool failure must not take the server down");
    for id in [2, 3] {
        let r = responses.iter().find(|r| r["id"] == id).expect("response");
        assert!(
            r["error"].is_null(),
            "must be a tool error, not a protocol error: {r:#?}"
        );
        assert_eq!(r["result"]["isError"], true, "{r:#?}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("exit 3"),
            "the error should mirror exit code 3's meaning: {text}"
        );
    }
    assert!(responses
        .iter()
        .any(|r| r["id"] == 4 && r["result"]["tools"].is_array()));

    assert_eq!(before, coordination_state(repo), "a failed tool wrote");
}

/// The modern era end to end, through the real binary: probe with
/// `server/discover`, then call a tool with the version in `_meta` — and still
/// not one byte changed.
///
/// The unit tests cover the envelope in-process. What this adds is that the two
/// eras share one process and one repository without either leaking into the
/// other's results.
#[test]
fn a_modern_session_works_and_is_equally_read_only() {
    let tmp = init_repo();
    let repo = tmp.path();
    assert!(pact(repo, "worker-c", &["lease", "acquire", "src/db.rs"])
        .status
        .success());
    let before = coordination_state(repo);

    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "modern-test", "version": "0"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let (responses, code, _) = serve(
        repo,
        &[("PACT_AGENT", "observer")],
        &[
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "server/discover",
                "params": {"_meta": meta},
            }),
            serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "pact_lease_list", "arguments": {}, "_meta": meta},
            }),
            // A version this server does not implement, in the same session.
            serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/list",
                "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "1900-01-01"}},
            }),
            // And a legacy request afterwards: one process, both eras.
            serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        ],
    );
    assert_eq!(code, 0);
    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no response {id}: {responses:#?}"))
    };

    let discover = &by_id(1)["result"];
    assert_eq!(discover["resultType"], "complete");
    assert!(discover["supportedVersions"]
        .as_array()
        .is_some_and(|v| v.iter().any(|s| s == "2026-07-28")));
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "pact"
    );

    let called = &by_id(2)["result"];
    assert_eq!(called["resultType"], "complete");
    assert_eq!(called["structuredContent"][0]["lease"]["path"], "src/db.rs");

    assert_eq!(by_id(3)["error"]["code"], -32022);
    assert_eq!(by_id(3)["error"]["data"]["requested"], "1900-01-01");

    // The legacy request in the same process gets the legacy shape.
    assert!(by_id(4)["result"]["resultType"].is_null());
    assert!(by_id(4)["result"]["tools"].is_array());

    assert_eq!(
        before,
        coordination_state(repo),
        "a modern session must be exactly as read-only as a legacy one"
    );
}

/// Malformed input on a channel the client also writes to must not kill the
/// server: the next well-formed request has to still be answered.
#[test]
fn a_parse_error_does_not_end_the_session() {
    let tmp = init_repo();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["mcp", "serve"])
        .current_dir(tmp.path())
        .env("PACT_AGENT", "observer")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{{ this is not json").unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":9,"method":"tools/list"}}"#).unwrap();
    }
    child.stdin.take();
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(0));

    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON per line"))
        .collect();
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert_eq!(lines[0]["error"]["code"], -32700);
    assert_eq!(lines[0]["id"], Value::Null);
    assert!(lines[1]["result"]["tools"].is_array());
}

/// Nothing may reach stdout that is not an MCP message — the transport says so
/// outright, and a stray banner there is a class of bug that breaks every
/// client at once while looking fine in a terminal.
#[test]
fn the_startup_banner_goes_to_stderr_not_stdout() {
    let tmp = init_repo();
    let (responses, _, stderr) = serve(tmp.path(), &[("PACT_AGENT", "observer")], &[initialize()]);
    // One request, one line: `serve` already parsed every stdout line as JSON,
    // so anything non-JSON there would have panicked before this.
    assert_eq!(responses.len(), 1);
    assert!(
        stderr.contains("read-only"),
        "the banner should say what this server is: {stderr}"
    );
}
