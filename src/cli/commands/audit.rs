//! `pact audit` — the flag surface, and the summary-or-check decision.
//!
//! Owns one rule, stated on [`run_audit`] below: a finding is a *result*, not
//! an error. It is returned as an exit code rather than raised, so it can never
//! print `error:` and can never be confused with a usage failure. The analysis
//! itself lives in [`crate::audit`]; this file only decides which of it to run.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::{audit, output, repo};

/// `pact audit`: the summary, or one named check, plus an optional `--export`.
///
/// Returns the process exit code rather than raising, in the same shape as
/// `doctor`: a finding is a *result*, not an error, so it must not print
/// `error:` and must not be confusable with a usage failure. 1 means "the check
/// found something", which is the documented generic-failure code reused rather
/// than a new one invented. `--export` never changes this: writing the file
/// is orthogonal to what `--check`/no-`--check` decide for stdout and the
/// exit code.
/// Everything `pact audit` was invoked with, grouped rather than passed as
/// eight positional arguments — the flags have grown past the point where a
/// caller can get their order right by eye.
pub(in crate::cli) struct AuditArgs {
    pub(in crate::cli) check: Option<String>,
    pub(in crate::cli) since: Option<String>,
    pub(in crate::cli) include_annotated: bool,
    pub(in crate::cli) expect: Option<String>,
    pub(in crate::cli) allow_main: Vec<String>,
    pub(in crate::cli) compare: Option<PathBuf>,
    pub(in crate::cli) export: Option<PathBuf>,
    pub(in crate::cli) strict: bool,
}

pub(in crate::cli) fn run_audit(cwd: &Path, json: bool, args: AuditArgs) -> Result<i32> {
    let AuditArgs {
        check,
        since,
        include_annotated,
        expect,
        allow_main,
        compare,
        export,
        strict,
    } = args;
    let root = repo::find_repo_root(cwd)?;
    let since = match since {
        Some(s) => Some(audit::parse_since(&s)?),
        None => None,
    };

    if let Some(path) = export {
        let report = audit::export(&root, since, include_annotated)?;
        let text = serde_json::to_string_pretty(&report)
            .context("serializing the self-improvement report")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        // Human mode only: under `--json`, stdout must stay exactly the one
        // parseable value every other command promises (the check result or
        // the summary, printed below) — a second top-level JSON object here
        // would break that contract for a caller doing `| jq`. The file on
        // disk, at the path the caller just gave, is confirmation enough for
        // a machine consumer.
        if !json {
            output::line(&format!(
                "wrote self-improvement report to {}",
                path.display()
            ));
        }
    }

    if let Some(baseline) = compare {
        let comparison = audit::compare(&root, &baseline, since, include_annotated)?;
        output::emit(json, &comparison, audit::render_comparison);
        // Always 0. A comparison reports movement and passes no judgement, so
        // there is nothing for an exit code to mean — see `audit::Comparison`.
        return Ok(0);
    }

    match check {
        None => {
            let summary = audit::summary(&root, since, include_annotated)?;
            output::emit(json, &summary, audit::render_summary);
            Ok(0)
        }
        Some(name) => {
            let check = audit::Check::parse(&name, expect.as_deref(), &allow_main)?;
            let report = audit::run_check_strict(&root, check, since, include_annotated, strict)?;
            output::emit(json, &report, audit::render_check);
            // The whole point of a named check: a machine can branch on it.
            Ok(if report.findings() == 0 { 0 } else { 1 })
        }
    }
}
