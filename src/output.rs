use serde::Serialize;

/// Render `value` as pretty JSON when `json` is set, otherwise via `human`.
pub fn emit<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T) -> String) {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to serialize output: {e}"),
        }
    } else {
        println!("{}", human(value));
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
