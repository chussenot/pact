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
use std::io::{BufRead, Write};
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
            // The sixth tool reads the whole event log, so it is exactly the kind
            // of addition that could start creating `.pact/` on a read.
            call(7, "pact_audit_summary", serde_json::json!({})),
            call(8, "pact_audit_summary", serde_json::json!({"since": "30d"})),
        ],
    );

    assert_eq!(code, 0, "stdin EOF must be a clean exit");
    // Eight requests, one notification: eight responses.
    assert_eq!(responses.len(), 8, "got {responses:#?}");

    let by_id = |id: u64| {
        responses
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no response with id {id}"))
    };

    assert_eq!(by_id(1)["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "pact");
    // Pinned so a tool cannot be added without the read-only proof below covering
    // it: the byte-identical assertion is what makes a new tool safe, and an
    // unpinned count would let one slip in untested.
    assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 6);

    // The lease we planted, seen through MCP: holder, path and note.
    let leases = &by_id(3)["result"]["structuredContent"]["leases"];
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
    let events = &by_id(6)["result"]["structuredContent"]["events"];
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
        .expect("tool response")["result"]["structuredContent"]["leases"];
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

/// The message tools used to be the one part of this server that could fail for an
/// environmental reason: they went through `bd`, so an empty `PATH` made
/// `pact_msg_inbox` and `pact_msg_thread` return a tool error carrying exit 3.
///
/// **They cannot fail that way any more.** Messages live in
/// `.pact/messages.jsonl`, so an observer with no issue tracker installed gets a
/// real answer — an empty inbox is empty, not unavailable. What still has to hold is
/// everything this test was really protecting: the server stays up, keeps answering,
/// and writes nothing.
///
/// An empty `PATH` is what makes it deterministic: the machine running the test may
/// well have `bd` installed, and a test that only proves something where it is absent
/// proves nothing on the machine that matters.
#[test]
fn no_backend_on_path_still_answers_and_still_writes_nothing() {
    let tmp = init_repo();
    let repo = tmp.path();
    let before = coordination_state(repo);

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
            serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        ],
    );

    assert_eq!(code, 0, "the server must not go down");

    // An inbox with no messages in it: a real, successful, empty answer.
    let inbox = responses.iter().find(|r| r["id"] == 2).expect("response");
    assert!(inbox["error"].is_null(), "{inbox:#?}");
    assert_eq!(
        inbox["result"]["isError"], false,
        "a missing backend is no longer an error here: {inbox:#?}"
    );
    assert_eq!(
        inbox["result"]["structuredContent"]["messages"],
        serde_json::json!([]),
        "{inbox:#?}"
    );

    // An unknown thread id is still a tool error — but for the honest reason that no
    // such message exists, not because a subprocess was missing.
    let thread = responses.iter().find(|r| r["id"] == 3).expect("response");
    assert!(thread["error"].is_null(), "{thread:#?}");
    assert_eq!(thread["result"]["isError"], true, "{thread:#?}");
    let text = thread["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains("exit 3"),
        "exit 3 is unreachable from a msg path now: {text}"
    );

    assert!(responses
        .iter()
        .any(|r| r["id"] == 4 && r["result"]["tools"].is_array()));

    assert_eq!(before, coordination_state(repo), "a read-only server wrote");
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
    assert_eq!(
        called["structuredContent"]["leases"][0]["lease"]["path"],
        "src/db.rs"
    );

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
    // By id/null, not by position: each line runs on its own thread (see
    // `Server::run`'s doc comment), so the two responses may legitimately
    // land in either order.
    let parse_error = lines
        .iter()
        .find(|l| l["id"] == Value::Null)
        .unwrap_or_else(|| panic!("no null-id response: {lines:#?}"));
    assert_eq!(parse_error["error"]["code"], -32700);
    let tools_list = lines
        .iter()
        .find(|l| l["id"] == 9)
        .unwrap_or_else(|| panic!("no response to id 9: {lines:#?}"));
    assert!(tools_list["result"]["tools"].is_array());
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

/// The bug this file exists to guard: one hung Beads call must not starve
/// every OTHER in-flight tool on the same `pact mcp serve` session — including
/// `pact_lease_list`, which never touches Beads at all.
///
/// Deliberately does not use the `serve` helper above: that one waits for the
/// whole process to exit (`wait_with_output`), which cannot distinguish "the
/// second response arrived promptly while the first was still stuck" from
/// "both eventually returned around the same bounded timeout" — exactly the
/// distinction this test exists to make. So it reads stdout incrementally
/// instead, the same real transport `serve()` in production reads.
#[cfg(unix)]
#[test]
fn a_hung_beads_call_does_not_delay_other_in_flight_requests() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = init_repo();
    let repo = tmp.path();

    // A `.beads/` shaped as a br (SQLite) workspace, so `BeadsCli::locate`
    // resolves to exactly one binary name with no ambiguity from a real
    // `bd`/`br` that might also be on this machine's PATH.
    std::fs::create_dir(repo.join(".beads")).unwrap();
    std::fs::write(repo.join(".beads/stub.db"), b"").unwrap();

    // A `br` on PATH that never exits — standing in for a wedged backend
    // (hung on a TTY/credential prompt, an internal bug, a write lock).
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let stub = bin_dir.join("br");
    std::fs::write(&stub, "#!/bin/sh\nsleep 999999\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Prepended, not replacing PATH outright: the stub's own `#!/bin/sh` needs
    // `sleep` resolvable, and that comes from whatever PATH this test machine
    // already has.
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_pact"))
        .args(["mcp", "serve"])
        .current_dir(repo)
        .env("PACT_AGENT", "observer")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pact mcp serve");

    // Streamed on a background thread so the test can assert on arrival
    // timing rather than on the whole process exiting.
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        // First: a Beads-backed call that will hang behind the stub above.
        writeln!(
            stdin,
            "{}",
            call(1, "pact_msg_inbox", serde_json::json!({"agent": "someone"}))
        )
        .unwrap();
        // Immediately after: a call that never touches Beads.
        writeln!(
            stdin,
            "{}",
            call(2, "pact_lease_list", serde_json::json!({}))
        )
        .unwrap();
    }

    // The second response must appear well within a few seconds — long before
    // the stub could ever exit on its own (it never does) and long before
    // `pact_msg_inbox`'s own bounded Beads timeout (default 30s) would fire.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut id_2_response = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(line) => {
                let v: Value = serde_json::from_str(&line).expect("valid JSON line");
                if v["id"] == 2 {
                    id_2_response = Some(v);
                    break;
                }
                // id 1 is not expected within this window at all — it is
                // parked behind the hung stub — but if it somehow arrived
                // first that would not contradict this test either way.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Clean up before asserting: the hung call is still bound by its own
    // Beads timeout, which this test has no reason to wait out.
    let _ = child.kill();
    let _ = child.wait();

    let id_2_response = id_2_response.unwrap_or_else(|| {
        panic!("pact_lease_list's response never arrived while a Beads call was hung")
    });
    assert_eq!(id_2_response["result"]["isError"], false);
    assert!(id_2_response["result"]["structuredContent"]["leases"].is_array());
}
