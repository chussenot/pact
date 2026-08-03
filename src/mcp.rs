//! A strictly read-only MCP server, behind the off-by-default `mcp` feature.
//!
//! ## What it is for
//!
//! pact's coordination state is answerable from the CLI, which assumes the
//! asker can run a shell command. Not every observer can: an orchestrator
//! watching a fleet, or a dashboard agent whose only tool surface is MCP, has
//! no way to ask "who holds what, and is anyone blocked" without a human
//! relaying it — the exact failure mode `pact msg` exists to remove one level
//! up. `pact mcp serve` gives those observers the five questions and nothing
//! else.
//!
//! ## Why it is read-only, and why that is not a limitation
//!
//! Every mutation stays on the CLI on purpose. A lease is a promise made *by a
//! named agent* that it is working on a path, and its value is entirely in that
//! being true — `pact log` is a record of who said they would do what. An
//! observer is not an agent: it holds no files, does no work, and cannot honour
//! a claim. Letting it acquire a lease would put a claim in the log that no
//! process stands behind, which is worse than no claim at all, because the next
//! agent renegotiates against a peer that does not exist.
//!
//! The same argument applies to messages, one step sharper. `pact msg send`
//! addresses work to an agent or to a path, and a sender checks `msg sent` to
//! see whether a decision landed. A message from an observer is a message
//! nobody can reply to.
//!
//! ## The read-marking trap
//!
//! `pact msg read <id>` **writes**: it adds a `read-by-<agent>` label to the
//! bead, and that label is precisely what a sender checks to decide whether to
//! re-send. So the obvious implementation of a "show me this thread" tool —
//! call `msg::read_thread` — would have this server quietly telling every
//! sender their message had been received by an agent that never saw it, and
//! the re-send that should have happened would not.
//!
//! [`msg::peek_thread`] is the non-marking twin, in the same spirit as
//! [`lease::peek`] beside `lease::list`. Both tools that touch messages use it,
//! and both say so in the description the client shows the model, because an
//! orchestrator deciding whether to nudge an agent needs to know that looking
//! did not count as delivery.
//!
//! The lease equivalent is subtler and was the second trap: `pact lease ls`
//! garbage-collects expired lock files — that is its documented job — so the
//! tool that mirrors it uses [`lease::peek`], which does not. Answering a
//! question must not change the answer (pact-rnc.19), and here it must not
//! change it for the *agents*, either.
//!
//! ## Why the protocol is hand-rolled
//!
//! Same reason as `src/otel.rs`, and the bar is even lower here: MCP's stdio
//! transport is newline-delimited JSON-RPC 2.0, one message per line, with
//! nothing on stdout that is not a message. That is `serde_json`, which pact
//! already depends on, plus `std::io::BufRead`. The `mcp` feature is asserted
//! to add zero crates by the same `cargo tree` equality check that guards
//! `otel`.
//!
//! ## Protocol revision
//!
//! This server speaks the **initialization-based** ("legacy", in the spec's
//! terms) revisions: the client sends `initialize`, we answer with our
//! capabilities, it sends `notifications/initialized`, and `tools/list` and
//! `tools/call` follow. [`PROTOCOL_VERSION`] is what we advertise;
//! [`KNOWN_PROTOCOL_VERSIONS`] is echoed back verbatim when the client asks for
//! one of them, per the negotiation rule ("If the server supports the requested
//! protocol version, it MUST respond with the same version. Otherwise, the
//! server MUST respond with another protocol version it supports").
//!
//! Revision `2026-07-28` replaced that handshake with per-request `_meta`
//! versioning and a mandatory `server/discover`. It is deliberately NOT
//! implemented here: it is a second era rather than a newer field set, and
//! guessing at it is how you ship a server that is wrong in both. A client that
//! probes `server/discover` first gets a `-32601`, which is exactly the signal
//! the spec's stdio backward-compatibility rules tell it to read as "this
//! server is legacy, fall back to `initialize`" — so a dual-era client works
//! today and a modern-only one fails loudly instead of subtly.
//!
//! ## Lifecycle
//!
//! No daemon, no port, no background thread. The client spawns the process and
//! owns it; we read stdin until EOF and exit 0. That is the spec's primary and
//! only portable shutdown signal ("Servers SHOULD exit promptly when their
//! standard input is closed").

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::{beads, doctor, events, lease, msg, otel, output};

/// The revision we advertise when the client asks for something we do not know.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions whose `initialize` / `tools/list` / `tools/call` shapes are the
/// ones implemented here, newest last. Every field this server emits — a tool's
/// `name`/`description`/`inputSchema`, a result's `content[]`/`isError` — is
/// present and unchanged across all three, so echoing any of them back is
/// honest rather than optimistic.
///
/// `2025-11-25` is absent on purpose: it is initialization-based and would
/// probably work, but "probably" is not something to put on the wire. A client
/// asking for it gets [`PROTOCOL_VERSION`] and downgrades, which the negotiation
/// rule provides for.
const KNOWN_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

// JSON-RPC 2.0 error codes. Only the reserved ones; MCP defines no others for
// the methods implemented here, and inventing a code in the reserved range is
// how a client's error handling stops working.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Default and ceiling for `pact_events_tail`.
///
/// Bounded because the event log is append-only and unbounded, and an observer
/// polling it is the caller least able to notice it is shipping a megabyte per
/// call into a model's context. 50 is `pact log`'s own default.
const EVENTS_DEFAULT: usize = 50;
const EVENTS_MAX: usize = 500;

/// One tool: the wire name, the description the model is shown, and the input
/// schema.
///
/// Every description opens by saying the tool is read-only. That is not
/// decoration: the client shows this text to a model that is deciding what it
/// is allowed to do, and a model that believes it can claim a lease here will
/// try, fail, and — worse — report to a human that pact refused it.
struct Tool {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
}

/// `{"type": "object", "additionalProperties": false}` — the spec's recommended
/// spelling for "no parameters", which accepts only an empty object.
fn no_args() -> Value {
    json!({ "type": "object", "additionalProperties": false })
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "pact_lease_list",
        description: "Read-only. Lists the advisory file leases agents currently hold in this \
             repository: for each, the holder, the path, its age in seconds, the TTL remaining \
             (negative once the lease is past it), the holder's note explaining what they are \
             doing, and whether pact considers it expired and reclaimable. Same shape as \
             `pact lease ls --json`, with one deliberate difference: the CLI garbage-collects \
             expired lock files as it lists them and this tool never writes, so an expired lease \
             may still be reported here after the CLI would have swept it. Acquiring, renewing, \
             releasing and stealing leases are not available over MCP — they are promises only a \
             working agent can make, and they stay on the pact CLI.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "include_expired": {
                        "type": "boolean",
                        "description": "Include leases past their TTL and grace period, which another agent may take. Default false, matching `pact lease ls` without --all.",
                    }
                },
                "additionalProperties": false,
            })
        },
    },
    Tool {
        name: "pact_msg_inbox",
        description: "Read-only. Lists the messages addressed to one agent identity, newest last, \
             with sender, subject, body and an unread flag. IMPORTANT: unlike `pact msg inbox` \
             followed by `pact msg read`, this tool does NOT mark anything read — pact records \
             read state as a `read-by-<agent>` label on the message, and senders check it to \
             decide whether a decision landed. Reading a message here therefore does not affect \
             delivery state in any way, and the recipient will still see it as unread. Sending, \
             replying and marking read are not available over MCP.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "The agent identity whose inbox to read, as shown by `pact agents`. Required: an observer may be watching several, so there is no default.",
                    },
                    "unread_only": {
                        "type": "boolean",
                        "description": "Report only messages this agent has not read. Default false.",
                    }
                },
                "required": ["agent"],
                "additionalProperties": false,
            })
        },
    },
    Tool {
        name: "pact_msg_thread",
        description: "Read-only. Returns every message in one thread, oldest first, with full \
             bodies — the conversation `pact msg read <id>` shows. IMPORTANT: `pact msg read` \
             marks the whole thread read for the reading agent by writing a `read-by-<agent>` \
             label; this tool does NOT mark them. Nothing about delivery state changes, senders will still \
             see the thread as unread by its recipients, and an agent that has not read it still \
             needs to. Any id in the thread works, not only the root.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Any message id in the thread, e.g. from pact_msg_inbox. The whole thread is returned regardless of which member is named.",
                    },
                    "agent": {
                        "type": "string",
                        "description": "Optional. Whose `read` flag to report on each message. Omit to report each message's own recipient.",
                    }
                },
                "required": ["id"],
                "additionalProperties": false,
            })
        },
    },
    Tool {
        name: "pact_doctor",
        description: "Read-only. Runs pact's health checks on this repository and returns each \
             one's name, ok/warning status and detail, plus whether the repository is healthy \
             overall. Answers questions like whether the coordination protocol in AGENTS.md is \
             current, whether a Beads backend is on PATH, and whether the protocol files would \
             survive a clone. Checks only inspect; none of them repairs anything.",
        schema: no_args,
    },
    Tool {
        name: "pact_events_tail",
        description: "Read-only. Returns the last N entries of the lease event log — every \
             acquire, renew, release, steal and expiry, with the agent, the path, the note and \
             the timestamp. This is `pact log`, and it is the cheapest way to see whether a fleet \
             is still moving or has gone quiet. Appending to the log is a side effect of the \
             lease commands; this tool only reads it.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "How many of the most recent events to return. Default 50, maximum 500; larger values are clamped rather than refused.",
                        "minimum": 1,
                        "maximum": EVENTS_MAX,
                    }
                },
                "additionalProperties": false,
            })
        },
    },
];

/// Everything a tool call needs, resolved once at startup.
pub struct Server {
    root: PathBuf,
}

impl Server {
    pub fn new(root: PathBuf) -> Self {
        Server { root }
    }

    /// Read newline-delimited JSON-RPC from `input` and write responses to
    /// `output`, until `input` reaches EOF. Returns the process exit code.
    ///
    /// Split from [`serve`] on the real streams so the framing is testable
    /// without a subprocess: every protocol-level test in this module drives
    /// this function over a `&[u8]` and a `Vec<u8>`.
    pub fn run(&self, input: impl BufRead, output: &mut impl Write) -> Result<i32> {
        for line in input.lines() {
            let line = line?;
            // A blank line is not a message. The spec forbids embedded newlines
            // rather than promising there are no empty ones, and answering a
            // parse error to a stray "\n" would be noise on a channel where
            // every byte we write must be a message.
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle(&line) {
                // One line, always: `to_string` never emits a newline, and a
                // pretty-printed response would break the framing outright.
                writeln!(output, "{response}")?;
                // Unbuffered from the client's point of view. Without this the
                // response can sit in our BufWriter while the client blocks
                // reading it and neither side moves — a deadlock that looks
                // exactly like a hung server.
                output.flush()?;
            }
        }
        Ok(0)
    }

    /// One request line in, at most one response line out. `None` for a
    /// notification, which by JSON-RPC must never be answered.
    fn handle(&self, line: &str) -> Option<String> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // id is unknown when the request could not be parsed, and JSON-RPC
            // says to use null there.
            Err(e) => return Some(error_response(Value::Null, PARSE_ERROR, &e.to_string())),
        };
        let Some(object) = value.as_object() else {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message must be an object",
            ));
        };

        // Absent id means notification. Present-but-null is not a notification,
        // it is a request with a null id, and it gets a response.
        let id = object.get("id").cloned();
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return id
                .map(|id| error_response(id, INVALID_REQUEST, "missing or non-string \"method\""));
        };
        let params = object.get("params");

        let Some(id) = id else {
            // Notifications: `notifications/initialized` is the one we expect and
            // it needs no reply, and an unknown notification must also be
            // answered with silence rather than a method-not-found — a response
            // to something with no id is a protocol violation in itself.
            return None;
        };

        match method {
            "initialize" => Some(ok_response(id, self.initialize(params))),
            "tools/list" => Some(ok_response(id, tools_list())),
            "tools/call" => Some(self.tools_call(id, params)),
            _ => Some(error_response(
                id,
                METHOD_NOT_FOUND,
                &format!("unknown method \"{method}\""),
            )),
        }
    }

    fn initialize(&self, params: Option<&Value>) -> Value {
        let requested = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str);
        let version = match requested {
            Some(v) if KNOWN_PROTOCOL_VERSIONS.contains(&v) => v,
            _ => PROTOCOL_VERSION,
        };
        json!({
            "protocolVersion": version,
            // No `listChanged`: the tool list is a const in this file, so it
            // cannot change while the process lives, and claiming otherwise
            // would invite a client to wait for a notification we will never
            // send.
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "pact",
                "version": env!("CARGO_PKG_VERSION"),
            },
            // Shown to the model by most clients, so it carries the one fact
            // that changes what the model should do with these tools.
            "instructions": "pact coordinates coding agents working on one repository. This \
                 server is strictly read-only: it observes leases, messages, health and the event \
                 log, and cannot acquire or release a lease, send a message, or mark a message \
                 read. Reading a message or thread here does NOT mark it read and does not \
                 affect delivery state. Every mutation is a `pact` CLI command run by the agent \
                 doing the work.",
        })
    }

    fn tools_call(&self, id: Value, params: Option<&Value>) -> String {
        // Bound before the field reads rather than chained through `and_then`,
        // which cannot name the lifetime the borrowed `&str` comes from.
        let Some(params) = params else {
            return error_response(id, INVALID_PARAMS, "\"params\" is required for tools/call");
        };
        let Some(name) = params["name"].as_str() else {
            return error_response(id, INVALID_PARAMS, "missing or non-string \"name\"");
        };
        // Absent `arguments` is legal for a tool whose schema requires nothing,
        // so it reads as empty rather than as an error.
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(map)) => map,
            Some(_) => {
                return error_response(id, INVALID_PARAMS, "\"arguments\" must be an object")
            }
        };

        // An unknown tool is a protocol error, not a tool error: the spec puts
        // "unknown tool" under JSON-RPC errors, and a model cannot self-correct
        // its way out of a name that does not exist.
        let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
            return error_response(id, METHOD_NOT_FOUND, &format!("unknown tool \"{name}\""));
        };

        // One span per call, named for the tool. The tool NAME only — never an
        // argument: a path, an agent name or a message id has no business in a
        // span attribute, exactly as for `pact.subcommand`.
        //
        // `tool.name` and not the `name` off the wire, and the compiler is what
        // insists: `Span::set` takes `&'static str`, so the only strings that can
        // reach it are the five literals in `TOOLS`. An unbounded attribute is
        // not merely discouraged here, it does not typecheck.
        let mut span = otel::span("mcp tools/call");
        span.set("pact.mcp.tool", tool.name);

        match self.dispatch(tool.name, args) {
            Ok(value) => ok_response(id, tool_result(value)),
            Err(e) => {
                let code = output::code_for(&e);
                span.fail(if code == 3 {
                    "bd-missing"
                } else {
                    "tool-error"
                });
                // A tool EXECUTION error, per the spec: reported inside a
                // successful result with `isError`, not as a JSON-RPC error, so
                // the client hands the text to the model and it can recover.
                // The exit code is named because it is pact's documented API
                // (3 = no Beads CLI on PATH) and because an orchestrator that
                // knows the CLI should branch on the same number it would have
                // got from a shell.
                ok_response(id, tool_error(&format!("{e:#}"), code))
            }
        }
    }

    /// The read-only body of every tool. Returns the value that becomes
    /// `structuredContent`.
    ///
    /// Every arm reuses the module the CLI uses — `lease`, `msg`, `doctor`,
    /// `events` — rather than reimplementing a query, so a fix to how pact reads
    /// its own state cannot land in one and miss the other. Where a read shells
    /// out to `bd`/`br`, the failure travels up as the same `ExitError` the CLI
    /// raises and becomes a tool error above.
    fn dispatch(&self, name: &str, args: &Map<String, Value>) -> Result<Value> {
        match name {
            // `peek`, NOT `list`: `lease ls` sweeps expired lock files as it
            // lists them, which is a write, and it is the documented job of that
            // command rather than of this question (pact-rnc.19).
            "pact_lease_list" => {
                let include_expired = flag(args, "include_expired");
                Ok(serde_json::to_value(lease::peek(
                    &self.root,
                    include_expired,
                )?)?)
            }
            "pact_msg_inbox" => {
                let agent = required_str(args, "agent")?;
                let cli = beads::BeadsCli::locate()?;
                let messages = msg::inbox(&cli, &self.root, agent, flag(args, "unread_only"))?;
                Ok(serde_json::to_value(messages)?)
            }
            // `peek_thread`, NOT `read_thread`: the latter writes a
            // `read-by-<agent>` label. See the module docs.
            "pact_msg_thread" => {
                let id = required_str(args, "id")?;
                let viewer = args.get("agent").and_then(Value::as_str);
                let cli = beads::BeadsCli::locate()?;
                Ok(serde_json::to_value(msg::peek_thread(
                    &cli, &self.root, viewer, id,
                )?)?)
            }
            "pact_doctor" => Ok(serde_json::to_value(doctor::checks(&self.root))?),
            "pact_events_tail" => {
                // Clamped, not refused: a model that asks for 10_000 wants "as
                // much as I can have", and an error teaches it nothing it can
                // act on. The schema states the ceiling.
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map_or(EVENTS_DEFAULT, |n| (n as usize).clamp(1, EVENTS_MAX));
                Ok(serde_json::to_value(events::recent(&self.root, limit)?)?)
            }
            // Unreachable: `tools_call` checked the name against TOOLS.
            other => Err(anyhow::anyhow!("unknown tool \"{other}\"")),
        }
    }
}

/// Serve MCP on the real stdin/stdout. Returns the process exit code.
pub fn serve(root: PathBuf) -> Result<i32> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // Announced on stderr, which the spec reserves for exactly this and which
    // clients are told not to read as failure. It must never go to stdout: one
    // non-message byte there and the client's parser is looking at garbage.
    output::warn(&format!(
        "pact mcp: read-only server on stdio, {} tools, repo {}",
        TOOLS.len(),
        root.display()
    ));
    Server::new(root).run(stdin.lock(), &mut stdout)
}

fn tools_list() -> Value {
    json!({
        "tools": TOOLS
            .iter()
            .map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// A successful tool result: the JSON both as text and as `structuredContent`.
///
/// Both, because the spec says a tool returning structured content SHOULD also
/// return the serialized JSON in a text block — clients on revisions before
/// `structuredContent` existed only read `content`, and a model reading either
/// one gets the same answer.
fn tool_result(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|e| {
        // Cannot happen for values we just built out of serde_json, and a panic
        // here would take down a server the client cannot see the stderr of.
        format!("{{\"error\": \"failed to serialize result: {e}\"}}")
    });
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false,
    })
}

fn tool_error(message: &str, exit_code: i32) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": format!("{message}\n\n(pact would exit {exit_code} for this; \
                 3 means no Beads CLI on PATH, 4 not a git repository)"),
        }],
        "isError": true,
    })
}

fn ok_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// A boolean argument, absent meaning false.
fn flag(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn required_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("\"{key}\" is required and must be a non-empty string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new(PathBuf::from("/nonexistent-repo-for-framing-tests"))
    }

    /// Drive one line through and parse the response.
    fn respond(line: &str) -> Value {
        let raw = server().handle(line).expect("expected a response");
        serde_json::from_str(&raw).expect("response must be valid JSON")
    }

    #[test]
    fn initialize_answers_with_our_capabilities() {
        let r = respond(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        );
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "pact");
        assert_eq!(
            r["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(r["result"]["capabilities"]["tools"].is_object());
        // The one fact the model must have.
        let instructions = r["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("read-only"));
        assert!(instructions.contains("does NOT mark it read"));
    }

    /// The negotiation rule: echo a version we know, otherwise answer with ours.
    #[test]
    fn initialize_negotiates_the_protocol_version() {
        for known in KNOWN_PROTOCOL_VERSIONS {
            let r = respond(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{known}"}}}}"#
            ));
            assert_eq!(r["result"]["protocolVersion"], *known);
        }
        for unknown in ["1.0.0", "2026-07-28", ""] {
            let r = respond(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{unknown}"}}}}"#
            ));
            assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
        }
        // Absent params must not panic.
        let r = respond(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_reports_the_five_tools_with_schemas() {
        let r = respond(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = r["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "pact_lease_list",
                "pact_msg_inbox",
                "pact_msg_thread",
                "pact_doctor",
                "pact_events_tail"
            ]
        );
        for t in tools {
            // Every schema must be a valid JSON Schema *object* — the spec is
            // explicit that inputSchema must not be null.
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            let description = t["description"].as_str().unwrap();
            // The constraint this whole module exists under, asserted per tool
            // so a sixth tool cannot be added without saying it.
            assert!(
                description.starts_with("Read-only."),
                "{} does not say it is read-only",
                t["name"]
            );
        }
    }

    /// The trap, as a test: both message tools must warn that looking is not
    /// reading, because that is the one way this server could mislead a caller
    /// into a decision (not re-sending) that costs something.
    #[test]
    fn message_tools_document_that_they_do_not_mark_read() {
        for name in ["pact_msg_inbox", "pact_msg_thread"] {
            let tool = TOOLS.iter().find(|t| t.name == name).unwrap();
            assert!(
                tool.description.contains("does NOT mark"),
                "{name} must tell the caller it does not mark messages read"
            );
            assert!(
                tool.description.contains("read-by-"),
                "{name} must name the label, so the caller knows what is not written"
            );
        }
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let r = respond(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resources/list"));
        assert!(r["result"].is_null());
    }

    /// `server/discover` is the modern era's mandatory probe. Answering
    /// method-not-found is what tells a dual-era client to fall back to
    /// `initialize`, so this is load-bearing rather than incidental.
    #[test]
    fn server_discover_is_rejected_so_clients_fall_back() {
        let r = respond(r#"{"jsonrpc":"2.0","id":4,"method":"server/discover","params":{}}"#);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_null_id() {
        let r = respond("{not json at all");
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert_eq!(r["id"], Value::Null);
    }

    #[test]
    fn a_non_object_message_is_an_invalid_request() {
        let r = respond("[1, 2, 3]");
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn a_request_without_a_method_is_an_invalid_request() {
        let r = respond(r#"{"jsonrpc":"2.0","id":7}"#);
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
    }

    /// Notifications get no response at all — answering one is a protocol
    /// violation, and `notifications/initialized` is sent by every client.
    #[test]
    fn notifications_are_never_answered() {
        for line in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            r#"{"jsonrpc":"2.0","method":"some/unknown/notification"}"#,
        ] {
            assert!(
                server().handle(line).is_none(),
                "answered a notification: {line}"
            );
        }
    }

    /// A null id is a request, not a notification, and must be answered.
    #[test]
    fn an_explicit_null_id_still_gets_a_response() {
        let r = respond(r#"{"jsonrpc":"2.0","id":null,"method":"tools/list"}"#);
        assert_eq!(r["id"], Value::Null);
        assert!(r["result"]["tools"].is_array());
    }

    #[test]
    fn unknown_tool_is_a_protocol_error_not_a_tool_error() {
        let r = respond(
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"pact_lease_acquire","arguments":{}}}"#,
        );
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pact_lease_acquire"));
    }

    #[test]
    fn a_missing_tool_name_is_invalid_params() {
        let r = respond(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{}}"#);
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn non_object_arguments_are_invalid_params() {
        let r = respond(
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"pact_doctor","arguments":"nope"}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    /// A missing required argument is a TOOL error, not a protocol error: the
    /// model can fix it by adding the argument, and the spec puts input
    /// validation failures on the `isError` side for exactly that reason.
    #[test]
    fn a_missing_required_argument_is_a_tool_error() {
        let r = respond(
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"pact_msg_inbox","arguments":{}}}"#,
        );
        assert!(
            r["error"].is_null(),
            "should be a result, not a protocol error"
        );
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent"));
    }

    #[test]
    fn events_limit_is_clamped_not_refused() {
        // The repo path does not exist, so `recent` returns an empty list rather
        // than failing — which is what makes this a clean test of the clamp.
        for limit in ["1", "50", "99999"] {
            let r = respond(&format!(
                r#"{{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{{"name":"pact_events_tail","arguments":{{"limit":{limit}}}}}}}"#
            ));
            assert_eq!(r["result"]["isError"], false, "limit {limit}");
            assert!(r["result"]["structuredContent"].is_array());
        }
    }

    #[test]
    fn a_successful_result_carries_both_text_and_structured_content() {
        let r = respond(
            r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"pact_events_tail"}}"#,
        );
        assert_eq!(r["result"]["content"][0]["type"], "text");
        // The text block must be the serialized structured content, per the
        // spec's backwards-compatibility guidance.
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let reparsed: Value = serde_json::from_str(text).expect("text block must be the JSON");
        assert_eq!(reparsed, r["result"]["structuredContent"]);
    }

    /// Framing, end to end: several messages in, one response line each, and a
    /// clean exit 0 when the stream ends. This is the loop `serve` runs on the
    /// real stdio.
    #[test]
    fn the_loop_answers_line_per_line_and_exits_zero_on_eof() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            "\n", // a stray blank line must not produce output
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut out = Vec::new();
        let code = server()
            .run(std::io::BufReader::new(input.as_bytes()), &mut out)
            .expect("run");
        assert_eq!(code, 0, "stdin EOF must be a clean exit");

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per request, none for the rest");
        for line in &lines {
            let v: Value = serde_json::from_str(line).expect("each line is one message");
            assert_eq!(v["jsonrpc"], "2.0");
            // The framing rule: no message may contain an embedded newline.
            assert!(!line.contains('\n'));
        }
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["result"]["protocolVersion"],
            PROTOCOL_VERSION
        );
    }

    /// Descriptions are shown to a model, so they must not lie about the CLI
    /// they describe. Each tool names the command it mirrors; a rename that did
    /// not update the text would make the tool's own advice unfollowable.
    #[test]
    fn every_tool_names_the_cli_command_it_mirrors() {
        for (name, command) in [
            ("pact_lease_list", "pact lease ls"),
            ("pact_msg_inbox", "pact msg inbox"),
            ("pact_msg_thread", "pact msg read"),
            ("pact_events_tail", "pact log"),
        ] {
            let tool = TOOLS.iter().find(|t| t.name == name).unwrap();
            assert!(
                tool.description.contains(command),
                "{name} should point at `{command}`"
            );
        }
    }
}
