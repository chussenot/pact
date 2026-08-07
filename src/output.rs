use std::io::{self, Write};

use serde::Serialize;

/// Replace any control character other than `\n`/`\t` with the Unicode
/// replacement character (`\u{FFFD}`).
///
/// Message bodies, subjects and lease notes are free text supplied by other
/// agents and printed verbatim by design (byte fidelity for `--body-file`,
/// pact-rnc.25) — but "verbatim" must stop at bytes that are actually
/// terminal commands. A body containing a raw ESC (`\x1b`) sequence would
/// otherwise be interpreted by whatever terminal displays it: clear the
/// screen, move the cursor, rewrite the prompt. `c.is_control()` is the same
/// filter `ratatui-core` already applies before drawing a `Span` (`pact ui`
/// is not exposed to this), so both surfaces agree on what "displayable"
/// means. `\n` and `\t` are explicitly exempted: multi-line bodies and
/// tab-formatted tables are legitimate output, not an attack. Substitution
/// rather than deletion keeps the 1-char-for-1-char length/position
/// intuition and can't accidentally concatenate text that was meant to stay
/// apart.
fn sanitize_for_terminal(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// Write `s` and a newline to `w`. A closed reader (EPIPE) is not an error.
///
/// Exit-status decision (pact-rnc.26): a broken pipe produces **no panic, no
/// message and no special exit code** — the process keeps whatever status its
/// actual work earned, which is 0 when the work succeeded. The conventional
/// unix answer is to die as if by SIGPIPE (status 141), but pact's side effect
/// (the bead created, the lock file written) has *already landed* by the time
/// anything is printed, and pact's exit codes are a documented API (README:
/// 0/1/2/3/4 — 141 and 101 appear nowhere). Any non-zero status here tells the
/// calling agent "your command failed"; it retries, and we get the duplicate
/// messages this bug was reported for. Losing the tail of a report whose
/// reader already walked away is strictly cheaper than making a completed
/// action look failed, so we drop the bytes and stay quiet.
fn write_line(w: &mut impl Write, s: &str) {
    let s = sanitize_for_terminal(s);
    if let Err(e) = writeln!(w, "{s}") {
        if e.kind() != io::ErrorKind::BrokenPipe {
            // A real write failure is worth mentioning, but still not fatal:
            // the work is done either way. Ignore a failing stderr too.
            let _ = writeln!(io::stderr(), "warning: failed writing output: {e}");
        }
    }
}

/// Print a line to stdout. A closed reader (EPIPE) must not panic.
pub fn line(s: &str) {
    write_line(&mut io::stdout().lock(), s);
}

/// Print a line to stderr (warnings). Also EPIPE-safe.
pub fn warn(s: &str) {
    write_line(&mut io::stderr().lock(), s);
}

/// Render `value` as pretty JSON when `json` is set, otherwise via `human`.
pub fn emit<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T) -> String) {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(s) => line(&s),
            Err(e) => warn(&format!("failed to serialize output: {e}")),
        }
    } else {
        line(&human(value));
    }
}

/// An error tagged with the process exit code it should produce.
/// Exit codes are part of pact's API (see README), so any module can raise
/// one via [`exit_with`] and `main` will honor it instead of the generic `1`.
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExitError {}

pub fn exit_with(code: i32, message: impl Into<String>) -> anyhow::Error {
    ExitError {
        code,
        message: message.into(),
    }
    .into()
}

/// Exit code to use for an error, per the documented API: 2 = lease held by
/// another agent, 3 = Beads CLI not found, 4 = not in a git repo, 1 = generic.
pub fn code_for(err: &anyhow::Error) -> i32 {
    err.downcast_ref::<ExitError>().map_or(1, |e| e.code)
}

/// A `--json` caller's error shape when no richer, purpose-built one exists
/// at the catch site (see `msg::SendFailure` for the one case that has one).
///
/// AGENTS.md tells every agent to prefer `--json` over parsing human-
/// formatted text and to branch on the exit code, not the message — but
/// every failure used to print only to stderr as plain prose, so a `--json`
/// caller got nothing to parse on the single most routine non-zero outcome
/// two agents contending on a file will ever produce (pact-m7j.5.1). This is
/// deliberately the plain `{error, exit_code}` fallback, not a per-error-kind
/// shape: extracting structured fields (a lease conflict's holder, age,
/// remaining) would need the conflict's own catch site to build a typed
/// error the way `SendFailure` does, which is a larger, case-by-case
/// commitment this fix does not make.
#[derive(Serialize)]
struct ErrorJson {
    error: String,
    exit_code: i32,
}

/// Prints `err` as the fallback JSON shape above, to stdout — the same
/// stream a successful `--json` run uses, so a caller has exactly one place
/// to look regardless of whether the command succeeded.
pub fn emit_error_json(err: &anyhow::Error, code: i32) {
    let payload = ErrorJson {
        error: format!("{err:#}"),
        exit_code: code,
    };
    match serde_json::to_string_pretty(&payload) {
        Ok(s) => line(&s),
        Err(e) => warn(&format!("failed to serialize error output: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that always fails with the given kind, like a pipe whose
    /// reader has exited.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "test writer refuses to write"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(self.0, "test writer refuses to flush"))
        }
    }

    #[test]
    fn broken_pipe_does_not_panic() {
        write_line(&mut Failing(io::ErrorKind::BrokenPipe), "confirmation");
    }

    #[test]
    fn other_write_errors_do_not_panic_either() {
        write_line(
            &mut Failing(io::ErrorKind::PermissionDenied),
            "confirmation",
        );
    }

    #[test]
    fn working_writer_gets_the_line() {
        let mut buf = Vec::new();
        write_line(&mut buf, "hello");
        assert_eq!(buf, b"hello\n");
    }

    #[test]
    fn sanitize_replaces_control_bytes_but_keeps_newline_and_tab() {
        let attack = "before\x1b[2J\x1b[H\x07 a\tb\nc after";
        assert_eq!(
            sanitize_for_terminal(attack),
            "before\u{FFFD}[2J\u{FFFD}[H\u{FFFD} a\tb\nc after"
        );
    }

    #[test]
    fn sanitize_is_a_no_op_on_plain_text() {
        assert_eq!(
            sanitize_for_terminal("plain text, no surprises"),
            "plain text, no surprises"
        );
    }

    /// `write_line` is the single funnel every rendered surface goes through
    /// (`line`, `warn`, and everything `emit` calls) — this pins that the
    /// escape sequence never reaches the writer, not just that the helper
    /// function works in isolation.
    #[test]
    fn write_line_strips_escape_sequences_before_writing() {
        let mut buf = Vec::new();
        write_line(&mut buf, "clear-screen: \x1b[2J\x07 done");
        assert!(!buf.contains(&0x1b), "ESC byte leaked into output: {buf:?}");
        assert!(!buf.contains(&0x07), "BEL byte leaked into output: {buf:?}");
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "clear-screen: \u{FFFD}[2J\u{FFFD} done\n"
        );
    }

    #[test]
    fn public_wrappers_do_not_panic() {
        line("out-fix test line");
        warn("out-fix test warning");
    }
}
