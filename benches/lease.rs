//! Criterion benchmarks for the lease hot path (pact-fib).
//!
//! pact's central claim about performance is a *design* claim: coordination is
//! files on one filesystem, and messaging — the only part that shells out to
//! another program — is deliberately **not** on the lease path. This file is what
//! turns that into a number, because "fast by design" is exactly the kind of
//! sentence this repository requires evidence for.
//!
//! ## Why `#[path]` includes rather than a `[lib]` target
//!
//! pact is a binary crate: there is no library for a bench to `use`. The
//! idiomatic fix is to add one, and it was rejected here for two reasons. It
//! restructures `main.rs` and moves 400 unit tests between cargo targets, which
//! is a large diff to buy a benchmark. And `mise run lint` runs
//! `cargo clippy --all-targets` with **no features**, so a feature-gated lib
//! would fail that leg while a `#[path]` bench compiles standalone under both.
//!
//! The result is that the bench cannot affect the shipped binary, in the
//! strongest available sense: the binary does not know this file exists. The
//! cost is one extra compilation of the module tree when benching, and a loud
//! compile error if a module gains a dependency this list does not carry — which
//! is the failure mode to want.
//!
//! ## What is deliberately NOT here
//!
//! No `bd`. Every benchmark runs against a tempdir with no Beads workspace, so
//! nothing in the measured region can reach a backend. `pact msg` costs are a
//! different question with a different answer (a subprocess, tens of
//! milliseconds) and belong in their own measurement — see docs/performance.md.
//!
//! ## Every repo here is seeded to the log's steady state, and that is the point
//!
//! `events::append` computes its chain hash from the previous line, and finds that
//! line by reading the whole file — so it is **O(size of the event log)**, and
//! every lease operation appends at least once. The log is capped (rewritten to
//! `KEEP_LINES` once it passes `MAX_LINES`), so the cost is bounded, but the bound
//! is thousands of lines rather than nothing.
//!
//! A first version of this file did not account for that and produced numbers that
//! could not be compared with each other: each benchmark had its own tempdir whose
//! log grew at its own rate, so `refresh_reentrant` looked 6x faster than
//! `acquire_clean` when the real difference was that one wrote half as many events
//! per iteration and therefore sat lower on the curve.
//!
//! So every repo is seeded to [`STEADY_LINES`] first. Appends then oscillate
//! between the trim floor and the cap exactly as they do in a repository that has
//! been in use — which is the operating point worth quoting, and it makes the
//! benchmarks comparable to each other.
//!
//! ## The one place a subprocess IS on the acquire path
//!
//! `lease acquire` stamps the path's git blob id so `pact watch` can diff against
//! it later, and `git_history::hash_objects` only skips that when the path does
//! not exist on disk. So leasing a file you are about to *create* is
//! subprocess-free, and leasing one that already exists shells out to
//! `git hash-object -w`. Both are benchmarked, separately and by name, rather
//! than one being quoted as "the" acquire cost.

// Most of the included module tree is unused from here, and `cargo bench` builds
// with `cfg(test)` on, so those modules' own `mod tests` blocks compile too.
// Neither is a defect in the modules; both would otherwise be noise.
#![allow(dead_code, unused_imports)]

use std::path::{Path, PathBuf};
use std::process::Command;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// The module closure `lease` reaches for, transitively. Every one of these is
// compiled into the bench binary; none is modified.
//
// `crate::attrs` is NOT in this list and is not missing: it is a
// `#[macro_export]` macro defined in otel.rs, so including that module puts the
// macro at this crate's root the same way it lands at pact's.
#[path = "../src/agents_md.rs"]
mod agents_md;
#[path = "../src/beads.rs"]
mod beads;
#[path = "../src/events.rs"]
mod events;
#[path = "../src/git_history.rs"]
mod git_history;
#[path = "../src/identity.rs"]
mod identity;
#[path = "../src/lease.rs"]
mod lease;
#[path = "../src/msg.rs"]
mod msg;
#[path = "../src/otel.rs"]
mod otel;
#[path = "../src/output.rs"]
mod output;
#[path = "../src/repo.rs"]
mod repo;
#[path = "../src/watch.rs"]
mod watch;

/// Lines to pre-write into `.pact/events.jsonl`.
///
/// Between the trim floor (4000) and the cap (5000) that `events.rs` documents, so
/// a benchmark starts where a used repository sits and the periodic trim is
/// included in what gets measured rather than avoided.
const STEADY_LINES: usize = 4500;

/// A repo pact will accept, with no Beads workspace and no git object database,
/// and an event log already at its steady-state size.
///
/// `.git` as a plain directory is the `Placement::Plain` identity path — the one
/// an ordinary checkout takes, and the one 80% of crucible's events came through.
fn plain_repo() -> tempfile::TempDir {
    let tmp = seedless_repo();
    seed_log(tmp.path(), STEADY_LINES);
    tmp
}

/// The same repo with an EMPTY event log, for the one benchmark that exists to
/// quantify the floor rather than the operating point.
fn seedless_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(tmp.path().join(".git")).expect(".git");
    tmp
}

/// Write `lines` plausible events, shaped like the ones pact writes — same keys,
/// same rough length — so the read cost being measured is the real one.
///
/// Deliberately NOT chain-hashed: `append` reads the last line's `chain_hash` and
/// mixes it in, and whether that value is a real chain point or a stand-in changes
/// nothing about the cost of finding it. Nothing here verifies the chain.
fn seed_log(root: &Path, lines: usize) {
    if lines == 0 {
        return;
    }
    let dir = root.join(".pact");
    std::fs::create_dir_all(&dir).expect(".pact");
    let mut buf = String::with_capacity(lines * 160);
    for i in 0..lines {
        buf.push_str(&format!(
            r#"{{"at":"2026-08-13T09:00:{:02}.000000000+00:00","agent":"seed-{:03}","kind":"acquired","path":"src/seed{}.rs","detail":"seeded so the log starts at a realistic size","ttl_secs":900,"chain_hash":"{:016x}","invoked_from":"main","scope":"shared","pact_version":"0.8.0"}}"#,
            i % 60,
            i % 20,
            i,
            i as u64 * 0x9e3779b97f4a7c15u64,
        ));
        buf.push('\n');
    }
    std::fs::write(dir.join("events.jsonl"), buf).expect("seed events.jsonl");
}

/// A real git checkout, needed only where a subprocess is the thing under test.
fn git_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_git(tmp.path(), &["init", "--quiet"]);
    run_git(tmp.path(), &["config", "user.email", "bench@example.com"]);
    run_git(tmp.path(), &["config", "user.name", "bench"]);
    tmp
}

fn run_git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// Hand `path` back so the next iteration measures a *clean* acquire rather than
/// a re-entrant refresh.
///
/// Public API on purpose, rather than unlinking the lock directly: the lock's
/// filename encoding is `lease`'s business, and a bench that reimplements it
/// measures pact until the day the encoding changes and then measures nothing.
/// Every caller runs this in an `iter_batched` *setup*, so it is never timed.
fn unlock(root: &Path, path: &str) {
    let _ = lease::release(root, "bench-a", path, true);
}

// ---------------------------------------------------------------- acquire/release

fn bench_lease_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease");
    // One logical operation per iteration, so criterion reports ops/sec
    // (`thrpt`) beside ns/op.
    group.throughput(Throughput::Elements(1));

    // Clean acquire of a path that does not exist on disk: no git subprocess.
    let tmp = plain_repo();
    group.bench_function("acquire_clean", |b| {
        b.iter_batched(
            || unlock(tmp.path(), "src/bench.rs"),
            |_| {
                lease::acquire(tmp.path(), "bench-a", "src/bench.rs", 900, false, None)
                    .expect("acquire")
            },
            criterion::BatchSize::SmallInput,
        )
    });

    // The refusal path: read the lock, decide it is somebody else's, log the
    // `refused` event, build the message. What an agent hits at exit 2.
    let contended = plain_repo();
    lease::acquire(contended.path(), "holder", "src/hot.rs", 900, false, None).expect("hold");
    group.bench_function("acquire_contended_exit2", |b| {
        b.iter(|| {
            lease::acquire(contended.path(), "waiter", "src/hot.rs", 900, false, None)
                .expect_err("must be refused")
        })
    });

    // Re-entrant refresh: same agent, same path, so the write path runs but no
    // takeover decision is made.
    let refresh = plain_repo();
    lease::acquire(refresh.path(), "bench-a", "src/keep.rs", 900, false, None).expect("hold");
    group.bench_function("refresh_reentrant", |b| {
        b.iter(|| {
            lease::acquire(refresh.path(), "bench-a", "src/keep.rs", 900, false, None)
                .expect("refresh")
        })
    });

    // Release of a lock this agent holds. Re-acquired outside the timed region.
    let rel = plain_repo();
    group.bench_function("release", |b| {
        b.iter_batched(
            || {
                lease::acquire(rel.path(), "bench-a", "src/drop.rs", 900, false, None)
                    .expect("setup acquire");
            },
            |_| lease::release(rel.path(), "bench-a", "src/drop.rs", false).expect("release"),
            criterion::BatchSize::SmallInput,
        )
    });

    // THE BUDGETED ONE. A full claim-and-hand-back cycle is what an agent
    // actually costs the fleet per file it touches, so it is the number
    // docs/performance.md commits to.
    let round = plain_repo();
    group.bench_function("roundtrip_acquire_release", |b| {
        b.iter(|| {
            lease::acquire(round.path(), "bench-a", "src/round.rs", 900, false, None)
                .expect("acquire");
            lease::release(round.path(), "bench-a", "src/round.rs", false).expect("release")
        })
    });

    group.finish();
}

/// All-or-nothing batching, which is what the protocol tells agents to use.
fn bench_acquire_many(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease/acquire_many");
    for n in [1usize, 5, 20] {
        let paths: Vec<String> = (0..n).map(|i| format!("src/m{i}.rs")).collect();
        let tmp = plain_repo();
        // Throughput in PATHS, so ops/sec here reads as paths/sec and the three
        // sizes are directly comparable.
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &paths, |b, paths| {
            b.iter_batched(
                || {
                    for p in paths {
                        unlock(tmp.path(), p);
                    }
                },
                |_| {
                    lease::acquire_many(tmp.path(), "bench-a", paths, 900, false, None)
                        .expect("acquire_many")
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

// ------------------------------------------------------------------- event log

/// Every lease operation appends at least one event, so this is not a component
/// cost — it is most of the cost of everything above.
///
/// Benched at three log sizes because `append` finds the previous chain point by
/// reading the whole file. The shape of that curve is the single most useful
/// number here: it says whether the coordination primitive is bounded by syscalls
/// or by the size of its own history.
fn bench_events_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("events/append");
    group.throughput(Throughput::Elements(1));

    let ev = seed_event();
    for lines in [0usize, 1000, STEADY_LINES] {
        // One repo per size, seeded once. Appends grow the log during the run, and
        // the trim caps that growth — so the large sizes measure steady state and
        // the 0 case measures a ramp from empty, which is why it is labelled as a
        // floor rather than as an operating point.
        let tmp = seedless_repo();
        seed_log(tmp.path(), lines);
        group.bench_with_input(BenchmarkId::from_parameter(lines), &lines, |b, _| {
            b.iter(|| events::append(tmp.path(), &ev))
        });
    }

    // THE DECISIVE PAIR. `stamp_context` stamps `head` — a `git rev-parse`
    // SUBPROCESS — for exactly four kinds: acquired, stolen, released,
    // force-released. Those are the hold boundaries, which is to say the lease hot
    // path. Every other kind skips it.
    //
    // So this is the same append twice, differing only in `kind`, at the same log
    // size. The gap is the subprocess, isolated, and it is the reason a fresh
    // repository's append is not much cheaper than a full one's: the fixed cost
    // dominates the log read.
    let tmp = seedless_repo();
    seed_log(tmp.path(), STEADY_LINES);
    for kind in ["acquired", "notified"] {
        let mut ev = seed_event();
        ev.kind = kind.to_string();
        group.bench_with_input(BenchmarkId::new("by_kind", kind), &ev, |b, ev| {
            b.iter(|| events::append(tmp.path(), ev))
        });
    }

    group.finish();
}

/// One realistic event, reused by every append benchmark.
fn seed_event() -> events::Event {
    events::Event {
        at: "2026-08-13T09:00:00+00:00".to_string(),
        agent: "bench-a".to_string(),
        kind: "acquired".to_string(),
        path: Some("src/bench.rs".to_string()),
        detail: Some("a note of the length a real one has".to_string()),
        ttl_secs: Some(900),
        covers_lines: None,
        actor: None,
        displaced: None,
        chain_hash: None,
        invoked_from: None,
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
    }
}

// ------------------------------------------------------------ topology resolve

/// `RepoContext::resolve` runs on essentially every command, so its three real
/// branches are worth telling apart: an ordinary checkout, a checkout that *has*
/// linked worktrees (one extra directory scan), and a linked worktree itself
/// (a `.git` file, then the commondir walk).
fn bench_repo_resolve(c: &mut Criterion) {
    let mut group = c.benchmark_group("repo/resolve");
    group.throughput(Throughput::Elements(1));

    let plain = plain_repo();
    group.bench_function("plain", |b| {
        b.iter(|| repo::RepoContext::resolve(plain.path()))
    });

    // A main worktree that has linked ones: same identity path plus the
    // `has_linked_worktrees` scan.
    let with_wt = plain_repo();
    std::fs::create_dir_all(with_wt.path().join(".git/worktrees/wt-a")).expect("worktrees");
    group.bench_function("plain_with_worktrees", |b| {
        b.iter(|| repo::RepoContext::resolve(with_wt.path()))
    });

    // A real linked worktree, which is the only way to exercise the commondir
    // walk honestly — the `.git` file, its gitdir, and the `commondir` read.
    if let Some((_main, wt)) = real_worktree() {
        group.bench_function("linked_worktree_commondir", |b| {
            b.iter(|| repo::RepoContext::resolve(&wt))
        });
    } else {
        eprintln!("SKIP repo/resolve/linked_worktree_commondir: `git worktree add` unavailable");
    }

    group.finish();
}

/// A main checkout with one linked worktree, or `None` if git cannot make one.
/// The TempDir is returned so it outlives the benchmark that reads it.
fn real_worktree() -> Option<(tempfile::TempDir, PathBuf)> {
    let main = git_repo();
    std::fs::write(main.path().join("seed.txt"), b"seed").ok()?;
    run_git(main.path(), &["add", "seed.txt"]);
    run_git(main.path(), &["commit", "--quiet", "-m", "seed"]);
    let wt = main.path().join("wt-a");
    let ok = Command::new("git")
        .arg("-C")
        .arg(main.path())
        .args(["worktree", "add", "--quiet"])
        .arg(&wt)
        .args(["-b", "bench-wt"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !wt.join(".git").is_file() {
        return None;
    }
    Some((main, wt))
}

// --------------------------------------------------- the one subprocess, named

/// The same clean acquire, but on a path that EXISTS in a real git checkout — so
/// `hash_objects` runs `git hash-object -w` to stamp `content_hash`.
///
/// Benched separately and named for what it is. The gap between this and
/// `lease/acquire_clean` is the cost of one git subprocess, which is the whole
/// point of measuring both: it is the difference between pact's own work and the
/// work it delegates.
fn bench_acquire_with_git_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("lease/with_subprocess");
    group.throughput(Throughput::Elements(1));

    let tmp = git_repo();
    std::fs::write(tmp.path().join("existing.rs"), b"fn main() {}\n").expect("write");
    group.bench_function("acquire_hashes_an_existing_file", |b| {
        b.iter_batched(
            || unlock(tmp.path(), "existing.rs"),
            |_| {
                lease::acquire(tmp.path(), "bench-a", "existing.rs", 900, false, None)
                    .expect("acquire")
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_lease_ops,
    bench_acquire_many,
    bench_events_append,
    bench_repo_resolve,
    bench_acquire_with_git_hash,
);
criterion_main!(benches);
