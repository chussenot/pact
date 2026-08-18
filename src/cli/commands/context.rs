//! `pact context set` — what the run was operating under.
//!
//! Owns the decision NOT to be idempotent. Setting a key twice records both
//! rows and the later one wins, because a run that revised its policy mid-flight
//! did revise it, and a log that overwrites cannot say so. See [`run_context_set`].

use anyhow::Result;
use std::path::Path;

use crate::{events, identity, output, repo};

/// `pact context set <key> <value>`.
///
/// One append, chain-hashed with everything else, because the constraints a run
/// operated under belong in the same log as its behaviour — see
/// [`events::CONTEXT_KIND`] for the failure this exists to prevent.
///
/// Deliberately NOT idempotent: setting a key twice records both rows and the
/// later one wins. A run that revised its policy mid-flight did revise it, and
/// flattening that to one value would hide exactly the kind of thing an audit is
/// looking for.
pub(in crate::cli) fn run_context_set(
    cwd: &Path,
    json: bool,
    key: &str,
    value: &str,
    agent_flag: Option<&str>,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    // An identity, like every other row: "who declared this policy" is as much
    // part of the record as the policy, and an orchestrator setting it is the
    // normal case.
    let agent = identity::resolve_agent(agent_flag)?;

    let key = key.trim();
    if key.is_empty() {
        return Err(output::exit_with(5, "a context key cannot be empty"));
    }
    // `=` and whitespace are refused so a row always renders unambiguously as
    // `key=value` — in `pact log`, in the audit header, and in any script that
    // splits on the first `=`. The value is free text precisely because it is
    // the half nothing has to parse.
    if key.contains('=') || key.chars().any(char::is_whitespace) {
        return Err(output::exit_with(
            5,
            format!(
                "invalid context key {key:?}: no whitespace and no '=' (the value may contain both)"
            ),
        ));
    }

    events::append(
        &root,
        &events::Event {
            at: chrono::Utc::now().to_rfc3339(),
            agent: agent.clone(),
            kind: events::CONTEXT_KIND.to_string(),
            context_key: Some(key.to_string()),
            context_value: Some(value.to_string()),
            ..Default::default()
        },
    );

    #[derive(serde::Serialize)]
    struct ContextReport<'a> {
        key: &'a str,
        value: &'a str,
        agent: &'a str,
    }
    output::emit(
        json,
        &ContextReport {
            key,
            value,
            agent: &agent,
        },
        |r: &ContextReport| {
            format!(
                "recorded {}={} for this run (by {})",
                r.key, r.value, r.agent
            )
        },
    );
    Ok(())
}
