//! `--check chain-integrity` (pact-m7j.2.5): does every chain-tracked line's
//! `chain_hash` match what it should be, given the line before it?
//!
//! Separate from the other checks on purpose — this one is about the log's own
//! physical integrity, not about lease behaviour, and a line with no
//! `chain_hash` is not a finding here (see `Event::chain_hash`).

use anyhow::Result;

use crate::audit::CheckReport;

/// Verify the log's own hash chain, over the RAW file.
pub(in crate::audit) fn detect(
    repo_root: &std::path::Path,
    report: &mut CheckReport,
) -> Result<()> {
    // The chain is a property of PHYSICAL line adjacency in the raw
    // file, not of `load`'s annotation-filtered, `--since`-narrowed
    // view: an annotation line and anything it covers are still real
    // entries the writer's hash chain ran through. Reads the log a
    // second time rather than reusing `events` above for exactly that
    // reason — `--since`/`--include-annotated` apply to every other
    // check but must not apply to this one.
    let (raw, _) = crate::events::numbered(repo_root)?;
    let (mismatches, tracked, untracked) = crate::events::verify_chain(&raw);
    report.events_scanned = raw.len();
    // These two describe the lease-hold reconstruction this check
    // does not perform; zeroed rather than left showing the filtered
    // view's numbers, which would describe a scan this check never ran.
    report.excluded_by_annotation = 0;
    report.orphaned_closes = 0;
    report.chain_breaks = mismatches;
    report.chain_tracked = tracked;
    report.chain_untracked = untracked;
    Ok(())
}

/// The scope line, informational regardless of findings.
pub(in crate::audit) fn scope(r: &CheckReport, out: &mut Vec<String>) {
    // Informational regardless of findings: a reader needs to know how
    // much of the log this check could even speak to before it says
    // whether that portion is intact — see `Event::chain_hash`'s doc
    // comment on why an untracked line is not itself a finding.
    out.push(format!(
        "  {} line(s) chain-tracked, {} line(s) predate chain tracking or were not written \
         by pact",
        r.chain_tracked, r.chain_untracked
    ));
}

/// What this check prints when it found nothing.
pub(in crate::audit) fn clean() -> String {
    "every chain-tracked line matches the line before it — no gap, edit or forgery \
     detected in the tracked portion of the log"
        .to_string()
}

/// Every break found, and what a non-empty list means.
pub(in crate::audit) fn findings(r: &CheckReport, out: &mut Vec<String>) {
    for m in &r.chain_breaks {
        out.push(String::new());
        out.push(format!("CHAIN BREAK at line {}", m.line));
        out.push(format!("  {} {} at {}", m.agent, m.kind, m.at));
        out.push(format!(
            "  expected chain_hash {}, found {}",
            m.expected, m.found
        ));
    }
    if !r.chain_breaks.is_empty() {
        out.push(String::new());
        out.push(format!(
            "{} line(s) whose chain_hash does not match the line before it — a hand-edited or\n\
             forged line, or the file was altered outside pact. This is about the log's own\n\
             physical integrity and is unrelated to {} line(s) elsewhere that simply predate\n\
             chain tracking or were not written by pact; those are not evidence of tampering by\n\
             themselves.",
            r.chain_breaks.len(),
            r.chain_untracked
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::fixtures::*;
    use crate::audit::{render_check, run_check, Check};

    /// pact-m7j.2.5's acceptance criteria: a hand-edited `chain_hash` — the
    /// shape a forged or tampered line actually takes on disk, since nobody but
    /// `append_bounded` can compute one that verifies — must be flagged, and
    /// flagged distinctly from the genuine lines around it.
    #[test]
    fn a_hand_edited_chain_hash_is_flagged_distinctly_from_genuine_history() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        crate::events::append(tmp.path(), &chain_event("agent-a", "acquired", "src/a.rs"));
        crate::events::append(tmp.path(), &chain_event("agent-a", "released", "src/a.rs"));

        let log_path = tmp.path().join(".pact").join("events.jsonl");
        let contents = std::fs::read_to_string(&log_path).unwrap();
        let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
        assert_eq!(
            lines.len(),
            2,
            "fixture must have written exactly two lines"
        );
        let mut tampered: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        tampered["chain_hash"] = serde_json::Value::String("0000000000000000".to_string());
        lines[1] = tampered.to_string();
        std::fs::write(&log_path, lines.join("\n") + "\n").unwrap();

        let r = run_check(tmp.path(), Check::ChainIntegrity, None, false).unwrap();
        assert_eq!(r.findings(), 1, "exactly the tampered line, nothing else");
        assert_eq!(r.chain_breaks[0].line, 2);
        assert_eq!(r.chain_tracked, 2, "both lines still carry SOME chain_hash");
        assert_eq!(r.chain_untracked, 0);

        let text = render_check(&r);
        assert!(text.contains("CHAIN BREAK"), "{text}");
        assert!(text.contains("line 2"), "{text}");
    }

    /// The other half of the same acceptance criteria: a log with NO
    /// `chain_hash` anywhere — every log written before pact-m7j.2.5, including
    /// this repository's own committed history — must report cleanly. A missing
    /// field is not evidence of tampering; treating it as such would flag every
    /// pre-existing repository the moment this shipped.
    #[test]
    fn a_pre_existing_history_log_with_no_chain_hash_anywhere_reports_cleanly() {
        let tmp = with_log(&[
            &ev("2026-08-01T10:00:00Z", "agent-a", "acquired", "src/a.rs"),
            &ev("2026-08-01T10:05:00Z", "agent-a", "released", "src/a.rs"),
        ]);
        let r = run_check(tmp.path(), Check::ChainIntegrity, None, false).unwrap();
        assert_eq!(
            r.findings(),
            0,
            "no chain_hash anywhere must not read as tampering"
        );
        assert_eq!(r.chain_tracked, 0);
        assert_eq!(r.chain_untracked, 2);

        let text = render_check(&r);
        assert!(!text.contains("CHAIN BREAK"), "{text}");
        assert!(text.contains("predate chain tracking"), "{text}");
    }

    /// A forged line appended with no `chain_hash` of its own — the bead's other
    /// named scenario — is not a mismatch (there is nothing on it to mismatch),
    /// but it must show up as untracked rather than silently extending the
    /// tracked run, so a reader can see tracking stopped where it should not
    /// have.
    #[test]
    fn a_forged_line_with_no_chain_hash_after_a_real_chain_counts_as_untracked_not_a_break() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        crate::events::append(tmp.path(), &chain_event("agent-a", "acquired", "shared.rs"));

        // Hand-appended: a forged "released" for a path a peer still holds,
        // with no chain_hash field at all — exactly what appending via a text
        // editor rather than `pact` produces.
        let log_path = tmp.path().join(".pact").join("events.jsonl");
        let mut forged = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(
            forged,
            "{}",
            ev("2026-08-06T00:00:00Z", "attacker", "released", "shared.rs")
        )
        .unwrap();
        drop(forged);

        let r = run_check(tmp.path(), Check::ChainIntegrity, None, false).unwrap();
        assert_eq!(r.findings(), 0, "a missing hash is not a mismatch");
        assert_eq!(r.chain_tracked, 1);
        assert_eq!(r.chain_untracked, 1);
    }
}
