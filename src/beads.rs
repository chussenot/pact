//! Subprocess adapter over the Beads CLI (`bd`: Go, embedded Dolt).
//! Never reads/writes the Beads database or JSONL directly; always shells out.
//!
//! [`BeadsCli::locate`] is more than `which("bd")`, because the store on disk —
//! not the working directory, and not a preference — decides what pact may talk
//! to. `bd init` lays down `.beads/embeddeddolt/`. A tool that reports "inbox
//! empty" because it opened the wrong database is worse than one that is missing,
//! so the workspace is classified first and the candidate list is one binary long.
//!
//! pact supported a second backend, `br` (beads-rust, SQLite), through 0.7.9.
//! The two never shared a store, and — more importantly — never shared the same
//! guarantees: `br` had no `--id`/`--force` equivalent, so a replayed `msg send`
//! duplicated the message there while the protocol told agents to re-send when
//! they could not confirm one. A `br` workspace is still *detected*, so it can be
//! refused with an accurate message rather than a misleading "CLI not found".
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result};

use crate::otel;
use crate::output::exit_with;

const TESTED_BD_MIN: (u64, u64, u64) = (1, 1, 0);
/// Widened to the 1.2 line after pact-0re, which is what testing against 1.2.1
/// cost: bd 1.2 dropped `create --id --force`'s upsert and refuses with "already
/// exists" instead, and `bd init` now declines to touch a legacy SQLite (br)
/// workspace at all. Both are handled, and the handling is version-agnostic —
/// 1.1's upsert still takes the success path — so the floor stays at 1.1.0.
const TESTED_BD_MAX_EXCLUSIVE: (u64, u64, u64) = (1, 3, 0);

/// Default ceiling on how long [`BeadsCli::run`] waits for the child before
/// treating it as hung, overridable via `PACT_BEADS_TIMEOUT_SECS` — the same
/// env-var-configurable-behaviour convention as `PACT_STATE_DIR` and
/// `PACT_WORKTREE_SCOPE`. Also what makes the timeout testable at all: a test
/// can shrink it to a second instead of waiting out a real 30.
const DEFAULT_BEADS_TIMEOUT_SECS: u64 = 30;

/// How often [`BeadsCli::run`] polls the child for exit while it waits. Short
/// enough that the timeout is honoured to within a fraction of a second,
/// nowhere near frequent enough to be the ~10x/second budget `tui.rs` guards.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

fn beads_timeout() -> std::time::Duration {
    let secs = std::env::var("PACT_BEADS_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BEADS_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

pub struct BeadsCli {
    // pub(crate) so tui.rs's tests can construct one directly without a real
    // `bd` on PATH, instead of adding a test-only constructor for one field.
    // It stays the ONLY field on purpose: `is_br()` derives the backend from
    // it, so adding br support did not have to touch the struct literals in
    // main.rs, tui.rs and agents.rs, which br-dev does not own.
    pub(crate) binary: &'static str,
}

/// The directory a Beads subprocess must run in: the main worktree when this is
/// a linked one, otherwise the checkout itself.
///
/// Exit 3 for the bare-repository topology, reusing "backend unavailable"
/// rather than inventing a code — there genuinely is no Beads workspace to talk
/// to, and the alternative is letting `bd` create one in whatever directory
/// happened to be current. A store nobody can find again is worse than a clear
/// refusal, and the exit code an agent branches on is unchanged.
fn beads_root(repo_root: &Path) -> Result<PathBuf> {
    let ctx = crate::repo::RepoContext::resolve(repo_root);
    if ctx.is_bare_topology() {
        return Err(exit_with(
            3,
            format!(
                "no Beads workspace: {} is a worktree of a BARE repository, so there is no main \
                 checkout to hold `.beads/`. Leases and the event log work (state is under {}), \
                 messaging does not. Add a normal worktree and run the Beads CLI there, or use a \
                 non-bare clone for message traffic.",
                ctx.worktree_root.display(),
                ctx.state_dir.display()
            ),
        ));
    }
    Ok(ctx.shared_root)
}

/// Which Beads CLI an existing `.beads/` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workspace {
    Bd,
    /// A br (beads-rust, SQLite) store. **Detected but no longer supported** —
    /// see [`missing_backend_message`]. It stays a distinct state precisely
    /// because `br` will still be on the user's PATH, so collapsing it into
    /// `None` would answer "no Beads CLI found" while the CLI sits right there.
    Br,
    /// No `.beads/` above the cwd at all — nothing to be compatible with yet.
    None,
}

impl BeadsCli {
    /// Locate a Beads CLI on PATH, preferring the one that can actually read
    /// this repo's workspace. Exit code 3 if the needed binary is not found.
    pub fn locate() -> Result<Self> {
        // cwd, not the git root: `repo::find_repo_root` is not reachable from
        // here without changing this signature, and every caller already runs
        // inside the repo. A `.beads/` is found by walking up regardless.
        // Detect from the store the commands will actually use, not from the
        // cwd: in a linked worktree there is no `.beads/` to walk up to (the
        // worktree usually sits outside the main checkout), so cwd-based
        // detection would report `None` and fall through to a *preference* —
        // picking `br` for a repository whose data is in `bd`'s Dolt store.
        // Best-effort: any resolution failure keeps the old cwd behaviour.
        let detect_from = std::env::current_dir().ok().and_then(|cwd| {
            let root = crate::repo::find_repo_root(&cwd).ok()?;
            beads_root(&root).ok().or(Some(cwd))
        });
        let workspace = detect_workspace(detect_from.as_deref());
        for binary in candidates(workspace) {
            if which(binary).is_some() {
                return Ok(BeadsCli { binary });
            }
        }
        Err(exit_with(3, missing_backend_message(workspace)))
    }

    pub fn binary(&self) -> &'static str {
        self.binary
    }

    /// `bd --version` output, trimmed, for `pact doctor`.
    pub fn version(&self, repo_root: &Path) -> Result<String> {
        Ok(self.run(repo_root, &["--version"])?.trim().to_string())
    }

    /// Run `bd <args>` in `repo_root`, capturing stdout; the backend's own
    /// reason is surfaced on failure, from whichever stream it used.
    ///
    /// This is the only place pact spawns a Beads process, so it is also the
    /// only place that has to be instrumented (pact-aw7.6): every message
    /// command shells out at least once, the TUI's Messages tab once did it per
    /// refresh tick, and not turning that into ten subprocesses a second was the
    /// single hardest constraint in the mascot feature — measured by nothing.
    ///
    /// Bounded by [`beads_timeout`] (`PACT_BEADS_TIMEOUT_SECS`, default 30s): a
    /// child that never exits — wedged on a TTY/credential prompt, an internal
    /// bug, a backend write-lock — used to hang this call, and everything built
    /// on it, forever. Past the deadline the child is killed and this returns
    /// exit 3, the same "backend unavailable" code `beads_root` already uses
    /// for the bare-repository topology: a hung subprocess is the same class of
    /// problem as no Beads workspace to talk to.
    pub fn run(&self, repo_root: &Path, args: &[&str]) -> Result<String> {
        // One Beads store per repository, not per checkout. A linked worktree
        // has no `.beads/` of its own, so running the backend in the caller's
        // directory would make `msg send` from worktree A invisible to `msg
        // inbox` in worktree B — or, worse, let `bd` initialise a second empty
        // store in the worktree and report an empty inbox, which is the exact
        // failure this module's header is about. Resolved here because this is
        // the only place pact spawns a backend.
        let repo_root = &beads_root(repo_root)?;
        let shape = argv_shape(args);
        let mut sp = otel::span("pact.beads.exec");
        // `process.executable.name` and `process.exit.code` are the registry
        // names for exactly this. `process.command_args` is NOT used and must
        // not be: `--title=` and `--description=` carry the message subject and
        // body, and shipping a colleague's prose to a collector is not a thing
        // an observability change gets to do quietly.
        sp.set("process.executable.name", self.binary);
        sp.set("pact.beads.argv_shape", shape.join(" "));
        sp.set("pact.beads.subcommand", subcommand(&shape).to_string());
        if let Some(v) = BACKEND_VERSION.get() {
            sp.set("pact.beads.version", v.clone());
        }
        let started = std::time::Instant::now();

        // `spawn`, not `output`, because `output` blocks until the child exits
        // with no way to give up early. Stdin closed to match `output`'s own
        // behaviour (a prompt reading it gets immediate EOF rather than our
        // terminal). Stdout/stderr are drained on their own threads rather than
        // read after the fact, so a chatty child cannot fill a pipe buffer and
        // block while we are merely sleeping between polls below — the same
        // problem `output` avoids internally, reimplemented here because we
        // also need to poll for a timeout.
        let mut child = match Command::new(self.binary)
            .args(args)
            .current_dir(repo_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {} {:?}", self.binary, args))
        {
            Ok(c) => c,
            Err(e) => {
                sp.fail("spawn");
                self.record(&shape, started, "spawn");
                return Err(e);
            }
        };

        let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
        let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf);
            let _ = stdout_tx.send(buf);
        });
        let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf);
            let _ = stderr_tx.send(buf);
        });

        let timeout = beads_timeout();
        let deadline = started + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(POLL_INTERVAL);
                }
                Ok(None) => {
                    // Past the deadline: kill and reap so no zombie is left
                    // behind, then report it the same way `beads_root` reports
                    // "no Beads workspace" — exit 3, reusing "backend
                    // unavailable" rather than inventing a code.
                    let _ = child.kill();
                    let _ = child.wait();
                    sp.fail("timeout");
                    self.record(&shape, started, "timeout");
                    return Err(exit_with(
                        3,
                        format!(
                            "{} {:?} did not finish within {}s (PACT_BEADS_TIMEOUT_SECS) and was \
                             killed; the Beads backend may be hung on a prompt, an internal bug, \
                             or a write lock",
                            self.binary,
                            args,
                            timeout.as_secs()
                        ),
                    ));
                }
                Err(e) => {
                    sp.fail("wait");
                    self.record(&shape, started, "wait");
                    return Err(anyhow::Error::from(e)
                        .context(format!("waiting on {} {:?}", self.binary, args)));
                }
            }
        };
        let stdout = stdout_rx.recv().unwrap_or_default();
        let stderr = stderr_rx.recv().unwrap_or_default();

        // `.code()` is None when a signal killed it, which is not an exit code
        // and must not be faked as one.
        if let Some(code) = status.code() {
            sp.set("process.exit.code", i64::from(code));
        }
        if !status.success() {
            let outcome = if status.code().is_some() {
                "exit"
            } else {
                "signal"
            };
            sp.fail(outcome);
            self.record(&shape, started, outcome);
            anyhow::bail!(
                "{} {:?} failed ({}): {}",
                self.binary,
                args,
                status,
                failure_reason(
                    &String::from_utf8_lossy(&stdout),
                    &String::from_utf8_lossy(&stderr),
                )
            );
        }
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        // Learn the version for free from the one call that already asks for
        // it, rather than probing. See BACKEND_VERSION. `set` returning Ok
        // means we are the call that learned it, and that call has to carry the
        // attribute itself: measured on `pact doctor`, the `--version` span was
        // the *only* beads span in the whole process, so a version attached
        // only to later spans would never have been exported at all.
        if args == ["--version"] && BACKEND_VERSION.set(stdout.trim().to_string()).is_ok() {
            sp.set("pact.beads.version", stdout.trim().to_string());
        }
        self.record(&shape, started, "ok");
        Ok(stdout)
    }

    /// The aggregate behind the span: how much wall clock a pact command spends
    /// waiting on Beads, and how many spawns it took. Dimensions are the
    /// backend, the subcommand and a three-valued outcome — all bounded, none
    /// derived from a path or an issue id, because a metric label is where
    /// unbounded cardinality actually hurts.
    fn record(&self, shape: &[&str], started: std::time::Instant, outcome: &'static str) {
        let attrs = otel::attrs![
            "process.executable.name" => self.binary,
            "pact.beads.subcommand" => subcommand(shape).to_string(),
            "pact.outcome" => outcome,
        ];
        otel::record_ms(
            "pact.beads.duration",
            started.elapsed().as_secs_f64() * 1000.0,
            &attrs,
        );
    }
}

/// The backend's version string, learned for free the one time something asks
/// the backend for it (`pact doctor`), and reused by every later span in the
/// same process.
///
/// Deliberately never probed on demand: spawning `bd --version` to decorate a
/// span would double the exact subprocess cost the span exists to measure, and
/// telemetry that changes what it observes is worse than no telemetry. A trace
/// with no `pact.beads.version` means nothing in that command needed it.
static BACKEND_VERSION: OnceLock<String> = OnceLock::new();

/// The first token of the shape: `create`, `label`, `--version`. Bounded by the
/// call sites in `msg.rs`, which is what makes it safe as a metric dimension.
fn subcommand<'a>(shape: &[&'a str]) -> &'a str {
    shape.first().copied().unwrap_or("")
}

/// argv reduced to its *shape*: keep the flag names, keep the leading verbs,
/// drop everything else.
///
/// The rule is deliberately paranoid rather than clever, because the thing on
/// the other side of a mistake is a collector holding somebody's message body:
///
/// - `--title=<subject>` and `--description=<body>` are how `msg send` passes
///   user prose, so a flag is truncated at its `=` and only the name survives.
/// - a positional is dropped outright. In practice they are issue ids
///   (`show <id>… --json`) and `read-by-<agent>` labels — not free text, but
///   unbounded, and an id tells you nothing about the shape of the call.
/// - the exception is the leading verb chain (`list`, `label add`), capped at
///   two tokens of pure lowercase ASCII. An id (`pact-aw7.6`) or any prose
///   fails that test, so the chain stops at the first thing that is not
///   obviously a subcommand.
fn argv_shape<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    let mut in_verbs = true;
    for arg in args {
        if arg.starts_with('-') {
            in_verbs = false;
            out.push(arg.split('=').next().unwrap_or(arg));
        } else if in_verbs && out.len() < 2 && is_verb(arg) {
            out.push(arg);
        } else {
            in_verbs = false;
        }
    }
    out
}

fn is_verb(token: &str) -> bool {
    !token.is_empty() && token.len() <= 16 && token.bytes().all(|b| b.is_ascii_lowercase())
}

/// Why the backend said it failed. Reading stderr alone was correct only while
/// bd was the only backend: bd prints `Error: …` to stderr, but br leaves
/// stderr empty and puts a JSON envelope on **stdout**, so every br failure
/// reached the user as `br [...] failed (exit status: 2): ` — nothing after the
/// colon, and the message *and* the actionable hint the backend supplied thrown
/// away. A diagnostic that lies about the thing you ran it to diagnose.
fn failure_reason(stdout: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = stdout.trim();
    // br: {"error":{"code":…,"message":…,"hint":…}}. The hint is the half that
    // tells you what to do about it ("Run: br init"), so it is kept, not summarised.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(message) = v.pointer("/error/message").and_then(|m| m.as_str()) {
            return match v.pointer("/error/hint").and_then(|h| h.as_str()) {
                Some(hint) if !hint.is_empty() => format!("{message} ({hint})"),
                _ => message.to_string(),
            };
        }
    }
    stdout.to_string()
}

/// The binaries to try, in order, for a given workspace. An existing store is
/// not a preference, it is a constraint: exactly one binary can read it, so the
/// list is one long and a missing binary becomes an honest exit 3 rather than a
/// silent fallback onto the other backend's empty database.
fn candidates(workspace: Workspace) -> &'static [&'static str] {
    match workspace {
        Workspace::Bd => &["bd"],
        // Nothing can serve a br workspace any more, so `locate` falls straight
        // through to the refusal rather than offering a binary that would read
        // the wrong store.
        Workspace::Br => &[],
        Workspace::None => &["bd"],
    }
}

fn missing_backend_message(workspace: Workspace) -> String {
    match workspace {
        Workspace::Bd | Workspace::None => "bd (beads) not found on PATH — install it: \
             https://github.com/gastownhall/beads"
            .to_string(),
        // Not "no Beads CLI found": `br` is almost certainly installed and
        // working, and this repository's data really is in its store. Saying the
        // CLI is missing would send the reader off to install something they
        // already have. What changed is pact, so pact says so, names the last
        // version that worked, and gives both ways out.
        Workspace::Br => "this repo's .beads/ is a br (beads-rust, SQLite) workspace, and \
             pact no longer supports br — pact 0.7.9 was the last release that did.\n\
             \n\
             Either migrate the store to bd (https://github.com/gastownhall/beads), or pin \
             pact 0.7.9 if you need br. pact has not touched your .beads/ and will not."
            .to_string(),
    }
}

/// Walk up from `start` for the first `.beads/` and read which backend made it.
fn detect_workspace(start: Option<&Path>) -> Workspace {
    let Some(start) = start else {
        return Workspace::None;
    };
    for dir in start.ancestors() {
        let beads = dir.join(".beads");
        if beads.is_dir() {
            return classify_workspace(&beads);
        }
    }
    Workspace::None
}

fn classify_workspace(beads_dir: &Path) -> Workspace {
    // Dolt first, deliberately: a bd repo that has picked up a stray
    // `beads.db` — which is exactly what one accidental `br init` leaves
    // behind — must keep answering as bd, because that is where the data is.
    if beads_dir.join("embeddeddolt").is_dir() {
        return Workspace::Bd;
    }
    if sqlite_db(beads_dir).is_some() {
        return Workspace::Br;
    }
    Workspace::None
}

/// Both backends' stores sitting in one `.beads/`, as `(used, ignored)`.
///
/// `classify_workspace` resolves this correctly — Dolt first, because that is
/// where the data is — and that correct tiebreak is exactly what makes the
/// situation invisible. A repo can carry an empty `beads.db` shadowing a full
/// `embeddeddolt/` and every pact command keeps answering normally.
///
/// It is not hypothetical: `br --db /tmp/elsewhere.db init` ignored its own
/// `--db` and initialised in the cwd of a live repo, at exit 0, with no
/// warning, leaving a second database next to the real one. Nothing leases the
/// Beads store and nothing checked it, so an agent that had correctly leased
/// both files it edited still wrote shared state no lease covered.
impl BeadsCli {
    /// Whether this backend accepts `--actor`, asked by running it rather than
    /// keyed off a version number.
    ///
    /// Attribution is the difference between an audit trail that answers "who did
    /// this" and one that says the human owns every bead in a fleet of twenty
    /// agents. Both backends support the flag today — `bd` 1.1.2 documents
    /// precedence `--actor` > `$BEADS_ACTOR` > `git user.name` > `$USER`, and `br`
    /// 0.2.19 accepts `--actor <ACTOR>` — so this exists to notice if that stops
    /// being true, not because it is currently in doubt.
    ///
    /// NOT cached (pact-m7j.9.9) — re-run on every call, the same freedom
    /// [`version`](Self::version) already has and for the same reason: an
    /// ordinary CLI invocation is a fresh process either way, but `pact mcp
    /// serve` and `pact ui` are long-lived, and a process-lifetime cache
    /// answered a `bd`/`br` swapped mid-session with whatever the FIRST call
    /// saw, forever. `pact doctor`'s "Beads CLI" check is the one caller today,
    /// so the accepted cost is one extra `bd/br create --help` subprocess per
    /// doctor invocation. A failure to run is reported as unsupported, which is
    /// the safe direction — it understates rather than promising attribution
    /// pact cannot deliver.
    pub fn supports_actor(&self, repo_root: &Path) -> bool {
        Command::new(self.binary)
            .args(["create", "--help"])
            .current_dir(repo_root)
            .output()
            .map(|o| {
                let help = String::from_utf8_lossy(&o.stdout);
                let err = String::from_utf8_lossy(&o.stderr);
                help.contains("--actor") || err.contains("--actor")
            })
            .unwrap_or(false)
    }
}

/// Who a bead is assigned to, or `None` for every "cannot tell": no such bead, no
/// backend, an unparseable answer, or an empty assignee.
///
/// Every one of those must be indistinguishable from "not assigned", because the
/// only caller warns on the answer (pact-mqw.4) and a warning produced by a
/// missing backend is a warning agents learn to ignore. Three bd outages happened
/// in the crucible run alone.
///
/// One subprocess, and the only thing it reads is the assignee field. `show` is
/// used rather than `list --assignee=` because the question is about a known id,
/// not a search.
pub fn assignee_of(cli: &BeadsCli, repo_root: &Path, id: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Assigned {
        #[serde(default)]
        assignee: Option<String>,
    }
    let out = cli.run(repo_root, &["show", id, "--json"]).ok()?;
    // Both backends return an array even for one id.
    let issues: Vec<Assigned> = serde_json::from_str(&out).ok()?;
    let who = issues.into_iter().next()?.assignee?;
    (!who.trim().is_empty()).then_some(who)
}

/// The first bead id in a free-text note, if it holds one.
///
/// Deliberately narrow: `<prefix>-<three alnum>` plus optional `.N` suffixes,
/// which is the shape both bd and br generate (`pact-mqw`, `pact-mqw.4`,
/// `crucible-2o3.27`). Ordinary hyphenated prose does not match, because its
/// second component is almost never exactly three characters — and the few false
/// positives that do slip through (`pact-msg` inside a message id) resolve to no
/// bead and are silent, which is the safe direction.
///
/// The FIRST match only, so one acquire costs at most one backend lookup however
/// chatty the note is.
pub fn bead_id_in(note: &str) -> Option<&str> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.';
    for (start, _) in note.char_indices() {
        // Only at a token boundary, or "impact-abc" would yield "pact-abc".
        if start > 0 && is_word(note[..start].chars().next_back().unwrap_or(' ')) {
            continue;
        }
        let rest = &note[start..];
        let token: &str = rest.split(|c: char| !is_word(c)).next().unwrap_or("");
        if let Some(id) = looks_like_bead_id(token) {
            return Some(id);
        }
    }
    None
}

/// `<prefix>-<exactly three alnum>(.N)*`, and nothing else.
fn looks_like_bead_id(token: &str) -> Option<&str> {
    let (prefix, tail) = token.rsplit_once('-')?;
    if prefix.is_empty() || !prefix.starts_with(|c: char| c.is_ascii_lowercase()) {
        return None;
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let mut parts = tail.split('.');
    let base = parts.next()?;
    if base.len() != 3 || !base.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    // Trailing `.N` segments only; anything else means this is prose that happened
    // to end in three characters, like a sentence ending in "-abc.".
    let mut end = prefix.len() + 1 + base.len();
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        end += 1 + part.len();
    }
    Some(&token[..end])
}

pub fn conflicting_stores(repo_root: &Path) -> Option<(String, String)> {
    let beads = repo_root.join(".beads");
    let dolt = beads.join("embeddeddolt").is_dir();
    let sqlite = sqlite_db(&beads)?;
    if !dolt {
        return None;
    }
    let name = sqlite.file_name()?.to_string_lossy().into_owned();
    Some((
        ".beads/embeddeddolt/ (bd)".to_string(),
        format!(".beads/{name} (br)"),
    ))
}

/// The sentence [`conflicting_stores`] is worth, worded once so every caller
/// that needs to say it says it identically (pact-m7j.10.7). `doctor.rs`
/// originated this wording; `run_msg` (main.rs) and the MCP message tools
/// reuse it rather than inventing their own phrasing for the same fact — a
/// second, ignored store sitting next to the one pact actually queries.
pub fn conflict_warning(repo_root: &Path) -> Option<String> {
    let (used, ignored) = conflicting_stores(repo_root)?;
    Some(format!(
        "two stores in .beads/ — pact uses {used} and ignores {ignored}; \
         remove the one you do not want, or the backends will disagree"
    ))
}

/// Every distinct, non-empty `actor` recorded in `.beads/interactions.jsonl`
/// (pact-juz.4), or `None` if the file does not exist at all — distinct from
/// an empty `Vec`, which means the file exists but recorded nobody.
///
/// A plain file read, not a bypass of "never touch the Beads DB directly"
/// (CLAUDE.md): that rule is about live transactional state (message
/// read-labels, issue status) that must only ever be asked through `bd`/`br`
/// so a concurrent writer is never missed. `interactions.jsonl` is the
/// opposite shape — an append-only, already-committed audit log
/// (docs/architecture.md documents its exact JSON), the same kind of file
/// `.pact/events.jsonl` is on pact's own side. Reading it is a diagnostic
/// question, not a state mutation, and it is read here rather than through a
/// `bd`/`br` subprocess because neither CLI exposes "list every actor that
/// has ever acted" as a query — this is the one source that has it.
///
/// Whether `br` writes this same file has not been confirmed (the bead this
/// exists for only had a `bd` repo to check against) — absence is read as
/// "not applicable", never as "zero actors", so a `br`-only repo where this
/// file simply does not exist reports cleanly rather than falsely.
pub fn interaction_actors(repo_root: &Path) -> Option<Vec<String>> {
    let contents = std::fs::read_to_string(repo_root.join(".beads/interactions.jsonl")).ok()?;
    let mut actors: Vec<String> = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| v.get("actor")?.as_str().map(str::to_string))
        .filter(|a| !a.is_empty())
        .collect();
    actors.sort();
    actors.dedup();
    Some(actors)
}

/// Current assignee per bead, reconstructed by replaying `.beads/interactions.jsonl`
/// (pact-as5.6) — the offline answer to what [`assignee_of`] asks a subprocess.
///
/// Read under exactly the same licence as [`interaction_actors`], for the same
/// reasons: one already-committed append-only audit log, read-only, best-effort,
/// never the Beads DB. Absent, empty or wholly unparseable file, or one with no
/// assignee rows at all, all give an empty map — the caller reports "no beads
/// data" and passes. A single malformed line is skipped, never fatal.
///
/// Rows are replayed in `created_at` order and the last `new_value` wins; an empty
/// `new_value` means unassigned, which drops the bead from the map rather than
/// recording `""`.
///
/// **The honest limits, all three, because a check that quietly got weaker is worse
/// than one that says so.** This is strictly LESS sensitive than asking `bd show`:
/// it finds fewer divergences, never more, and never a false one.
///
/// 1. The export records CHANGES. A bead assigned at creation and never reassigned
///    has no `field=assignee` row and cannot be resolved at all. Measured in this
///    repo: 5 of 264 rows are assignee changes, 257 are status.
/// 2. **The sidecar is opt-in.** bd writes `interactions.jsonl` only when
///    `audit.enabled: true` is set in `.beads/config.yaml`, and it is off by
///    default — verified against bd 1.2 by running `bd update --assignee` with and
///    without it. So in a default bd repository the file never appears and every
///    caller reports "no beads data" forever.
/// 3. Even enabled, it is a committed export and lags the live store, so an
///    assignment made in the current session may not be visible yet.
///
/// Those are the price of needing no subprocess. The live cross-check at
/// `lease acquire` time still asks [`assignee_of`], where the answer is current.
pub fn interaction_assignees(repo_root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(contents) = std::fs::read_to_string(repo_root.join(".beads/interactions.jsonl")) else {
        return out;
    };
    let mut rows: Vec<(String, String, String)> = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["extra"]["field"].as_str() == Some("assignee"))
        .filter_map(|v| {
            Some((
                v["created_at"].as_str().unwrap_or_default().to_string(),
                v["issue_id"].as_str()?.to_string(),
                v["extra"]["new_value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect();
    // The export is append-ordered already; sorting costs one line and survives a
    // merge of two branches' logs, which append-order does not.
    rows.sort();
    for (_, issue, who) in rows {
        if who.trim().is_empty() {
            out.remove(&issue);
        } else {
            out.insert(issue, who);
        }
    }
    out
}

fn sqlite_db(beads_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(beads_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        // Not `extension() == "db"` on the whole read_dir: br also writes
        // `<name>.db-wal` / `-shm` siblings, which must not count on their own.
        .find(|p| p.is_file() && p.extension().is_some_and(|e| e == "db"))
}

/// Warning text for versions outside pact's tested range for that backend.
pub fn version_compat_warning(version_output: &str) -> Option<String> {
    let (min, max) = (TESTED_BD_MIN, TESTED_BD_MAX_EXCLUSIVE);
    let parsed = parse_triplet(version_output)?;
    if parsed >= min && parsed < max {
        None
    } else {
        Some(format!(
            "outside tested range {}.{}.{} <= version < {}.{}.{}",
            min.0, min.1, min.2, max.0, max.1, max.2
        ))
    }
}

fn parse_triplet(s: &str) -> Option<(u64, u64, u64)> {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|p| !p.is_empty())
        .find_map(|part| {
            let mut it = part.split('.');
            let major = it.next()?.parse().ok()?;
            let minor = it.next()?.parse().ok()?;
            let patch = it.next()?.parse().ok()?;
            if it.next().is_some() {
                return None;
            }
            Some((major, minor, patch))
        })
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one that matters: `msg send`'s real argv carries the subject and the
    /// body, and neither may reach a collector (pact-aw7.6). Verbatim from
    /// `msg::create_args`.
    #[test]
    fn the_argv_shape_keeps_flag_names_and_drops_every_value() {
        let args = [
            "create",
            "--type=message",
            "--json",
            "--no-inherit-labels",
            "--title=lease handoff on src/tui.rs",
            "--description=I am done with the refresh loop, it is yours",
            "--assignee=integrator",
            "--actor=beads-span",
            "--parent=pact-aw7.6",
        ];
        let shape = argv_shape(&args).join(" ");
        assert_eq!(
            shape,
            "create --type --json --no-inherit-labels --title --description \
             --assignee --actor --parent"
        );
        for leaked in ["lease handoff", "refresh loop", "integrator", "pact-aw7.6"] {
            assert!(!shape.contains(leaked), "{leaked:?} leaked into {shape:?}");
        }
    }

    /// The other shapes `msg.rs` produces. Ids and `read-by-<agent>` labels are
    /// positionals: bounded-ish, but nothing about the shape of the call, and
    /// unbounded as a metric dimension.
    #[test]
    fn positionals_are_dropped_but_a_two_word_subcommand_survives() {
        let shape = |a: &[&str]| argv_shape(a).join(" ");
        assert_eq!(
            shape(&["list", "--include-infra", "--json", "--assignee=alice"]),
            "list --include-infra --json --assignee"
        );
        assert_eq!(
            shape(&["show", "pact-aw7.6", "pact-l94", "--json"]),
            "show --json"
        );
        assert_eq!(
            shape(&["label", "add", "pact-aw7.6", "read-by-beads-span"]),
            "label add"
        );
        assert_eq!(shape(&["--version"]), "--version");
        assert_eq!(shape(&[]), "");
        // A verb chain stops at the first token that is not obviously one, so a
        // hypothetical positional title cannot ride in as a subcommand.
        assert_eq!(
            shape(&["create", "Fix the thing", "--json"]),
            "create --json"
        );
        assert_eq!(subcommand(&argv_shape(&["label", "add"])), "label");
    }

    #[test]
    fn which_finds_a_real_binary_on_path() {
        // `sh` is a safe cross-platform stand-in; asserts the PATH-walking
        // logic itself works without depending on `bd` being installed here.
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn a_failure_reported_only_on_stdout_still_reaches_the_user() {
        // br's real envelope, verbatim: stderr empty, everything on stdout.
        let stdout = r#"{"error":{"code":"ISSUE_NOT_FOUND","message":"Issue not found: no-such-id","hint":"Run 'br list' to see available issues."}}"#;
        let reason = failure_reason(stdout, "");
        assert!(reason.contains("Issue not found: no-such-id"), "{reason}");
        assert!(reason.contains("Run 'br list'"), "hint dropped: {reason}");

        // bd's shape: stderr wins, and stdout (often JSON on the happy path)
        // must not be pasted in beside it.
        assert_eq!(
            failure_reason("{\"issues\":[]}", "Error: no beads database found\n"),
            "Error: no beads database found"
        );

        // Neither stream is a JSON envelope: say whatever the backend did say
        // rather than the empty string the stderr-only reader produced.
        assert_eq!(
            failure_reason("plain stdout complaint\n", ""),
            "plain stdout complaint"
        );
        assert_eq!(failure_reason("", ""), "");
    }

    #[test]
    fn detects_versions_outside_the_tested_range() {
        assert_eq!(version_compat_warning("bd version 1.1.0"), None);
        assert_eq!(version_compat_warning("bd 1.1.9"), None);
        // 1.2 is inside the window as of pact-0re: it behaves DIFFERENTLY from
        // 1.1 on a replayed create, and pact handles both, which is what being
        // tested against it means.
        assert_eq!(version_compat_warning("bd version 1.2.1"), None);
        assert!(version_compat_warning("bd version 1.3.0")
            .unwrap()
            .contains("outside tested range"));
        assert!(version_compat_warning("bd version 0.9.0")
            .unwrap()
            .contains("outside tested range"));
    }

    #[test]
    fn ignores_unparseable_versions() {
        assert_eq!(version_compat_warning("beads unknown"), None);
    }

    /// One backend, one window — and it is read off the version string rather
    /// than assumed, so a binary that reports something unexpected still gets
    /// judged against the range pact actually tested.
    #[test]
    fn every_version_is_judged_against_bds_window() {
        // A real string from the binary on this machine, verbatim.
        assert_eq!(version_compat_warning("bd version 1.1.2 (20e493e56)"), None);
        assert!(version_compat_warning("bd version 0.2.19")
            .unwrap()
            .contains("1.1.0 <= version < 1.3.0"));
    }

    fn workspace_in(dir: &std::path::Path, entries: &[&str]) -> Workspace {
        let beads = dir.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        for e in entries {
            if e.ends_with('/') {
                std::fs::create_dir_all(beads.join(e.trim_end_matches('/'))).unwrap();
            } else {
                std::fs::write(beads.join(e), b"").unwrap();
            }
        }
        classify_workspace(&beads)
    }

    /// The layouts the two `init`s actually produce, checked here so the
    /// backend choice cannot drift away from what is on disk.
    #[test]
    fn a_workspace_is_classified_by_the_store_its_init_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        // `bd init`: embedded Dolt, no SQLite file anywhere.
        assert_eq!(
            workspace_in(&tmp.path().join("bd"), &["embeddeddolt/", "config.yaml"]),
            Workspace::Bd
        );
        // `br init`: a SQLite db plus the JSONL export.
        assert_eq!(
            workspace_in(&tmp.path().join("br"), &["beads.db", "issues.jsonl"]),
            Workspace::Br
        );
        // One stray `br init` inside a bd repo leaves both. The Dolt store has
        // the data, so bd wins — otherwise a single accident silently empties
        // every agent's inbox.
        assert_eq!(
            workspace_in(
                &tmp.path().join("both"),
                &["embeddeddolt/", "beads.db", "issues.jsonl"]
            ),
            Workspace::Bd
        );
        // WAL/shm siblings are not a database on their own.
        assert_eq!(
            workspace_in(&tmp.path().join("wal"), &["beads.db-wal", "beads.db-shm"]),
            Workspace::None
        );
        assert_eq!(workspace_in(&tmp.path().join("bare"), &[]), Workspace::None);
    }

    #[test]
    fn detect_workspace_walks_up_and_tolerates_no_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.path().join(".beads").join("embeddeddolt")).unwrap();
        assert_eq!(detect_workspace(Some(&nested)), Workspace::Bd);
        assert_eq!(detect_workspace(None), Workspace::None);
    }

    /// An existing store is a constraint, not a preference: falling back to the
    /// other backend would open an empty database and report an empty repo.
    #[test]
    fn a_br_workspace_is_refused_with_a_message_that_names_the_last_version() {
        assert_eq!(candidates(Workspace::Bd), ["bd"]);
        assert_eq!(candidates(Workspace::None), ["bd"]);
        // Nothing can serve it, so `locate` reaches the refusal instead of
        // handing back a binary that would read the wrong store.
        assert!(candidates(Workspace::Br).is_empty());

        // The refusal must NOT claim the CLI is missing: `br` is almost certainly
        // installed and working, and sending the reader off to install what they
        // already have is the one wrong answer available here.
        let br = missing_backend_message(Workspace::Br);
        assert!(
            !br.contains("not found on PATH"),
            "br is on their PATH; the missing thing is pact's support: {br}"
        );
        assert!(
            br.contains("no longer supports br") && br.contains("0.7.9"),
            "must say what changed and name the last version that worked: {br}"
        );
        assert!(
            br.contains("migrate") && br.contains("pin"),
            "must give both ways out: {br}"
        );

        let bd = missing_backend_message(Workspace::Bd);
        assert!(bd.starts_with("bd (beads) not found"), "{bd}");
    }

    #[test]
    fn interaction_actors_is_none_without_the_file_and_deduped_sorted_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(interaction_actors(tmp.path()), None);

        let beads = tmp.path().join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        std::fs::write(
            beads.join("interactions.jsonl"),
            "{\"actor\":\"agent-b\"}\n\
             not json\n\
             {\"actor\":\"agent-a\"}\n\
             {\"actor\":\"agent-b\"}\n\
             {\"actor\":\"\"}\n\
             {\"no_actor_field\":true}\n",
        )
        .unwrap();
        assert_eq!(
            interaction_actors(tmp.path()),
            Some(vec!["agent-a".to_string(), "agent-b".to_string()])
        );
    }

    /// A plain `.git/` directory is all `beads_root` needs to treat a temp dir
    /// as an ordinary (non-bare, non-worktree) checkout — the same shortcut
    /// `tests/cli.rs` and `tests/mcp.rs` use for their `init_repo` helpers.
    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        tmp
    }

    /// Restores `PACT_BEADS_TIMEOUT_SECS` on drop (including on panic), so one
    /// test's override can never leak into whichever test runs next in this
    /// process — unit tests across every module in this binary-only crate share
    /// one address space and one environment.
    struct TimeoutOverride(Option<String>);
    impl TimeoutOverride {
        fn set(secs: &str) -> Self {
            let previous = std::env::var("PACT_BEADS_TIMEOUT_SECS").ok();
            std::env::set_var("PACT_BEADS_TIMEOUT_SECS", secs);
            TimeoutOverride(previous)
        }
    }
    impl Drop for TimeoutOverride {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("PACT_BEADS_TIMEOUT_SECS", v),
                None => std::env::remove_var("PACT_BEADS_TIMEOUT_SECS"),
            }
        }
    }

    /// The bug this whole change exists to fix: `sleep 100` stands in for a
    /// `bd`/`br` that never exits — wedged on a prompt, a bug, a write lock.
    /// `BeadsCli` is directly constructible with an arbitrary `binary` (see the
    /// struct's own doc comment; `msg.rs` and `main.rs` already build one this
    /// way in their own tests), so no real Beads CLI is needed to prove `run`
    /// returns instead of hanging.
    #[test]
    fn a_hung_child_is_killed_and_reported_as_exit_3_within_the_timeout() {
        let _override = TimeoutOverride::set("1");
        let repo = init_repo();
        let cli = BeadsCli { binary: "sleep" };

        let started = std::time::Instant::now();
        let err = cli
            .run(repo.path(), &["100"])
            .expect_err("a child that never exits must not make run() hang");
        let elapsed = started.elapsed();

        assert_eq!(
            crate::output::code_for(&err),
            3,
            "a hung backend reuses exit 3, the same code as no Beads workspace: {err:#}"
        );
        let message = format!("{err:#}");
        assert!(
            message.contains("PACT_BEADS_TIMEOUT_SECS"),
            "the message should name the knob to raise: {message}"
        );
        // Generous relative to the 1s timeout so a loaded CI box cannot flake
        // this, but nowhere near the 100s `sleep` would have taken uncapped —
        // the whole point is that this returns, not that it is instant.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "run() took {elapsed:?} against a 1s timeout; it did not return promptly"
        );
    }

    /// The fast path must be unaffected by the new poll loop: a command that
    /// exits immediately still returns its real stdout, not an artefact of
    /// polling (a partial read, an extra newline, a timeout error).
    #[test]
    fn a_normal_call_still_returns_promptly_with_its_real_stdout() {
        let repo = init_repo();
        let cli = BeadsCli { binary: "echo" };

        let started = std::time::Instant::now();
        let out = cli
            .run(repo.path(), &["hello-from-a-fast-child"])
            .expect("a fast, well-behaved child must still succeed");
        let elapsed = started.elapsed();

        assert_eq!(out.trim(), "hello-from-a-fast-child");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "polling logic must not slow down the fast path: {elapsed:?}"
        );
    }

    /// pact-m7j.9.9: `supports_actor` used to cache its answer in a
    /// process-lifetime `OnceLock`, correct for a fresh-process CLI invocation
    /// but wrong for `pact mcp serve`/`pact ui`, which stay up long enough to
    /// see the installed `bd`/`br` swapped out from under them. Points
    /// `BeadsCli` at an absolute path to a stub script (never at a bare name on
    /// `PATH`) so this cannot race any other test in this process that shells
    /// out by binary name.
    #[cfg(unix)]
    #[test]
    fn supports_actor_is_re_derived_every_call_not_cached_for_the_process() {
        use std::os::unix::fs::PermissionsExt;

        let repo = init_repo();
        let stub_dir = tempfile::tempdir().unwrap();
        let script = stub_dir.path().join("stub-bd");
        let marker = stub_dir.path().join("stub-bd.actor");

        // The stub is written ONCE and never rewritten; what changes between
        // calls is a marker file it reads. The earlier version rewrote the
        // executable in place between the two calls, which flaked repeatedly
        // under the full parallel suite and never once in isolation — writing
        // an executable that was just exec'd is a race with the loader, and
        // `supports_actor` folds any spawn failure into `false` by design (see
        // its doc comment), which is indistinguishable from the cached answer
        // this test exists to rule out. Toggling a data file the stub reads
        // tests exactly the same property with nothing to race.
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ -f \"$0.actor\" ]; then\n  echo 'usage: create [--actor ACTOR] [--json]'\nelse\n  echo 'usage: create [--title] [--json]'\nfi\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let binary: &'static str =
            Box::leak(script.to_string_lossy().into_owned().into_boxed_str());
        let cli = BeadsCli { binary };
        assert!(
            !cli.supports_actor(repo.path()),
            "stub help text names no --actor, so this must report false"
        );

        // Same `BeadsCli`, same process, the backend's answer changed —
        // standing in for a `bd`/`br` upgrade or PATH change during a
        // long-lived `pact mcp serve`/`pact ui` session.
        std::fs::write(&marker, b"").unwrap();
        // Retried, and self-diagnosing on failure. `supports_actor` folds a
        // spawn failure into `false` by design (see its doc comment), so at
        // this assertion a transient failure to fork is indistinguishable from
        // the cached answer the test exists to rule out — and under the fully
        // parallel suite it does transiently fail. Retrying cannot mask the
        // real bug: a genuinely cached answer returns `false` every time.
        //
        // If it ever fails anyway, the panic carries what a direct spawn of
        // the same stub actually did, so the next reader gets evidence instead
        // of a fourth hypothesis.
        let re_derived = (0..5).any(|_| cli.supports_actor(repo.path()));
        assert!(
            re_derived,
            "a cached answer would still report false here; it must be re-derived per call. \
             Direct probe of the stub: {:?}",
            Command::new(binary)
                .args(["create", "--help"])
                .current_dir(repo.path())
                .output()
                .map(|o| format!(
                    "status={} stdout={:?} stderr={:?}",
                    o.status,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ))
        );
    }
}
