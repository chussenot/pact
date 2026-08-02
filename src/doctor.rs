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

    // AGENTS.md being current is not enough: Claude Code loads CLAUDE.md and
    // never AGENTS.md, so a repo can pass every other check and still run a
    // Claude fleet that has never seen the protocol.
    let claude_reaches = agents_md::claude_md_reaches_protocol(root).unwrap_or(false);
    checks.push(DoctorCheck {
        name: "CLAUDE.md reaches the protocol",
        ok: claude_reaches,
        detail: if claude_reaches {
            "imports AGENTS.md".to_string()
        } else {
            "Claude Code loads CLAUDE.md, not AGENTS.md — run `pact init`".to_string()
        },
    });

    checks.push(match beads::BeadsCli::locate() {
        Ok(cli) => {
            let version = cli.version(root).unwrap_or_else(|e| {
                format!("found, but `{} --version` failed: {e:#}", cli.binary())
            });
            let mut detail = format!("{} ({version})", cli.binary());
            if let Some(warning) = beads::version_compat_warning(&version) {
                detail.push_str(&format!(" — warning: {warning}"));
            }
            DoctorCheck {
                name: "Beads CLI",
                ok: true,
                detail,
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

    match lease::corrupt_count(root) {
        Ok(0) => checks.push(DoctorCheck {
            name: "corrupt leases",
            ok: true,
            detail: "none".to_string(),
        }),
        Ok(n) => checks.push(DoctorCheck {
            name: "corrupt leases",
            ok: false,
            detail: format!(
                "{n} unreadable lock file{} (remove manually from .pact/leases/)",
                if n == 1 { "" } else { "s" }
            ),
        }),
        Err(e) => checks.push(DoctorCheck {
            name: "corrupt leases",
            ok: false,
            detail: format!("{e:#}"),
        }),
    }

    let healthy = checks.iter().all(|c| c.ok);
    DoctorReport { healthy, checks }
}
