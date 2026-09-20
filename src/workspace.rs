//! Per-issue workspaces.
//!
//! [`DirWorkspace`] (slice 1) is plain directories under a root; [`GitWorktreeWorkspace`]
//! (slice 2) is a real `git worktree` per issue, behind the same trait. The containment
//! invariant belongs here rather than at the call sites, and it is checked before *deletion* as
//! well as before launch — deletion is the more dangerous of the two, and the spec only
//! mandates the check for launch.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::worktree_key;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("refusing to operate on {path}: outside workspace root {root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
    #[error("{path} is not a git repository")]
    NotAGitRepo { path: PathBuf },
    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },
}

#[derive(Debug)]
pub struct Prepared {
    pub path: PathBuf,
    /// True only when this call created the directory. Gates first-time setup.
    pub created_now: bool,
    /// The branch this run's commits land on, for impls that have one. It is the only durable
    /// artifact a dispatched run leaves behind — the worktree directory is scratch space — so
    /// it is reported upwards rather than kept private to the impl.
    pub branch: Option<String>,
}

pub trait Workspace: Send + Sync {
    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError>;
    fn remove(&self, issue_id: &str, identifier: &str) -> Result<(), WorkspaceError>;
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf;
}

/// Shared by every [`Workspace`] impl: refuse a path whose resolved parent is not the root
/// itself or a descendant of it. Compares the *parent*, not `path`, because the leaf may not
/// exist yet and a non-existent path cannot be canonicalised.
fn guard_within(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    let parent = path.parent().unwrap_or(path);
    let resolved = parent
        .canonicalize()
        .map_err(|source| WorkspaceError::Io { path: parent.to_path_buf(), source })?;
    if resolved != root && !resolved.starts_with(root) {
        return Err(WorkspaceError::OutsideRoot {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        });
    }
    Ok(())
}

pub struct DirWorkspace {
    root: PathBuf,
}

impl DirWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;
        // Canonicalise once so later containment checks compare resolved paths, not the
        // symlink-laden strings the caller happened to pass in.
        let root = root
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every path leaving this type passes through here.
    fn guard(&self, path: &Path) -> Result<(), WorkspaceError> {
        guard_within(&self.root, path)
    }
}

impl Workspace for DirWorkspace {
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf {
        self.root.join(worktree_key(issue_id, identifier))
    }

    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;
        let created_now = !path.exists();
        std::fs::create_dir_all(&path)
            .map_err(|source| WorkspaceError::Io { path: path.clone(), source })?;
        Ok(Prepared { path, created_now, branch: None })
    }

    fn remove(&self, issue_id: &str, identifier: &str) -> Result<(), WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;
        if path.exists() {
            std::fs::remove_dir_all(&path)
                .map_err(|source| WorkspaceError::Io { path: path.clone(), source })?;
        }
        Ok(())
    }
}

/// A real `git worktree` per issue, checked out on its own branch off whatever `repo`'s HEAD
/// happens to be when the worktree is created.
///
/// **Uncommitted work is discarded on removal.** `remove` force-removes the worktree, which
/// throws away anything the agent never committed. The alternative — refusing to remove a
/// dirty worktree — would strand every issue that reaches a terminal state with so much as an
/// untracked scratch file, and nothing upstream of this type inspects worktree contents before
/// asking for cleanup. An agent that wants work preserved has to commit it; that is the only
/// signal this type can see.
///
/// **Committed work survives it.** The branch outlives the worktree whenever it carries commits
/// `repo`'s HEAD does not already have. Cleanup is driven by a ticket reaching a terminal state,
/// and closing a ticket is not a decision to discard the run's output — so the directory goes
/// and the branch stays.
pub struct GitWorktreeWorkspace {
    root: PathBuf,
    repo: PathBuf,
}

impl GitWorktreeWorkspace {
    /// `repo` is the git repository worktrees are created from — its HEAD is the branch point,
    /// and its `.git` directory is where every worktree's admin state lives.
    pub fn new(root: impl Into<PathBuf>, repo: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;
        let root = root
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;

        let repo = repo.into();
        let repo = repo
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: repo.clone(), source })?;
        if Self::git(&repo, &["rev-parse", "--git-dir"]).is_err() {
            return Err(WorkspaceError::NotAGitRepo { path: repo });
        }

        // A process killed mid-run never reaches `remove`, so git's own admin entry for that
        // worktree can outlive the directory if something outside this type deleted it by
        // hand. Pruning at startup is the one point where that bookkeeping gets reconciled;
        // it is a no-op whenever every registered worktree's directory is still present, which
        // is the case after an ordinary kill-and-restart.
        let _ = Self::git(&repo, &["worktree", "prune"]);

        Ok(Self { root, repo })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn guard(&self, path: &Path) -> Result<(), WorkspaceError> {
        guard_within(&self.root, path)
    }

    fn git(repo: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .map_err(|source| WorkspaceError::Io { path: repo.to_path_buf(), source })?;
        if !output.status.success() {
            return Err(WorkspaceError::Git {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Namespaced under `symphony/` and named after the same key as the directory, so the
    /// branch holding a finished run's work can be found from the issue identifier by eye
    /// rather than by recomputing a hash. `worktree_key` already keys off the dispatch id, so
    /// two issues that happen to share an identifier still get two distinct branches.
    ///
    /// Dots are dropped even though the directory name keeps them: `a..b` is a legal directory
    /// and an illegal ref, and a hostile identifier reaches both.
    fn branch_for(issue_id: &str, identifier: &str) -> String {
        let key: String = worktree_key(issue_id, identifier)
            .chars()
            .map(|c| if c == '.' { '_' } else { c })
            .collect();
        format!("symphony/{key}")
    }

    /// True when `branch` exists and holds commits `repo`'s HEAD does not already contain.
    ///
    /// `merge-base --is-ancestor` exits non-zero both for a branch that is ahead and for one
    /// that does not exist, so existence is established first rather than inferred from it.
    fn branch_carries_work(repo: &Path, branch: &str) -> bool {
        let full = format!("refs/heads/{branch}");
        if Self::git(repo, &["rev-parse", "--verify", "--quiet", &full]).is_err() {
            return false;
        }
        Self::git(repo, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_err()
    }

    fn is_worktree_checkout(path: &Path) -> bool {
        path.join(".git").is_file()
    }
}

impl Workspace for GitWorktreeWorkspace {
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf {
        self.root.join(worktree_key(issue_id, identifier))
    }

    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;

        // A real worktree checkout has a `.git` *file* (pointing at the admin directory back
        // in `repo`), not a `.git` directory. Trusting bare `path.exists()` here would silently
        // treat a leftover plain directory — e.g. from a `DirWorkspace` deployment migrating to
        // this type, or any other stray write to the workspace root — as an already-prepared
        // worktree, when nothing ever registered it with git.
        let branch = Self::branch_for(issue_id, identifier);
        if Self::is_worktree_checkout(&path) {
            return Ok(Prepared { path, created_now: false, branch: Some(branch) });
        }

        let path_str = path.to_string_lossy().into_owned();
        // A branch left behind by an earlier run holds that run's commits, so this attaches to
        // it rather than resetting it — otherwise a re-dispatch after `Done`, or a crash
        // between `prepare` and `remove`, would throw the agent's work away. `-B` stays the
        // path for a branch carrying nothing HEAD does not already have, so a run that crashed
        // before committing anything still cannot turn every future `prepare` for this issue
        // into a permanent "branch already exists" failure. If `path` exists but is not a
        // worktree checkout, git itself refuses with a clear error rather than this type
        // guessing at what to do with foreign state.
        if Self::branch_carries_work(&self.repo, &branch) {
            Self::git(&self.repo, &["worktree", "add", &path_str, &branch])?;
        } else {
            Self::git(&self.repo, &["worktree", "add", "-B", &branch, &path_str])?;
        }
        Ok(Prepared { path, created_now: true, branch: Some(branch) })
    }

    fn remove(&self, issue_id: &str, identifier: &str) -> Result<(), WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;

        if !path.exists() {
            return Ok(());
        }

        let path_str = path.to_string_lossy().into_owned();
        Self::git(&self.repo, &["worktree", "remove", "--force", &path_str])?;

        // `-d` rather than `-D`: git's own merged check is the test for whether this branch
        // still holds the run's work. One carrying nothing HEAD does not already have is
        // deleted exactly as before, so an issue that produced no commit leaves no litter; one
        // carrying commits outlives its worktree.
        //
        // Best-effort either way: a branch that was already deleted, or never created because
        // `prepare` failed before reaching it, must not turn a successful worktree removal into
        // an error.
        let branch = Self::branch_for(issue_id, identifier);
        let _ = Self::git(&self.repo, &["branch", "-d", &branch]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "symphony-ws-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn prepare_creates_once_then_reuses() {
        let root = tmp_root("reuse");
        let ws = DirWorkspace::new(&root).unwrap();

        let a = ws.prepare("id-1", "MT-1").unwrap();
        assert!(a.created_now);
        assert!(a.path.is_dir());

        let b = ws.prepare("id-1", "MT-1").unwrap();
        assert!(!b.created_now, "an existing workspace must be reused, not recreated");
        assert_eq!(a.path, b.path);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn workspaces_are_preserved_until_explicitly_removed() {
        let root = tmp_root("persist");
        let ws = DirWorkspace::new(&root).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("artifact.txt"), b"warm").unwrap();

        // Re-preparing must not wipe accumulated state; that warmth is the point.
        ws.prepare("id-1", "MT-1").unwrap();
        assert!(p.join("artifact.txt").exists());

        ws.remove("id-1", "MT-1").unwrap();
        assert!(!p.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn removing_an_absent_workspace_is_not_an_error() {
        let root = tmp_root("absent");
        let ws = DirWorkspace::new(&root).unwrap();
        assert!(ws.remove("id-nope", "MT-nope").is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hostile_identifiers_stay_inside_the_root() {
        let root = tmp_root("escape");
        let ws = DirWorkspace::new(&root).unwrap();

        for bad in ["../../etc", "/etc/passwd", "..", "a/../../b"] {
            let p = ws.prepare("id-x", bad).unwrap();
            assert!(p.path.starts_with(ws.root()), "{bad} escaped the root: {}", p.path.display());
            assert_eq!(p.path.parent().unwrap(), ws.root(), "must be exactly one level deep");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    // ---- GitWorktreeWorkspace -------------------------------------------------

    /// A throwaway repo with one commit, so `git worktree add` has a HEAD to branch from.
    /// Config is set repo-local rather than relying on the machine having a global identity.
    fn tmp_repo(tag: &str) -> PathBuf {
        let p = tmp_root(&format!("repo-{tag}"));
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&p)
                .args(args)
                .output()
                .expect("git must be on PATH to run these tests");
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        p
    }

    fn is_registered_worktree(repo: &Path, path: &Path) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.strip_prefix("worktree ").is_some_and(|p| Path::new(p) == path))
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet", branch])
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn constructing_over_a_non_repo_fails_clearly() {
        let root = tmp_root("not-a-repo-root");
        let not_a_repo = tmp_root("not-a-repo-target");
        assert!(matches!(
            GitWorktreeWorkspace::new(&root, &not_a_repo),
            Err(WorkspaceError::NotAGitRepo { .. })
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&not_a_repo).ok();
    }

    #[test]
    fn a_worktree_is_created_once_then_reused_across_attempts() {
        let root = tmp_root("wt-reuse");
        let repo = tmp_repo("wt-reuse");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let a = ws.prepare("id-1", "MT-1").unwrap();
        assert!(a.created_now);
        assert!(a.path.join(".git").is_file(), "a worktree checkout has a `.git` file, not dir");
        assert!(is_registered_worktree(&repo, &a.path));

        let b = ws.prepare("id-1", "MT-1").unwrap();
        assert!(!b.created_now, "an existing worktree must be reused, not recreated");
        assert_eq!(a.path, b.path);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn two_issues_sharing_an_identifier_get_two_distinct_worktrees() {
        let root = tmp_root("wt-two-issues");
        let repo = tmp_repo("wt-two-issues");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let a = ws.prepare("id-a", "MT-1").unwrap();
        let b = ws.prepare("id-b", "MT-1").unwrap();

        assert_ne!(a.path, b.path);
        assert!(is_registered_worktree(&repo, &a.path));
        assert!(is_registered_worktree(&repo, &b.path));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_stale_plain_directory_at_the_target_path_is_not_silently_treated_as_a_worktree() {
        // Reproduces what a `DirWorkspace` deployment migrating to git worktrees would leave
        // behind: a plain, non-empty directory sitting exactly where a worktree would go,
        // because `worktree_key` is deterministic and both types compute the same leaf path.
        let root = tmp_root("wt-stale-plain-dir");
        let repo = tmp_repo("wt-stale-plain-dir");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let stale = ws.path_for("id-1", "MT-1");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("leftover.txt"), b"from a previous DirWorkspace run").unwrap();

        // Must not silently report this foreign directory as an already-prepared worktree.
        let err = ws.prepare("id-1", "MT-1").unwrap_err();
        assert!(matches!(err, WorkspaceError::Git { .. }), "expected a git error, got {err:?}");
        assert!(!is_registered_worktree(&repo, &stale), "must never register foreign state");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn removing_a_worktree_discards_uncommitted_work_and_prunes_the_branch() {
        let root = tmp_root("wt-remove");
        let repo = tmp_repo("wt-remove");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("scratch.txt"), b"never committed").unwrap();
        let branch = GitWorktreeWorkspace::branch_for("id-1", "MT-1");
        assert!(branch_exists(&repo, &branch));

        ws.remove("id-1", "MT-1").unwrap();

        assert!(!p.exists());
        assert!(!is_registered_worktree(&repo, &p));
        assert!(!branch_exists(&repo, &branch), "remove must prune the branch too");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn removing_an_absent_worktree_is_not_an_error() {
        let root = tmp_root("wt-absent");
        let repo = tmp_repo("wt-absent");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        assert!(ws.remove("id-nope", "MT-nope").is_ok());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn hostile_identifiers_stay_inside_the_root_for_git_worktrees_too() {
        let root = tmp_root("wt-escape");
        let repo = tmp_repo("wt-escape");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        for (i, bad) in ["../../etc", "/etc/passwd", "..", "a/../../b"].iter().enumerate() {
            let p = ws.prepare(&format!("id-{i}"), bad).unwrap();
            assert!(p.path.starts_with(ws.root()), "{bad} escaped the root: {}", p.path.display());
            assert_eq!(p.path.parent().unwrap(), ws.root(), "must be exactly one level deep");
        }
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Commit a file inside a worktree, standing in for what a dispatched agent leaves behind.
    /// Identity comes from the repo-local config `tmp_repo` set; worktrees share it.
    fn commit_in(worktree: &Path, file: &str, msg: &str) {
        std::fs::write(worktree.join(file), msg.as_bytes()).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(worktree).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["add", file]);
        git(&["commit", "-q", "-m", msg]);
    }

    fn head_of(worktree: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_branch_holding_committed_work_outlives_the_worktree_it_is_removed_with() {
        // Cleanup fires when a ticket reaches a terminal state. Closing a ticket an agent
        // already worked must not be what destroys the commits that work produced.
        let root = tmp_root("wt-keep-branch");
        let repo = tmp_repo("wt-keep-branch");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&p, "work.txt", "the agent output");
        let branch = GitWorktreeWorkspace::branch_for("id-1", "MT-1");

        ws.remove("id-1", "MT-1").unwrap();

        assert!(!p.exists(), "the directory is scratch space and still goes");
        assert!(branch_exists(&repo, &branch), "the run's commits must survive cleanup");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn re_preparing_an_issue_does_not_reset_a_branch_that_holds_work() {
        // `-B` is what makes a crashed `prepare` recoverable, and it is also a force-reset:
        // applied to a branch a finished run already committed to, it discards that run.
        let root = tmp_root("wt-no-reset");
        let repo = tmp_repo("wt-no-reset");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let first = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&first, "work.txt", "the agent output");
        let committed = head_of(&first);
        ws.remove("id-1", "MT-1").unwrap();

        let again = ws.prepare("id-1", "MT-1").unwrap();
        assert!(again.path.join("work.txt").exists(), "the earlier run's file must come back");
        assert_eq!(head_of(&again.path), committed, "the branch must not have been reset");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_prepared_worktree_reports_the_branch_its_work_will_land_on() {
        let root = tmp_root("wt-report-branch");
        let repo = tmp_repo("wt-report-branch");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let fresh = ws.prepare("id-1", "MT-1").unwrap();
        let reused = ws.prepare("id-1", "MT-1").unwrap();
        let named = fresh.branch.as_deref().expect("a git worktree always has a branch");
        assert!(named.starts_with("symphony/MT-1-"), "{named} does not name its issue");
        assert_eq!(fresh.branch, reused.branch, "reuse reports the same branch as creation");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_branch_names_the_issue_whose_work_it_holds() {
        // Finding a finished run's output must not mean recomputing a hash by hand.
        let branch = GitWorktreeWorkspace::branch_for("id-1", "MT-1");
        assert!(branch.starts_with("symphony/MT-1-"), "{branch} does not name its issue");
        assert_ne!(
            branch,
            GitWorktreeWorkspace::branch_for("id-2", "MT-1"),
            "two issues sharing an identifier still need distinct branches"
        );
    }

    #[test]
    fn a_hostile_identifier_cannot_produce_a_branch_git_refuses() {
        // The identifier reaches the ref namespace now, not just the filesystem, and git's
        // rules there are not the filesystem's: `..`, a trailing `.lock`, a bare `@`.
        let root = tmp_root("wt-branch-refs");
        let repo = tmp_repo("wt-branch-refs");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        for (i, bad) in ["..", "a..b", "x.lock", "@", "-dash", "a/../b"].iter().enumerate() {
            let id = format!("id-{i}");
            ws.prepare(&id, bad).expect("a hostile identifier must not fail preparation");
            let branch = GitWorktreeWorkspace::branch_for(&id, bad);
            assert!(branch_exists(&repo, &branch), "git refused the branch name {branch}");
        }

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }
}
