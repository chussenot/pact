//! Health checks shared by `pact doctor` and the ui's Doctor tab, so both
//! surfaces show exactly the same thing instead of drifting apart.

use std::path::Path;

use serde::Serialize;

use crate::{agents_md, beads, lease, repo};

#[derive(Serialize)]
pub struct DoctorCheck {
    pub name: &'static str,
    /// Does this check pass? A warning passes: `warn` is a louder `ok`, not a
    /// softer failure, so `!ok` always implies `!warn`.
    pub ok: bool,
    /// Passes, but you should know. Rendered `!` rather than `✓`, and never
    /// affects `healthy` or the exit code — pact reports the situation instead
    /// of deciding it is wrong. `bd` outside its tested range is the other one.
    #[serde(default)]
    pub warn: bool,
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
        warn: false,
        detail: root.display().to_string(),
    }];

    let pact_present = root.join(".pact").join("leases").is_dir();
    checks.push(DoctorCheck {
        name: ".pact/ present",
        ok: pact_present,
        warn: false,
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
        warn: false,
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
        warn: false,
        detail: if claude_reaches {
            "imports AGENTS.md".to_string()
        } else {
            "Claude Code loads CLAUDE.md, not AGENTS.md — run `pact init`".to_string()
        },
    });

    // The files above can be perfectly current and still reach nobody: a
    // gitignored AGENTS.md is written, then silently refused by `git add`, and
    // the clone that was supposed to be onboarded gets nothing. pact's own repo
    // shipped that way for its whole history.
    let unreachable: Vec<String> = ["AGENTS.md", "CLAUDE.md"]
        .iter()
        .filter(|f| root.join(f).exists())
        .filter_map(|f| match repo::reach(root, f) {
            repo::Reach::Ignored { source } => Some(format!("{f} (ignored by {source})")),
            _ => None,
        })
        .collect();
    checks.push(DoctorCheck {
        // Warns, never fails (pact-1q0). Ignoring AGENTS.md can be a deliberate
        // choice — keeping the protocol local to one machine is a legitimate
        // setup — and pact does not get to overrule it. A check that stays red
        // on a decision the user already made is a check people learn to skip,
        // which would cost exactly the visibility this one exists for.
        name: "protocol files reach a clone",
        ok: true,
        warn: !unreachable.is_empty(),
        detail: if unreachable.is_empty() {
            "tracked or committable".to_string()
        } else {
            format!(
                "{} — `git add` refuses these silently, so a clone gets no protocol; \
                 if that is not deliberate, un-ignore them (e.g. add `!AGENTS.md` to \
                 .gitignore) and commit",
                unreachable.join(", ")
            )
        },
    });

    checks.push(match beads::BeadsCli::locate() {
        Ok(cli) => {
            let version = cli.version(root).unwrap_or_else(|e| {
                format!("found, but `{} --version` failed: {e:#}", cli.binary())
            });
            let mut detail = format!("{} ({version})", cli.binary());
            let compat = beads::version_compat_warning(&version);
            if let Some(warning) = &compat {
                detail.push_str(&format!(" — {warning}"));
            }
            DoctorCheck {
                name: "Beads CLI",
                ok: true,
                // An untested bd usually works; saying so with a green tick hid
                // the caveat in the tail of the line, where nobody read it.
                warn: compat.is_some(),
                detail,
            }
        }
        Err(e) => DoctorCheck {
            name: "Beads CLI",
            ok: false,
            warn: false,
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
                warn: false,
                detail: format!("{stale} stale (`pact lease ls` collects them)"),
            });
        }
        Err(e) => checks.push(DoctorCheck {
            name: "stale leases",
            ok: false,
            warn: false,
            detail: format!("{e:#}"),
        }),
    }

    match lease::corrupt_count(root) {
        Ok(0) => checks.push(DoctorCheck {
            name: "corrupt leases",
            ok: true,
            warn: false,
            detail: "none".to_string(),
        }),
        Ok(n) => checks.push(DoctorCheck {
            name: "corrupt leases",
            ok: false,
            warn: false,
            detail: format!(
                "{n} unreadable lock file{} (remove manually from .pact/leases/)",
                if n == 1 { "" } else { "s" }
            ),
        }),
        Err(e) => checks.push(DoctorCheck {
            name: "corrupt leases",
            ok: false,
            warn: false,
            detail: format!("{e:#}"),
        }),
    }

    let healthy = checks.iter().all(|c| c.ok);
    DoctorReport { healthy, checks }
}
