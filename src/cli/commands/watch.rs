//! `pact watch` — register, retire and list path subscriptions.
//!
//! Owns the reason this command has no exit code of its own: the registry is
//! append-only and per-agent, so a subscription cannot conflict with anything.
//! There is no contested state here to report, which is what makes `watch add`
//! the safe answer to an exit 2 instead of a retry loop.

use anyhow::Result;
use std::path::Path;

use crate::cli::WatchAction;
use crate::{events, identity, lease, output, repo, watch};

/// `pact watch`: register, retire and list path subscriptions.
///
/// No `--json` special-casing beyond `output::emit`, and no exit code of its
/// own: registering a subscription cannot conflict with anything, because the
/// registry is append-only and per-agent.
pub(in crate::cli) fn run_watch(
    cwd: &Path,
    agent_flag: Option<&str>,
    json: bool,
    action: WatchAction,
) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;
    match action {
        WatchAction::Add { path } => {
            let agent = identity::resolve_agent(agent_flag)?;
            let (stored, prefix) = watch::add(&root, &agent, &path)?;
            // The event log records the subscription too, so `pact log` shows
            // a fleet forming its dependency graph, not just taking locks.
            events::append(
                &root,
                &events::Event {
                    at: chrono::Utc::now().to_rfc3339(),
                    agent: agent.clone(),
                    kind: "watched".to_string(),
                    path: Some(stored.clone()),
                    detail: prefix.then(|| "everything beneath this path".to_string()),
                    ttl_secs: None,
                    covers_lines: None,
                    actor: None,
                    displaced: None,
                    chain_hash: None,
                    invoked_from: None,
                    collected_from: None,
                    scope: None,
                    pact_version: None,
                    content_hash: None,
                    subscriber: None,
                    message_id: None,
                    protocol_hash: None,
                    head: None,
                    holder: None,
                    holder_remaining_secs: None,
                    holder_branch: None,
                    holder_worktree: None,
                    ..Default::default()
                },
            );
            #[derive(serde::Serialize)]
            struct Added {
                path: String,
                prefix: bool,
                agent: String,
            }
            output::emit(
                json,
                &Added {
                    path: stored,
                    prefix,
                    agent,
                },
                |a: &Added| {
                    format!(
                        "watching {}{} — you will be sent {} when a holder releases it",
                        a.path,
                        if a.prefix {
                            "/ (and everything under it)"
                        } else {
                            ""
                        },
                        // A reserved key has no content, so the notice carries the
                        // fact of release and no diff (pact-bsf). Promising a diff
                        // here would be a promise pact cannot keep on exactly the
                        // paths agents subscribe to while blocked.
                        if lease::is_mutex(&a.path) {
                            "word that it is free"
                        } else {
                            "a diff"
                        }
                    )
                },
            );
        }
        WatchAction::Rm { path } => {
            let agent = identity::resolve_agent(agent_flag)?;
            let removed = watch::remove(&root, &agent, &path)?;
            if removed {
                events::append(
                    &root,
                    &events::Event {
                        at: chrono::Utc::now().to_rfc3339(),
                        agent: agent.clone(),
                        kind: "unwatched".to_string(),
                        path: Some(lease::normalize_path(&root, &path)),
                        detail: None,
                        ttl_secs: None,
                        covers_lines: None,
                        actor: None,
                        displaced: None,
                        chain_hash: None,
                        invoked_from: None,
                        collected_from: None,
                        scope: None,
                        pact_version: None,
                        content_hash: None,
                        subscriber: None,
                        message_id: None,
                        protocol_hash: None,
                        head: None,
                        holder: None,
                        holder_remaining_secs: None,
                        holder_branch: None,
                        holder_worktree: None,
                        ..Default::default()
                    },
                );
            }
            #[derive(serde::Serialize)]
            struct Removed {
                path: String,
                removed: bool,
            }
            output::emit(
                json,
                &Removed {
                    path: path.clone(),
                    removed,
                },
                |r: &Removed| {
                    if r.removed {
                        format!("no longer watching {}", r.path)
                    } else {
                        // Says what it found rather than implying it undid
                        // something: a silent no-op reads as success.
                        format!("not watching {} — nothing to remove", r.path)
                    }
                },
            );
        }
        WatchAction::Ls => {
            let watches = watch::active(&root)?;
            output::emit(json, &watches, |ws: &Vec<watch::ActiveWatch>| {
                if ws.is_empty() {
                    return "no watches — `pact watch add <path>` to subscribe".to_string();
                }
                let mut rows = vec![format!("{:<44} {:<20} SINCE", "PATH", "AGENT")];
                for w in ws {
                    rows.push(format!(
                        "{:<44} {:<20} {}",
                        format!("{}{}", w.path, if w.prefix { "/**" } else { "" }),
                        w.agent,
                        w.since
                    ));
                }
                rows.push(String::new());
                rows.push(format!("{} watch(es)", ws.len()));
                rows.join("\n")
            });
        }
    }
    Ok(())
}
