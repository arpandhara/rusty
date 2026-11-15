use anyhow::{bail, Context, Result};

use crate::reset::plan::{ChangeKind, ResetPlan};
use crate::state::index::save_index_atomic;
use crate::state::operation::{
    abort_journal, build_planned_index, cleanup_orphan_operation_dirs, finalize_journal,
    prepare_journal, write_journal, OperationJournal, OperationPhase,
};
use crate::state::repository::Repository;
use crate::state::worktree::{blob_path, inspect_path, list_files_under, PathState};

/// Execute a validated reset plan.
///
/// The caller must hold the repository lock. The plan was produced under that
/// same lock and is never recomputed here.
pub fn apply_plan(repo: &Repository, plan: &ResetPlan, command_args: Vec<String>) -> Result<()> {
    if plan.destinations.move_ref {
        bail!(
            "Ref movement is not enabled yet (Phase 4). \
             Re-run with '--dry-run' to inspect the ref update, or omit '--move-ref'."
        );
    }

    if !plan.destinations.index && !plan.destinations.worktree {
        bail!("No executable destination selected");
    }

    if !plan.is_executable() {
        bail!("Refusing to execute a plan with fatal safety conflicts");
    }

    // Remove leftovers from operations that completed but crashed before
    // cleaning their backup directory. Safe because we hold the lock and there
    // is no active journal.
    cleanup_orphan_operation_dirs(repo, Some(&plan.operation_id))?;

    preflight(repo, plan)?;

    let mut journal = match prepare_journal(repo, plan, command_args) {
        Ok(journal) => journal,
        Err(e) => {
            let _ = cleanup_orphan_operation_dirs(repo, None);
            return Err(e);
        }
    };

    if let Err(e) = write_journal(repo, &journal) {
        let _ = cleanup_orphan_operation_dirs(repo, None);
        return Err(e);
    }

    if plan.destinations.worktree {
        if let Err(e) = crate::state::operation::apply_plan_worktree(repo, &journal) {
            return fail_with_rollback(repo, &journal, e.context("Worktree update failed"));
        }
        journal.phase = OperationPhase::WorktreeApplied;
        if let Err(e) = write_journal(repo, &journal) {
            return fail_with_rollback(repo, &journal, e.context("Failed to advance journal"));
        }
    }

    if plan.destinations.index {
        let index = match build_planned_index(
            &crate::state::operation::operation_dir(repo, &journal.operation_id)?,
            &journal,
        ) {
            Ok(index) => index,
            Err(e) => {
                return fail_with_rollback(repo, &journal, e.context("Failed to build index"));
            }
        };

        if let Err(e) = save_index_atomic(&index, &repo.rusty_dir) {
            return fail_with_rollback(repo, &journal, e.context("Index update failed"));
        }

        journal.phase = OperationPhase::IndexApplied;
        if let Err(e) = write_journal(repo, &journal) {
            return fail_with_rollback(repo, &journal, e.context("Failed to advance journal"));
        }
    }

    // Commit point: every selected destination is durable. From here on,
    // recovery rolls forward instead of rolling back.
    journal.phase = OperationPhase::Committed;
    if let Err(e) = write_journal(repo, &journal) {
        return fail_with_rollback(repo, &journal, e.context("Failed to mark operation committed"));
    }

    if let Err(e) = finalize_journal(repo, &journal, "completed", false) {
        // The operation is committed; leave the journal in place so the next
        // invocation can finish the bookkeeping without redoing user-visible
        // work.
        return Err(e.context(
            "Reset was applied, but finalizing the operation ledger failed. \
             The next rusty invocation will complete recovery.",
        ));
    }

    print_summary(plan);
    Ok(())
}

fn fail_with_rollback(
    repo: &Repository,
    journal: &OperationJournal,
    original: anyhow::Error,
) -> Result<()> {
    match abort_journal(repo, journal) {
        Ok(()) => Err(original.context(
            "Reset failed before the commit point. Original index and worktree files were restored.",
        )),
        Err(recovery_error) => Err(original.context(format!(
            "Reset failed before the commit point and immediate rollback also failed ({}). \
             The active journal was kept; the next rusty invocation will retry recovery.",
            recovery_error
        ))),
    }
}

/// Verify that the worktree still matches what the plan observed, and that all
/// target blobs are readable, before the journal is written or any file is
/// touched.
fn preflight(repo: &Repository, plan: &ResetPlan) -> Result<()> {
    for change in &plan.worktree_changes {
        let current = inspect_path(&repo.root, &change.path)
            .with_context(|| format!("Failed to inspect '{}' before reset", change.path))?;

        match change.kind {
            ChangeKind::Create => {
                let new_blob = change
                    .new_blob
                    .as_deref()
                    .context("Create change has no target blob")?;
                match &current {
                    PathState::Absent | PathState::EmptyDirectory => {}
                    PathState::File { hash, .. } if hash == new_blob => {
                        // Already in the target state; applying again is
                        // idempotent (useful during roll-forward).
                    }
                    PathState::File { .. } => bail!(
                        "Worktree changed after the plan was made: '{}' now exists with different content. \
                         Re-run reset to plan against the new state.",
                        change.path
                    ),
                    PathState::NonEmptyDirectory => {
                        if !plan_deletes_all_contents(repo, plan, &change.path)? {
                            bail!(
                                "Cannot create '{}': a non-empty directory is in the way",
                                change.path
                            );
                        }
                    }
                }
            }
            ChangeKind::Modify => {
                let new_blob = change
                    .new_blob
                    .as_deref()
                    .context("Modify change has no target blob")?;
                match &current {
                    PathState::File { hash, .. }
                        if hash == new_blob || Some(hash) == change.old_blob.as_ref() => {}
                    PathState::File { .. } => bail!(
                        "Worktree changed after the plan was made: '{}' was modified. \
                         Re-run reset to plan against the new state.",
                        change.path
                    ),
                    _ => bail!(
                        "Worktree changed after the plan was made: '{}' is no longer a file. \
                         Re-run reset to plan against the new state.",
                        change.path
                    ),
                }
            }
            ChangeKind::Delete => match (&current, &change.old_blob) {
                (PathState::Absent, _) => {}
                (PathState::EmptyDirectory, None) => {}
                (PathState::NonEmptyDirectory, None) => {
                    if !plan_deletes_all_contents(repo, plan, &change.path)? {
                        bail!(
                            "Cannot delete '{}': the directory still contains files outside this reset",
                            change.path
                        );
                    }
                }
                (PathState::File { hash, .. }, Some(old)) if hash == old => {}
                _ => bail!(
                    "Worktree changed after the plan was made: '{}' no longer matches the planned deletion. \
                     Re-run reset to plan against the new state.",
                    change.path
                ),
            },
        }

        if let Some(blob) = &change.new_blob {
            blob_path(&repo.rusty_dir, blob)
                .with_context(|| format!("Target blob for '{}' is not readable", change.path))?;
        }
    }

    Ok(())
}

/// True when every file currently beneath `dir_path` is scheduled for deletion
/// by this plan. Used for safe file/directory transitions.
fn plan_deletes_all_contents(
    repo: &Repository,
    plan: &ResetPlan,
    dir_path: &str,
) -> Result<bool> {
    let files = match list_files_under(&repo.root, dir_path) {
        Ok(files) => files,
        Err(_) => return Ok(false),
    };

    Ok(files.iter().all(|path| {
        plan.worktree_changes
            .iter()
            .any(|change| change.kind == ChangeKind::Delete && &change.path == path)
    }))
}

fn print_summary(plan: &ResetPlan) {
    if plan.destinations.index
        && let Some(update) = &plan.index_update
    {
        println!(
            "Index updated: {} file(s) added/updated, {} removed.",
            update.entries_to_add_or_update.len(),
            update.entries_to_remove.len()
        );
    }

    if plan.destinations.worktree {
        println!(
            "Worktree updated: {} file(s).",
            plan.worktree_changes.len()
        );
    }

    println!(
        "Reset operation {} completed. Backup records are in .rusty/ledger/operations.jsonl.",
        plan.operation_id
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::Commit;
    use crate::index::Index;
    use crate::reset::plan::{
        ChangeKind, IndexUpdatePlan, ResetDestinations, RefUpdatePlan, WorktreeChange,
    };
    use crate::repository::get_head_commit;
    use crate::state::operation::{active_journal_path, ledger_path};
    use crate::tree::{Tree, TreeEntry};
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: Repository,
        old_blob: String,
        new_blob: String,
        new_commit: String,
    }

    fn write_blob(rusty_dir: &Path, content: &[u8]) -> String {
        let hash = hex::encode(Sha256::digest(content));
        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::write(rusty_dir.join("objects").join("blobs").join(&hash), content).unwrap();
        hash
    }

    fn write_tree(rusty_dir: &Path, entries: Vec<TreeEntry>) -> String {
        let tree = Tree { entries };
        let bytes = serde_json::to_vec(&tree).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
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
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::write(rusty_dir.join("objects").join("commits").join(&hash), bytes).unwrap();
        hash
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

    fn plan(fixture: &Fixture, with_index: bool, with_worktree: bool) -> ResetPlan {
        ResetPlan {
            operation_id: "apply-test-op".to_string(),
            target_commit: fixture.new_commit.clone(),
            destinations: ResetDestinations {
                move_ref: false,
                index: with_index,
                worktree: with_worktree,
            },
            filtered_paths: None,
            ref_update: None::<RefUpdatePlan>,
            index_update: with_index.then(|| IndexUpdatePlan {
                entries_to_add_or_update: vec![(
                    "file.txt".to_string(),
                    fixture.new_blob.clone(),
                )],
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
    fn index_only_apply_leaves_worktree_alone() {
        let fixture = fixture();
        let plan = plan(&fixture, true, false);
        let _lock = fixture.repo.lock().unwrap();

        apply_plan(&fixture.repo, &plan, vec!["reset".to_string()]).unwrap();

        let index = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            index.entries.get("file.txt").unwrap().blob_hash,
            fixture.new_blob
        );
        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt")).unwrap(),
            b"old"
        );
        assert!(!active_journal_path(&fixture.repo).exists());
        assert_eq!(
            fs::read_to_string(ledger_path(&fixture.repo))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn worktree_apply_changes_file_and_keeps_index_when_not_selected() {
        let fixture = fixture();
        let plan = plan(&fixture, false, true);
        let _lock = fixture.repo.lock().unwrap();

        apply_plan(&fixture.repo, &plan, vec!["reset".to_string()]).unwrap();

        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt")).unwrap(),
            b"new"
        );
        let index = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            index.entries.get("file.txt").unwrap().blob_hash,
            fixture.old_blob
        );
    }

    #[test]
    fn worktree_apply_updates_index_when_both_selected() {
        let fixture = fixture();
        let plan = plan(&fixture, true, true);
        let _lock = fixture.repo.lock().unwrap();

        apply_plan(&fixture.repo, &plan, vec!["reset".to_string()]).unwrap();

        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt")).unwrap(),
            b"new"
        );
        let index = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert_eq!(
            index.entries.get("file.txt").unwrap().blob_hash,
            fixture.new_blob
        );
        assert_eq!(
            get_head_commit(&fixture.repo.rusty_dir).unwrap().unwrap(),
            fixture.new_commit
        );
    }

    #[test]
    fn drift_after_plan_is_rejected_without_mutation() {
        let fixture = fixture();
        let plan = plan(&fixture, false, true);
        let _lock = fixture.repo.lock().unwrap();

        fs::write(fixture.repo.root.join("file.txt"), b"user edit").unwrap();

        let err = apply_plan(&fixture.repo, &plan, vec![]).unwrap_err();
        assert!(err.to_string().contains("Worktree changed after the plan"));

        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt")).unwrap(),
            b"user edit"
        );
        assert!(!active_journal_path(&fixture.repo).exists());
    }

    #[test]
    fn ref_movement_is_rejected_in_this_phase() {
        let fixture = fixture();
        let mut plan = plan(&fixture, true, false);
        plan.destinations.move_ref = true;
        let _lock = fixture.repo.lock().unwrap();

        let err = apply_plan(&fixture.repo, &plan, vec![]).unwrap_err();
        assert!(err.to_string().contains("Phase 4"));
    }

    #[test]
    fn file_to_directory_transition_applies() {
        let fixture = fixture();
        let nested_blob = write_blob(&fixture.repo.rusty_dir, b"nested");
        let plan = ResetPlan {
            operation_id: "transition-file-to-dir".to_string(),
            target_commit: fixture.new_commit.clone(),
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: true,
            },
            filtered_paths: None,
            ref_update: None,
            index_update: Some(IndexUpdatePlan {
                entries_to_add_or_update: vec![(
                    "file.txt/child".to_string(),
                    nested_blob.clone(),
                )],
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
                    new_blob: Some(nested_blob),
                },
            ],
            conflicts: Vec::new(),
        };

        let _lock = fixture.repo.lock().unwrap();
        apply_plan(&fixture.repo, &plan, vec![]).unwrap();

        assert!(fixture.repo.root.join("file.txt").is_dir());
        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt/child")).unwrap(),
            b"nested"
        );
        let index = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert!(!index.contains("file.txt"));
        assert!(index.contains("file.txt/child"));
    }

    #[test]
    fn directory_to_file_transition_applies() {
        let fixture = fixture();
        let old_child = write_blob(&fixture.repo.rusty_dir, b"old child");
        let new_top = write_blob(&fixture.repo.rusty_dir, b"new top");

        // Replace the file in the worktree with a tracked directory.
        fs::remove_file(fixture.repo.root.join("file.txt")).unwrap();
        fs::create_dir(fixture.repo.root.join("file.txt")).unwrap();
        fs::write(fixture.repo.root.join("file.txt/child"), b"old child").unwrap();
        let mut index = Index::new();
        index.add("file.txt/child".to_string(), old_child.clone());
        index.save(&fixture.repo.rusty_dir).unwrap();

        let plan = ResetPlan {
            operation_id: "transition-dir-to-file".to_string(),
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
                    old_blob: Some(old_child),
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

        let _lock = fixture.repo.lock().unwrap();
        apply_plan(&fixture.repo, &plan, vec![]).unwrap();

        assert!(fixture.repo.root.join("file.txt").is_file());
        assert_eq!(
            fs::read(fixture.repo.root.join("file.txt")).unwrap(),
            b"new top"
        );
        let index = Index::load(&fixture.repo.rusty_dir).unwrap();
        assert!(index.contains("file.txt"));
        assert!(!index.contains("file.txt/child"));
    }

    #[test]
    fn binary_worktree_content_round_trips() {
        let fixture = fixture();
        let binary: &[u8] = b"\x00\x01\x02\xff\xfe\n\x00";
        let binary_blob = write_blob(&fixture.repo.rusty_dir, binary);
        let mut plan = plan(&fixture, false, true);
        plan.worktree_changes[0].new_blob = Some(binary_blob.clone());
        let _lock = fixture.repo.lock().unwrap();

        apply_plan(&fixture.repo, &plan, vec![]).unwrap();

        assert_eq!(fs::read(fixture.repo.root.join("file.txt")).unwrap(), binary);
    }
}
