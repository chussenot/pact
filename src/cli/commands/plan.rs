//! `pact plan lint <manifest>` — contention prevention, as a check.
//!
//! Owns one agreement: paths are normalized through the repo root exactly as
//! `lease acquire` would normalize them. A lint that resolved paths differently
//! from the leases it exists to protect would pass a plan whose waves collide,
//! which is precisely the failure it was written to catch.
//!
//! Returns an exit code rather than exiting — errors 1, warnings alone 0, and
//! no new code invented for it. See [`run_plan_lint`].

use anyhow::Result;
use std::path::Path;

use crate::{output, plan, repo};

/// `pact plan lint <manifest>` — the contention-prevention step, as a check.
///
/// Returns the exit code rather than exiting, same shape as [`super::doctor::run_doctor`]: errors
/// are 1, warnings alone are 0. Deliberately NOT a new exit code — the table just
/// retired 3, and "errors found" is already what 1 means for `pact audit`.
pub(in crate::cli) fn run_plan_lint(cwd: &Path, json: bool, manifest: &str) -> Result<i32> {
    // A repo root only so that paths normalize exactly as `lease acquire` would:
    // one file must be one path however the manifest spelled it, or this check and
    // the lease it protects disagree about what they are discussing.
    let root = repo::find_repo_root(cwd)?;
    let report = plan::run(&root, Path::new(manifest))?;
    output::emit(json, &report, plan::render);
    Ok(i32::from(report.errors() > 0))
}
