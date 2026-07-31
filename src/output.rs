use std::io::{self, Write};

use serde::Serialize;

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
    fn public_wrappers_do_not_panic() {
        line("out-fix test line");
        warn("out-fix test warning");
    }
}
