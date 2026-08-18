//! The event-log fixtures every check's tests are written against.
//!
//! Test-only, and shared rather than per-check on purpose: a log line's shape is
//! the one thing every check agrees on, so a second copy of `ev` is a second
//! place for the wire format to drift. Each builder is the smallest event that
//! will exercise the field it is named for — the checks add their own rows on
//! top.

use chrono::Utc;

use crate::events::Event;

/// Write a log and audit it. Takes raw lines so a test can plant a truncated
/// one, an unknown kind, or outright junk.
pub(in crate::audit) fn with_log(lines: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let pact = tmp.path().join(".pact");
    std::fs::create_dir_all(&pact).unwrap();
    std::fs::write(pact.join("events.jsonl"), lines.join("\n")).unwrap();
    tmp
}

pub(in crate::audit) fn ev(at: &str, agent: &str, kind: &str, path: &str) -> String {
    format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}"}}"#)
}

/// A content-hash-bearing event, for the merge-divergence walk.
pub(in crate::audit) fn ev_hash(
    at: &str,
    agent: &str,
    kind: &str,
    path: &str,
    hash: &str,
) -> String {
    format!(
        r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","content_hash":"{hash}"}}"#
    )
}

/// A refusal with the holder's remaining lease recorded (pact-1gv.1), so the
/// impatience half of retry-storm has something to judge.
pub(in crate::audit) fn ev_refused(
    at: &str,
    agent: &str,
    path: &str,
    holder: &str,
    remaining: i64,
) -> String {
    // ONE line. The log is line-delimited, so a pretty-printed record is two
    // torn lines rather than one event — which is exactly how the first cut of
    // these tests reported zero findings against a log full of refusals.
    format!(
        r#"{{"at":"{at}","agent":"{agent}","kind":"refused","path":"{path}","holder":"{holder}","holder_remaining_secs":{remaining}}}"#
    )
}

/// An acquire whose note names a bead, which is the only thing
/// claim-lease-divergence reads off the event.
pub(in crate::audit) fn ev_note(at: &str, agent: &str, path: &str, note: &str) -> String {
    format!(
        r#"{{"at":"{at}","agent":"{agent}","kind":"acquired","path":"{path}","detail":"{note}"}}"#
    )
}

/// Plant a Beads interactions export beside an existing log (pact-as5.6). Raw
/// lines, so a fixture can plant junk — parse-tolerance is this reader's whole
/// contract.
pub(in crate::audit) fn with_interactions(tmp: &tempfile::TempDir, lines: &[&str]) {
    let beads = tmp.path().join(".beads");
    std::fs::create_dir_all(&beads).unwrap();
    std::fs::write(beads.join("interactions.jsonl"), lines.join("\n")).unwrap();
}

pub(in crate::audit) fn assignee_row(at: &str, issue: &str, new_value: &str) -> String {
    format!(
        r#"{{"id":"int-1","kind":"field_change","created_at":"{at}","actor":"someone","issue_id":"{issue}","extra":{{"field":"assignee","new_value":"{new_value}","old_value":""}}}}"#
    )
}

/// Plant watch records beside an existing log, so a fixture can say what an agent
/// was subscribed to and WHEN.
pub(in crate::audit) fn with_watches(tmp: &tempfile::TempDir, lines: &[&str]) {
    std::fs::write(
        tmp.path().join(".pact").join("watches.jsonl"),
        lines.join("\n"),
    )
    .unwrap();
}

pub(in crate::audit) fn watch_rec(at: &str, agent: &str, kind: &str, path: &str) -> String {
    format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}"}}"#)
}

/// A lease that lapsed is the same smell already realised, whatever its
/// duration.
pub(in crate::audit) fn ev_ttl(at: &str, agent: &str, kind: &str, path: &str, ttl: u64) -> String {
    format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","ttl_secs":{ttl}}}"#)
}

pub(in crate::audit) fn annotation(covers: &[usize], note: &str) -> String {
    let lines: Vec<String> = covers.iter().map(|n| n.to_string()).collect();
    format!(
        r#"{{"at":"2026-08-06T12:00:00Z","agent":"maintainer","kind":"annotation","detail":"{note}","covers_lines":[{}],"actor":"maintainer"}}"#,
        lines.join(",")
    )
}

pub(in crate::audit) fn annotation_with_actor(covers: &[usize], note: &str, actor: &str) -> String {
    let lines: Vec<String> = covers.iter().map(|n| n.to_string()).collect();
    format!(
        r#"{{"at":"2026-08-06T12:00:00Z","agent":"maintainer","kind":"annotation","detail":"{note}","covers_lines":[{}],"actor":"{actor}"}}"#,
        lines.join(",")
    )
}

/// A real, chain-hashed `Event`, built through the same struct pact itself
/// writes rather than through `ev()`'s bare JSON — `chain_hash` is computed
/// by `events::append`, so the fixture must go through it to get one at all.
pub(in crate::audit) fn chain_event(agent: &str, kind: &str, path: &str) -> Event {
    Event {
        at: Utc::now().to_rfc3339(),
        agent: agent.to_string(),
        kind: kind.to_string(),
        path: Some(path.to_string()),
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
    }
}

pub(in crate::audit) fn ev_from(
    at: &str,
    agent: &str,
    kind: &str,
    path: &str,
    from: &str,
) -> String {
    format!(
        r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","invoked_from":"{from}","scope":"shared"}}"#
    )
}

pub(in crate::audit) fn ev_meta(
    at: &str,
    agent: &str,
    kind: &str,
    path: &str,
    extra: &str,
) -> String {
    format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}"{extra}}}"#)
}

// `with_log`'s `.git` is a bare, empty directory — enough to satisfy
// `find_repo_root`, but not a real git repository `git log` can read.
// `Check::CommitCorrelation` needs a REAL repository, so those tests get
// their own fixture that actually runs `git init` and `git commit`.
pub(in crate::audit) fn with_git_log(lines: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success(), "git init failed");
    let pact = tmp.path().join(".pact");
    std::fs::create_dir_all(&pact).unwrap();
    std::fs::write(pact.join("events.jsonl"), lines.join("\n")).unwrap();
    tmp
}

/// Writes (or rewrites) `file` and commits it under `at`, an RFC3339
/// timestamp shared by author and committer date so the fixture's
/// commits line up exactly with the hand-written event timestamps above
/// them.
pub(in crate::audit) fn git_commit(repo: &std::path::Path, file: &str, at: &str) {
    std::fs::write(repo.join(file), at).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_AUTHOR_DATE", at)
            .env("GIT_COMMITTER_NAME", "tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_DATE", at)
            .status()
            .unwrap()
    };
    assert!(run(&["add", file]).success());
    assert!(run(&["commit", "--quiet", "-m", &format!("touch {file}")]).success());
}

/// [`git_commit`] with a `Pact-Agent` trailer, so a fixture can say which
/// agent made a commit — which is the fact git itself cannot supply, since
/// every agent in every fleet so far commits under one git identity.
pub(in crate::audit) fn git_commit_as(repo: &std::path::Path, file: &str, at: &str, agent: &str) {
    if let Some(parent) = repo.join(file).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(repo.join(file), format!("{at} {agent}")).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_AUTHOR_DATE", at)
            .env("GIT_COMMITTER_NAME", "tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_DATE", at)
            .status()
            .unwrap()
    };
    assert!(run(&["add", file]).success());
    assert!(run(&[
        "commit",
        "--quiet",
        "-m",
        &format!("touch {file}"),
        "--trailer",
        &format!("Pact-Agent={agent}"),
    ])
    .success());
}

/// An event carrying `head` on both boundaries, for the range path.
pub(in crate::audit) fn ev_head(
    at: &str,
    agent: &str,
    kind: &str,
    path: &str,
    head: &str,
) -> String {
    format!(r#"{{"at":"{at}","agent":"{agent}","kind":"{kind}","path":"{path}","head":"{head}"}}"#)
}

/// Short HEAD right now, so a fixture can record a hash git will actually resolve.
pub(in crate::audit) fn head_of(repo: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
