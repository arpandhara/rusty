//! Ref and HEAD mutation for reset, plus the last-reset recovery pointer.
//!
//! All writes go through the atomic byte writer, so a crash leaves either the
//! old ref or the new ref, never a truncated hash. Ref names are validated
//! before they are joined onto `.rusty`, and only `refs/heads/*` may be moved
//! by reset.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::path_check::{validate_object_hash, validate_ref_path};
use crate::state::index::{fsync_dir_best_effort, write_bytes_atomic};
use crate::state::repository::Repository;

pub const RESET_RECOVERY_SCHEMA_VERSION: u32 = 1;

/// Durable record of the ref state before the most recent ref-moving reset.
///
/// Stored as JSON at `.rusty/refs/recover/last-reset`. It is written *before*
/// the ref is moved and is backed up by the operation journal so a rollback can
/// restore the previous record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetRecoveryRecord {
    pub schema_version: u32,
    pub operation_id: String,
    pub timestamp_unix_ms: u64,
    /// `Some("refs/heads/main")` when attached, `None` when detached.
    pub ref_name: Option<String>,
    /// Commit the ref pointed to before the reset. `None` means unborn.
    pub from_commit: Option<String>,
    /// Commit the reset moved the ref to.
    pub to_commit: String,
}

/// Path of the last-reset recovery pointer.
pub fn recovery_record_path(repo: &Repository) -> PathBuf {
    repo.rusty_dir
        .join("refs")
        .join("recover")
        .join("last-reset")
}

/// Validate a ref that reset is allowed to move.
pub fn validate_branch_ref(ref_name: &str) -> Result<()> {
    if !ref_name.starts_with("refs/heads/") {
        bail!(
            "Refusing to update '{}': reset only moves refs/heads/* references",
            ref_name
        );
    }

    validate_ref_path(ref_name).with_context(|| format!("Unsafe ref name '{}'", ref_name))?;

    if ref_name
        .split('/')
        .any(|component| component == ".rusty" || component == ".git")
    {
        bail!(
            "Unsafe ref name '{}': targets repository metadata",
            ref_name
        );
    }

    Ok(())
}

/// Resolve a validated branch ref to its file under `.rusty`.
pub fn ref_file_path(repo: &Repository, ref_name: &str) -> Result<PathBuf> {
    validate_branch_ref(ref_name)?;

    let refs_root = repo.rusty_dir.join("refs");
    let path = repo.rusty_dir.join(ref_name);

    if !path.starts_with(&refs_root) {
        bail!("Unsafe ref path '{}'", path.display());
    }

    Ok(path)
}

/// Read a branch ref, returning `None` when the branch is unborn.
pub fn read_ref_value(repo: &Repository, ref_name: &str) -> Result<Option<String>> {
    let path = ref_file_path(repo, ref_name)?;

    match fs::read_to_string(&path) {
        Ok(content) => {
            let value = content.trim();
            if value.is_empty() {
                Ok(None)
            } else {
                Ok(Some(value.to_string()))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Failed to read ref '{}'", path.display())),
    }
}

/// Write (or delete, when `commit` is `None`) a branch ref atomically.
pub fn write_ref_value(
    repo: &Repository,
    ref_name: &str,
    commit: Option<&str>,
) -> Result<()> {
    let path = ref_file_path(repo, ref_name)?;

    match commit {
        Some(hash) => {
            validate_object_hash(hash)?;
            let data = format!("{}\n", hash);
            write_bytes_atomic(&path, data.as_bytes())
        }
        None => {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Failed to remove ref '{}'", path.display()))
                }
            }
            prune_empty_ref_dirs(repo, &path);
            if let Some(parent) = path.parent() {
                fsync_dir_best_effort(parent);
            }
            Ok(())
        }
    }
}

/// Remove empty directories left behind after deleting a nested branch ref.
fn prune_empty_ref_dirs(repo: &Repository, ref_file: &Path) {
    let stop = repo.rusty_dir.join("refs").join("heads");
    let mut current = ref_file.parent();

    while let Some(dir) = current {
        if dir == stop || !dir.starts_with(&stop) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

/// Write detached `HEAD` atomically. `None` writes an empty HEAD (unborn).
pub fn write_detached_head(repo: &Repository, commit: Option<&str>) -> Result<()> {
    let path = repo.rusty_dir.join("HEAD");

    match commit {
        Some(hash) => {
            validate_object_hash(hash)?;
            let data = format!("{}\n", hash);
            write_bytes_atomic(&path, data.as_bytes())
        }
        None => write_bytes_atomic(&path, b"\n"),
    }
}

/// Move the ref named by a plan to `commit`. `None` means detached `HEAD`.
pub fn update_ref_to(repo: &Repository, ref_name: Option<&str>, commit: &str) -> Result<()> {
    match ref_name {
        Some(name) => write_ref_value(repo, name, Some(commit)),
        None => write_detached_head(repo, Some(commit)),
    }
}

/// Restore the ref state captured in the operation journal.
pub fn restore_original_ref(
    repo: &Repository,
    symbolic_ref: Option<&str>,
    head_commit: Option<&str>,
) -> Result<()> {
    match symbolic_ref {
        Some(name) => write_ref_value(repo, name, head_commit),
        None => write_detached_head(repo, head_commit),
    }
}

/// Read the current value of a ref, or detached `HEAD` when `ref_name` is
/// `None`.
pub fn current_ref_value(repo: &Repository, ref_name: Option<&str>) -> Result<Option<String>> {
    match ref_name {
        Some(name) => read_ref_value(repo, name),
        None => repo.head_commit(),
    }
}

/// Refuse to point a ref at a commit object that does not exist.
pub fn ensure_commit_object(repo: &Repository, commit: &str) -> Result<()> {
    validate_object_hash(commit)?;
    let path = repo
        .rusty_dir
        .join("objects")
        .join("commits")
        .join(commit);

    if !path.is_file() {
        bail!(
            "Commit object {} is missing; refusing to move a ref to it",
            commit
        );
    }

    Ok(())
}

/// Persist the recovery pointer atomically.
pub fn write_recovery_record(repo: &Repository, record: &ResetRecoveryRecord) -> Result<()> {
    let path = recovery_record_path(repo);
    let data =
        serde_json::to_vec_pretty(record).context("Failed to serialize reset recovery record")?;
    write_bytes_atomic(&path, &data)
}

/// Read and validate the recovery pointer. Returns `None` when no ref-moving
/// reset has completed.
pub fn read_recovery_record(repo: &Repository) -> Result<Option<ResetRecoveryRecord>> {
    let path = recovery_record_path(repo);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read reset recovery record '{}'", path.display()))
        }
    };

    let record: ResetRecoveryRecord = serde_json::from_slice(&data).with_context(|| {
        format!(
            "Failed to parse reset recovery record '{}'",
            path.display()
        )
    })?;

    if record.schema_version != RESET_RECOVERY_SCHEMA_VERSION {
        bail!(
            "Unsupported reset recovery record schema {} in '{}' (expected {})",
            record.schema_version,
            path.display(),
            RESET_RECOVERY_SCHEMA_VERSION
        );
    }

    validate_object_hash(&record.to_commit)?;
    ensure_commit_object(repo, &record.to_commit)?;

    if let Some(from) = &record.from_commit {
        validate_object_hash(from)?;
        ensure_commit_object(repo, from)?;
    }

    if let Some(ref_name) = &record.ref_name {
        validate_branch_ref(ref_name)?;
    }

    if record
        .operation_id
        .is_empty()
        || !record
            .operation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!(
            "Invalid operation id '{}' in reset recovery record",
            record.operation_id
        );
    }

    Ok(Some(record))
}

/// Restore the ref recorded in a recovery pointer to its pre-reset value.
pub fn restore_recovery_record(repo: &Repository, record: &ResetRecoveryRecord) -> Result<()> {
    if record.ref_name.is_none() && record.from_commit.is_none() {
        bail!("Cannot restore an unborn detached HEAD");
    }

    match &record.ref_name {
        Some(name) => write_ref_value(repo, name, record.from_commit.as_deref()),
        None => write_detached_head(repo, record.from_commit.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, Repository) {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repository::from_path(&rusty_dir).unwrap();
        (tmp, repo)
    }

    fn commit_hash(seed: u8) -> String {
        format!("{:02x}", seed).repeat(32)
    }

    fn write_commit_object(repo: &Repository, hash: &str) {
        fs::write(
            repo.rusty_dir.join("objects").join("commits").join(hash),
            b"{}",
        )
        .unwrap();
    }

    #[test]
    fn rejects_refs_outside_heads() {
        assert!(validate_branch_ref("refs/heads/main").is_ok());
        assert!(validate_branch_ref("refs/tags/v1").is_err());
        assert!(validate_branch_ref("refs/heads/../escape").is_err());
        assert!(validate_branch_ref("refs/heads/.rusty/x").is_err());
        assert!(validate_branch_ref("/refs/heads/main").is_err());
    }

    #[test]
    fn writes_and_reads_attached_ref() {
        let (_tmp, repo) = fixture();
        let hash = commit_hash(0xaa);
        write_commit_object(&repo, &hash);

        write_ref_value(&repo, "refs/heads/main", Some(&hash)).unwrap();
        assert_eq!(
            read_ref_value(&repo, "refs/heads/main").unwrap(),
            Some(hash.clone())
        );

        write_ref_value(&repo, "refs/heads/main", None).unwrap();
        assert_eq!(read_ref_value(&repo, "refs/heads/main").unwrap(), None);
        assert!(!repo.rusty_dir.join("refs/heads/main").exists());
    }

    #[test]
    fn deletes_nested_ref_and_prunes_directories() {
        let (_tmp, repo) = fixture();
        let hash = commit_hash(0xbb);
        write_commit_object(&repo, &hash);

        write_ref_value(&repo, "refs/heads/feature/topic", Some(&hash)).unwrap();
        assert!(repo.rusty_dir.join("refs/heads/feature/topic").exists());

        write_ref_value(&repo, "refs/heads/feature/topic", None).unwrap();
        assert!(!repo.rusty_dir.join("refs/heads/feature").exists());
        assert!(repo.rusty_dir.join("refs/heads").exists());
    }

    #[test]
    fn writes_and_reads_detached_head() {
        let (_tmp, repo) = fixture();
        let hash = commit_hash(0xcc);
        write_commit_object(&repo, &hash);

        write_detached_head(&repo, Some(&hash)).unwrap();
        assert_eq!(repo.head_commit().unwrap(), Some(hash));

        write_detached_head(&repo, None).unwrap();
        assert_eq!(repo.head_commit().unwrap(), None);
    }

    #[test]
    fn recovery_record_round_trips() {
        let (_tmp, repo) = fixture();
        let from = commit_hash(0x01);
        let to = commit_hash(0x02);
        write_commit_object(&repo, &from);
        write_commit_object(&repo, &to);

        let record = ResetRecoveryRecord {
            schema_version: RESET_RECOVERY_SCHEMA_VERSION,
            operation_id: "op-123".to_string(),
            timestamp_unix_ms: 42,
            ref_name: Some("refs/heads/main".to_string()),
            from_commit: Some(from.clone()),
            to_commit: to.clone(),
        };

        write_recovery_record(&repo, &record).unwrap();
        assert_eq!(read_recovery_record(&repo).unwrap(), Some(record));
    }

    #[test]
    fn rejects_recovery_record_with_missing_commit() {
        let (_tmp, repo) = fixture();
        let record = ResetRecoveryRecord {
            schema_version: RESET_RECOVERY_SCHEMA_VERSION,
            operation_id: "op-123".to_string(),
            timestamp_unix_ms: 42,
            ref_name: Some("refs/heads/main".to_string()),
            from_commit: None,
            to_commit: commit_hash(0x03),
        };

        write_recovery_record(&repo, &record).unwrap();
        assert!(read_recovery_record(&repo).is_err());
    }

    #[test]
    fn restores_attached_ref_from_record() {
        let (_tmp, repo) = fixture();
        let from = commit_hash(0x04);
        let to = commit_hash(0x05);
        write_commit_object(&repo, &from);
        write_commit_object(&repo, &to);
        write_ref_value(&repo, "refs/heads/main", Some(&to)).unwrap();

        let record = ResetRecoveryRecord {
            schema_version: RESET_RECOVERY_SCHEMA_VERSION,
            operation_id: "op-restore".to_string(),
            timestamp_unix_ms: 1,
            ref_name: Some("refs/heads/main".to_string()),
            from_commit: Some(from.clone()),
            to_commit: to,
        };

        restore_recovery_record(&repo, &record).unwrap();
        assert_eq!(
            read_ref_value(&repo, "refs/heads/main").unwrap(),
            Some(from)
        );
    }

    #[test]
    fn restores_unborn_attached_ref_by_deleting_it() {
        let (_tmp, repo) = fixture();
        let to = commit_hash(0x06);
        write_commit_object(&repo, &to);
        write_ref_value(&repo, "refs/heads/main", Some(&to)).unwrap();

        let record = ResetRecoveryRecord {
            schema_version: RESET_RECOVERY_SCHEMA_VERSION,
            operation_id: "op-unborn".to_string(),
            timestamp_unix_ms: 1,
            ref_name: Some("refs/heads/main".to_string()),
            from_commit: None,
            to_commit: to,
        };

        restore_recovery_record(&repo, &record).unwrap();
        assert!(!repo.rusty_dir.join("refs/heads/main").exists());
        assert_eq!(repo.head_commit().unwrap(), None);
    }
}
