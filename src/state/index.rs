use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::index::Index;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Serialize an index to its on-disk representation.
pub fn serialize_index(index: &Index) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(index).context("Failed to serialize index")
}

/// Read the raw index file, returning `None` when the file does not exist.
///
/// An empty file is treated as a valid empty index by [`Index::load`], but it
/// is still returned here so callers can back up the exact bytes they saw.
pub fn read_index_bytes(repo_path: &Path) -> Result<Option<Vec<u8>>> {
    let index_path = repo_path.join("index");
    match fs::read(&index_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e)
            .with_context(|| format!("Failed to read index at '{}'", index_path.display())),
    }
}

/// Parse raw index bytes. Empty bytes mean an empty index.
pub fn parse_index_bytes(bytes: &[u8]) -> Result<Index> {
    if bytes.is_empty() {
        return Ok(Index::new());
    }
    serde_json::from_slice(bytes).context("Failed to parse index file")
}

/// SHA-256 checksum of raw index bytes, or `None` when there was no index file.
pub fn checksum_bytes(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(|data| {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    })
}

/// Save an index atomically by writing to a temporary sibling file, fsyncing,
/// and renaming over `index`.
pub fn save_index_atomic(index: &Index, repo_path: &Path) -> Result<()> {
    let bytes = serialize_index(index)?;
    write_bytes_atomic(&repo_path.join("index"), &bytes)
}

/// Atomically replace `path` with `bytes`.
///
/// The temporary file is created next to the destination so the final rename
/// never crosses a filesystem boundary. This is deliberately more robust than
/// staging under `.rusty`: the worktree and `.rusty` may live on different
/// mounts (or different devices after a bind mount), and `rename(2)` is only
/// atomic within one filesystem.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("Path '{}' has no parent directory", path.display()))?;

    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;

    let temp_path = temp_sibling(path);

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("Failed to create temporary file '{}'", temp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Failed to write temporary file '{}'", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to fsync temporary file '{}'", temp_path.display()))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }

    // Windows refuses to replace a read-only destination. On POSIX this is a
    // no-op from the rename's perspective, but clearing the bit also makes the
    // subsequent delete/modify semantics predictable for callers.
    make_writable_best_effort(path);

    if let Err(e) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(e).with_context(|| {
            format!(
                "Failed to atomically replace '{}' with '{}'",
                path.display(),
                temp_path.display()
            )
        });
    }

    fsync_dir_best_effort(parent);
    Ok(())
}

/// Best-effort removal of the read-only bit. Errors are ignored because the
/// caller will surface the real rename/delete error if the operation still
/// fails.
pub fn make_writable_best_effort(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return;
        }
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

/// Fsync a directory so a preceding rename is durable. Best effort: some
/// platforms (notably Windows) cannot open a directory as a file.
pub fn fsync_dir_best_effort(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(file) = File::open(dir) {
            let _ = file.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".{}.rusty-tmp-{}-{}",
        file_name,
        std::process::id(),
        counter
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_replaces_existing_content() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("index");
        fs::write(&path, b"old").unwrap();

        write_bytes_atomic(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        // No temporary siblings left behind.
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("rusty-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn atomic_write_creates_missing_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("index");

        write_bytes_atomic(&path, b"data").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"data");
    }

    #[test]
    fn index_checksum_distinguishes_empty_and_missing() {
        assert_eq!(checksum_bytes(None), None);
        assert!(checksum_bytes(Some(b"")).is_some());
    }
}
