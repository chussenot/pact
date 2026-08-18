use anyhow::Result;
use std::path::Path;

use crate::cli::util::{one_line, since, table};
use crate::{events, msg, output, repo};

/// One row of the activity feed. Deliberately one flat shape for both sources,
/// because the question `pact log` answers — "is the fleet alive and what is it
/// doing" — does not care which storage a fact came from.
#[derive(serde::Serialize)]
struct LogEvent {
    at: String,
    agent: String,
    kind: String,
    /// The leased path, or the recipient of a message.
    target: Option<String>,
    detail: Option<String>,
}

/// pact-rnc.13: lease events and messages in ONE chronological stream, so
/// nobody has to `ls .pact/leases/` and `bd list --json | jq` to find out what
/// happened — the anti-pattern docs/architecture.md warns against.
///
/// The two halves have different histories and that is fine, not an error:
/// messages are derived from bd, so they go back as far as the repo does, while
/// `.pact/events.jsonl` only starts when a lease was first taken after this
/// feature shipped. An existing repo therefore shows message history with no
/// lease history until the next acquire; an empty (or missing) feed is normal.
/// bd is optional the same way it is for `pact agents`: without it you still
/// get the lease half, with a warning.
pub(in crate::cli) fn run_log(cwd: &Path, json: bool, limit: usize) -> Result<()> {
    let root = repo::find_repo_root(cwd)?;

    let mut feed: Vec<LogEvent> = events::recent(&root, limit)?
        .into_iter()
        .map(|e| LogEvent {
            at: e.at,
            agent: e.agent,
            kind: e.kind,
            target: e.path,
            detail: e.detail,
        })
        .collect();

    match msg::all_messages(&root) {
        Ok(messages) => feed.extend(messages.into_iter().map(|m| LogEvent {
            at: m.created_at,
            agent: m.from,
            kind: "message".to_string(),
            target: Some(m.to),
            detail: Some(m.subject.unwrap_or(m.body)),
        })),
        Err(e) => output::warn(&format!("warning: message history unavailable: {e:#}")),
    }

    // Parsed instants, not string order: bd stamps end in `Z` and pact's in
    // `+00:00`, which sort differently as bytes than as time (pact-rnc.20).
    feed.sort_by_key(|e| instant(&e.at));
    if feed.len() > limit {
        feed.drain(..feed.len() - limit);
    }

    output::emit(json, &feed, |feed: &Vec<LogEvent>| render_log(feed));
    Ok(())
}

/// Sortable instant. Unparsable stamps sort oldest, so a corrupt line ends up
/// out of the way instead of pretending to be the latest news.
fn instant(rfc3339: &str) -> (i64, u32) {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => (t.timestamp(), t.timestamp_subsec_nanos()),
        Err(_) => (i64::MIN, 0),
    }
}

/// The feed, oldest last — a log reads top-to-bottom, newest at the bottom
/// where a terminal leaves the cursor. Ages rather than timestamps, because the
/// question is "is this happening now" (same reasoning as `pact agents`).
fn render_log(feed: &[LogEvent]) -> String {
    if feed.is_empty() {
        return "no activity recorded yet".to_string();
    }
    let mut rows = vec![vec![
        "WHEN".to_string(),
        "AGENT".to_string(),
        "EVENT".to_string(),
        "TARGET".to_string(),
        "DETAIL".to_string(),
    ]];
    rows.extend(feed.iter().map(|e| {
        vec![
            since(&e.at),
            e.agent.clone(),
            e.kind.clone(),
            e.target.clone().unwrap_or_default(),
            one_line(e.detail.as_deref().unwrap_or(""), 50),
        ]
    }));
    format!("{}\n\n{} event(s), oldest first", table(&rows), feed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_event(at: &str, agent: &str, kind: &str, target: &str) -> LogEvent {
        LogEvent {
            at: at.to_string(),
            agent: agent.to_string(),
            kind: kind.to_string(),
            target: Some(target.to_string()),
            detail: Some("wiring the CLI".to_string()),
        }
    }

    /// pact-rnc.13 + pact-rnc.20: the feed merges two sources that stamp time
    /// differently — bd writes `Z`, pact writes `+00:00` — and a byte compare
    /// interleaves them wrongly. `+02:00` is the trap: its digits are the
    /// largest while its instant is the earliest.
    #[test]
    fn log_merges_both_sources_in_real_time_order() {
        let mut feed = vec![
            log_event("2026-07-31T09:00:05Z", "msg-fix", "message", "cli-wire"),
            // 08:59:00Z — the earliest instant, but the largest byte string.
            log_event("2026-07-31T10:59:00+02:00", "lease-fix", "acquired", "l.rs"),
            log_event("2026-07-31T09:00:00+00:00", "cli-wire", "acquired", "m.rs"),
        ];
        feed.sort_by_key(|e| instant(&e.at));
        let order: Vec<&str> = feed.iter().map(|e| e.agent.as_str()).collect();
        assert_eq!(order, vec!["lease-fix", "cli-wire", "msg-fix"]);
        let mut by_bytes = feed.iter().map(|e| e.at.clone()).collect::<Vec<_>>();
        by_bytes.sort();
        assert_ne!(
            by_bytes.first().map(String::as_str),
            Some(feed[0].at.as_str()),
            "a string sort really does produce a different order here"
        );
        assert!(
            instant("not a timestamp") < instant("2026-07-31T09:00:00Z"),
            "garbage sorts out of the way, not to the top of the news"
        );

        let out = render_log(&feed);
        let rows: Vec<&str> = out.lines().take(4).collect();
        assert!(
            rows[1].contains("lease-fix") && rows[1].contains("acquired"),
            "{out}"
        );
        assert!(
            rows[3].contains("message") && rows[3].contains("cli-wire"),
            "{out}"
        );
        assert!(out.contains("3 event(s)"), "{out}");
        // An existing repo has message history and an empty lease feed; that
        // is normal, not an error.
        assert_eq!(render_log(&[]), "no activity recorded yet");
    }
}
