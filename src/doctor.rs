//! Health checks shared by `pact doctor` and the ui's Doctor tab, so both
//! surfaces show exactly the same thing instead of drifting apart.

use std::path::Path;

use serde::Serialize;

use crate::{agents_md, beads, lease};

#[derive(Serialize)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

#[derive(Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub checks: Vec<DoctorCheck>,
}

/// `root` must already be a resolved repo root (see `repo::find_repo_root`)
/// — without one, none of these checks mean anything, so callers treat
/// resolving it as a hard prerequisite rather than folding it into the report.
pub fn checks(root: &Path) -> DoctorReport {
    let mut checks = vec![DoctorCheck {
        name: "git repo",
        ok: true,
        detail: root.display().to_string(),
    }];

    let pact_present = root.join(".pact").join("leases").is_dir();
    checks.push(DoctorCheck {
        name: ".pact/ present",
        ok: pact_present,
        detail: if pact_present {
            "present".to_string()
        } else {
            "missing — run `pact init`".to_string()
        },
    });

    let agents_md_current = agents_md::is_current(root).unwrap_or(false);
    checks.push(DoctorCheck {
        name: "AGENTS.md block current",
        ok: agents_md_current,
        detail: if agents_md_current {
            "up to date".to_string()
        } else {
            "missing or stale — run `pact init`".to_string()
        },
    });

    checks.push(match beads::BeadsCli::locate() {
        Ok(cli) => {
            let version = cli.version(root).unwrap_or_else(|e| {
                format!("found, but `{} --version` failed: {e:#}", cli.binary())
            });
            DoctorCheck {
                name: "Beads CLI",
                ok: true,
                detail: format!("{} ({version})", cli.binary()),
            }
        }
        Err(e) => DoctorCheck {
            name: "Beads CLI",
            ok: false,
            detail: format!("{e:#}"),
        },
    });

    // peek, not list: a diagnostic that mutates is not a diagnostic — running
    // doctor twice used to give two different stale counts because the first run
    // unlinked the locks it was reporting (pact-rnc.19). `pact lease ls` still
    // collects them.
    match lease::peek(root, true) {
        Ok(entries) => {
            let stale = entries.iter().filter(|e| e.expired).count();
            checks.push(DoctorCheck {
                name: "stale leases",
                ok: true,
                detail: format!("{stale} stale (`pact lease ls` collects them)"),
            });
        }
        Err(e) => checks.push(DoctorCheck {
            name: "stale leases",
            ok: false,
            detail: format!("{e:#}"),
        }),
    }

    let healthy = checks.iter().all(|c| c.ok);
    DoctorReport { healthy, checks }
}
