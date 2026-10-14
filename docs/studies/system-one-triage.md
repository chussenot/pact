---
title: System One triage
description: Whether calibrated System One verdicts, through the judgment crate, can help agents coordinate through pact — where they fit, where they must never go, and what one live measurement on pact's own mail says.
audience: contributors
---

# System One triage

**Question.** A [System One](https://github.com/chussenot/judgment/blob/main/docs/concepts/system-one.md)
model does not generate text: it answers typed questions about a JSON state with
calibrated probabilities, and the [judgment](https://github.com/chussenot/judgment)
crate turns those into verdicts a program can gate on, test without a key and tune
against labelled cases. Is there a place in pact where such a verdict helps agents
coordinate?

**Answer.** One, and it is narrower than it looks: **ranking authored mail by what
it needs from its reader**, on the read side only. Measured on pact's own 33
messages against the hosted model, the probabilities order the messages well (ROC
AUC 0.89 to 0.97) and threshold badly at the default 0.5. Everywhere else pact
already has a fact, and a probability would replace a fact with a guess. The
recommendation is to compose pact and `jud` from outside, with no code in pact,
until a larger labelled corpus says the ranking is worth a feature
([`pact-6qj`](#follow-ups)).

## What rules most uses out

pact's own invariants decide this before any model is asked anything.

- **Attribution, never judgment.** The phrase recurs through the audit and doctor
  code. No check fails on an inference, and none should start.
- **Exit codes are API** ([cli.md](../cli.md)). A gate whose input is a probability
  makes them non-deterministic.
- **pact has already deleted a heuristic like this.** Whether a message was a watch
  notice used to be guessed from English in its subject; it is now the stored
  `kind` field, and the guess is gone ([messaging.md](../messaging.md#a-notification-is-not-correspondence)).
  A model must never answer a question pact can answer by recording a fact.
- **No runtime dependency, no async runtime.** The `otel` feature hand-writes OTLP
  over `std::net` rather than take tokio. judgment's default `http` feature is
  reqwest and tokio, and its `SystemOne` trait is async.
- **The messaging evidence.** *"The load-bearing messages are a tiny minority and
  cannot be predicted. Any rule that suppresses volume suppresses that one too"*
  ([field-runs.md](field-runs.md#what-the-four-runs-actually-say-about-messaging)).
  So a verdict may reorder and escalate. It may never hide, drop or discourage a
  message.

The last point also rules out the **send side**. Telling a sender "this reads like
a status ping" is the restrained-prose experiment again, which collapsed messaging
to four messages between 28 agents, one of them the reason a panic did not ship
([dogfooding.md](dogfooding.md#the-protocol-block-was-tuned-twice-in-opposite-directions-and-overshot-both)).

## Where a verdict could go, ranked

| # | Use | Verdict | Why |
|---|---|---|---|
| 1 | **Rank authored mail** for whoever reads a fleet's inbox: an orchestrator, a human, `pact ui`, an MCP triage agent | **Strong** | The problem is measured: a `BLOCKER` unread for 38 minutes among 41 status pings; 11 automatic notices to one authored message; *"Read 10 hours late — sorry. You were right to ask"* in this repository's own store. A ranking surfaces the minority without cutting volume, which no prose rule managed |
| 2 | **Alarm on an unread warning** in `pact doctor` | **Strong, as #1's action** | The fleet-liveness shape: act only when the guess (`breaks_something` over its bar) and a recorded fact (unread for N minutes) agree |
| 3 | **Label run corpora for audit and the studies** (share of status pings, of warnings) | **Good, lowest risk** | Automates the hand-labelling this page's siblings did. Offline, and reproducible only from recordings |
| 4 | **Watch notices**: did this diff change an interface I depend on? | **Speculative** | No measurement of Jev on code diffs exists; per-path coalescing already took most of the volume; for Rust, a semver checker answers exactly |
| 5 | **`plan lint` build coupling** ([file-disjoint is not build-disjoint](../fleet-patterns.md#file-disjoint-is-not-build-disjoint)) | **Weak** | A manifest holds paths only. A static reference graph is exact where a model would guess |
| 6 | Liveness, `lease sweep --suspect`, steals | **No** | The model would see the facts the rule already sees, and a wrong reclaim costs a live agent's work |
| 7 | `merge --verify`, gates, `double-win`, exit codes | **No** | Deterministic by nature |

One more use needs no model at all: `pact handoff --confidence high|medium|low` is
self-reported and never measured. Relating each tier to an outcome pact can derive
is judgment's *metrics*, not its model, and is a few lines in audit
([`pact-6qj.5`](#follow-ups)).

## The measurement

The rubric is judgment's
[`examples/jud/agent-mail.jud`](https://github.com/chussenot/judgment/blob/main/examples/jud/agent-mail.jud):
three Nouls about one message, `{subject, body}`.

| Question | Asks |
|---|---|
| `needs_action` | does it ask the recipient to decide, approve, answer, run a check, make an edit, or check their own work? |
| `breaks_something` | does it warn that something the recipient owns or relies on is broken, lost, changed or wrong? |
| `status_only` | is it only a report that work is done or under way? |

Three Nouls rather than one Choice, because real mail is multi-label: *"activity.rs
landed (054fc51) — gate please, and one thing fleet.rs should know"* asks for
something and warns about something, and a Choice over {status, request, warning}
would drop one.

**Corpus.** Every authored message in this repository's `.pact/messages.jsonl` on
2026-10-14: 33 of them, subject and body verbatim, the 11 watch notices left out
(they are a stored `kind`, not a question). One annotator labelled them after reading
each whole message; three are marked as close calls.
[`agent-mail-cases.jud`](https://github.com/chussenot/judgment/blob/main/examples/jud/agent-mail-cases.jud)
holds them.

**Run.** `jud record` against `https://api.typesafe.ai`, model `jev-latest`, which
answered as `jev-1.13.0`: 33 requests in 5.5 s, median 144 ms and worst 526 ms
each, 39,374 input tokens in all (median 1,080 a message, largest 2,730), 60 output
tokens each. The same 33 requests asked again moved no probability by more than
0.06 and flipped no verdict at 0.5.

| Question | Labelled yes | Accuracy at 0.5 | ROC AUC | Bar `jud tune` proposes | Accuracy there | Precision / recall there |
|---|---|---|---|---|---|---|
| `needs_action` | 23 of 33 | 0.85 (0.69 to 0.93) | 0.90 | 0.45 | 0.85 | 0.85 / 0.96 |
| `breaks_something` | 14 of 33 | 0.61 (0.44 to 0.75) | 0.89 | 0.75 | 0.79 | 0.68 / 0.93 |
| `status_only` | 4 of 33 | 0.88 (0.73 to 0.95) | 0.97 | 0.15 | 0.97 | 0.80 / 1.00 |

Intervals are 95% Wilson intervals, from `jud eval`.

What it says:

- **The order is the result; the default bar is not.** At 0.5, `breaks_something`
  says yes to 25 of 33 messages and `jud eval` cannot show it beats always
  answering no. Sorted by the same probabilities, the eight highest are all
  labelled warnings, and the first non-warning is ninth. A ranker needs exactly
  this property and no bar at all; an alarm needs a bar moved to about 0.75.
- **The message that sat unread for ten hours scores 0.97 / 0.96.** *"pact-8ws.1
  needs one line out of main.rs"* (`needs-one-line`) is 0.97 on `needs_action` and
  0.96 on `breaks_something`: tied third of 33 on the warning question, and in the
  top ten on a question 23 of the 33 messages say yes to. A ranked inbox would
  have put it on the first screen.
- **The model never says "status only" above 0.5.** The four status reports score
  between 0.17 and 0.45, and only one of the other 29 scores among them
  (`bump-is-yours`, 0.29, which hands over a release). Read as a yes/no question at
  0.5 it is useless; ranked, it separates them almost perfectly.
- **The one real miss is defensible.** `swept-staged-deletion` scores 0.41 on
  `breaks_something`: the sender writes that the damage is *already undone*, and
  the label says it still changed the recipient's staged state. A second annotator
  might side with the model.

What it does not say:

- **The bars are in-sample.** They were read off the same 33 cases they are scored
  on, with no held-out split, and the rubric's wording was written after reading the
  messages. ROC AUC needs no bar and is the less optimistic number, but it is still
  one corpus.
- **This corpus is the restrained one.** It postdates the protocol that reserves
  messages for what needs an answer, so only 4 of 33 are status reports. The run
  where 41 of 85 messages were status pings is where ranking would matter most, and
  it is not in this set.
- **The hosted API means agent mail leaves the machine.** Messages carry code and
  file paths. The local alternative, tev1 (0.8B parameters on Ollama), has not been
  measured on this rubric.

## How it should be wired, if it is

1. **Now, and possibly for good: compose.** `pact msg inbox --json` and `pact msg
   read <id> --json` carry `subject`, `body` and `notice`; a script or an MCP
   triage agent hands each authored message to `jud examples/jud/agent-mail.jud`
   and sorts. Every invariant above holds, because pact runs no model and calls
   nothing ([`pact-6qj.3`](#follow-ups)). This ran against the live model on a
   scratch repository with one status report and one warning in the inbox:

   ```bash
   pact msg inbox --json \
     | jq -c '.[] | select(.notice | not) | {id, state: {subject, body}}' \
     | while read -r m; do
         jq -c .state <<<"$m" | jud agent-mail.jud \
           | jq -c --arg id "$(jq -r .id <<<"$m")" \
               '{id: $id, breaks: .breaks_something.probability, action: .needs_action.probability}'
       done | jq -s 'sort_by(-.breaks)'
   ```

   One request per message, so a long inbox wants a cache keyed by message id.
2. **Only if a labelled corpus earns it: an optional feature**, read by `pact ui`,
   `pact doctor` and `pact audit`, never by `msg inbox` uncached and never by
   anything that sets an exit code. Link judgment with `--no-default-features`
   (serde, indexmap, sha2, tracing; no tokio) behind a blocking transport, the way
   `otel` avoided its SDK. Cache verdicts by message id: ids are derived from
   content, so a cached verdict cannot go stale. Do not shell out to `jud` on a
   command's path: a subprocess boundary insulates from storage, never from what a
   command means ([the bd lesson](../architecture.md#and-since-090-no-backend-at-all)),
   and judgment is 0.x, where a minor release may break
   ([`pact-6qj.4`](#follow-ups)).

**Decision criteria for step 2**, on a held-out split of a field-run corpus:
`breaks_something` recall of at least 0.9 at a precision an operator accepts, and
tev1 close enough to the hosted model that mail can stay on the machine.

## Reproduce

From a checkout of judgment, with no key, from the committed recordings:

```bash
jud check examples/jud/agent-mail.jud examples/jud/agent-mail-cases.jud
jud eval  examples/jud/agent-mail.jud examples/jud/agent-mail-cases.jud --replay examples/recordings/agent_mail
jud tune  examples/jud/agent-mail.jud examples/jud/agent-mail-cases.jud --replay examples/recordings/agent_mail
```

Against the model, with `TYPESAFE_API_KEY` set (33 requests):

```bash
jud record examples/jud/agent-mail.jud examples/jud/agent-mail-cases.jud --out /tmp/agent-mail
```

## Follow-ups

| Bead | What |
|---|---|
| `pact-6qj` | epic: System One triage of agent mail |
| `pact-6qj.1` | label a field-run mail corpus, two annotators, held-out split |
| `pact-6qj.2` | record and grade it on jev-1.13 (hosted) and tev1 (local); go or no-go |
| `pact-6qj.3` | document the composition recipe, outside pact |
| `pact-6qj.4` | design spike for an optional feature in `ui` and `doctor` |
| `pact-6qj.5` | `pact audit`: a calibration line for `handoff --confidence`, no model |
| `judgment-2l1` | in judgment: `jud eval` reports ROC AUC, so a well-ranking question with a misplaced bar is not discarded |
