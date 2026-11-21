use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::objects;
use crate::path_check::{safe_join, validate_object_hash};

/// The state of a worktree path immediately before a reset applies a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathState {
    Absent,
    File { hash: String, mode: Option<u32> },
    EmptyDirectory,
    NonEmptyDirectory,
}

/// Verify that tracked files in the index have not been modified or deleted,
/// and that no untracked file would be overwritten by `target_files`.
pub fn ensure_clean_working_tree(
    repo_path: &Path,
    target_files: &BTreeMap<String, String>,
) -> Result<()> {
    let repo_root = repo_path
        .parent()
        .context("Invalid repository path")?
        .canonicalize()?;

    let index = Index::load(repo_path)?;

    // Check tracked files for modifications
    for (path, entry) in &index.entries {
        let file_path = safe_join(&repo_root, path)?;

        if !file_path.exists() {
            anyhow::bail!(
                "Cannot proceed: local changes would be lost (deleted '{}')",
                path
            );
        }

        if file_path.is_file() {
            let current_hash = objects::hash_file(&file_path)?;

            if current_hash != entry.blob_hash {
                anyhow::bail!(
                    "Cannot proceed: local changes would be overwritten in '{}'",
                    path
                );
            }
        }
    }

    // Check untracked files that target branch would overwrite
    for target_path in target_files.keys() {
        let file_path = safe_join(&repo_root, target_path)?;

        if file_path.exists() && !index.contains(target_path) {
            anyhow::bail!(
                "Cannot proceed: untracked file '{}' would be overwritten",
                target_path
            );
        }
    }

    Ok(())
}

/// Restore working tree files to match `files`.
/// Tracked files missing from `files` are removed.
pub fn restore_working_tree(repo_path: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    let repo_root = repo_path
        .parent()
        .context("Invalid repository path")?
        .canonicalize()?;

    let current_index = Index::load(repo_path)?;

    // 1. Remove tracked files that do not exist in the target commit
    for path in current_index.entries.keys() {
        if !files.contains_key(path) && safe_join(&repo_root, path)?.exists() {
            remove_worktree_path(&repo_root, path)?;
            prune_empty_parents(&repo_root, path);
            println!("  - {}", path);
        }
    }

    // 2. Restore files from target commit
    for (path, blob_hash) in files {
        write_blob_to_worktree(repo_path, &repo_root, path, blob_hash, "checkout", None)?;
    }

    Ok(())
}

/// Inspect a worktree path without following symlinks.
pub fn inspect_path(repo_root: &Path, rel_path: &str) -> Result<PathState> {
    let path = safe_join(repo_root, rel_path)?;

    match fs::symlink_metadata(&path) {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(PathState::Absent)
        }
        Err(e) => Err(e).with_context(|| format!("Failed to inspect '{}'", path.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("Refusing to operate on symlink '{}'", path.display())
        }
        Ok(meta) if meta.is_dir() => {
            let mut entries = fs::read_dir(&path)
                .with_context(|| format!("Failed to read directory '{}'", path.display()))?;
            if entries.next().is_none() {
                Ok(PathState::EmptyDirectory)
            } else {
                Ok(PathState::NonEmptyDirectory)
            }
        }
        Ok(meta) if meta.is_file() => {
            let hash = objects::hash_file(&path)?;
            Ok(PathState::File {
                hash,
                mode: file_mode(&meta),
            })
        }
        Ok(_) => bail!("Refusing to operate on special file '{}'", path.display()),
    }
}

/// List every regular file beneath `rel_path`, returning repository-relative
/// slash-separated paths. Symlinks and special files cause an error because a
/// reset must never traverse or delete through them.
pub fn list_files_under(repo_root: &Path, rel_path: &str) -> Result<Vec<String>> {
    let start = safe_join(repo_root, rel_path)?;
    let mut files = Vec::new();
    collect_files(&start, rel_path, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files(current: &Path, rel_path: &str, files: &mut Vec<String>) -> Result<()> {
    let entries = fs::read_dir(current)
        .with_context(|| format!("Failed to read directory '{}'", current.display()))?;

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        let child_rel = format!("{}/{}", rel_path, name);

        if file_type.is_symlink() {
            bail!("Refusing to traverse symlink '{}'", entry.path().display());
        } else if file_type.is_dir() {
            collect_files(&entry.path(), &child_rel, files)?;
        } else if file_type.is_file() {
            files.push(child_rel);
        } else {
            bail!(
                "Refusing to operate on special file '{}'",
                entry.path().display()
            );
        }
    }

    Ok(())
}

/// Resolve a blob hash to an existing object file, validating the hash before
/// using it as a path component.
pub fn blob_path(repo_path: &Path, blob_hash: &str) -> Result<PathBuf> {
    validate_object_hash(blob_hash)?;
    let path = repo_path.join("objects").join("blobs").join(blob_hash);
    if !path.is_file() {
        bail!("Missing blob object {}", blob_hash);
    }
    Ok(path)
}

/// Read a blob from the object store.
pub fn read_blob(repo_path: &Path, blob_hash: &str) -> Result<Vec<u8>> {
    let path = blob_path(repo_path, blob_hash)?;
    fs::read(&path).with_context(|| format!("Missing or unreadable blob object {}", blob_hash))
}

/// Write `blob_hash` to `rel_path` using a temporary sibling file and an atomic
/// rename. The temporary file lives in the destination directory so the
/// rename never crosses a filesystem boundary.
pub fn write_blob_to_worktree(
    repo_path: &Path,
    repo_root: &Path,
    rel_path: &str,
    blob_hash: &str,
    operation_id: &str,
    mode: Option<u32>,
) -> Result<()> {
    let data = read_blob(repo_path, blob_hash)?;
    write_bytes_to_worktree(repo_root, rel_path, &data, operation_id, mode)
}

/// Write raw bytes to a worktree path atomically.
pub fn write_bytes_to_worktree(
    repo_root: &Path,
    rel_path: &str,
    data: &[u8],
    operation_id: &str,
    mode: Option<u32>,
) -> Result<()> {
    // Validate before creating parents so traversal is rejected early.
    safe_join(repo_root, rel_path)?;
    ensure_parent_dirs(repo_root, rel_path)?;
    // Re-validate after creating parents in case a symlink appeared while we
    // were creating directories.
    let dest = safe_join(repo_root, rel_path)?;

    let parent = dest
        .parent()
        .with_context(|| format!("Path '{}' has no parent", dest.display()))?;
    let file_name = dest
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let temp_path = parent.join(format!(
        ".{}.rusty-tmp-{}",
        file_name,
        sanitize_operation_id(operation_id)
    ));

    // A previous crash may have left the same temporary name behind. It is
    // safe to remove: the name is scoped to this operation id.
    let _ = fs::remove_file(&temp_path);

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("Failed to create temporary file '{}'", temp_path.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write '{}'", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to fsync '{}'", temp_path.display()))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }

    if let Some(mode) = mode {
        set_mode(&temp_path, mode)?;
    }

    // If a directory is in the way and it is empty, remove it. A non-empty
    // directory is rejected by plan validation, but we re-check here to avoid
    // ever deleting untracked content.
    match fs::symlink_metadata(&dest) {
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir(&dest).with_context(|| {
                format!(
                    "Cannot replace non-empty directory '{}' with a file",
                    dest.display()
                )
            })?;
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("Refusing to replace symlink '{}'", dest.display())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let _ = fs::remove_file(&temp_path);
            return Err(e)
                .with_context(|| format!("Failed to inspect '{}'", dest.display()));
        }
    }

    crate::state::index::make_writable_best_effort(&dest);

    if let Err(e) = fs::rename(&temp_path, &dest) {
        let _ = fs::remove_file(&temp_path);
        return Err(e).with_context(|| {
            format!(
                "Failed to atomically replace '{}' (temporary file '{}')",
                dest.display(),
                temp_path.display()
            )
        });
    }

    crate::state::index::fsync_dir_best_effort(parent);
    Ok(())
}

/// Remove a file (or an empty directory) at `rel_path`.
pub fn remove_worktree_path(repo_root: &Path, rel_path: &str) -> Result<()> {
    let path = safe_join(repo_root, rel_path)?;

    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("Failed to inspect '{}'", path.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("Refusing to delete symlink '{}'", path.display())
        }
        Ok(meta) if meta.is_dir() => fs::remove_dir(&path).with_context(|| {
            format!(
                "Cannot delete directory '{}': it is not empty",
                path.display()
            )
        }),
        Ok(_) => {
            crate::state::index::make_writable_best_effort(&path);
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete '{}'", path.display()))
        }
    }
}

/// Remove empty parent directories after a deletion, stopping at the
/// repository root. Non-empty directories and permission failures are left
/// alone; an empty directory remaining is harmless.
pub fn prune_empty_parents(repo_root: &Path, rel_path: &str) {
    let mut current = repo_root.join(rel_path);
    while current.pop() {
        if current == repo_root || !current.starts_with(repo_root) {
            break;
        }
        if fs::remove_dir(&current).is_err() {
            break;
        }
    }
}

/// Remove a leftover temporary sibling file created by
/// [`write_bytes_to_worktree`] for the given operation.
pub fn remove_stale_temp_file(repo_root: &Path, rel_path: &str, operation_id: &str) -> Result<()> {
    let dest = safe_join(repo_root, rel_path)?;
    let Some(parent) = dest.parent() else {
        return Ok(());
    };
    let file_name = dest
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let temp_path = parent.join(format!(
        ".{}.rusty-tmp-{}",
        file_name,
        sanitize_operation_id(operation_id)
    ));

    match fs::remove_file(&temp_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "Failed to remove stale temporary file '{}'",
                temp_path.display()
            )
        }),
    }
}

/// Restore a file from raw backup bytes and (optionally) its original mode.
pub fn restore_file(repo_root: &Path, rel_path: &str, data: &[u8], mode: Option<u32>) -> Result<()> {
    let dest = safe_join(repo_root, rel_path)?;
    ensure_parent_dirs(repo_root, rel_path)?;

    // A partially applied reset can leave an empty directory where the
    // original file used to be (for example during a file -> tree transition).
    if let Ok(meta) = fs::symlink_metadata(&dest)
        && meta.is_dir()
    {
        fs::remove_dir(&dest).with_context(|| {
            format!(
                "Cannot restore '{}': directory is not empty",
                dest.display()
            )
        })?;
    }

    crate::state::index::write_bytes_atomic(&dest, data)?;
    if let Some(mode) = mode {
        set_mode(&dest, mode)?;
    }
    Ok(())
}

/// Recreate an originally empty directory at `rel_path`.
pub fn restore_empty_directory(repo_root: &Path, rel_path: &str) -> Result<()> {
    let dest = safe_join(repo_root, rel_path)?;
    ensure_parent_dirs(repo_root, rel_path)?;
    match fs::symlink_metadata(&dest) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => {
            remove_worktree_path(repo_root, rel_path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to inspect '{}'", dest.display()))
        }
    }
    create_directory(&dest)?;
    Ok(())
}

/// Ensure each parent component of `rel_path` exists and is a real directory.
pub fn ensure_parent_dirs(repo_root: &Path, rel_path: &str) -> Result<()> {
    let components: Vec<&str> = rel_path.split('/').collect();
    if components.len() <= 1 {
        return Ok(());
    }

    let mut current = repo_root.to_path_buf();
    for component in &components[..components.len() - 1] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("Refusing to traverse symlink '{}'", current.display())
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "Cannot create '{}': '{}' is not a directory",
                rel_path,
                current.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_directory(&current)?;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to inspect '{}'", current.display()))
            }
        }
    }

    Ok(())
}

fn create_directory(path: &Path) -> Result<()> {
    fs::create_dir(path)
        .with_context(|| format!("Failed to create directory '{}'", path.display()))?;
    if let Some(parent) = path.parent() {
        crate::state::index::fsync_dir_best_effort(parent);
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("Failed to restore mode on '{}'", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn file_mode(meta: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn sanitize_operation_id(operation_id: &str) -> String {
    operation_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_and_delete_nested_file() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let repo = root.join(".rusty");
        fs::create_dir_all(repo.join("objects").join("blobs")).unwrap();

        write_bytes_to_worktree(root, "a/b/c.txt", b"hello", "op-1", None).unwrap();
        assert_eq!(fs::read(root.join("a/b/c.txt")).unwrap(), b"hello");

        remove_worktree_path(root, "a/b/c.txt").unwrap();
        prune_empty_parents(root, "a/b/c.txt");
        assert!(!root.join("a").exists());
    }

    #[test]
    fn inspect_path_distinguishes_empty_and_nonempty_directory() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("full")).unwrap();
        fs::write(root.join("full/file"), b"x").unwrap();
        fs::write(root.join("file"), b"x").unwrap();

        assert_eq!(inspect_path(root, "empty").unwrap(), PathState::EmptyDirectory);
        assert_eq!(
            inspect_path(root, "full").unwrap(),
            PathState::NonEmptyDirectory
        );
        assert!(matches!(
            inspect_path(root, "file").unwrap(),
            PathState::File { .. }
        ));
        assert_eq!(inspect_path(root, "missing").unwrap(), PathState::Absent);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_read_only_file_succeeds() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("file"), b"old").unwrap();
        fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o444)).unwrap();

        write_bytes_to_worktree(root, "file", b"new", "op-2", Some(0o444)).unwrap();

        assert_eq!(fs::read(root.join("file")).unwrap(), b"new");
        let mode = fs::metadata(root.join("file")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
    }
}
