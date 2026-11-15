use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::state::lock::RepoLock;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Repository {
    pub root: PathBuf,
    pub rusty_dir: PathBuf,
    pub lock_path: PathBuf,
}

#[allow(dead_code)]
impl Repository {
    /// Discover the repository root by walking up from the current working directory.
    pub fn discover() -> Result<Self> {
        let mut current_dir = env::current_dir()?;

        loop {
            let rusty_dir = current_dir.join(".rusty");

            if rusty_dir.is_dir() {
                return Self::from_path(&rusty_dir);
            }

            if !current_dir.pop() {
                break;
            }
        }

        anyhow::bail!("Not a rusty repository");
    }

    /// Construct a `Repository` context from the `.rusty` directory path.
    pub fn from_path(rusty_dir: &Path) -> Result<Self> {
        let canonical_rusty = rusty_dir.canonicalize()?;
        let canonical_root = canonical_rusty
            .parent()
            .context("Cannot find repository parent root directory")?
            .to_path_buf();
        let lock_path = canonical_rusty.join("lock");

        Ok(Self {
            root: canonical_root,
            rusty_dir: canonical_rusty,
            lock_path,
        })
    }

    /// Acquire exclusive lock for mutating repository state.
    ///
    /// This refuses to proceed while an interrupted operation journal exists;
    /// call [`crate::state::operation::recover_if_needed`] first.
    pub fn lock(&self) -> Result<RepoLock> {
        RepoLock::acquire(&self.rusty_dir)
    }

    /// Acquire the lock for crash recovery, bypassing the active-journal check.
    pub fn lock_for_recovery(&self) -> Result<RepoLock> {
        RepoLock::acquire_for_recovery(&self.rusty_dir)
    }

    /// Check whether crash recovery is active (an unfinished operation journal exists).
    pub fn is_recovery_active(&self) -> bool {
        self.rusty_dir.join("operations").join("active.json").exists()
    }

    /// Resolve HEAD commit hash, if any.
    pub fn head_commit(&self) -> Result<Option<String>> {
        crate::repository::get_head_commit(&self.rusty_dir)
    }

    /// Read HEAD contents: returns `Some(ref_name)` if symbolic (e.g. "refs/heads/main"),
    /// or `None` if detached or unborn.
    pub fn current_symbolic_ref(&self) -> Result<Option<String>> {
        let head_path = self.rusty_dir.join("HEAD");
        if !head_path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&head_path)?;
        let content = content.trim();
        if let Some(branch_ref) = content.strip_prefix("ref: ") {
            Ok(Some(branch_ref.trim().to_string()))
        } else {
            Ok(None)
        }
    }

    /// Return the simple branch name (e.g. "main") if attached, or None if detached.
    pub fn current_branch_name(&self) -> Result<Option<String>> {
        if let Some(sym_ref) = self.current_symbolic_ref()?
            && let Some(branch_name) = sym_ref.strip_prefix("refs/heads/")
        {
            return Ok(Some(branch_name.to_string()));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_repository_context_and_symbolic_ref() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(&rusty_dir).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();
        assert_eq!(repo.current_branch_name().unwrap(), Some("feature-x".to_string()));
        assert_eq!(repo.current_symbolic_ref().unwrap(), Some("refs/heads/feature-x".to_string()));
        assert!(!repo.is_recovery_active());
        assert_eq!(repo.lock_path, rusty_dir.canonicalize().unwrap().join("lock"));
        let lock = repo.lock().unwrap();
        drop(lock);
    }

    #[test]
    fn test_repository_detached_head() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(&rusty_dir).unwrap();
        fs::write(rusty_dir.join("HEAD"), "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890\n").unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();
        assert_eq!(repo.current_branch_name().unwrap(), None);
        assert_eq!(repo.current_symbolic_ref().unwrap(), None);
        assert_eq!(
            repo.head_commit().unwrap(),
            Some("abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string())
        );
    }
}
