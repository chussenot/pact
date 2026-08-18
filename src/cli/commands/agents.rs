use anyhow::Result;
use std::path::Path;

use crate::cli::util::{age_of, since, table};
use crate::lease::human_secs;
use crate::{agents, events, output, repo};

pub(in crate::cli) fn run_agents(cwd: &Path, json: bool, for_path: Option<&str>) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;

    // "Whose file is this?" — the question `lease ls` could never answer once a
    // lease was released, and the scriptable half of pact-o38. Answered from the
    // event log, so there is no registry to keep in sync.
    if let Some(path) = for_path {
        #[derive(serde::Serialize)]
        struct OwnerReport<'a> {
            path: &'a str,
            agent: Option<String>,
            /// `acquired` / `released` / `renewed` / `expired` / `stolen`.
            last: Option<String>,
            at: Option<String>,
            note: Option<String>,
        }
        let owner = events::owner_of(&root, path)?;
        let report = OwnerReport {
            path,
            agent: owner.as_ref().map(|o| o.agent.clone()),
            last: owner.as_ref().map(|o| o.kind.clone()),
            at: owner.as_ref().map(|o| o.at.clone()),
            note: owner.as_ref().and_then(|o| o.detail.clone()),
        };
        // Exits 0 with agent: null when nobody has touched it. "No owner" is an
        // answer, not a failure — the same reason `whoami` never raises.
        output::emit(json, &report, |r: &OwnerReport| match &r.agent {
            None => format!("{}: no agent has acted on this path", r.path),
            Some(agent) => {
                let ago =
                    r.at.as_deref()
                        .and_then(age_of)
                        .map(|s| format!("{} ago", human_secs(s)))
                        .unwrap_or_else(|| "at an unknown time".to_string());
                let note = r
                    .note
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| format!("\n  note: {n}"))
                    .unwrap_or_default();
                format!(
                    "{}: {agent} ({} {ago}){note}",
                    r.path,
                    r.last.as_deref().unwrap_or("acted")
                )
            }
        });
        return Ok(());
    }

    // bd is optional here, exactly as it is for `pact lease`: without it we can
    // still name whoever holds a lease. `locate()` covers bd being absent;
    // `agents::list` covers bd being present but unable to answer, folding that
    // into the same lease-only listing with a warning on stderr. So this `?` is
    // now only for unreadable lease files (pact-rnc.6).
    let found = agents::list(&root)?;

    output::emit(json, &found, |found: &Vec<agents::AgentInfo>| {
        if found.is_empty() {
            return "no agents seen in this repo yet".to_string();
        }
        let mut rows = vec![vec![
            "AGENT".to_string(),
            "LAST SEEN".to_string(),
            "LEASES".to_string(),
            "SENT".to_string(),
            "RECV".to_string(),
        ]];
        rows.extend(found.iter().map(|a| {
            vec![
                // AGENTS.md tells agents to check a recipient with this command,
                // so a name that has only ever been addressed must not read as
                // an agent — otherwise the check confirms typos (pact-rnc.5).
                // A grammar-invalid name gets a distinct flag rather than the
                // same "?": it is not a typo of something real, it is a string
                // no `pact` process could ever run under — planted straight
                // into bd/br by something other than pact (pact-m7j.6.3).
                if !a.name_valid {
                    format!("{} [INVALID]", a.name)
                } else if a.answers() {
                    a.name.clone()
                } else {
                    format!("{} ?", a.name)
                },
                since(&a.last_seen),
                a.leases_held.to_string(),
                a.messages_sent.to_string(),
                a.messages_received.to_string(),
            ]
        }));
        let mut out = table(&rows);
        if found.iter().any(|a| !a.name_valid) {
            out.push_str(
                "\n\n[INVALID] fails pact's identity grammar [a-z0-9][a-z0-9-]{1,31} — written \
                 straight into the shared store, not through pact; never a real identity",
            );
        }
        if found.iter().any(|a| a.name_valid && !a.answers()) {
            out.push_str(
                "\n\n? addressed but never seen acting — nobody has ever run pact under \
                 that name, so nobody is reading its mail (usually a typo'd --to)",
            );
        }
        out
    });
    Ok(())
}
