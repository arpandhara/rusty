use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::path_check::safe_join;
use crate::reset::plan::{ChangeKind, IndexUpdatePlan, ResetDestinations, ResetPlan};
use crate::state::index::{
    checksum_bytes, fsync_dir_best_effort, parse_index_bytes, read_index_bytes, save_index_atomic,
    write_bytes_atomic,
};
use crate::state::repository::Repository;
use crate::state::worktree::{
    inspect_path, list_files_under, remove_stale_temp_file, remove_worktree_path,
    restore_empty_directory, restore_file, write_blob_to_worktree, PathState,
};

pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

/// The durable phase marker. A journal at [`OperationPhase::Committed`] is
/// rolled forward; every earlier phase is rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Prepared,
    WorktreeApplied,
    IndexApplied,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalKind {
    Absent,
    File,
    EmptyDirectory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub path: String,
    pub kind: OriginalKind,
    /// Backup file path relative to the operation directory, when the original
    /// was a regular file.
    pub backup_path: Option<String>,
    /// Unix permission bits (including setuid/setgid/sticky) when known.
    pub mode: Option<u32>,
    /// SHA-256 of the backed-up bytes, verified before restoration.
    #[serde(default)]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBackup {
    pub existed: bool,
    pub backup_path: Option<String>,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginalRefState {
    pub head_commit: Option<String>,
    pub symbolic_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationJournal {
    pub schema_version: u32,
    pub operation_id: String,
    pub phase: OperationPhase,
    pub created_at_unix_ms: u64,
    pub command_args: Vec<String>,
    pub plan: ResetPlan,
    pub original_ref: OriginalRefState,
    pub index: IndexBackup,
    pub backups: Vec<BackupEntry>,
}

/// One append-only line in `.rusty/ledger/operations.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecord {
    pub schema_version: u32,
    pub operation_id: String,
    pub timestamp_unix_ms: u64,
    pub command_args: Vec<String>,
    pub from_commit: Option<String>,
    pub to_commit: String,
    pub branch_ref: Option<String>,
    pub destinations: ResetDestinations,
    pub changed_paths: Vec<String>,
    /// `completed`, `rolled_back`, or `rolled_forward`.
    pub status: String,
    /// True when the record was produced by startup crash recovery.
    pub recovered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    RolledBack,
    RolledForward,
}

impl std::fmt::Display for RecoveryOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryOutcome::RolledBack => write!(f, "rolled back an interrupted reset"),
            RecoveryOutcome::RolledForward => write!(f, "completed an interrupted reset"),
        }
    }
}

pub fn operations_dir(repo: &Repository) -> PathBuf {
    repo.rusty_dir.join("operations")
}

pub fn active_journal_path(repo: &Repository) -> PathBuf {
    operations_dir(repo).join("active.json")
}

pub fn ledger_path(repo: &Repository) -> PathBuf {
    repo.rusty_dir.join("ledger").join("operations.jsonl")
}

pub fn validate_operation_id(operation_id: &str) -> Result<()> {
    let valid = !operation_id.is_empty()
        && operation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');

    if !valid {
        bail!(
            "Invalid operation id '{}': only ASCII letters, digits, and '-' are allowed",
            operation_id
        );
    }
    Ok(())
}

pub fn operation_dir(repo: &Repository, operation_id: &str) -> Result<PathBuf> {
    validate_operation_id(operation_id)?;
    Ok(operations_dir(repo).join(operation_id))
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Persist the journal atomically and durably. The journal is the recovery
/// contract: it must be on disk before any user file or the index is touched.
pub fn write_journal(repo: &Repository, journal: &OperationJournal) -> Result<()> {
    let ops_dir = operations_dir(repo);
    fs::create_dir_all(&ops_dir)
        .with_context(|| format!("Failed to create '{}'", ops_dir.display()))?;
    fsync_dir_best_effort(&ops_dir);

    let journal_path = active_journal_path(repo);
    let data = serde_json::to_vec_pretty(journal).context("Failed to serialize operation journal")?;
    write_bytes_atomic(&journal_path, &data)
}

pub fn load_active_journal(repo: &Repository) -> Result<Option<OperationJournal>> {
    let path = active_journal_path(repo);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read operation journal '{}'", path.display()))
        }
    };

    let journal: OperationJournal = serde_json::from_slice(&data)
        .with_context(|| format!("Failed to parse operation journal '{}'", path.display()))?;

    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        bail!(
            "Unsupported operation journal schema {} in '{}' (expected {}). \
             Refusing to guess how to recover it.",
            journal.schema_version,
            path.display(),
            JOURNAL_SCHEMA_VERSION
        );
    }

    validate_operation_id(&journal.operation_id)?;
    Ok(Some(journal))
}

fn verify_checksum(data: &[u8], expected: Option<&str>, label: &str) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = checksum_bytes(Some(data)).unwrap_or_default();
    if actual != expected {
        bail!(
            "Corrupt {}: checksum mismatch (expected {}, got {}). \
             Refusing to restore unverified data.",
            label,
            expected,
            actual
        );
    }
    Ok(())
}

/// Capture the current state of a worktree path into the operation backup
/// directory.
pub fn capture_original(
    repo: &Repository,
    op_dir: &Path,
    rel_path: &str,
) -> Result<BackupEntry> {
    match inspect_path(&repo.root, rel_path)? {
        PathState::Absent => Ok(BackupEntry {
            path: rel_path.to_string(),
            kind: OriginalKind::Absent,
            backup_path: None,
            mode: None,
            checksum: None,
        }),
        PathState::File { mode, .. } => {
            let rel_backup = format!("backups/files/{}", rel_path);
            let source = safe_join(&repo.root, rel_path)?;
            let data = fs::read(&source)
                .with_context(|| format!("Failed to back up '{}'", source.display()))?;
            let checksum = checksum_bytes(Some(&data));
            let backup_path = op_dir.join(&rel_backup);
            write_bytes_atomic(&backup_path, &data)?;
            Ok(BackupEntry {
                path: rel_path.to_string(),
                kind: OriginalKind::File,
                backup_path: Some(rel_backup),
                mode,
                checksum,
            })
        }
        PathState::EmptyDirectory => Ok(BackupEntry {
            path: rel_path.to_string(),
            kind: OriginalKind::EmptyDirectory,
            backup_path: None,
            mode: None,
            checksum: None,
        }),
        PathState::NonEmptyDirectory => bail!(
            "Refusing to reset '{}': it is a non-empty directory and may contain untracked files",
            rel_path
        ),
    }
}

/// Capture a worktree path, treating a non-empty directory as an empty
/// directory when every file beneath it is already scheduled for deletion.
/// Child paths get their own file backups, so rollback can rebuild the tree.
fn capture_original_for_change(
    repo: &Repository,
    op_dir: &Path,
    plan: &ResetPlan,
    rel_path: &str,
) -> Result<BackupEntry> {
    if matches!(
        inspect_path(&repo.root, rel_path)?,
        PathState::NonEmptyDirectory
    ) {
        let files = list_files_under(&repo.root, rel_path)?;
        let all_deleted = files.iter().all(|path| {
            plan.worktree_changes
                .iter()
                .any(|change| change.kind == ChangeKind::Delete && &change.path == path)
        });

        if !all_deleted {
            bail!(
                "Refusing to reset '{}': it is a non-empty directory containing files \
                 outside this reset's selected scope",
                rel_path
            );
        }

        return Ok(BackupEntry {
            path: rel_path.to_string(),
            kind: OriginalKind::EmptyDirectory,
            backup_path: None,
            mode: None,
            checksum: None,
        });
    }

    capture_original(repo, op_dir, rel_path)
}

/// Copy the current index into the operation directory.
pub fn backup_index(repo: &Repository, op_dir: &Path) -> Result<IndexBackup> {
    let bytes = read_index_bytes(&repo.rusty_dir)?;
    match bytes {
        Some(data) => {
            let rel_backup = "backups/index".to_string();
            write_bytes_atomic(&op_dir.join(&rel_backup), &data)?;
            Ok(IndexBackup {
                existed: true,
                backup_path: Some(rel_backup),
                checksum: checksum_bytes(Some(&data)),
            })
        }
        None => Ok(IndexBackup {
            existed: false,
            backup_path: None,
            checksum: None,
        }),
    }
}

/// Restore the index that was present before the operation.
pub fn restore_index(repo: &Repository, op_dir: &Path, backup: &IndexBackup) -> Result<()> {
    let index_path = repo.rusty_dir.join("index");
    if backup.existed {
        let rel = backup
            .backup_path
            .as_deref()
            .context("Index backup is marked as existing but has no backup path")?;
        let data = fs::read(op_dir.join(rel)).with_context(|| {
            format!(
                "Missing index backup '{}' needed for recovery",
                op_dir.join(rel).display()
            )
        })?;
        verify_checksum(&data, backup.checksum.as_deref(), "index backup")?;
        write_bytes_atomic(&index_path, &data)
    } else {
        match fs::remove_file(&index_path) {
            Ok(()) => {
                fsync_dir_best_effort(&repo.rusty_dir);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| {
                format!("Failed to remove index '{}' during recovery", index_path.display())
            }),
        }
    }
}

/// Restore one worktree path to the state captured in `entry`.
pub fn restore_original(repo: &Repository, op_dir: &Path, entry: &BackupEntry) -> Result<()> {
    match entry.kind {
        OriginalKind::Absent => match inspect_path(&repo.root, &entry.path)? {
            PathState::Absent => Ok(()),
            PathState::File { .. } | PathState::EmptyDirectory => {
                remove_worktree_path(&repo.root, &entry.path)?;
                crate::state::worktree::prune_empty_parents(&repo.root, &entry.path);
                Ok(())
            }
            PathState::NonEmptyDirectory => bail!(
                "Cannot roll back '{}': it is now a non-empty directory. \
                 Move its contents aside and retry recovery.",
                entry.path
            ),
        },
        OriginalKind::File => {
            if matches!(
                inspect_path(&repo.root, &entry.path)?,
                PathState::NonEmptyDirectory
            ) {
                bail!(
                    "Cannot roll back '{}': it is now a non-empty directory",
                    entry.path
                );
            }
            let rel = entry
                .backup_path
                .as_deref()
                .context("File backup entry has no backup path")?;
            let data = fs::read(op_dir.join(rel)).with_context(|| {
                format!(
                    "Missing file backup '{}' needed for recovery",
                    op_dir.join(rel).display()
                )
            })?;
            verify_checksum(
                &data,
                entry.checksum.as_deref(),
                &format!("file backup for '{}'", entry.path),
            )?;
            restore_file(&repo.root, &entry.path, &data, entry.mode)
        }
        OriginalKind::EmptyDirectory => {
            match inspect_path(&repo.root, &entry.path)? {
                PathState::Absent => {}
                PathState::File { .. } => remove_worktree_path(&repo.root, &entry.path)?,
                PathState::EmptyDirectory => {}
                PathState::NonEmptyDirectory => bail!(
                    "Cannot roll back '{}': it is now a non-empty directory",
                    entry.path
                ),
            }
            restore_empty_directory(&repo.root, &entry.path)
        }
    }
}

fn remove_stale_temps(repo: &Repository, journal: &OperationJournal) {
    for change in &journal.plan.worktree_changes {
        let _ = remove_stale_temp_file(&repo.root, &change.path, &journal.operation_id);
    }
}

/// Apply the worktree portion of a plan. Deletions run first so a file can be
/// replaced by a directory tree and vice versa.
pub fn apply_plan_worktree(repo: &Repository, journal: &OperationJournal) -> Result<()> {
    // Delete deepest paths first so replacing a directory tree with a file at
    // an ancestor works without ever recursively deleting untracked content.
    let mut deletions: Vec<_> = journal
        .plan
        .worktree_changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Delete)
        .collect();
    deletions.sort_by(|a, b| {
        let a_depth = a.path.matches('/').count();
        let b_depth = b.path.matches('/').count();
        b_depth.cmp(&a_depth).then_with(|| a.path.cmp(&b.path))
    });

    for change in deletions {
        remove_worktree_path(&repo.root, &change.path)?;
        crate::state::worktree::prune_empty_parents(&repo.root, &change.path);
    }

    for change in journal
        .plan
        .worktree_changes
        .iter()
        .filter(|change| change.kind != ChangeKind::Delete)
    {
        let new_blob = change
            .new_blob
            .as_deref()
            .context("Create/modify worktree change has no target blob")?;
        let mode = journal
            .backups
            .iter()
            .find(|backup| backup.path == change.path && backup.kind == OriginalKind::File)
            .and_then(|backup| backup.mode);
        write_blob_to_worktree(
            &repo.rusty_dir,
            &repo.root,
            &change.path,
            new_blob,
            &journal.operation_id,
            mode,
        )?;
    }

    Ok(())
}

/// Rebuild the exact index a plan describes from the pre-operation backup.
///
/// This is deterministic and does not depend on the current on-disk index, so
/// it is safe to replay during roll-forward after a crash at any point.
pub fn build_planned_index(op_dir: &Path, journal: &OperationJournal) -> Result<Index> {
    let base = if journal.index.existed {
        let rel = journal
            .index
            .backup_path
            .as_deref()
            .context("Index backup is marked as existing but has no backup path")?;
        let data = fs::read(op_dir.join(rel)).with_context(|| {
            format!(
                "Missing index backup '{}' needed to rebuild index",
                op_dir.join(rel).display()
            )
        })?;
        verify_checksum(&data, journal.index.checksum.as_deref(), "index backup")?;
        parse_index_bytes(&data)?
    } else {
        Index::new()
    };

    let mut index = base;
    if let Some(IndexUpdatePlan {
        entries_to_add_or_update,
        entries_to_remove,
    }) = &journal.plan.index_update
    {
        for (path, blob_hash) in entries_to_add_or_update {
            index.add(path.clone(), blob_hash.clone());
        }
        for path in entries_to_remove {
            index.remove(path);
        }
    }

    Ok(index)
}

/// Clear the active journal and operation directory after the ledger record
/// has been made durable.
pub fn finalize_journal(
    repo: &Repository,
    journal: &OperationJournal,
    status: &str,
    recovered: bool,
) -> Result<()> {
    append_ledger(repo, &ledger_record_from_journal(journal, status, recovered))?;

    let active = active_journal_path(repo);
    match fs::remove_file(&active) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to clear operation journal '{}'", active.display())
            })
        }
    }
    fsync_dir_best_effort(&operations_dir(repo));

    let op_dir = operation_dir(repo, &journal.operation_id)?;
    match fs::remove_dir_all(&op_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Operation completed but its backup directory '{}' could not be removed",
                    op_dir.display()
                )
            })
        }
    }

    Ok(())
}

/// Undo a failed in-process operation while the caller still holds the lock.
/// If this itself fails, the active journal remains for startup recovery.
pub fn abort_journal(repo: &Repository, journal: &OperationJournal) -> Result<()> {
    let op_dir = operation_dir(repo, &journal.operation_id)?;
    rollback(repo, &op_dir, journal)?;
    finalize_journal(repo, journal, "rolled_back", false)
}

fn path_depth(path: &str) -> usize {
    path.matches('/').count()
}

/// Remove whatever currently occupies `rel_path`, without ever deleting a
/// non-empty directory. Rollback clears all affected paths deepest-first and
/// only then restores backups parents-first; this makes file <-> directory
/// transitions reversible in both directions.
fn clear_current_path(repo: &Repository, rel_path: &str) -> Result<()> {
    let path = safe_join(&repo.root, rel_path)?;

    match fs::symlink_metadata(&path) {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to inspect '{}'", path.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("Cannot roll back through symlink '{}'", path.display())
        }
        Ok(meta) if meta.is_file() => {
            crate::state::index::make_writable_best_effort(&path);
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove '{}'", path.display()))?;
            crate::state::worktree::prune_empty_parents(&repo.root, rel_path);
            Ok(())
        }
        Ok(meta) if meta.is_dir() => {
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "Cannot roll back '{}': the directory is not empty and may contain \
                             untracked files",
                            path.display()
                        )
                    })
                }
            }
            crate::state::worktree::prune_empty_parents(&repo.root, rel_path);
            Ok(())
        }
        Ok(_) => bail!("Cannot roll back special file '{}'", path.display()),
    }
}

fn rollback(repo: &Repository, op_dir: &Path, journal: &OperationJournal) -> Result<()> {
    remove_stale_temps(repo, journal);

    // Phase 1: clear current state deepest-first so a parent file can be
    // restored after its created descendants have been removed, and a parent
    // directory can be restored after its file descendants have been removed.
    let mut entries: Vec<&BackupEntry> = journal.backups.iter().collect();
    entries.sort_by(|a, b| {
        path_depth(&b.path)
            .cmp(&path_depth(&a.path))
            .then_with(|| b.path.cmp(&a.path))
    });
    for entry in &entries {
        clear_current_path(repo, &entry.path)?;
    }

    // Phase 2: restore originals parents-first so child paths have a parent
    // directory to land in.
    entries.sort_by(|a, b| {
        path_depth(&a.path)
            .cmp(&path_depth(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });
    for entry in &entries {
        restore_original(repo, op_dir, entry)?;
    }

    if journal.plan.destinations.index {
        restore_index(repo, op_dir, &journal.index)?;
    }

    Ok(())
}

fn roll_forward(repo: &Repository, op_dir: &Path, journal: &OperationJournal) -> Result<()> {
    apply_plan_worktree(repo, journal)?;

    if journal.plan.destinations.index {
        let index = build_planned_index(op_dir, journal)?;
        save_index_atomic(&index, &repo.rusty_dir)?;
    }

    Ok(())
}

pub fn ledger_record_from_journal(
    journal: &OperationJournal,
    status: &str,
    recovered: bool,
) -> LedgerRecord {
    let mut changed_paths = BTreeSet::new();

    if let Some(update) = &journal.plan.index_update {
        for (path, _) in &update.entries_to_add_or_update {
            changed_paths.insert(path.clone());
        }
        for path in &update.entries_to_remove {
            changed_paths.insert(path.clone());
        }
    }

    for change in &journal.plan.worktree_changes {
        changed_paths.insert(change.path.clone());
    }

    LedgerRecord {
        schema_version: LEDGER_SCHEMA_VERSION,
        operation_id: journal.operation_id.clone(),
        timestamp_unix_ms: now_unix_ms(),
        command_args: journal.command_args.clone(),
        from_commit: journal
            .plan
            .ref_update
            .as_ref()
            .and_then(|update| update.from_commit.clone())
            .or_else(|| journal.original_ref.head_commit.clone()),
        to_commit: journal.plan.target_commit.clone(),
        branch_ref: journal
            .plan
            .ref_update
            .as_ref()
            .and_then(|update| update.target_ref.clone())
            .or_else(|| journal.original_ref.symbolic_ref.clone()),
        destinations: journal.plan.destinations,
        changed_paths: changed_paths.into_iter().collect(),
        status: status.to_string(),
        recovered,
    }
}

pub fn ledger_contains(repo: &Repository, operation_id: &str) -> Result<bool> {
    let path = ledger_path(repo);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read ledger '{}'", path.display()))
        }
    };

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
            && value.get("operation_id").and_then(|v| v.as_str()) == Some(operation_id)
        {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Append a completed/recovered record to the operation ledger. Appending is
/// idempotent per operation id, so replaying recovery never duplicates a line.
pub fn append_ledger(repo: &Repository, record: &LedgerRecord) -> Result<()> {
    if ledger_contains(repo, &record.operation_id)? {
        return Ok(());
    }

    let path = ledger_path(repo);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create '{}'", parent.display()))?;
    }

    let line = serde_json::to_string(record).context("Failed to serialize ledger record")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open ledger '{}'", path.display()))?;

    // A crash can leave a partial final line. Isolate it instead of joining
    // the new record to the truncated JSON.
    if fs::metadata(&path).map(|meta| meta.len() > 0).unwrap_or(false)
        && !file_ends_with_newline(&path)?
    {
        file.write_all(b"\n")?;
    }

    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("Failed to fsync ledger '{}'", path.display()))?;

    if let Some(parent) = path.parent() {
        fsync_dir_best_effort(parent);
    }

    Ok(())
}

fn file_ends_with_newline(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = fs::File::open(path)
        .with_context(|| format!("Failed to open '{}'", path.display()))?;
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

/// Complete or undo an interrupted operation. Returns `None` when there is
/// nothing to recover.
pub fn recover_if_needed(repo: &Repository) -> Result<Option<RecoveryOutcome>> {
    if !active_journal_path(repo).exists() {
        return Ok(None);
    }

    let _lock = repo.lock_for_recovery()?;

    // Another process may have recovered while we waited for the lock.
    let Some(journal) = load_active_journal(repo)? else {
        return Ok(None);
    };

    let op_dir = operation_dir(repo, &journal.operation_id)?;

    let (status, outcome) = if journal.phase == OperationPhase::Committed {
        roll_forward(repo, &op_dir, &journal)?;
        ("rolled_forward", RecoveryOutcome::RolledForward)
    } else {
        rollback(repo, &op_dir, &journal)?;
        ("rolled_back", RecoveryOutcome::RolledBack)
    };

    finalize_journal(repo, &journal, status, true)?;

    Ok(Some(outcome))
}

/// Remove operation directories that have no active journal. Must be called
/// while holding the repository lock.
pub fn cleanup_orphan_operation_dirs(repo: &Repository, keep: Option<&str>) -> Result<()> {
    if active_journal_path(repo).exists() {
        return Ok(());
    }

    let ops_dir = operations_dir(repo);
    let entries = match fs::read_dir(&ops_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read '{}'", ops_dir.display()))
        }
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if keep == Some(name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.starts_with(&ops_dir) {
            fs::remove_dir_all(&path)
                .with_context(|| format!("Failed to remove orphaned operation '{}'", path.display()))?;
        }
    }

    Ok(())
}

/// Build the journal for a plan, capturing backups first.
pub fn prepare_journal(
    repo: &Repository,
    plan: &ResetPlan,
    command_args: Vec<String>,
) -> Result<OperationJournal> {
    let op_dir = operation_dir(repo, &plan.operation_id)?;
    if op_dir.exists() {
        bail!(
            "Operation directory '{}' already exists; refusing to overwrite it",
            op_dir.display()
        );
    }

    fs::create_dir_all(op_dir.join("backups").join("files"))
        .with_context(|| format!("Failed to create operation directory '{}'", op_dir.display()))?;

    let index = if plan.destinations.index {
        backup_index(repo, &op_dir)?
    } else {
        IndexBackup {
            existed: false,
            backup_path: None,
            checksum: None,
        }
    };

    let mut backups = Vec::new();
    if plan.destinations.worktree {
        for change in &plan.worktree_changes {
            backups.push(capture_original_for_change(repo, &op_dir, plan, &change.path)?);
        }
    }

    Ok(OperationJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        operation_id: plan.operation_id.clone(),
        phase: OperationPhase::Prepared,
        created_at_unix_ms: now_unix_ms(),
        command_args,
        plan: plan.clone(),
        original_ref: OriginalRefState {
            head_commit: repo.head_commit()?,
            symbolic_ref: repo.current_symbolic_ref()?,
        },
        index,
        backups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::Commit;
    use crate::reset::plan::{ChangeKind, IndexUpdatePlan, RefUpdatePlan, WorktreeChange};
    use crate::tree::{Tree, TreeEntry};
    use sha2::{Digest, Sha256};
    use std::fs;
    use tempfile::tempdir;

    fn write_blob(rusty_dir: &Path, content: &[u8]) -> String {
        let hash = hex::encode(Sha256::digest(content));
        fs::write(rusty_dir.join("objects").join("blobs").join(&hash), content).unwrap();
        hash
    }

    fn write_tree(rusty_dir: &Path, entries: Vec<TreeEntry>) -> String {
        let tree = Tree { entries };
        let bytes = serde_json::to_vec(&tree).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        fs::write(rusty_dir.join("objects").join("trees").join(&hash), bytes).unwrap();
        hash
    }

    fn write_commit(rusty_dir: &Path, tree_hash: &str, parents: Vec<String>, msg: &str) -> String {
        let commit = Commit {
            tree: tree_hash.to_string(),
            message: msg.to_string(),
            parents,
        };
        let bytes = serde_json::to_vec(&commit).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        fs::write(rusty_dir.join("objects").join("commits").join(&hash), bytes).unwrap();
        hash
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: Repository,
        old_blob: String,
        new_blob: String,
        new_commit: String,
    }

    fn fixture() -> Fixture {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let old_blob = write_blob(&rusty_dir, b"old");
        let new_blob = write_blob(&rusty_dir, b"new");
        let old_tree = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: "file.txt".to_string(),
                object_hash: old_blob.clone(),
                object_type: "blob".to_string(),
            }],
        );
        let new_tree = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: "file.txt".to_string(),
                object_hash: new_blob.clone(),
                object_type: "blob".to_string(),
            }],
        );
        let old_commit = write_commit(&rusty_dir, &old_tree, vec![], "old");
        let new_commit = write_commit(&rusty_dir, &new_tree, vec![old_commit.clone()], "new");
        fs::write(rusty_dir.join("refs").join("heads").join("main"), &new_commit).unwrap();

        let mut index = Index::new();
        index.add("file.txt".to_string(), old_blob.clone());
        index.save(&rusty_dir).unwrap();
        fs::write(tmp.path().join("file.txt"), b"old").unwrap();

        Fixture {
            repo: Repository::from_path(&rusty_dir).unwrap(),
            _tmp: tmp,
            old_blob,
            new_blob,
            new_commit,
        }
    }

    fn plan_for(fixture: &Fixture, with_index: bool, with_worktree: bool) -> ResetPlan {
        ResetPlan {
            operation_id: "op-test-1".to_string(),
            target_commit: fixture.new_commit.clone(),
            destinations: ResetDestinations {
                move_ref: false,
                index: with_index,
                worktree: with_worktree,
            },
            filtered_paths: None,
            ref_update: None::<RefUpdatePlan>,
            index_update: with_index.then(|| IndexUpdatePlan {
                entries_to_add_or_update: vec![("file.txt".to_string(), fixture.new_blob.clone())],
                entries_to_remove: Vec::new(),
            }),
            worktree_changes: if with_worktree {
                vec![WorktreeChange {
                    path: "file.txt".to_string(),
                    kind: ChangeKind::Modify,
                    old_blob: Some(fixture.old_blob.clone()),
                    new_blob: Some(fixture.new_blob.clone()),
                }]
            } else {
                Vec::new()
            },
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn journal_round_trip() {
        let fixture = fixture();
        let plan = plan_for(&fixture, true, true);
        let journal = prepare_journal(&fixture.repo, &plan, vec!["reset".to_string()]).unwrap();
        write_journal(&fixture.repo, &journal).unwrap();

        let loaded = load_active_journal(&fixture.repo).unwrap().unwrap();
        assert_eq!(loaded.operation_id, "op-test-1");
        assert_eq!(loaded.phase, OperationPhase::Prepared);
        assert_eq!(loaded.backups.len(), 1);
        assert_eq!(loaded.backups[0].kind, OriginalKind::File);

        fs::remove_file(active_journal_path(&fixture.repo)).unwrap();
    }

    #[test]
    fn rollback_restores_index_and_worktree() {
        let fixture = fixture();
        let plan = plan_for(&fixture, true, true);
        let journal = prepare_journal(&fixture.repo, &plan, vec![]).unwrap();
        write_journal(&fixture.repo, &journal).unwrap();

        // Simulate a partially applied worktree and index.
        fs::write(fixture.repo.root.join("file.txt"), b"new").unwrap();
        let mut index = Index::new();
        index.add("file.txt".to_string(), fixture.new_blob.clone());
        index.save(&fixture.repo.rusty_dir).unwrap();

        let outcome = recover_if_needed(&fixture.repo).unwrap().unwrap();
        assert_eq!(outcome, RecoveryOutcome::RolledBack);
        assert_eq!(fs::read(fixture.repo.root.join("file.txt")).unwrap(), b"old");
        let restored = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            restored.entries.get("file.txt").unwrap().blob_hash,
            fixture.old_blob
        );
        assert!(!active_journal_path(&fixture.repo).exists());
        assert!(ledger_contains(&fixture.repo, "op-test-1").unwrap());
    }

    #[test]
    fn roll_forward_completes_committed_operation() {
        let fixture = fixture();
        let plan = plan_for(&fixture, true, true);
        let mut journal = prepare_journal(&fixture.repo, &plan, vec![]).unwrap();
        journal.phase = OperationPhase::Committed;
        write_journal(&fixture.repo, &journal).unwrap();

        let outcome = recover_if_needed(&fixture.repo).unwrap().unwrap();
        assert_eq!(outcome, RecoveryOutcome::RolledForward);
        assert_eq!(fs::read(fixture.repo.root.join("file.txt")).unwrap(), b"new");
        let index = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            index.entries.get("file.txt").unwrap().blob_hash,
            fixture.new_blob
        );
        assert!(!active_journal_path(&fixture.repo).exists());
    }

    #[test]
    fn rollback_restores_directory_to_file_transition() {
        let fixture = fixture();
        let old_child = write_blob(&fixture.repo.rusty_dir, b"old child");
        let new_top = write_blob(&fixture.repo.rusty_dir, b"new top");

        fs::remove_file(fixture.repo.root.join("file.txt")).unwrap();
        fs::create_dir(fixture.repo.root.join("file.txt")).unwrap();
        fs::write(fixture.repo.root.join("file.txt/child"), b"old child").unwrap();
        let mut index = Index::new();
        index.add("file.txt/child".to_string(), old_child.clone());
        index.save(&fixture.repo.rusty_dir).unwrap();

        let plan = ResetPlan {
            operation_id: "op-dir-to-file-rollback".to_string(),
            target_commit: fixture.new_commit.clone(),
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: true,
            },
            filtered_paths: None,
            ref_update: None,
            index_update: Some(IndexUpdatePlan {
                entries_to_add_or_update: vec![("file.txt".to_string(), new_top.clone())],
                entries_to_remove: vec!["file.txt/child".to_string()],
            }),
            worktree_changes: vec![
                WorktreeChange {
                    path: "file.txt/child".to_string(),
                    kind: ChangeKind::Delete,
                    old_blob: Some(old_child.clone()),
                    new_blob: None,
                },
                WorktreeChange {
                    path: "file.txt".to_string(),
                    kind: ChangeKind::Create,
                    old_blob: None,
                    new_blob: Some(new_top),
                },
            ],
            conflicts: Vec::new(),
        };

        let journal = prepare_journal(&fixture.repo, &plan, vec![]).unwrap();
        write_journal(&fixture.repo, &journal).unwrap();
        apply_plan_worktree(&fixture.repo, &journal).unwrap();
        assert!(fixture.repo.root.join("file.txt").is_file());

        abort_journal(&fixture.repo, &journal).unwrap();

        assert!(fixture.repo.root.join("file.txt").is_dir());
        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt/child")).unwrap(),
            b"old child"
        );
        let restored = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            restored.entries.get("file.txt/child").unwrap().blob_hash,
            old_child
        );
        assert!(!restored.contains("file.txt"));
    }

    #[test]
    fn rollback_restores_file_to_directory_transition() {
        let fixture = fixture();
        let nested = write_blob(&fixture.repo.rusty_dir, b"nested");
        let plan = ResetPlan {
            operation_id: "op-file-to-dir-rollback".to_string(),
            target_commit: fixture.new_commit.clone(),
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: true,
            },
            filtered_paths: None,
            ref_update: None,
            index_update: Some(IndexUpdatePlan {
                entries_to_add_or_update: vec![("file.txt/child".to_string(), nested.clone())],
                entries_to_remove: vec!["file.txt".to_string()],
            }),
            worktree_changes: vec![
                WorktreeChange {
                    path: "file.txt".to_string(),
                    kind: ChangeKind::Delete,
                    old_blob: Some(fixture.old_blob.clone()),
                    new_blob: None,
                },
                WorktreeChange {
                    path: "file.txt/child".to_string(),
                    kind: ChangeKind::Create,
                    old_blob: None,
                    new_blob: Some(nested),
                },
            ],
            conflicts: Vec::new(),
        };

        let journal = prepare_journal(&fixture.repo, &plan, vec![]).unwrap();
        write_journal(&fixture.repo, &journal).unwrap();
        apply_plan_worktree(&fixture.repo, &journal).unwrap();
        assert!(fixture.repo.root.join("file.txt").is_dir());

        abort_journal(&fixture.repo, &journal).unwrap();

        assert!(fixture.repo.root.join("file.txt").is_file());
        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt")).unwrap(),
            b"old"
        );
        assert!(!fixture.repo.root.join("file.txt/child").exists());
        let restored = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            restored.entries.get("file.txt").unwrap().blob_hash,
            fixture.old_blob
        );
    }

    #[test]
    fn ledger_append_is_idempotent() {
        let fixture = fixture();
        let plan = plan_for(&fixture, true, false);
        let journal = prepare_journal(&fixture.repo, &plan, vec!["reset".to_string()]).unwrap();
        let record = ledger_record_from_journal(&journal, "completed", false);

        append_ledger(&fixture.repo, &record).unwrap();
        append_ledger(&fixture.repo, &record).unwrap();

        let content = fs::read_to_string(ledger_path(&fixture.repo)).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn rejects_unsafe_operation_id() {
        assert!(validate_operation_id("../escape").is_err());
        assert!(validate_operation_id("ok-123").is_ok());
    }

    #[test]
    fn old_commit_exists_for_ledger_consistency() {
        // The fixture should point HEAD at a commit object that exists.
        let fixture = fixture();
        let head = fixture.repo.head_commit().unwrap().unwrap();
        assert_eq!(head, fixture.new_commit);
        assert!(fixture.repo.rusty_dir.join("objects/commits").join(head).exists());
    }
}
