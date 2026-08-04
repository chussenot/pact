use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::output::exit_with;

/// Walk up from `start` looking for a `.git` entry, returning the containing directory.
/// Shared by `lease`, `agents_md`, and `doctor` — kept separate from `main.rs` so
/// `main.rs` stays dispatch-only per the project layout.
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut dir = start
        .canonicalize()
        .with_context(|| format!("resolving {}", start.display()))?;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(exit_with(
                4,
                format!(
                    "not in a git repository (no .git found above {})",
                    start.display()
                ),
            ));
        }
    }
}

/// Where pact's state lives, and which topology put it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    /// `.git` is a directory: an ordinary checkout. `<root>/.pact/`.
    Plain,
    /// A linked worktree whose common `.git` sits inside a main worktree.
    /// State is the MAIN worktree's `<shared_root>/.pact/`.
    MainWorktree,
    /// A linked worktree of a BARE repository. There is no main worktree to
    /// hold state, so it is anchored inside the common gitdir: `<common>/pact/`.
    CommonGitdir,
    /// The `.git` file could not be followed. Falls back to per-worktree state
    /// rather than guessing, and says so through `doctor`.
    LocalFallback,
    /// `PACT_WORKTREE_SCOPE=local` asked for per-worktree isolation.
    ScopedLocal,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Plain => "plain",
            Placement::MainWorktree => "main-worktree",
            Placement::CommonGitdir => "common-gitdir",
            Placement::LocalFallback => "local-fallback",
            Placement::ScopedLocal => "scoped-local",
        }
    }
}

/// One resolution of "where does coordination state live", shared by every
/// module instead of each deriving it again.
///
/// ## Why worktrees have to share
///
/// A lease is advisory: its entire value is that a *peer* can see it. Give two
/// linked worktrees of one repository their own `.pact/` and both agents
/// "acquire" `src/api.ts`, both are told they succeeded, and neither learns the
/// other exists — an advisory lock that advises nobody, which is strictly worse
/// than no lock, because it reports success. Worktrees are one repository being
/// edited from several directories, so they get one coordination space.
///
/// ## The chain
///
/// `.git` is a directory in an ordinary checkout and a FILE in a linked
/// worktree. The file says `gitdir: <path>` pointing at
/// `<common>/worktrees/<name>`, and a `commondir` file in there points back at
/// the common `.git`. Both hops are plain file reads — no `git` subprocess, for
/// the same reason the rest of pact needs a `.git` directory rather than the
/// binary (`reach` and `commit_paths` above are the deliberate exceptions,
/// because gitignore semantics are not reimplementable).
///
/// Whether the common dir has a working tree around it is decided by its name,
/// which is the heuristic git itself effectively uses: a common dir called
/// `.git` sits inside a checkout, one called `repo.git` is bare.
#[derive(Debug, Clone)]
pub struct RepoContext {
    /// The checkout the command is running in.
    pub worktree_root: PathBuf,
    /// Where coordination is shared. The main worktree when there is one;
    /// otherwise the same as `worktree_root`.
    pub shared_root: PathBuf,
    /// This worktree's own gitdir — `<root>/.git` when plain.
    pub git_dir: PathBuf,
    /// The resolved state directory, final component included: `<x>/.pact` for
    /// every placement except `CommonGitdir`, which uses `<common>/pact`.
    pub state_dir: PathBuf,
    pub is_linked_worktree: bool,
    /// `<name>` from `.git/worktrees/<name>` for a linked worktree; the
    /// shared root's directory name for a main worktree that has linked ones.
    /// `None` when this repository has no worktrees at all.
    pub worktree_name: Option<String>,
    /// Does this repository have linked worktrees? Decides whether the lease
    /// payload carries `branch`/`worktree`, so a repo that never uses worktrees
    /// keeps byte-identical lock files.
    pub has_worktrees: bool,
    pub placement: Placement,
    /// Set when resolution failed and `LocalFallback` was taken. `doctor`
    /// surfaces it; nothing panics.
    pub warning: Option<String>,
}

/// `PACT_WORKTREE_SCOPE`. Shared is the default and the only sane one for
/// advisory leases; `local` exists for the rare case where two worktrees are
/// deliberately being treated as unrelated projects.
fn scope_is_local() -> bool {
    matches!(std::env::var("PACT_WORKTREE_SCOPE").as_deref(), Ok("local"))
}

/// Read a `gitdir:`-style pointer file and resolve it against `base`.
///
/// Split out to be unit-testable: the forms that turn up in the wild are an
/// absolute path, a relative one, a trailing newline, and a path with spaces in
/// it — so the value is everything after the prefix, trimmed, and never split
/// on whitespace.
fn parse_gitdir_pointer(contents: &str) -> Option<&str> {
    let value = contents.lines().next()?.strip_prefix("gitdir:")?.trim();
    (!value.is_empty()).then_some(value)
}

/// Resolve `path` against `base` when relative, and normalise it. Falls back to
/// the lexical join if the target cannot be canonicalised, so a resolution that
/// merely crosses a missing directory still produces something usable.
fn resolve_against(base: &Path, path: &str) -> PathBuf {
    let joined = {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        }
    };
    joined.canonicalize().unwrap_or(joined)
}

impl RepoContext {
    /// Resolve for a root already found by [`find_repo_root`].
    ///
    /// Infallible on purpose. Every failure mode degrades to per-worktree state
    /// plus a `warning` that `doctor` prints: a malformed `.git` file is a
    /// reason to coordinate less, never a reason for `pact lease acquire` to
    /// panic in the middle of a fleet.
    pub fn resolve(worktree_root: &Path) -> Self {
        let mut ctx = Self::resolve_topology(worktree_root);
        if !scope_is_local() {
            return ctx;
        }
        // `local` moves the state, and moves NOTHING else. Resolving the topology
        // first and overriding after is what lets `doctor` say "you are in linked
        // worktree wt-auth AND your leases are invisible to its siblings" — an
        // early return here would report an ordinary checkout, so the one command
        // that exists to explain the situation would describe a different repo.
        ctx.state_dir = ctx.worktree_root.join(".pact");
        ctx.placement = Placement::ScopedLocal;
        ctx
    }

    fn resolve_topology(worktree_root: &Path) -> Self {
        let worktree_root = worktree_root.to_path_buf();
        let dot_git = worktree_root.join(".git");

        let local = |placement: Placement, warning: Option<String>| RepoContext {
            state_dir: worktree_root.join(".pact"),
            shared_root: worktree_root.clone(),
            git_dir: dot_git.clone(),
            worktree_root: worktree_root.clone(),
            is_linked_worktree: false,
            worktree_name: None,
            has_worktrees: false,
            placement,
            warning,
        };

        // The identity path, and it must stay byte-for-byte what pact did
        // before worktrees were understood at all: an ordinary checkout gets
        // `<root>/.pact` and no file reads beyond this one `metadata` call.
        if dot_git.is_dir() {
            let mut ctx = local(Placement::Plain, None);
            ctx.has_worktrees = has_linked_worktrees(&dot_git);
            if ctx.has_worktrees {
                // The main worktree of a repo that HAS linked ones still needs a
                // label, or a conflict message can name the loser's worktree and
                // not the holder's.
                ctx.worktree_name = worktree_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
            }
            return ctx;
        }

        if !dot_git.is_file() {
            // `find_repo_root` only promised the entry exists. A `.git` that is
            // neither file nor directory is not something to interpret.
            return local(
                Placement::LocalFallback,
                Some(format!(
                    "{} is neither a file nor a directory; using per-worktree state",
                    dot_git.display()
                )),
            );
        }

        let contents = match std::fs::read_to_string(&dot_git) {
            Ok(c) => c,
            Err(e) => {
                return local(
                    Placement::LocalFallback,
                    Some(format!("cannot read {}: {e}", dot_git.display())),
                )
            }
        };
        let Some(pointer) = parse_gitdir_pointer(&contents) else {
            return local(
                Placement::LocalFallback,
                Some(format!(
                    "{} has no `gitdir:` line; using per-worktree state",
                    dot_git.display()
                )),
            );
        };
        let git_dir = resolve_against(&worktree_root, pointer);
        if !git_dir.is_dir() {
            return local(
                Placement::LocalFallback,
                Some(format!(
                    "{} points at {}, which is not a directory",
                    dot_git.display(),
                    git_dir.display()
                )),
            );
        }

        let worktree_name = git_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());

        // `commondir` is what makes a linked worktree findable from itself.
        // Without it there is nothing to share, so this is the last fallback.
        let common_file = git_dir.join("commondir");
        let common = match std::fs::read_to_string(&common_file) {
            Ok(c) => c.lines().next().map(|l| l.trim().to_string()),
            Err(_) => None,
        };
        let Some(common) = common.filter(|c| !c.is_empty()) else {
            return RepoContext {
                state_dir: worktree_root.join(".pact"),
                shared_root: worktree_root.clone(),
                git_dir,
                worktree_root: worktree_root.clone(),
                is_linked_worktree: true,
                worktree_name,
                has_worktrees: true,
                placement: Placement::LocalFallback,
                warning: Some(format!(
                    "no readable `commondir` in {}; using per-worktree state, so leases will NOT be shared with sibling worktrees",
                    common_file.display()
                )),
            };
        };
        let common_dir = resolve_against(&git_dir, &common);

        // The heuristic the spec of this feature names: a common dir called
        // `.git` is inside a working tree, anything else (`repo.git`) is bare.
        let in_a_worktree = common_dir.file_name().map(|n| n == ".git").unwrap_or(false);
        match (in_a_worktree, common_dir.parent()) {
            (true, Some(main)) => RepoContext {
                state_dir: main.join(".pact"),
                shared_root: main.to_path_buf(),
                git_dir,
                worktree_root,
                is_linked_worktree: true,
                worktree_name,
                has_worktrees: true,
                placement: Placement::MainWorktree,
                warning: None,
            },
            // Bare repository plus worktrees. There is no checkout to put
            // `.pact/` beside, so it goes inside the common gitdir — which every
            // worktree of the repo can already find, and which is exactly as
            // per-machine as `.pact/` is meant to be.
            _ => RepoContext {
                state_dir: common_dir.join("pact"),
                // No main worktree: nothing to point Beads at. `shared_root`
                // stays this worktree so path handling is unaffected, and
                // `beads_root` refuses instead of inventing a checkout.
                shared_root: worktree_root.clone(),
                git_dir,
                worktree_root,
                is_linked_worktree: true,
                worktree_name,
                has_worktrees: true,
                placement: Placement::CommonGitdir,
                warning: None,
            },
        }
    }

    /// True when this repository has no main worktree to run Beads in.
    pub fn is_bare_topology(&self) -> bool {
        self.placement == Placement::CommonGitdir
    }

    /// The branch this worktree has checked out, from its own `HEAD`.
    ///
    /// Informational only, and `None` for a detached HEAD or an unreadable file
    /// — a lease is still perfectly valid without it.
    pub fn branch(&self) -> Option<String> {
        let head = std::fs::read_to_string(self.git_dir.join("HEAD")).ok()?;
        let first = head.lines().next()?.trim();
        Some(
            first
                .strip_prefix("ref: refs/heads/")
                .or_else(|| first.strip_prefix("ref: "))?
                .to_string(),
        )
    }
}

/// Does `<gitdir>/worktrees/` hold at least one entry?
///
/// Existence alone is not enough: `git worktree prune` leaves the directory
/// behind, and treating an empty one as "has worktrees" would start writing
/// `branch`/`worktree` into lock files of a repo that has none — breaking the
/// byte-compatibility this flag exists to protect.
fn has_linked_worktrees(git_dir: &Path) -> bool {
    std::fs::read_dir(git_dir.join("worktrees"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// pact's state directory, creating it (and `leases/`) if absent.
///
/// Takes the worktree root every caller already has and resolves the shared
/// location itself, so no module has to know about worktrees to store state in
/// the right place. Resolution is a `metadata` call in the common case, which is
/// why it is not cached: the alternative is a process-wide memo that would have
/// to be keyed per root anyway, since the unit tests drive many tempdir repos
/// through one process.
pub fn pact_dir(repo_root: &Path) -> Result<PathBuf> {
    let dir = RepoContext::resolve(repo_root).state_dir;
    std::fs::create_dir_all(dir.join("leases"))
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Where the state directory would be, without creating anything. Read-only
/// callers use this: asking a question must not leave a directory behind
/// (pact-rnc.27, and the same principle as the non-mutating `lease::peek` in
/// pact-rnc.19). Callers must treat a missing directory as "no state yet", not
/// an error.
pub fn pact_dir_path(repo_root: &Path) -> PathBuf {
    RepoContext::resolve(repo_root).state_dir
}

/// Whether a file `pact init` writes will actually reach someone who clones
/// the repo.
///
/// `init`'s whole promise is "run it once, commit the result, every clone is
/// onboarded". A gitignored `AGENTS.md` breaks that silently and completely:
/// the file is there locally, `git add` refuses it without a word, and the
/// clone gets no protocol. It happened in pact's own repo — a global
/// `~/.gitignore` rule meant `AGENTS.md` was never committed once.
///
/// Answering this needs real gitignore semantics — global excludes, nested
/// ignore files, negations, precedence — so it asks `git` instead of
/// reimplementing them. This is the only place pact shells out to git: the rest
/// of the tool needs a `.git` directory, not the binary, so an absent or
/// failing `git` yields [`Reach::Unknown`] and `doctor` declines to guess.
pub enum Reach {
    /// Tracked, so a clone gets it.
    Tracked,
    /// Untracked *and* ignored — the silent failure.
    Ignored { source: String },
    /// Untracked but committable: normal before the first commit.
    Untracked,
    /// No usable `git` to ask.
    Unknown,
}

pub fn reach(repo_root: &Path, rel: &str) -> Reach {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
    };

    match git(&["ls-files", "--error-unmatch", "--", rel]) {
        Ok(o) if o.status.success() => return Reach::Tracked,
        // Exit 1 is "not tracked"; 128 is "not a repo". Both fall through to
        // check-ignore, which distinguishes them.
        Ok(_) => {}
        Err(_) => return Reach::Unknown,
    }

    match git(&["check-ignore", "-v", "--", rel]) {
        // `-v` prints "<source>:<line>:<pattern>\t<path>"; the source is the
        // actionable half — knowing *which* file ignores it is the fix.
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let source = stdout
                .split('\t')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            Reach::Ignored { source }
        }
        Ok(o) if o.status.code() == Some(1) => Reach::Untracked,
        _ => Reach::Unknown,
    }
}

/// What [`commit_paths`] did, so `init` can report it truthfully. Every
/// non-committed outcome is a *reported* one: silently not committing is the
/// exact failure mode that let `AGENTS.md` go uncommitted for a whole project.
pub enum CommitOutcome {
    Committed {
        sha: String,
        files: Vec<String>,
    },
    /// Files already match HEAD — a re-run of `pact init` with nothing to say.
    NothingToCommit,
    /// Could not commit; the files are still written. Carries the reason.
    Skipped(String),
}

/// Stage and commit exactly `paths`, and nothing else.
///
/// Scoped on purpose: `git commit -- <paths>` builds the commit from HEAD plus
/// those paths, so a user's unrelated staged work stays staged instead of being
/// swept into a commit pact authored. Committing is a side effect a tool should
/// keep as small as it can.
///
/// Never uses `git add -f`: a path the repo ignores is a decision pact does not
/// get to overrule, so that case is reported and left alone.
pub fn commit_paths(repo_root: &Path, paths: &[&str], message: &str) -> CommitOutcome {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(args)
            .output()
    };

    let present: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| repo_root.join(p).exists())
        .collect();
    if present.is_empty() {
        return CommitOutcome::NothingToCommit;
    }

    if let Some(ignored) = present.iter().find_map(|p| match reach(repo_root, p) {
        Reach::Ignored { source } => Some(format!("{p} is ignored by {source}")),
        _ => None,
    }) {
        return CommitOutcome::Skipped(format!(
            "{ignored} — un-ignore it and commit by hand, pact will not override a repo's ignore rules"
        ));
    }

    let mut add = vec!["add", "--"];
    add.extend_from_slice(&present);
    match git(&add) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return CommitOutcome::Skipped(format!(
                "`git add` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
        Err(e) => return CommitOutcome::Skipped(format!("cannot run git: {e}")),
    }

    let mut staged = vec!["diff", "--cached", "--name-only", "--"];
    staged.extend_from_slice(&present);
    let changed: Vec<String> = match git(&staged) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => return CommitOutcome::Skipped("could not ask git what changed".to_string()),
    };
    if changed.is_empty() {
        return CommitOutcome::NothingToCommit;
    }

    let mut commit = vec!["commit", "--quiet", "-m", message, "--"];
    commit.extend_from_slice(&present);
    match git(&commit) {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            // Hooks, an unset user.email, a signing key that is not there. The
            // files are written either way, so this is reported, not raised.
            let why = String::from_utf8_lossy(&o.stderr);
            let why = why.trim();
            let why = if why.is_empty() {
                String::from_utf8_lossy(&o.stdout).trim().to_string()
            } else {
                why.to_string()
            };
            return CommitOutcome::Skipped(format!("`git commit` failed: {why}"));
        }
        Err(e) => return CommitOutcome::Skipped(format!("cannot run git: {e}")),
    }

    let sha = match git(&["rev-parse", "--short", "HEAD"]) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "HEAD".to_string(),
    };
    CommitOutcome::Committed {
        sha,
        files: changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_root_from_nested_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_repo_root(&nested).unwrap();
        assert_eq!(found, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn errors_outside_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_repo_root(tmp.path()).is_err());
    }

    /// The four shapes a real `.git` file turns up in. Spaces matter because a
    /// checkout under "My Documents" is not exotic, and splitting on whitespace
    /// would truncate the path to the first word.
    #[test]
    fn gitdir_pointer_parses_every_real_form() {
        assert_eq!(
            parse_gitdir_pointer("gitdir: /abs/path/.git/worktrees/wt\n"),
            Some("/abs/path/.git/worktrees/wt")
        );
        assert_eq!(
            parse_gitdir_pointer("gitdir: ../main/.git/worktrees/wt"),
            Some("../main/.git/worktrees/wt")
        );
        assert_eq!(
            parse_gitdir_pointer("gitdir: /path with spaces/.git/worktrees/my wt\n"),
            Some("/path with spaces/.git/worktrees/my wt")
        );
        // git writes exactly one space; tolerate more rather than resolve "".
        assert_eq!(parse_gitdir_pointer("gitdir:   /padded\n"), Some("/padded"));
        // And the forms that must NOT parse into something usable.
        assert_eq!(parse_gitdir_pointer("gitdir:\n"), None);
        assert_eq!(parse_gitdir_pointer("gitdir: \n"), None);
        assert_eq!(parse_gitdir_pointer("ref: refs/heads/main\n"), None);
        assert_eq!(parse_gitdir_pointer(""), None);
    }

    #[test]
    fn pointers_resolve_relative_and_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("a/b")).unwrap();

        // Relative resolves against the base and normalises `..`.
        assert_eq!(resolve_against(&base, "a/b"), base.join("a/b"));
        assert_eq!(resolve_against(&base.join("a/b"), "../.."), base);
        // Absolute ignores the base entirely.
        assert_eq!(
            resolve_against(&base, &base.join("a").display().to_string()),
            base.join("a")
        );
        // A path that cannot be canonicalised still yields the lexical join,
        // rather than silently becoming the base.
        assert_eq!(resolve_against(&base, "nope"), base.join("nope"));
    }

    /// The identity path, asserted rather than assumed: an ordinary checkout must
    /// resolve to exactly what pact did before it understood worktrees.
    #[test]
    fn an_ordinary_checkout_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let ctx = RepoContext::resolve(&root);
        assert_eq!(ctx.placement, Placement::Plain);
        assert_eq!(ctx.state_dir, root.join(".pact"));
        assert_eq!(ctx.shared_root, root);
        assert!(!ctx.is_linked_worktree);
        assert!(!ctx.has_worktrees, "no worktrees/ dir means no worktrees");
        assert_eq!(ctx.worktree_name, None);
        assert!(ctx.warning.is_none());
        assert_eq!(pact_dir_path(&root), root.join(".pact"));
    }

    /// `git worktree prune` leaves an empty `worktrees/` behind. Treating that as
    /// "has worktrees" would start writing branch/worktree into the lock files of
    /// a repo that has none, which is the byte-compatibility this flag protects.
    #[test]
    fn an_empty_worktrees_dir_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(git.join("worktrees")).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(!RepoContext::resolve(&root).has_worktrees);

        std::fs::create_dir(git.join("worktrees/wt")).unwrap();
        assert!(RepoContext::resolve(&root).has_worktrees);
    }

    /// Hand-built worktree layout, so the two hops are tested without needing
    /// `git` on PATH. The integration tests use the real thing.
    #[test]
    fn a_linked_worktree_resolves_to_the_main_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let main = base.join("main");
        let wt = base.join("wt-auth");
        let gitdir = main.join(".git/worktrees/wt-auth");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        // Relative commondir, which is what git actually writes.
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feat/auth\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

        let ctx = RepoContext::resolve(&wt);
        assert_eq!(ctx.placement, Placement::MainWorktree);
        assert!(ctx.is_linked_worktree);
        assert_eq!(ctx.shared_root, main);
        assert_eq!(ctx.state_dir, main.join(".pact"));
        assert_eq!(ctx.worktree_name.as_deref(), Some("wt-auth"));
        assert!(ctx.has_worktrees);
        assert_eq!(ctx.branch().as_deref(), Some("feat/auth"));
        assert!(ctx.warning.is_none());
        // The whole point: state resolves out of the worktree.
        assert_eq!(pact_dir_path(&wt), main.join(".pact"));
    }

    /// A bare repository has no checkout to sit beside, so state goes inside the
    /// common gitdir and messaging is refused rather than pointed somewhere odd.
    #[test]
    fn a_worktree_of_a_bare_repo_anchors_in_the_common_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let common = base.join("repo.git");
        let wt = base.join("wt");
        let gitdir = common.join("worktrees/wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

        let ctx = RepoContext::resolve(&wt);
        assert_eq!(ctx.placement, Placement::CommonGitdir);
        assert!(ctx.is_bare_topology());
        assert_eq!(ctx.state_dir, common.join("pact"));
        assert!(
            ctx.warning.is_none(),
            "bare is a supported topology, not a failure"
        );
    }

    /// Every malformed form degrades to local state with a warning. None panics:
    /// a broken `.git` is a reason to coordinate less, never a reason for `lease
    /// acquire` to abort mid-fleet.
    #[test]
    fn malformed_git_files_fall_back_locally_with_a_warning() {
        for (label, contents) in [
            ("no gitdir line", "something else\n"),
            ("empty", ""),
            ("empty gitdir value", "gitdir:\n"),
            ("points nowhere", "gitdir: /definitely/not/here\n"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let wt = tmp.path().canonicalize().unwrap();
            std::fs::write(wt.join(".git"), contents).unwrap();

            let ctx = RepoContext::resolve(&wt);
            assert_eq!(ctx.placement, Placement::LocalFallback, "{label}");
            assert_eq!(ctx.state_dir, wt.join(".pact"), "{label}");
            assert!(ctx.warning.is_some(), "{label} must explain itself");
        }
    }

    /// A gitdir that exists but has no `commondir`: there is nothing to share, so
    /// this falls back too — and the warning has to say that leases will NOT
    /// reach siblings, because that is the surprising part.
    #[test]
    fn a_gitdir_without_commondir_falls_back_and_says_leases_are_not_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let gitdir = base.join("main/.git/worktrees/wt");
        let wt = base.join("wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

        let ctx = RepoContext::resolve(&wt);
        assert_eq!(ctx.placement, Placement::LocalFallback);
        assert_eq!(ctx.state_dir, wt.join(".pact"));
        let warning = ctx.warning.expect("must warn");
        assert!(warning.contains("commondir"), "{warning}");
        assert!(warning.contains("NOT be shared"), "{warning}");
    }
}
