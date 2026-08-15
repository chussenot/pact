//! Health checks shared by `pact doctor` and the ui's Doctor tab, so both
//! surfaces show exactly the same thing instead of drifting apart.

use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

use crate::{agents_md, beads, lease, otel, repo};

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub name: &'static str,
    /// Does this check pass? A warning passes: `warn` is a louder `ok`, not a
    /// softer failure, so `!ok` always implies `!warn`.
    pub ok: bool,
    /// Passes, but you should know. Rendered `!` rather than `✓`, and never
    /// affects `healthy` or the exit code — pact reports the situation instead
    /// of deciding it is wrong. A `.beads/` with no audit sidecar is another one:
    /// nothing is broken, but a check the reader may be relying on cannot run.
    #[serde(default)]
    pub warn: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub checks: Vec<DoctorCheck>,
}

/// The one-line verdict, spelled ONCE so the CLI and the TUI cannot disagree
/// about it.
///
/// The warning count is the load-bearing part, and it is reported whether or not
/// something else failed. It used to be dropped on the failure branch, so a repo
/// with both a broken check and a warning showed only "some checks failed" — and the
/// `!` line it refers to is exactly the one that scrolls off the top of a long
/// report, which is why the count exists at all.
///
/// `pact ui` rendered its own title from `healthy` alone and so lost the count
/// entirely, on the surface where scrolling off is MOST likely: a fixed-height panel
/// with two dozen checks in it, where a human sees "all checks passed" above a
/// visible `!`. Sharing the function is the fix, in the same spirit as `tab_rects`
/// being shared by rendering and mouse hit-testing so the two cannot drift.
pub fn summary(report: &DoctorReport) -> String {
    let warnings = report.checks.iter().filter(|c| c.warn).count();
    let head = if report.healthy {
        "all checks passed"
    } else {
        "some checks failed"
    };
    match warnings {
        0 => head.to_string(),
        1 => format!("{head}, 1 warning"),
        n => format!("{head}, {n} warnings"),
    }
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

    // Resolved once and reused: the worktree checks and `.pact/ present` must
    // agree about where state is, or the report explains one repository and
    // checks another.
    let ctx = repo::RepoContext::resolve(root);

    let pact_present = ctx.state_dir.join("leases").is_dir();
    checks.push(DoctorCheck {
        name: ".pact/ present",
        ok: pact_present,
        warn: false,
        detail: if pact_present {
            format!("present at {}", ctx.state_dir.display())
        } else {
            format!("missing at {} — run `pact init`", ctx.state_dir.display())
        },
    });

    worktree_checks(&ctx, &mut checks);

    // Warns rather than fails, and the detail carries the argument rather than
    // just the verdict: an ignored event log is not broken, it is a repository
    // that will lose its coordination history at the next clone — and the reader
    // needs to know that is a choice they can reverse with `pact init`.
    checks.push(clone_reach_check(
        root,
        agents_md::EVENTS_LOG_PATH,
        "event log survives a clone",
        "who held what",
    ));
    // Symmetric, and the guard finding 1 identified as missing. The message store became
    // committed in 0.9.0 and got no clone check, so the one state a repo should never be
    // in — gitignored while carrying a `merge=union` attribute — had nothing watching for
    // it. The field audit found a repo in exactly that state.
    checks.push(clone_reach_check(
        root,
        agents_md::MESSAGES_STORE_PATH,
        "message store survives a clone",
        "what agents said to each other",
    ));

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

    // pact-juz.3: not a pact bug when this fires — pact's own managed block
    // is exactly what the check above already polices. This catches a
    // DIFFERENT tool (confirmed in the field: `bd init` and `bd setup codex`,
    // each writing its own "Quick Reference" section unaware of the other)
    // duplicating ITS OWN content. Advisory only; pact does not own this
    // text and must never offer to touch it.
    let duplicate_headings = std::fs::read_to_string(root.join("AGENTS.md"))
        .map(|content| {
            agents_md::duplicated_headings_outside_managed_block(&content)
                .into_iter()
                .map(|(text, lines)| {
                    let located: Vec<String> = lines
                        .iter()
                        .map(
                            |&line| match agents_md::nearest_preceding_marker(&content, line) {
                                Some(marker) => format!("line {line} (near {marker})"),
                                None => format!("line {line}"),
                            },
                        )
                        .collect();
                    format!(
                        "{text:?} appears {} times: {}",
                        lines.len(),
                        located.join(", ")
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    checks.push(DoctorCheck {
        name: "no duplicated instruction blocks",
        ok: true,
        warn: !duplicate_headings.is_empty(),
        detail: if duplicate_headings.is_empty() {
            "no repeated heading found outside pact's own block".to_string()
        } else {
            duplicate_headings.join("; ")
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

    // GEMINI.md, .github/copilot-instructions.md and friends are managed the
    // same way CLAUDE.md is, so doctor must have the same opinion about them:
    // an instruction file pact writes and never re-checks goes stale in silence,
    // and a hand-edited block inside the markers reported a healthy repo.
    let managed = agents_md::managed_instruction_files(root);
    let stale = agents_md::stale_instruction_files(root).unwrap_or_default();
    checks.push(DoctorCheck {
        name: "other instruction files current",
        ok: stale.is_empty(),
        warn: false,
        detail: if managed.is_empty() {
            "none present".to_string()
        } else if stale.is_empty() {
            format!("{} up to date", managed.len())
        } else {
            format!(
                "missing or stale — run `pact init`: {}",
                stale
                    .iter()
                    .filter_map(|p| p.strip_prefix(root).ok())
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
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

    // `pact init` already warns about this on stderr at write time (see
    // `agents_md::resolve_write_target`), but only when a write actually
    // happens — a repo nobody has re-run `init` in stays silent about an
    // escaping symlink indefinitely. This asks the same question without
    // writing anything (pact-m7j.9.12).
    let escaping = agents_md::escaping_write_set_symlinks(root);
    checks.push(DoctorCheck {
        name: "write-set symlinks",
        ok: true,
        warn: !escaping.is_empty(),
        detail: if escaping.is_empty() {
            "no managed file is a symlink escaping the repository".to_string()
        } else {
            format!(
                "escapes the repository via a symlink — deliberate for a dotfiles-style layout, \
                 but worth confirming: {}",
                escaping
                    .iter()
                    .map(|(p, target)| format!(
                        "{} -> {}",
                        p.strip_prefix(root).unwrap_or(p).display(),
                        target.display()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    });

    // Built in, configured, and exporting are three different things, and the
    // gap between the last two is silent by construction: pact speaks http/json
    // over http:// only, so `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` (the
    // OTel *spec default*) or an `https://` endpoint (every hosted collector)
    // turns export off with no warning and no failure. A repo that switched the
    // feature on in CI got a green build, a green doctor, `pact --version`
    // saying `features: otel`, and no data. Choosing to speak one protocol is
    // defensible; being quiet about the choice is the defect.
    let otel_export = otel::export_status();
    checks.push(DoctorCheck {
        name: "otel export",
        ok: true,
        warn: otel_export.warn,
        detail: otel_export.detail,
    });

    // Warns rather than fails: pact still resolves correctly (Dolt first,
    // because that is where the data is), and two stores can legitimately
    // coexist mid-migration. But the correct tiebreak is what makes it
    // invisible — an empty store can shadow a full one and every command keeps
    // answering normally, until someone runs the other backend directly and
    // sees an empty issue list.
    let conflict_warning = beads::conflict_warning(root);
    checks.push(DoctorCheck {
        name: "one Beads store",
        ok: true,
        warn: conflict_warning.is_some(),
        detail: conflict_warning.unwrap_or_else(|| "no conflicting store".to_string()),
    });

    checks.push(match beads::BeadsCli::locate() {
        Ok(cli) => {
            let version = cli.version(root).unwrap_or_else(|e| {
                format!("found, but `{} --version` failed: {e:#}", cli.binary())
            });
            let mut detail = format!("{} ({version})", cli.binary());
            // Attribution, in the backend section because that is where the
            // question belongs: it is a property of the CLI pact found, not of
            // pact. Stated either way rather than only when broken — "who is this
            // bead recorded against" is asked when a trail already looks wrong,
            // and an absent line answers nothing.
            if cli.supports_actor(root) {
                detail.push_str(", attributes writes to the acting agent (--actor)");
            } else {
                detail.push_str(
                    ", does NOT accept --actor: every agent's bead activity will be recorded \
                     against this checkout's git user instead of the agent that caused it",
                );
            }
            DoctorCheck {
                name: "Beads CLI",
                ok: true,
                // No version warning any more (pact-as5.5). pact used to warn
                // outside a tested 1.1–1.2 window because bd's CLI semantics
                // reached pact's own messaging — bd 1.2 dropping `create --id
                // --force`'s upsert broke four tests with no source change. Since
                // 0.9.0 the only bd call pact makes is the `--version` printed on
                // this line, so a version pact has not tested cannot break
                // anything, and warning about it would be pure noise on every
                // future bd release.
                warn: false,
                detail,
            }
        }
        // A WARNING, not a failure, since 0.9.0. bd used to be required — messages
        // were beads, so no bd meant no messaging — and this check failed, which made
        // `pact doctor` exit 1 on a machine that had simply never installed the issue
        // tracker. Nothing pact does needs it any more, so a missing bd is a fact
        // worth reporting and not a broken repository. The one thing it still costs is
        // named, so the report is actionable rather than merely relaxed.
        Err(e) => DoctorCheck {
            name: "Beads CLI",
            ok: true,
            warn: true,
            detail: format!(
                "{e:#} — nothing pact does needs it, but `pact audit --check \
                 claim-lease-divergence` has no assignees to check against and \
                 `pact whoami` cannot name a backend"
            ),
        },
    });

    // pact-juz.2/pact-juz.4: the check above confirms the BINARY supports
    // --actor, not that anything is actually using it for the commands an
    // agent runs directly (`bd update --claim`, `bd close`, ...) — those
    // never pass through pact at all. Confirmed on a real 15-agent build:
    // 16 distinct pact agent identities in `.pact/events.jsonl`, and every
    // one of 16 `.beads/interactions.jsonl` entries attributed to the
    // operator's own git identity instead. This detects that symptom
    // automatically in case pact-juz.2's `BEADS_ACTOR` guidance is missed,
    // skipped, or a future backend stops honoring the env var.
    let pact_agents: std::collections::BTreeSet<String> = crate::events::recent(root, usize::MAX)
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.agent)
        .collect();
    checks.push(match beads::interaction_actors(root) {
        // Absence is read as "not applicable" — a repo whose backend does not
        // exist must not be told its attribution is broken.
        None => DoctorCheck {
            name: "Beads actor attribution",
            ok: true,
            warn: false,
            detail: "no .beads/interactions.jsonl — not applicable".to_string(),
        },
        Some(bd_actors) => {
            let bd_actors: std::collections::BTreeSet<String> = bd_actors.into_iter().collect();
            // The signal is MULTIPLE pact identities collapsing to a
            // disjoint set of bd actors, not merely zero overlap — a solo
            // session legitimately has one identity everywhere, and that is
            // not suspicious.
            let suspicious = pact_agents.len() > 1
                && !bd_actors.is_empty()
                && pact_agents.is_disjoint(&bd_actors);
            DoctorCheck {
                name: "Beads actor attribution",
                ok: true,
                warn: suspicious,
                detail: if suspicious {
                    format!(
                        "{} pact agent identities acted, but none of the {} Beads actor(s) \
                         recorded in .beads/interactions.jsonl match any of them — direct `bd` \
                         commands are not attributed to the agent that ran them. Run `pact whoami` \
                         and export the `BEADS_ACTOR` line it prints.",
                        pact_agents.len(),
                        bd_actors.len()
                    )
                } else {
                    format!(
                        "{} pact agent identities, {} Beads actor(s) recorded — no attribution \
                         gap detected",
                        pact_agents.len(),
                        bd_actors.len()
                    )
                },
            }
        }
    });

    // pact-as5.5: `pact audit --check claim-lease-divergence` is the only thing
    // left that asks a Beads-side question, and it reads the committed
    // `.beads/interactions.jsonl` export. bd writes that sidecar only when its audit
    // sidecar is recording, and `bd audit --help` says it is off by default.
    //
    // So the check is effectively opt-in, and its failure mode is silence: it
    // reports "no assignee history" and PASSES, forever, which is
    // indistinguishable from "your fleet never diverged". That is the one thing
    // doctor exists to prevent, so it says so. Warns rather than fails: the sidecar
    // is genuinely optional and a repo that does not want it is not broken.
    //
    // pact-83r.6 / field-audit finding 6: what is asked changed, because doctor used
    // to ask `bd config get audit.enabled` and report the answer as the verdict. That
    // read cannot support the verdict, for two measured reasons:
    //
    //   1. bd answers for a key nobody set. `bd config get anything.at.all` prints
    //      `(not set)` and exits 0, and after `bd config set audit.enabled true` — a
    //      command bd greets with `Warning: "audit.enabled" is not a recognized
    //      config key` and then honours anyway — the get reads back `true`. So the
    //      read reports this repository's `config.yaml`, and reports it whether or
    //      not bd's own allowlist agrees the key exists.
    //   2. `BD_AUDIT_ENABLED=1` enables the sidecar with NO config write at all. It
    //      lives in the environment of whoever runs bd, which is not doctor's
    //      process, so nothing pact can read reflects it.
    //
    // Together those mean pact cannot honestly answer "is recording on right now",
    // and a check that answers it anyway will call a correctly-configured repo broken
    // (env-var route, key unset) as readily as the reverse. So the check stops
    // claiming it and reports the fact it can stand behind: whether the export file
    // pact actually reads exists. That file is proof recording happened, which is the
    // question `pact audit --check claim-lease-divergence` cares about — and no bd is
    // spawned to learn it.
    //
    // Warns rather than fails either way: the sidecar is genuinely optional and a
    // repo that does not want it is not broken. What is worth a warning is believing
    // a check runs when it cannot.
    let beads_dir = root.join(".beads");
    checks.push(sidecar_check(
        beads_dir.is_dir(),
        beads_dir.join("interactions.jsonl").is_file(),
    ));

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
                "{n} unreadable lock file{} (`pact lease acquire <path> --steal` recovers one; \
                 `pact lease release <path> --force` removes it; manual deletion from \
                 .pact/leases/ also works)",
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

    // A crash between `temp_sibling`'s write and its rename into place leaves
    // a `staging-*`/`tmp-*` file behind that `corrupt leases` above cannot
    // see — that check only looks at `.lock` files, and these never got that
    // extension in the first place. Same shape as `corrupt leases` because
    // it is the same kind of debris: not an active hold, but noise nothing
    // else will ever surface.
    match lease::orphan_temp_count(root) {
        Ok(0) => checks.push(DoctorCheck {
            name: "orphaned staging files",
            ok: true,
            warn: false,
            detail: "none".to_string(),
        }),
        Ok(n) => checks.push(DoctorCheck {
            name: "orphaned staging files",
            ok: false,
            warn: false,
            detail: format!(
                "{n} leftover staging file{} from an interrupted write (remove manually from \
                 .pact/leases/)",
                if n == 1 { "" } else { "s" }
            ),
        }),
        Err(e) => checks.push(DoctorCheck {
            name: "orphaned staging files",
            ok: false,
            warn: false,
            detail: format!("{e:#}"),
        }),
    }

    // Pure visibility, not a failure: a wait marker is only collected by the
    // same agent retrying the same path or by that agent's own `release
    // --all`, and AGENTS.md tells a blocked agent to do neither ("message
    // them and pick up something else"). So a nonzero count here is ordinary
    // fleet behaviour, not damage — there is no ceiling to invent, only a
    // number worth knowing (pact-m7j.4.6).
    match lease::marker_count(root) {
        Ok(0) => checks.push(DoctorCheck {
            name: "stale wait markers",
            ok: true,
            warn: false,
            detail: "none".to_string(),
        }),
        Ok(n) => checks.push(DoctorCheck {
            name: "stale wait markers",
            ok: true,
            warn: true,
            detail: format!(
                "{n} marker{} in .pact/waits/ from a conflict nobody retried or swept with \
                 `lease release --all` — harmless, informational only",
                if n == 1 { "" } else { "s" }
            ),
        }),
        Err(e) => checks.push(DoctorCheck {
            name: "stale wait markers",
            ok: false,
            warn: false,
            detail: format!("{e:#}"),
        }),
    }

    let healthy = checks.iter().all(|c| c.ok);
    export(&checks);
    DoctorReport { healthy, checks }
}

/// Health as one number, because a two-valued gauge would throw away exactly
/// the distinction `warn` was added to make. Bigger is healthier, so `min()`
/// across checks is the worst thing wrong with the repo and one chart reads
/// without a legend. Same `(ok, warn)` match as the `✗ ! ✓` glyphs in
/// `run_doctor`, so the number and the tick can never disagree.
/// Everything about where coordination state came from, so that a surprising
/// answer can be explained from `pact doctor` alone rather than by reading
/// `.git` files by hand.
///
/// Reported even in an ordinary checkout, where it says "not a worktree" — the
/// question "is my peer even sharing my leases?" is asked exactly when something
/// is already confusing, and a check that is absent unless it fires is a check
/// nobody knows to look for.
fn worktree_checks(ctx: &repo::RepoContext, checks: &mut Vec<DoctorCheck>) {
    let scope = std::env::var("PACT_WORKTREE_SCOPE").unwrap_or_else(|_| "shared".to_string());
    let scope_local = scope == "local";

    checks.push(DoctorCheck {
        name: "worktree",
        ok: true,
        // A linked worktree whose resolution FELL BACK is the one case worth a
        // `!`: leases still work, they just do not reach the sibling worktrees
        // the agent probably assumes they do.
        warn: ctx.warning.is_some(),
        detail: match (&ctx.warning, ctx.is_linked_worktree) {
            (Some(w), _) => format!("resolution fell back to per-worktree state: {w}"),
            (None, true) => format!(
                "linked worktree {} of {}",
                ctx.worktree_name.as_deref().unwrap_or("<unnamed>"),
                ctx.shared_root.display()
            ),
            (None, false) if ctx.has_worktrees => format!(
                "main worktree; this repository also has linked worktrees, which share {}",
                ctx.state_dir.display()
            ),
            // Named explicitly rather than folded into "ordinary checkout": a
            // submodule IS ordinary as far as coordination goes, but a reader
            // asking "why don't my leases reach the superproject?" needs the
            // answer here rather than having to infer it from `state placement`.
            (None, false) if ctx.placement == repo::Placement::Submodule => {
                "not a worktree (submodule checkout — its own coordination space)".to_string()
            }
            (None, false) => "not a worktree (ordinary checkout)".to_string(),
        },
    });

    checks.push(DoctorCheck {
        name: "coordination scope",
        ok: true,
        // `local` in a repo with worktrees means advisory locks that advise
        // nobody. Legal, deliberate, and worth saying out loud every time.
        warn: scope_local && ctx.has_worktrees,
        detail: match (scope.as_str(), ctx.has_worktrees) {
            ("local", true) => format!(
                "PACT_WORKTREE_SCOPE=local — state is per-worktree at {}, so leases held here are \
                 INVISIBLE to sibling worktrees of this repository",
                ctx.state_dir.display()
            ),
            ("local", false) => {
                "PACT_WORKTREE_SCOPE=local (no worktrees here, so no effect)".to_string()
            }
            ("shared", _) => format!("shared (default) — state at {}", ctx.state_dir.display()),
            (other, _) => format!(
                "PACT_WORKTREE_SCOPE={other} is not recognised; treated as shared — state at {}",
                ctx.state_dir.display()
            ),
        },
    });

    checks.push(DoctorCheck {
        name: "state placement",
        ok: true,
        warn: false,
        detail: match ctx.placement {
            repo::Placement::Plain => {
                format!(
                    "{} — repo root (.git is a directory)",
                    ctx.placement.as_str()
                )
            }
            repo::Placement::MainWorktree => format!(
                "{} — the main worktree at {}",
                ctx.placement.as_str(),
                ctx.shared_root.display()
            ),
            repo::Placement::CommonGitdir => format!(
                "{} — {} lives inside the common gitdir because this is a worktree of a BARE \
                 repository, so there is no main checkout to hold it. Leases and `pact log` work; \
                 `pact msg` does not, and exits 3.",
                ctx.placement.as_str(),
                ctx.state_dir.display()
            ),
            // ok and NOT a warning: this is a healthy, supported topology. It
            // used to be reported as `local-fallback` with a warning about
            // sibling worktrees that do not exist, because a submodule's gitdir
            // has no `commondir` and that read as a broken worktree.
            repo::Placement::Submodule => format!(
                "{} — submodule checkout; coordination is scoped to this submodule, at {}. Its \
                 files belong to a different repository than the superproject's, so a lease on the \
                 same path in each is a lease on a different file.",
                ctx.placement.as_str(),
                ctx.state_dir.display()
            ),
            // Not `CommonGitdir`: the submodule's own checkout is a real,
            // non-bare working tree, so unlike a genuinely bare repo's
            // worktree there IS a main checkout to hold state and run Beads.
            repo::Placement::SubmoduleWorktree => format!(
                "{} — a linked worktree of a submodule; state is shared with that submodule's own \
                 checkout at {}, the same relationship an ordinary worktree has to its main checkout.",
                ctx.placement.as_str(),
                ctx.shared_root.display()
            ),
            repo::Placement::LocalFallback => format!(
                "{} — could not follow this worktree's .git; state is local at {}",
                ctx.placement.as_str(),
                ctx.state_dir.display()
            ),
            // Loud, because this is only ever set on purpose and a repository that
            // has it set by accident is one whose history is going somewhere nobody
            // is looking.
            repo::Placement::StateDirOverride => format!(
                "{} — PACT_STATE_DIR is set, so state is at {} instead of anywhere this \
                 repository's topology would put it. Intended for tests, the fleet harness and \
                 demos; if you did not set it, this repository is writing its history somewhere \
                 unexpected.",
                ctx.placement.as_str(),
                ctx.state_dir.display()
            ),
            repo::Placement::ScopedLocal => format!(
                "{} — PACT_WORKTREE_SCOPE=local put state at {}",
                ctx.placement.as_str(),
                ctx.state_dir.display()
            ),
        },
    });

    // Emitted unconditionally, like every other check here. A check that only
    // appears in the topology it describes cannot be verified in both
    // directions by scripts/check-docs.sh — and, worse, is a check nobody knows
    // to look for until it fires.
    //
    // It matters most for a linked worktree, which depends on a directory
    // somewhere else entirely: one that can be on a read-only mount or owned by
    // another user, neither of which an ordinary checkout can be relative to
    // itself.
    let writable = state_dir_writable(&ctx.state_dir);
    checks.push(DoctorCheck {
        name: "state dir writable",
        ok: writable.is_ok(),
        warn: false,
        detail: match writable {
            Ok(detail) => detail,
            Err(e) => format!("{} is NOT usable: {e}", ctx.state_dir.display()),
        },
    });

    // PACT_STATE_DIR (Placement::StateDirOverride) has no collision detection
    // of its own: point two UNRELATED checkouts at the same override directory
    // by mistake and their leases silently merge into one shared space — no
    // error, no warning, nothing distinguishes "my own history" from "someone
    // else's checkout landed here because an env var was copy-pasted".
    // Checked only under the override: an ordinary repo has no stray directory
    // to compare against, and every other placement rule already puts state
    // somewhere this repository, and only this repository, could have put it.
    //
    // Emitted unconditionally regardless, like every other check here, so it
    // stays checkable both directions by scripts/check-docs.sh and findable
    // before it ever fires.
    let foreign = if ctx.placement == repo::Placement::StateDirOverride {
        foreign_leases(ctx)
    } else {
        Vec::new()
    };
    checks.push(DoctorCheck {
        name: "state dir isolation",
        ok: true,
        warn: !foreign.is_empty(),
        detail: match ctx.placement {
            repo::Placement::StateDirOverride if foreign.is_empty() => format!(
                "PACT_STATE_DIR={} — no signs of another repository sharing it",
                ctx.state_dir.display()
            ),
            repo::Placement::StateDirOverride => format!(
                "PACT_STATE_DIR={} looks shared with a DIFFERENT repository — {} lease(s) do not \
                 match this checkout's topology and most likely came from elsewhere: {}. If that \
                 is not intentional, point PACT_STATE_DIR somewhere unique per repository; two \
                 checkouts sharing one space merge their leases with no other warning.",
                ctx.state_dir.display(),
                foreign.len(),
                foreign.join(", ")
            ),
            _ => "PACT_STATE_DIR is not set — not applicable".to_string(),
        },
    });

    // Emitted unconditionally, same reasoning as `state dir isolation` above:
    // findable before it ever fires. Meaningful only where `.pact/` is
    // actually shared across worktrees (`has_worktrees` covers both
    // directions — the main worktree with siblings, and a linked worktree
    // resolving to the main one) — a solo checkout has no sibling binary that
    // could be resolving a different, unmarked directory for the same repo.
    //
    // A pre-worktree-sharing binary has this feature simply absent from its
    // compiled code, not disabled by a setting, so it cannot be taught to
    // write the marker retroactively — this can only warn that the
    // possibility exists, not rule it out (pact-m7j.9.7).
    let marker_present = ctx.state_dir.join(repo::SCHEMA_FILE).is_file();
    checks.push(DoctorCheck {
        name: "worktree schema marker",
        ok: true,
        warn: ctx.has_worktrees && !marker_present,
        detail: if !ctx.has_worktrees {
            "no worktrees here — not applicable".to_string()
        } else if marker_present {
            format!(
                "{} carries the schema marker — a worktree-aware pact has touched it",
                ctx.state_dir.display()
            )
        } else {
            format!(
                "{} was never touched by a worktree-aware pact — if an older binary (built \
                 before cross-worktree sharing existed) still runs anywhere against this \
                 repository, it resolves its own separate, unmarked .pact/ instead of this one, \
                 with no visibility into this one's leases. Not fixable retroactively; the \
                 mitigation is rebuilding every binary that touches this repository.",
                ctx.state_dir.display()
            )
        },
    });
}

/// Leases under a `PACT_STATE_DIR` override that could not have been written
/// by ANY worktree of this process's own repository (pact-m7j.8.5).
///
/// Two signals, cheapest and most exact first:
///
/// - a lease naming a `worktree`: real worktree names come straight from
///   `.git/worktrees/<name>` (see [`repo::RepoContext`] and
///   `has_linked_worktrees`), so a name absent from that directory could not
///   have been stamped by any worktree of THIS repository, full stop.
/// - a lease with no worktree metadata — an ordinary checkout with no linked
///   worktrees, or an older lock file predating that field — falls back to
///   its `path`. The exact file is not required to exist: leasing a file that
///   does not exist yet is a documented workflow (docs/leases.md, "Working on
///   a new file you can't compile yet"), and that workflow always happens
///   inside a directory that already exists (`src/parser.rs` inside a real
///   `src/`). So the bar is lower — does the path's PARENT directory exist
///   under this checkout — and a path whose parent also does not exist is not
///   explained by that workflow; it most plausibly belongs to a different
///   repository entirely.
///
/// A heuristic, not a proof — same as `beads::conflicting_stores` before it —
/// which is exactly why this only ever warns.
fn foreign_leases(ctx: &repo::RepoContext) -> Vec<String> {
    let known_worktrees = known_worktree_names(ctx);
    lease::peek(&ctx.worktree_root, true)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| match &entry.lease.worktree {
            Some(name) => !known_worktrees.contains(name.as_str()),
            None => {
                let full = ctx.worktree_root.join(&entry.lease.path);
                !full.exists() && !full.parent().is_some_and(Path::exists)
            }
        })
        .map(|entry| format!("{} (held by {})", entry.lease.path, entry.lease.agent))
        .collect()
}

/// Every worktree name this repository's own gitdir could have produced: the
/// main worktree's own directory name, plus every entry under
/// `<shared_root>/.git/worktrees/` — the same directory `has_linked_worktrees`
/// already reads, just enumerated here instead of merely tested for
/// non-emptiness.
///
/// Read from `shared_root`, not `ctx.git_dir`: for a linked worktree
/// `git_dir` is that ONE worktree's own entry
/// (`<common>/worktrees/<name>`), not the directory its siblings are listed
/// under, while `shared_root` is the plain, non-bare checkout every
/// worktree-carrying placement already resolves TO.
fn known_worktree_names(ctx: &repo::RepoContext) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    if let Some(name) = ctx.shared_root.file_name() {
        names.insert(name.to_string_lossy().into_owned());
    }
    if let Ok(entries) = std::fs::read_dir(ctx.shared_root.join(".git").join("worktrees")) {
        names.extend(
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned()),
        );
    }
    names
}

/// Can we actually create and remove a file in the state dir?
///
/// A `metadata` check would pass on a read-only mount and on a directory owned by
/// another user, which are the two ways this realistically fails — so it writes
/// a probe and removes it. The unique-temp-name helper is the event log's, so two
/// concurrent doctors cannot collide on the probe.
///
/// Creates NOTHING when the directory is absent. `pact doctor` is a question, and
/// a question must not mutate (pact-rnc.27): a `create_dir_all` here would have
/// `doctor` quietly laying down `.pact/` in every repo it is ever run in, which
/// is also the one thing `.pact/ present` exists to report.
fn state_dir_writable(state_dir: &Path) -> std::result::Result<String, String> {
    if !state_dir.is_dir() {
        return Ok(format!(
            "{} does not exist yet — nothing to write to until `pact init`",
            state_dir.display()
        ));
    }
    let probe = state_dir.join(crate::events::unique_temp_name("doctor-probe"));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(format!("{} is writable", state_dir.display()))
        }
        Err(e) => Err(format!("cannot write in it: {e}")),
    }
}

fn status_code(c: &DoctorCheck) -> i64 {
    match (c.ok, c.warn) {
        (false, _) => 0,
        (true, true) => 1,
        (true, false) => 2,
    }
}

/// Statuses last exported by this process, so an unchanged report is not
/// re-sent.
static LAST_EXPORT: Mutex<Option<Vec<i64>>> = Mutex::new(None);

/// True the first time, and afterwards only when a verdict actually moved.
/// Takes the slot rather than reading the static so a test can own its own —
/// `cargo test` runs these threads in one process, and a test that raced the
/// global against every other test calling `checks()` would be flaky by design.
fn is_new(slot: &Mutex<Option<Vec<i64>>>, statuses: Vec<i64>) -> bool {
    let Ok(mut last) = slot.lock() else {
        // A poisoned lock means some other thread panicked mid-export. Losing a
        // gauge is not worth propagating that into a doctor run.
        return false;
    };
    if last.as_ref() == Some(&statuses) {
        return false;
    }
    *last = Some(statuses);
    true
}

/// One gauge per check, keyed by check name — `pact doctor` is a point-in-time
/// answer nobody recorded, which is how AGENTS.md sat gitignored-and-uncommitted
/// for the project's whole life waiting for a human to notice (pact-aw7.7).
///
/// Edge-triggered, not level: `tui.rs` calls `checks()` once a second while the
/// Doctor tab is open, and otel buffers every point until the process exits
/// (pact-aw7.9), so exporting unconditionally would grow that buffer all day to
/// say the same thing 3600 times an hour. Check names are a fixed set, so they
/// are safe as an attribute; nothing user-supplied is exported.
fn export(checks: &[DoctorCheck]) {
    if !is_new(&LAST_EXPORT, checks.iter().map(status_code).collect()) {
        return;
    }
    for c in checks {
        otel::gauge(
            "pact.doctor.check.status",
            status_code(c),
            &otel::attrs!["pact.doctor.check" => c.name],
        );
    }
}

/// Whether one committed append-only log will reach a clone.
///
/// One function for both logs (pact-83r.2 / finding 1). They ask the same question about
/// different files, and the events version existed alone while the message store — added
/// in 0.9.0 — had no equivalent. That asymmetry is what let a repo sit gitignored AND
/// carrying a `merge=union` attribute with nothing reporting it.
///
/// Never FAILS, only warns: an ignored log is not a broken repository, only one that will
/// lose this history at the next clone, and the reader can reverse it with `pact init`.
fn clone_reach_check(
    root: &Path,
    path: &'static str,
    name: &'static str,
    what: &str,
) -> DoctorCheck {
    let (ok, warn, detail) = match repo::reach(root, path) {
        repo::Reach::Tracked => (
            true,
            false,
            format!("{path} is tracked — {what} survives a clone"),
        ),
        repo::Reach::Untracked => (
            true,
            false,
            format!("{path} is not ignored and not yet committed — commit it and it travels"),
        ),
        repo::Reach::Ignored { ref source } => (
            true,
            true,
            format!(
                "{path} is ignored by {source}, so every clone of this repo starts with NO \
                 record of {what}. Leases, waits and read cursors are runtime state and SHOULD \
                 stay ignored; this file is one of the two pact cannot derive. Re-run \
                 `pact init` to narrow the rule."
            ),
        ),
        repo::Reach::Unknown => (
            true,
            false,
            format!("cannot ask git whether {path} is tracked"),
        ),
    };
    DoctorCheck {
        name,
        ok,
        warn,
        detail,
    }
}

/// The `Beads audit sidecar` verdict, as a pure function of the two facts it depends
/// on — so every state is testable, and no bd installation gets a vote.
///
/// Every state is `ok: true`. The sidecar is genuinely optional and a repo that does
/// not want it is not broken; what is worth a warning is believing a check runs when
/// it cannot.
///
/// **It reports the file, not the switch, and that is the whole of pact-83r.6.** The
/// question a reader wants answered is "is recording on", and pact cannot answer it:
/// `BD_AUDIT_ENABLED=1` enables the sidecar from the environment of whoever runs bd,
/// leaving nothing in the config for pact to read, while `bd config get
/// audit.enabled` answers for a key nobody set. Between them, a config-derived
/// verdict is wrong in both directions — it calls an env-var-enabled repo "OFF", and
/// calls a repo where somebody typed the key "on" whether or not a row was ever
/// written. So the check reports the artifact pact actually consumes: the export
/// file. Its presence is proof recording happened, and its absence is exactly the
/// condition under which `claim-lease-divergence` finds nothing.
///
/// The cost is the old "on, but stale" state, which is gone. It was never a fact —
/// it was `config get` disagreeing with the filesystem, and it fired on a repo
/// recording perfectly well through the env var. Losing an artefact of a broken
/// probe is the point of replacing the probe rather than adding to it.
///
/// **The remediation names both levers and pre-empts bd's spurious warning**, because
/// that warning is the actual trap here: bd 1.2.1 greets `bd config set audit.enabled
/// true` with `"audit.enabled" is not a recognized config key` and then honours it
/// (verified end to end — see [`beads::interaction_assignees`]). A reader who sees
/// that and concludes the fix failed will go looking for a different one that does
/// not exist.
fn sidecar_check(beads_dir: bool, sidecar: bool) -> DoctorCheck {
    let name = "Beads audit sidecar";
    if !beads_dir {
        return DoctorCheck {
            name,
            ok: true,
            warn: false,
            detail: "no .beads/ — not applicable".to_string(),
        };
    }
    let (warn, detail) = if sidecar {
        (
            false,
            ".beads/interactions.jsonl is present — `pact audit --check \
             claim-lease-divergence` has data to read. Whether bd is still recording is \
             not something pact can see (`BD_AUDIT_ENABLED=1` leaves no trace in bd's \
             config), so check the newest row's date if this repo has been quiet"
                .to_string(),
        )
    } else {
        (
            true,
            "no .beads/interactions.jsonl, so `pact audit --check claim-lease-divergence` \
             finds nothing and `Beads actor attribution` above has no actors — both pass \
             in silence, which is indistinguishable from a clean fleet. Turn recording on \
             with `BD_AUDIT_ENABLED=1` in the environment your agents run bd in, or `bd \
             config set audit.enabled true` to persist it — bd 1.2.1 answers that one \
             with `\"audit.enabled\" is not a recognized config key` and then honours it \
             anyway, so the warning is bd's allowlist being wrong, not the switch \
             failing. bd records from that point, not retroactively"
                .to_string(),
        )
    };
    DoctorCheck {
        name,
        ok: true,
        warn,
        detail,
    }
}

#[cfg(test)]
mod tests {
    fn report(states: &[(bool, bool)]) -> DoctorReport {
        let checks: Vec<DoctorCheck> = states
            .iter()
            .map(|(ok, warn)| DoctorCheck {
                name: "x",
                ok: *ok,
                warn: *warn,
                detail: String::new(),
            })
            .collect();
        DoctorReport {
            healthy: checks.iter().all(|c| c.ok),
            checks,
        }
    }

    /// The count must survive a FAILURE, and it must survive being rendered by the
    /// TUI — `pact ui` built its title from `healthy` alone and so showed
    /// "all checks passed" above a visible `!` on a panel two dozen checks tall.
    #[test]
    fn the_summary_always_counts_the_warnings_it_rendered() {
        assert_eq!(summary(&report(&[(true, false)])), "all checks passed");
        assert_eq!(
            summary(&report(&[(true, false), (true, true)])),
            "all checks passed, 1 warning"
        );
        assert_eq!(
            summary(&report(&[(true, true), (true, true)])),
            "all checks passed, 2 warnings"
        );
        // A warning next to a failure is the case that regressed before: the count
        // used to be dropped on this branch entirely.
        assert_eq!(
            summary(&report(&[(false, false), (true, true)])),
            "some checks failed, 1 warning"
        );
        assert_eq!(summary(&report(&[(false, false)])), "some checks failed");
    }

    use super::*;

    /// (ok, detail) for one check of a fresh report, by value so the caller can
    /// keep it past the temporary.
    fn instruction_check(root: &Path) -> (bool, String) {
        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "other instruction files current")
            .expect("doctor must have an opinion about instruction files");
        (c.ok, c.detail.clone())
    }

    /// Half of pact-4zx: `pact init` managed GEMINI.md and doctor had no
    /// opinion about it, so a hand-edited block inside the markers reported a
    /// healthy repo. Edited *inside* the markers on purpose — deleting the
    /// whole file is the case a "does it exist" check would already catch.
    #[test]
    fn a_hand_edited_instruction_file_is_reported_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_eq!(instruction_check(root), (true, "none present".to_string()));

        std::fs::write(root.join("GEMINI.md"), "# Gemini\n").unwrap();
        agents_md::ensure_instruction_files(root).unwrap();
        assert_eq!(instruction_check(root), (true, "1 up to date".to_string()));

        let gemini = root.join("GEMINI.md");
        let edited = std::fs::read_to_string(&gemini)
            .unwrap()
            .replace("Read `AGENTS.md`", "Read AGENTS.md maybe");
        std::fs::write(&gemini, edited).unwrap();

        let (ok, detail) = instruction_check(root);
        assert!(!ok, "a corrupted managed block must not pass");
        assert!(detail.contains("GEMINI.md"), "{detail}");
    }

    /// Two contracts in one line. The check must EXIST in both builds, because
    /// `scripts/check-docs.sh` compares doctor's names against docs/tui.md as
    /// an exact set and only the default build is walked there. And it must
    /// never fail: `healthy` feeds doctor's exit code, and telemetry — of all
    /// things — does not get to change an exit code that the protocol tells
    /// agents to branch on.
    #[test]
    fn the_otel_export_check_exists_in_both_builds_and_never_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let report = checks(tmp.path());
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "otel export")
            .expect("doctor must say whether telemetry is actually exporting");
        assert!(c.ok, "an otel check must not move doctor's exit code");
        assert!(!c.detail.is_empty());
    }

    /// pact-m7j.4.1: a crash between a lease write's staging file and its
    /// rename into place left `staging-*`/`tmp-*` debris in `.pact/leases/`
    /// that the `corrupt leases` check cannot see (it only looks at `.lock`
    /// files) and nothing else ever revisits.
    #[test]
    fn doctor_surfaces_orphaned_staging_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let leases_dir = repo::pact_dir_path(root).join("leases");
        std::fs::create_dir_all(&leases_dir).unwrap();
        std::fs::write(leases_dir.join("staging-1-ThreadId(1)-1"), b"{}").unwrap();
        std::fs::write(leases_dir.join("tmp-2-ThreadId(1)-2"), b"{}").unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "orphaned staging files")
            .expect("doctor must report orphaned staging files");
        assert!(
            !c.ok,
            "two leftover staging files must not pass: {}",
            c.detail
        );
        assert!(c.detail.contains('2'), "{}", c.detail);
    }

    /// A genuine linked worktree is more than a unit test needs to prove this
    /// check works: `has_worktrees` is decided purely by `.git/worktrees/`
    /// having an entry (see `an_empty_worktrees_dir_does_not_count` in
    /// repo.rs), so a hand-built minimal fixture exercises the same code path
    /// a real one would. `.pact/` is hand-created without calling
    /// `repo::pact_dir` — the function that stamps the marker — to simulate a
    /// directory that predates pact-m7j.9.7 (pact-m7j.9.7's own scope note:
    /// a real cross-worktree fixture is not required).
    #[test]
    fn doctor_warns_about_an_unmarked_pact_dir_in_a_worktree_shared_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("wt")).unwrap();
        std::fs::create_dir_all(root.join(".pact")).unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "worktree schema marker")
            .expect("doctor must report the worktree schema marker");
        assert!(c.ok, "a missing marker warns, it does not fail doctor");
        assert!(c.warn, "{}", c.detail);
        assert!(c.detail.contains("never touched"), "{}", c.detail);
    }

    /// `pact init` only warns about an escaping symlink at write time
    /// (`agents_md::resolve_write_target`); this is the same question asked
    /// without writing anything, so a repo nobody has re-run `init` in is not
    /// silent about it (pact-m7j.9.12).
    #[cfg(unix)]
    #[test]
    fn doctor_names_a_managed_file_that_symlinks_outside_the_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("victim-outside-repo.md");
        std::fs::write(&outside, "not part of this repo\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("AGENTS.md")).unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "write-set symlinks")
            .expect("doctor must report escaping write-set symlinks");
        assert!(c.ok, "an escaping symlink warns, it does not fail doctor");
        assert!(c.warn, "{}", c.detail);
        assert!(c.detail.contains("AGENTS.md"), "{}", c.detail);
        assert!(c.detail.contains("victim-outside-repo.md"), "{}", c.detail);
    }

    /// pact-juz.3: reproduces the field-observed shape — two SEPARATE tools
    /// (`bd init`, `bd setup codex`) each writing their own "Quick Reference"
    /// heading, entirely before pact's own managed block, unaware of the
    /// other. Not a pact bug — pact's own block is what the sibling
    /// "AGENTS.md block current" check already polices — but pact is already
    /// the tool walking this exact file, so it surfaces the duplication.
    #[test]
    fn doctor_names_duplicated_headings_written_by_other_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("AGENTS.md"),
            "# Agent Instructions\n\n\
             <!-- BEGIN BEADS INTEGRATION -->\n## Quick Reference\nbd ready\n<!-- END BEADS INTEGRATION -->\n\n\
             <!-- BEGIN BEADS CODEX SETUP -->\n## Quick Reference\nbd ready\n<!-- END BEADS CODEX SETUP -->\n",
        )
        .unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "no duplicated instruction blocks")
            .expect("doctor must report the new check");
        assert!(c.ok, "advisory only — it must not fail doctor");
        assert!(c.warn, "{}", c.detail);
        assert!(c.detail.contains("Quick Reference"), "{}", c.detail);
        assert!(
            c.detail.contains("BEADS CODEX SETUP"),
            "must name a surrounding marker for context: {}",
            c.detail
        );
    }

    /// A repo whose AGENTS.md only carries pact's own block (the common
    /// case) must report clean — this check must never re-flag pact's own
    /// heading as a duplicate of itself.
    #[test]
    fn doctor_reports_no_duplicates_when_only_pacts_own_block_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        agents_md::apply(root).unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "no duplicated instruction blocks")
            .expect("doctor must report the new check");
        assert!(c.ok && !c.warn, "{}", c.detail);
    }

    /// pact-juz.4: the field-observed shape — several distinct pact agent
    /// identities in `.pact/events.jsonl`, but every `.beads/interactions.jsonl`
    /// entry attributed to a name none of them share, because the agents ran
    /// `bd` directly without `BEADS_ACTOR` set.
    #[test]
    fn doctor_warns_when_multiple_pact_agents_never_appear_as_a_beads_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        lease::acquire(root, "agent-a", "a.rs", 900, false, None).unwrap();
        lease::acquire(root, "agent-b", "b.rs", 900, false, None).unwrap();
        let beads = root.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        std::fs::write(
            beads.join("interactions.jsonl"),
            "{\"actor\":\"Some Human\"}\n",
        )
        .unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "Beads actor attribution")
            .expect("doctor must report Beads actor attribution");
        assert!(c.ok, "pure visibility must never fail doctor's exit code");
        assert!(c.warn, "{}", c.detail);
        assert!(c.detail.contains("BEADS_ACTOR"), "{}", c.detail);
    }

    #[test]
    fn doctor_is_clean_when_a_beads_actor_matches_a_pact_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        lease::acquire(root, "agent-a", "a.rs", 900, false, None).unwrap();
        lease::acquire(root, "agent-b", "b.rs", 900, false, None).unwrap();
        let beads = root.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        std::fs::write(
            beads.join("interactions.jsonl"),
            "{\"actor\":\"agent-a\"}\n",
        )
        .unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "Beads actor attribution")
            .expect("doctor must report Beads actor attribution");
        assert!(c.ok && !c.warn, "{}", c.detail);
    }

    #[test]
    fn doctor_treats_a_missing_interactions_file_as_not_applicable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        lease::acquire(root, "agent-a", "a.rs", 900, false, None).unwrap();
        lease::acquire(root, "agent-b", "b.rs", 900, false, None).unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "Beads actor attribution")
            .expect("doctor must report Beads actor attribution");
        assert!(c.ok && !c.warn, "{}", c.detail);
        assert!(c.detail.contains("not applicable"), "{}", c.detail);
    }

    /// Every state of the sidecar verdict, decided without a bd installation getting
    /// a vote — which is the point of `sidecar_check` being pure. An earlier cut called
    /// `checks()` and so silently asserted whatever the test machine's own bd config
    /// happened to say.
    #[test]
    fn the_sidecar_verdict_warns_exactly_when_a_check_would_silently_not_run() {
        // Nothing to say about a repo with no issue tracker.
        let none = sidecar_check(false, false);
        assert!(none.ok && !none.warn, "{}", none.detail);
        assert!(none.detail.contains("not applicable"));

        for (sidecar, should_warn, why) in [
            (true, false, "the export exists: the check has data to read"),
            (false, true, "absent: the check passes in silence forever"),
        ] {
            let c = sidecar_check(true, sidecar);
            assert!(c.ok, "an optional sidecar must never FAIL doctor: {why}");
            assert_eq!(c.warn, should_warn, "{why}: {}", c.detail);
            assert!(
                c.detail.contains("claim-lease-divergence"),
                "every state must name what depends on it: {}",
                c.detail
            );
        }
    }

    /// pact-83r.6, and why it is worth its own test: the remediation must stay TRUE
    /// and stay COMPLETE.
    ///
    /// bd 1.2.1 rejects `audit.enabled` from its config-key allowlist, prints a
    /// warning, and then honours the key anyway — measured end to end, both
    /// directions, in a throwaway repo. So doctor must keep naming the command
    /// (dropping it strands a reader whose switch works) AND must pre-empt the
    /// warning, or the reader concludes the fix failed and hunts for one that does
    /// not exist. `BD_AUDIT_ENABLED=1` is bd's second lever, needs no config write and
    /// warns about nothing, so omitting it omits the easy answer.
    ///
    /// An earlier cut of this bead asserted the opposite — that the key is inert and
    /// the sidecar cannot be enabled at all — on the strength of a field report nobody
    /// had re-measured. These assertions exist so that claim cannot come back.
    #[test]
    fn the_sidecar_remediation_names_both_levers_and_pre_empts_bds_spurious_warning() {
        let absent = sidecar_check(true, false);

        assert!(
            absent.detail.contains("bd config set audit.enabled true"),
            "the config lever works on bd 1.2.1 and must still be named: {}",
            absent.detail
        );
        assert!(
            absent.detail.contains("BD_AUDIT_ENABLED=1"),
            "the env lever needs no config write and warns about nothing: {}",
            absent.detail
        );
        assert!(
            absent.detail.contains("not a recognized config key"),
            "a reader not warned about bd's warning will think it failed: {}",
            absent.detail
        );
        assert!(
            absent.detail.contains("honours it"),
            "and must be told the warning is spurious, not merely shown it: {}",
            absent.detail
        );

        // The present case must not claim recording is still ON: pact cannot see the
        // env lever, so that is precisely the assertion it is not entitled to make.
        let present = sidecar_check(true, true);
        assert!(
            present.detail.contains("not something pact can see"),
            "a check must not imply it verified what it cannot: {}",
            present.detail
        );
    }

    /// A solo session legitimately collapses to one identity everywhere —
    /// that is not the attribution gap this check exists to catch.
    #[test]
    fn doctor_does_not_warn_for_a_single_pact_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        lease::acquire(root, "agent-a", "a.rs", 900, false, None).unwrap();
        let beads = root.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        std::fs::write(
            beads.join("interactions.jsonl"),
            "{\"actor\":\"Some Human\"}\n",
        )
        .unwrap();

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "Beads actor attribution")
            .expect("doctor must report Beads actor attribution");
        assert!(c.ok && !c.warn, "{}", c.detail);
    }

    /// Two agents blocked on two different paths, neither ever retried nor
    /// released with `--all` — the shape AGENTS.md's own protocol leaves
    /// behind. `#[cfg(feature = "otel")]` because `lease::mark_conflict` only
    /// writes `.pact/waits/` markers in an otel build (pact-m7j.4.6); see
    /// `the_default_build_does_no_telemetry_filesystem_work` in lease.rs for
    /// why the default build must create none at all.
    #[cfg(feature = "otel")]
    #[test]
    fn doctor_reports_stale_wait_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        lease::acquire(root, "agent-a", "hot.rs", 900, false, None).unwrap();
        assert!(lease::acquire(root, "agent-b", "hot.rs", 900, false, None).is_err());
        lease::acquire(root, "agent-x", "warm.rs", 900, false, None).unwrap();
        assert!(lease::acquire(root, "agent-y", "warm.rs", 900, false, None).is_err());

        assert_eq!(lease::marker_count(root).unwrap(), 2);

        let report = checks(root);
        let c = report
            .checks
            .iter()
            .find(|c| c.name == "stale wait markers")
            .expect("doctor must report stale wait markers");
        assert!(c.ok, "pure visibility must never fail doctor's exit code");
        assert!(c.warn, "a nonzero count is worth knowing");
        assert!(c.detail.contains('2'), "{}", c.detail);
    }

    fn check(ok: bool, warn: bool) -> DoctorCheck {
        DoctorCheck {
            name: "x",
            ok,
            warn,
            detail: String::new(),
        }
    }

    /// The whole point of pact-aw7.7: `warn` is a third state, so the gauge has
    /// to have three values. A two-valued one would fold `!` into `✓` and hide
    /// the situation the warn level was just added to surface.
    #[test]
    fn the_gauge_keeps_warn_apart_from_ok_and_fail() {
        assert_eq!(status_code(&check(false, false)), 0);
        assert_eq!(status_code(&check(true, true)), 1);
        assert_eq!(status_code(&check(true, false)), 2);
        // A failing check that also warns is still a failure, exactly as the
        // glyph match in `run_doctor` renders it.
        assert_eq!(status_code(&check(false, true)), 0);
    }

    /// Edge, not level: `pact ui` calls `checks()` once a second and otel holds
    /// every point until the process exits, so an unchanged verdict must cost
    /// nothing.
    #[test]
    fn an_unchanged_report_is_not_exported_twice() {
        let slot = Mutex::new(None);
        assert!(is_new(&slot, vec![2, 2, 1]), "first report always exports");
        assert!(!is_new(&slot, vec![2, 2, 1]), "same verdicts, nothing new");
        assert!(is_new(&slot, vec![2, 0, 1]), "a check went red");
        assert!(
            is_new(&slot, vec![2, 1, 1]),
            "red to warn is still a change"
        );
    }
}
