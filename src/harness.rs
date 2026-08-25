//! Who is driving this pact process, and which conversation it belongs to:
//! the harness, the model, and the harness's own session identifiers.
//!
//! These four fields complete an attribution chain that previously stopped at
//! the agent name. `PACT_AGENT` says *which agent* acted; nothing said which
//! program was running it, which model was behind it, or which transcript the
//! action would be found in. A coordination post-mortem that cannot answer the
//! last of those has to infer the binding from timestamps — which is exactly
//! the inference `Event::head` was added to remove for git, and this removes
//! for session records.
//!
//! # Two sources, and the line between them
//!
//! **Declared** facts come from the launcher, via environment variables the
//! spawner sets. [`model`] is declared and *only* declared. pact will not
//! fingerprint a model, will not ask an API which one is in force, and will not
//! spawn a subprocess to find out. The spawner knows what it requested; that is
//! where declaration is cheap and truthful, and everywhere else it is a guess
//! wearing a fact's clothes.
//!
//! **Observed** facts come from the harness's own environment, when it exposes
//! them. [`harness`] and [`harness_session`] are observed. What is observed is
//! recorded in [`docs/harness-detection.md`], with the date of the capture and
//! the exact variable name, because these variables are undocumented and
//! reverse-engineered: they can vanish in any release of any harness.
//!
//! [`docs/harness-detection.md`]: https://github.com/chussenot/pact/blob/master/docs/harness-detection.md
//!
//! # Absence is a value; `"unknown"` is not
//!
//! Every function here returns `Option<String>` and every one of them returns
//! `None` rather than a placeholder. A row with no `model` says "nobody
//! declared one", which is a fact about the run and dates the log to before
//! whoever starts declaring it. A row reading `model: "unknown"` says the same
//! thing while looking like data, and `pact audit`'s models-by-events summary
//! would count it as a model. This is the discipline `ttl_secs`, `chain_hash`
//! and `invoked_from` already follow, for the same reason: a field that cannot
//! distinguish "not applicable" from "not recorded" is worse than no field.
//!
//! # No subprocess, no filesystem
//!
//! Every read here is `std::env::var`. That is deliberate and load-bearing:
//! these are stamped by `events::stamp_context`, which runs on the lease hot
//! path for every event of every kind. `stamp_context` already pays for one
//! `git rev-parse` on the four hold-boundary kinds, and `benches/lease.rs`
//! isolates that cost as its decisive pair precisely so a second subprocess
//! cannot be added here unnoticed.

/// The harness driving this process, when one is detectable.
///
/// `PACT_HARNESS` wins outright: a wrapper that pact cannot see through — `pw`,
/// a container entrypoint, a CI runner — can say what it is, and a declaration
/// beats a fingerprint that is guessing about it.
///
/// The only fingerprint pact carries today is Claude Code's `CLAUDECODE=1`
/// (observed 2026-08-19 in 2.1.235; see docs/harness-detection.md). Other
/// harnesses return `None` until someone captures and documents theirs — which
/// is an invitation, not a limitation, and the doc page says so.
///
/// Deliberately not `CLAUDE_CODE_ENTRYPOINT` or `CLAUDE_CODE_EXECPATH`, both of
/// which were present in the same capture: the first varies by how the session
/// was started and the second embeds a version, so neither is a stable answer to
/// "which harness". `CLAUDECODE` is the one variable whose whole job is to say
/// so.
pub fn harness() -> Option<String> {
    if let Some(declared) = declared("PACT_HARNESS") {
        return Some(declared);
    }
    match std::env::var("CLAUDECODE").as_deref() {
        Ok("1") => Some("claude-code".to_string()),
        _ => None,
    }
}

/// The model behind this agent — declared by the launcher; verified, if ever, by
/// joining session records (see recount).
///
/// `PACT_MODEL` or nothing. There is no fingerprint and there will not be one:
/// see the module docs for why a declared model is the only honest one pact can
/// record, and docs/fleet-patterns.md for the spawner-side pattern that makes
/// the declaration cheap.
pub fn model() -> Option<String> {
    declared("PACT_MODEL")
}

/// The harness's identifier for the conversation this process belongs to.
///
/// `PACT_HARNESS_SESSION`, else the harness's own variable, else absent.
///
/// Gated on [`harness`] returning `claude-code`, and the gate is not
/// bureaucracy: `CLAUDE_CODE_SESSION_ID` is inherited by every child process a
/// Claude Code session ever spawns, including shells the user is driving by hand
/// hours later and other harnesses launched from inside one. Reading it without
/// checking who is actually driving would attribute those to a session they are
/// not part of. An explicit override skips the gate, because a caller that names
/// the session outranks pact's opinion about who is asking.
pub fn harness_session() -> Option<String> {
    fingerprinted("PACT_HARNESS_SESSION", "CLAUDE_CODE_SESSION_ID")
}

/// The harness's identifier for *this* agent within the session — the id that
/// names its transcript, and the key recount joins on.
///
/// **There is no fingerprint for this field, and the absence is measured, not an
/// omission.** On 2026-08-19 a real Claude Code subagent was spawned and dumped
/// its complete environment; the variable-name list was identical to its
/// parent's, `CLAUDE_CODE_SESSION_ID` carried the *parent* session's uuid, and
/// the subagent's own id (`a99940ee56bb11045`, the `<id>` in its transcript
/// `subagents/agent-<id>.jsonl`) appeared nowhere in it. The id exists on disk
/// and in the parent's tool-result metadata. It is not in the child's
/// environment.
///
/// So `PACT_HARNESS_SUBAGENT` is the only way this is ever set: a declaration,
/// from a spawner or a harness that knows the id. Under Claude Code, nothing
/// does, and the field is simply absent there.
///
/// **The id is on disk, and pact does not go and get it.** It names a transcript
/// file, so this function could find it by rummaging through the harness's state
/// directory. It will not, and the reasons are the ones this whole module is
/// built on: that layout is undocumented and reverse-engineered, it is one
/// refactor from breaking, and a coordination tool reading its harness's
/// internals to label its own log entries is a coupling nobody wants to own.
/// Every function here is a `std::env::var` call and nothing else — that is a
/// property of the module, not an accident of what was convenient — and
/// docs/fleet-patterns.md tells fleets the same thing rather than suggesting they
/// scrape it themselves.
///
/// **What that leaves is a statement about the contract, not a forecast about a
/// consumer.** A join keyed on this field needs it present on both sides; under
/// Claude Code it is present on neither, so a keyed answer is not available and a
/// consumer must reach it some other way or say it could not.
///
/// An earlier version of this comment went further and predicted the shape of
/// recount's join — that its keyed tier would be rare and its topological ladder
/// would stay load-bearing. The modmill run measured the opposite: keyed fired on
/// 3 of 3 findings and the topological ladder ran 0 times. The measurement behind
/// the prediction was right — the field really is absent on every row — but the
/// prediction was not pact's to make, because what a consumer keys on is the
/// consumer's design and not a property of this field. Sentences like that read as
/// established fact to the next person (pact-k1n.5).
pub fn harness_subagent() -> Option<String> {
    fingerprinted("PACT_HARNESS_SUBAGENT", "")
}

/// Everything about this process's attribution, resolved once.
///
/// A struct rather than four calls at every consumer, because doctor, the event
/// stamp, the message stamp and the TUI all want the whole chain and a caller
/// that forgets one field produces a row that is silently less attributable
/// than its neighbours.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attribution {
    pub harness: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub subagent: Option<String>,
}

impl Attribution {
    pub fn resolve() -> Self {
        Self {
            harness: harness(),
            model: model(),
            session: harness_session(),
            subagent: harness_subagent(),
        }
    }

    /// `[claude-code, sonnet-4-6]`, or nothing at all when neither is known.
    ///
    /// The bracketed form for a holder line and a `lease ls` row. Empty rather
    /// than `[unknown]` when there is nothing to say, so an un-declared fleet's
    /// output is exactly what it was before this field existed rather than a
    /// column of placeholders.
    pub fn badge(&self) -> Option<String> {
        match (&self.harness, &self.model) {
            (Some(h), Some(m)) => Some(format!("[{h}, {m}]")),
            (Some(h), None) => Some(format!("[{h}]")),
            (None, Some(m)) => Some(format!("[{m}]")),
            (None, None) => None,
        }
    }
}

/// An environment variable read as a declaration: present, and not blank.
///
/// The blank check matters more than it looks. `PACT_MODEL=` in a launcher
/// script — an unset shell variable interpolated into an export — is how an
/// empty string reaches here, and an empty declared model is not a declaration.
/// Trimmed for the same reason: `PACT_AGENT` is validated, these are free text,
/// and a trailing newline from `$(...)` would otherwise be part of the value and
/// break every equality join downstream.
fn declared(var: &str) -> Option<String> {
    let raw = std::env::var(var).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Override, then the harness's own variable if the harness is one pact
/// recognises, then absent.
///
/// `fingerprint` may be empty, which means "no fingerprint documented for this
/// field" and short-circuits to `None` — see [`harness_subagent`], where that is
/// a measured fact rather than a gap.
fn fingerprinted(override_var: &str, fingerprint: &str) -> Option<String> {
    if let Some(declared) = declared(override_var) {
        return Some(declared);
    }
    if fingerprint.is_empty() || harness().as_deref() != Some("claude-code") {
        return None;
    }
    declared(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here mutates process-global environment, and cargo runs a
    /// module's tests on several threads of ONE process. Two tests setting
    /// `CLAUDECODE` at the same time would flake against each other in a way
    /// that looks like a logic bug, so they serialize on this.
    ///
    /// A plain `Mutex` rather than a crate: this is the only place in pact that
    /// needs it, and `identity.rs`'s one env test predates the problem by being
    /// alone.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `f` with exactly `vars` set and every other variable this module
    /// reads unset, then restores nothing — the guard's whole point is that the
    /// next test sets its own world.
    fn with_env<T>(vars: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        // Poisoning is irrelevant here: the mutex guards no data, only order.
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        for var in [
            "PACT_HARNESS",
            "PACT_MODEL",
            "PACT_HARNESS_SESSION",
            "PACT_HARNESS_SUBAGENT",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
        ] {
            std::env::remove_var(var);
        }
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        f()
    }

    #[test]
    fn nothing_declared_and_no_harness_is_absent_everywhere() {
        with_env(&[], || {
            assert_eq!(Attribution::resolve(), Attribution::default());
            assert_eq!(Attribution::default().badge(), None);
        });
    }

    #[test]
    fn claude_code_is_fingerprinted_from_claudecode() {
        with_env(&[("CLAUDECODE", "1")], || {
            assert_eq!(harness().as_deref(), Some("claude-code"));
        });
        // Only the exact value. `CLAUDECODE=0` is a harness saying no.
        with_env(&[("CLAUDECODE", "0")], || assert_eq!(harness(), None));
        with_env(&[("CLAUDECODE", "true")], || assert_eq!(harness(), None));
    }

    #[test]
    fn pact_harness_overrides_the_fingerprint() {
        with_env(&[("CLAUDECODE", "1"), ("PACT_HARNESS", "pw")], || {
            assert_eq!(harness().as_deref(), Some("pw"));
        });
    }

    #[test]
    fn model_is_declared_only() {
        with_env(&[("CLAUDECODE", "1")], || assert_eq!(model(), None));
        with_env(&[("PACT_MODEL", "sonnet-4-6")], || {
            assert_eq!(model().as_deref(), Some("sonnet-4-6"));
        });
    }

    #[test]
    fn session_id_is_read_only_when_the_harness_is_claude_code() {
        // The gate's whole reason: this variable is inherited by every child of
        // a Claude Code session, including ones that are not it.
        with_env(&[("CLAUDE_CODE_SESSION_ID", "abc")], || {
            assert_eq!(harness_session(), None);
        });
        with_env(
            &[("CLAUDECODE", "1"), ("CLAUDE_CODE_SESSION_ID", "abc")],
            || assert_eq!(harness_session().as_deref(), Some("abc")),
        );
        // A different harness declared explicitly re-closes the gate, even
        // though the variable is still there.
        with_env(
            &[
                ("CLAUDECODE", "1"),
                ("PACT_HARNESS", "codex"),
                ("CLAUDE_CODE_SESSION_ID", "abc"),
            ],
            || assert_eq!(harness_session(), None),
        );
    }

    #[test]
    fn an_override_skips_the_harness_gate() {
        with_env(&[("PACT_HARNESS_SESSION", "s-1")], || {
            assert_eq!(harness_session().as_deref(), Some("s-1"));
            assert_eq!(harness(), None, "and does not invent a harness");
        });
    }

    #[test]
    fn subagent_has_no_fingerprint_only_an_override() {
        // Measured 2026-08-19: a real subagent's environment carries the
        // PARENT's session id and nothing of its own. Even under the harness it
        // was captured from, there is nothing to read.
        with_env(
            &[("CLAUDECODE", "1"), ("CLAUDE_CODE_SESSION_ID", "parent")],
            || assert_eq!(harness_subagent(), None),
        );
        with_env(&[("PACT_HARNESS_SUBAGENT", "a99940ee56bb11045")], || {
            assert_eq!(harness_subagent().as_deref(), Some("a99940ee56bb11045"));
        });
    }

    #[test]
    fn blank_and_padded_declarations_are_not_declarations() {
        with_env(&[("PACT_MODEL", "")], || assert_eq!(model(), None));
        with_env(&[("PACT_MODEL", "   ")], || assert_eq!(model(), None));
        // A `$(...)` newline must not become part of the value: recount joins
        // on equality.
        with_env(&[("PACT_MODEL", "sonnet-4-6\n")], || {
            assert_eq!(model().as_deref(), Some("sonnet-4-6"));
        });
    }

    #[test]
    fn badge_degrades_one_field_at_a_time() {
        let both = Attribution {
            harness: Some("claude-code".into()),
            model: Some("sonnet-4-6".into()),
            ..Default::default()
        };
        assert_eq!(both.badge().as_deref(), Some("[claude-code, sonnet-4-6]"));
        assert_eq!(
            Attribution {
                harness: Some("claude-code".into()),
                ..Default::default()
            }
            .badge()
            .as_deref(),
            Some("[claude-code]")
        );
        assert_eq!(
            Attribution {
                model: Some("sonnet-4-6".into()),
                ..Default::default()
            }
            .badge()
            .as_deref(),
            Some("[sonnet-4-6]")
        );
    }

    /// The no-filesystem rule, enforced against the source rather than trusted to
    /// a comment.
    ///
    /// Every function here is a `std::env::var` call and nothing else, and the
    /// temptation to break that is specific and real: the subagent id is not in
    /// any environment variable, but it IS on disk — it names the transcript file
    /// — so the obvious next commit is one that goes and reads
    /// `~/.claude/projects/<...>/subagents/` to fill the field in. That layout is
    /// undocumented and reverse-engineered; a coordination tool reaching into its
    /// harness's state directory to label its own log rows is a coupling nobody
    /// wants to own, and it would break silently on any refactor of a program pact
    /// does not control.
    ///
    /// The cost side is separate and also real: these run in
    /// `events::stamp_context`, on the lease hot path, for every event of every
    /// kind. `benches/lease.rs` isolates the one subprocess already there so a
    /// second cannot arrive unnoticed; this catches the cheaper mistake of a
    /// filesystem read, which a benchmark would show as noise rather than as a
    /// spike.
    ///
    /// Scanned as text over the non-test half of the file, which is crude and
    /// exactly proportionate: the rule is "this module does not reach outside its
    /// own process", and a reviewer adding an import is who it needs to stop.
    #[test]
    fn this_module_never_reaches_outside_its_own_process() {
        let src = include_str!("harness.rs");
        let body = src
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(src);
        for forbidden in [
            "std::fs",
            "std::process",
            "Command",
            "File::",
            "read_to_string",
            "read_dir",
            "PathBuf",
            "home_dir",
        ] {
            assert!(
                !body.contains(forbidden),
                "harness.rs must resolve everything from the environment, and this \
                 reaches outside the process: {forbidden}. The subagent id is on \
                 disk and pact still does not go and get it — see \
                 `harness_subagent` and docs/harness-detection.md."
            );
        }
    }
}
