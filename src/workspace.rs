//! Per-issue workspaces.
//!
//! Slice 1 uses plain directories under a root; slice 2 swaps in git worktrees behind the same
//! trait. The containment invariant belongs here rather than at the call sites, and it is
//! checked before *deletion* as well as before launch — deletion is the more dangerous of the
//! two, and the spec only mandates the check for launch.

use std::path::{Path, PathBuf};

use crate::model::worktree_key;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("refusing to operate on {path}: outside workspace root {root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
}

pub struct Prepared {
    pub path: PathBuf,
    /// True only when this call created the directory. Gates first-time setup.
    pub created_now: bool,
}

pub trait Workspace: Send + Sync {
    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError>;
    fn remove(&self, issue_id: &str, identifier: &str) -> Result<(), WorkspaceError>;
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf;
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
        // Compare against the resolved parent: the leaf may not exist yet, and a non-existent
        // path cannot be canonicalised.
        let parent = path.parent().unwrap_or(path);
        let resolved = parent
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: parent.to_path_buf(), source })?;
        if resolved != self.root && !resolved.starts_with(&self.root) {
            return Err(WorkspaceError::OutsideRoot {
                path: path.to_path_buf(),
                root: self.root.clone(),
            });
        }
        Ok(())
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
        Ok(Prepared { path, created_now })
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
}
