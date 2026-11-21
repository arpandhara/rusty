use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::index::Index;
use crate::merge;
use crate::path_check::{validate_object_hash, validate_rel_path};
use crate::state::refs::validate_branch_ref;
use crate::state::repository::Repository;
use crate::state::revision::resolve_revision;
use crate::state::worktree::{inspect_path, list_files_under, PathState};
use crate::tree::load_tree_files;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetRequest {
    pub target: Option<String>,
    pub destinations: ResetDestinations,
    pub paths: Vec<String>,
    pub dry_run: bool,
    pub json: bool,
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetDestinations {
    pub move_ref: bool,
    pub index: bool,
    pub worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefUpdatePlan {
    pub target_ref: Option<String>,
    pub from_commit: Option<String>,
    pub to_commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Create,
    Modify,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeChange {
    pub path: String,
    pub kind: ChangeKind,
    /// Hash of the file on disk before the change. `None` means the path was
    /// absent or an empty directory.
    pub old_blob: Option<String>,
    pub new_blob: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexUpdatePlan {
    pub entries_to_add_or_update: Vec<(String, String)>,
    pub entries_to_remove: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConflict {
    pub path: Option<String>,
    pub message: String,
    pub fatal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetPlan {
    pub operation_id: String,
    pub target_commit: String,
    pub destinations: ResetDestinations,
    pub filtered_paths: Option<Vec<String>>,
    pub ref_update: Option<RefUpdatePlan>,
    pub index_update: Option<IndexUpdatePlan>,
    pub worktree_changes: Vec<WorktreeChange>,
    pub conflicts: Vec<SafetyConflict>,
}

impl ResetPlan {
    pub fn is_executable(&self) -> bool {
        !self.conflicts.iter().any(|c| c.fatal)
    }

    /// A full reset moves the ref and replaces both the index and the whole
    /// worktree. It is the only form allowed to abort an in-progress merge.
    pub fn is_full_reset(&self) -> bool {
        self.destinations.move_ref
            && self.destinations.index
            && self.destinations.worktree
            && self.filtered_paths.is_none()
    }

    /// True when at least one selected destination would actually change.
    pub fn has_mutations(&self) -> bool {
        if self
            .index_update
            .as_ref()
            .map(|update| {
                !update.entries_to_add_or_update.is_empty()
                    || !update.entries_to_remove.is_empty()
            })
            .unwrap_or(false)
        {
            return true;
        }

        if !self.worktree_changes.is_empty() {
            return true;
        }

        // Ref movement is a mutation when the target differs from the current
        // value. An unborn ref has no current value.
        self.ref_update
            .as_ref()
            .map(|update| update.from_commit.as_deref() != Some(update.to_commit.as_str()))
            .unwrap_or(false)
    }

    pub fn render_text_preview(&self) -> String {
        let mut out = String::new();
        out.push_str("================ Rusty Reset Plan ================\n");
        out.push_str(&format!("Operation ID  : {}\n", self.operation_id));
        out.push_str(&format!("Target Commit : {}\n", self.target_commit));

        out.push_str("Destinations  : ");
        let mut dests = Vec::new();
        if self.destinations.move_ref {
            dests.push("move-ref");
        }
        if self.destinations.index {
            dests.push("index");
        }
        if self.destinations.worktree {
            dests.push("worktree");
        }
        out.push_str(&dests.join(", "));
        out.push('\n');

        if let Some(paths) = &self.filtered_paths {
            out.push_str(&format!("Path Filter   : {}\n", paths.join(", ")));
        }

        out.push_str("--------------------------------------------------\n");

        if let Some(ref_up) = &self.ref_update {
            out.push_str("Ref Movement:\n");
            let ref_label = ref_up.target_ref.as_deref().unwrap_or("HEAD (detached)");
            let from_label = ref_up.from_commit.as_deref().unwrap_or("<unborn>");
            out.push_str(&format!(
                "  Branch Ref  : {}\n  From        : {}\n  To          : {}\n",
                ref_label, from_label, ref_up.to_commit
            ));
        }

        if let Some(idx) = &self.index_update {
            out.push_str("Index Updates:\n");
            out.push_str(&format!(
                "  Staged files to add/update : {}\n  Staged files to remove     : {}\n",
                idx.entries_to_add_or_update.len(),
                idx.entries_to_remove.len()
            ));
            for (path, blob) in &idx.entries_to_add_or_update {
                out.push_str(&format!("    + {} ({})\n", path, &blob[..blob.len().min(8)]));
            }
            for path in &idx.entries_to_remove {
                out.push_str(&format!("    - {}\n", path));
            }
        }

        if self.destinations.worktree {
            out.push_str("Working Tree Changes:\n");
            out.push_str(&format!(
                "  Total files changed: {}\n",
                self.worktree_changes.len()
            ));
            for ch in &self.worktree_changes {
                match ch.kind {
                    ChangeKind::Create => out.push_str(&format!("    + [create] {}\n", ch.path)),
                    ChangeKind::Modify => out.push_str(&format!("    * [modify] {}\n", ch.path)),
                    ChangeKind::Delete => out.push_str(&format!("    - [delete] {}\n", ch.path)),
                }
            }
        }

        if !self.conflicts.is_empty() {
            out.push_str("Safety Conflicts:\n");
            for c in &self.conflicts {
                let prefix = if c.fatal { "  [FATAL]" } else { "  [WARNING]" };
                if let Some(p) = &c.path {
                    out.push_str(&format!("{} {}: {}\n", prefix, p, c.message));
                } else {
                    out.push_str(&format!("{} {}\n", prefix, c.message));
                }
            }
        }

        out.push_str("==================================================\n");
        out
    }

    pub fn render_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("Failed to serialize reset plan to JSON")
    }
}

/// Normalize user-supplied path filters.
///
/// Accepts slash- or backslash-separated relative paths, removes `.` and
/// duplicate separators, rejects `..`, absolute paths, and repository metadata.
pub fn normalize_filter_paths(paths: &[String]) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();

    for raw in paths {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("Empty path argument provided");
        }

        let replaced = raw.replace('\\', "/");
        if replaced.starts_with('/') {
            bail!("Path '{}' is absolute; reset paths must be repository-relative", raw);
        }

        let mut parts = Vec::new();
        for part in replaced.split('/') {
            match part {
                "" | "." => continue,
                ".." => bail!("Path '{}' contains a '..' component", raw),
                _ => parts.push(part),
            }
        }

        if parts.is_empty() {
            bail!(
                "Path '{}' does not name a file or directory; \
                 omit path filters to select the complete tree",
                raw
            );
        }

        let path = parts.join("/");
        validate_rel_path(&path)
            .with_context(|| format!("Invalid path argument '{}'", raw))?;

        if is_metadata_path(&path) {
            bail!(
                "Path '{}' cannot target repository metadata (.rusty or .git)",
                raw
            );
        }

        if seen.insert(path.clone()) {
            normalized.push(path);
        }
    }

    Ok(normalized)
}

fn is_metadata_path(path: &str) -> bool {
    path.split('/')
        .any(|component| component == ".rusty" || component == ".git")
}

/// True when every file beneath `dir_path` is tracked, selected, and absent
/// from the target tree. In that case a file/directory transition is safe:
/// the child deletions run first and the emptied directory is pruned.
fn directory_contents_all_deleted(
    repo_root: &std::path::Path,
    dir_path: &str,
    valid_index_entries: &BTreeMap<String, String>,
    target_files: &BTreeMap<String, String>,
    selected: &dyn Fn(&str) -> Option<usize>,
) -> bool {
    match list_files_under(repo_root, dir_path) {
        Ok(files) => files.iter().all(|path| {
            valid_index_entries.contains_key(path)
                && selected(path).is_some()
                && !target_files.contains_key(path)
        }),
        Err(_) => false,
    }
}

fn filter_match(path: &str, filters: &[String], prefixes: &[String]) -> Option<usize> {
    filters
        .iter()
        .enumerate()
        .find(|(index, filter)| path == filter.as_str() || path.starts_with(&prefixes[*index]))
        .map(|(index, _)| index)
}

struct TargetResolution {
    commit_id: String,
    files: BTreeMap<String, String>,
}

fn resolve_target(
    repo: &Repository,
    target_spec: &str,
    request: &ResetRequest,
) -> Result<TargetResolution> {
    let head_is_unborn = repo.head_commit()?.is_none();

    if target_spec.eq_ignore_ascii_case("HEAD") && head_is_unborn {
        if request.destinations.move_ref {
            bail!(
                "HEAD is unborn: there is no commit to move the branch reference from. \
                 Create a commit first."
            );
        }
        if request.destinations.worktree {
            bail!(
                "HEAD is unborn: there is no target tree for a worktree reset. \
                 Specify an explicit commit or create a commit first."
            );
        }

        // An index-only reset against an unborn HEAD is a well-defined
        // operation: it empties the index. This is useful for unstaging
        // everything in a repository that has not been committed yet.
        return Ok(TargetResolution {
            commit_id: "<unborn>".to_string(),
            files: BTreeMap::new(),
        });
    }

    let resolved = resolve_revision(&repo.rusty_dir, target_spec)?;
    let files = load_tree_files(&repo.rusty_dir, &resolved.commit.tree)?;

    Ok(TargetResolution {
        commit_id: resolved.commit_id,
        files,
    })
}

/// Create a comprehensive ResetPlan by analyzing current state against target.
pub fn create_plan(repo: &Repository, request: &ResetRequest) -> Result<ResetPlan> {
    let operation_id = generate_operation_id();
    let filters = normalize_filter_paths(&request.paths)?;
    let prefixes: Vec<String> = filters.iter().map(|path| format!("{}/", path)).collect();
    let mut matched_filters = vec![false; filters.len()];

    let target_spec = request
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("HEAD");

    let target = resolve_target(repo, target_spec, request)?;
    let current_head_commit = repo.head_commit()?;
    let current_ref = repo.current_symbolic_ref()?;

    let mut conflicts = Vec::new();

    // ── Merge-in-progress guard ────────────────────────────────────────
    if merge::is_merge_in_progress(&repo.rusty_dir) {
        let is_full_reset = request.destinations.move_ref
            && request.destinations.index
            && request.destinations.worktree
            && filters.is_empty();

        if !is_full_reset {
            conflicts.push(SafetyConflict {
                path: None,
                message: "Merge in progress. Complete or abort the merge, or use full reset \
                          (--hard or --move-ref --index --worktree) to abort/discard."
                    .to_string(),
                fatal: true,
            });
        }
    }

    // ── Validate target tree ───────────────────────────────────────────
    let mut target_files = BTreeMap::new();
    for (path, blob_hash) in target.files {
        if is_metadata_path(&path) {
            conflicts.push(SafetyConflict {
                path: Some(path),
                message: "Target tree contains repository metadata (.rusty or .git)".to_string(),
                fatal: true,
            });
            continue;
        }
        if let Err(e) = validate_rel_path(&path) {
            conflicts.push(SafetyConflict {
                path: Some(path),
                message: format!("Unsafe target path: {}", e),
                fatal: true,
            });
            continue;
        }
        if let Err(e) = validate_object_hash(&blob_hash) {
            conflicts.push(SafetyConflict {
                path: Some(path),
                message: format!("Target tree contains an invalid object hash: {}", e),
                fatal: true,
            });
            continue;
        }
        target_files.insert(path, blob_hash);
    }

    // A target tree cannot contain both `a` (file) and `a/b` (path beneath it).
    for path in target_files.keys() {
        let components: Vec<&str> = path.split('/').collect();
        for end in 1..components.len() {
            let ancestor = components[..end].join("/");
            if target_files.contains_key(&ancestor) {
                conflicts.push(SafetyConflict {
                    path: Some(path.clone()),
                    message: format!(
                        "Target tree contains both file '{}' and a path beneath it",
                        ancestor
                    ),
                    fatal: true,
                });
            }
        }
    }

    // ── Validate current index ─────────────────────────────────────────
    let current_index = Index::load(&repo.rusty_dir)?;
    let mut valid_index_entries: BTreeMap<String, String> = BTreeMap::new();
    for (path, entry) in &current_index.entries {
        if is_metadata_path(path) {
            conflicts.push(SafetyConflict {
                path: Some(path.clone()),
                message: "Index contains repository metadata (.rusty or .git)".to_string(),
                fatal: true,
            });
            continue;
        }
        if let Err(e) = validate_rel_path(path) {
            conflicts.push(SafetyConflict {
                path: Some(path.clone()),
                message: format!("Unsafe index path: {}", e),
                fatal: true,
            });
            continue;
        }
        if let Err(e) = validate_object_hash(&entry.blob_hash) {
            conflicts.push(SafetyConflict {
                path: Some(path.clone()),
                message: format!("Index contains an invalid object hash: {}", e),
                fatal: true,
            });
            continue;
        }
        valid_index_entries.insert(path.clone(), entry.blob_hash.clone());
    }

    let selected = |path: &str| -> Option<usize> {
        if filters.is_empty() {
            // No path filter means the complete tree is selected.
            Some(usize::MAX)
        } else {
            filter_match(path, &filters, &prefixes)
        }
    };

    // Mark every filter that names at least one target or index path.
    if !filters.is_empty() {
        for path in target_files.keys().chain(valid_index_entries.keys()) {
            if let Some(index) = selected(path) {
                matched_filters[index] = true;
            }
        }
    }

    // ── Ref update plan ────────────────────────────────────────────────
    let ref_update = if request.destinations.move_ref {
        let update = RefUpdatePlan {
            target_ref: current_ref,
            from_commit: current_head_commit.clone(),
            to_commit: target.commit_id.clone(),
        };

        if update.to_commit == "<unborn>" {
            conflicts.push(SafetyConflict {
                path: None,
                message: "Cannot move a ref to an unborn target".to_string(),
                fatal: true,
            });
        } else if let Err(e) = validate_object_hash(&update.to_commit) {
            conflicts.push(SafetyConflict {
                path: None,
                message: format!("Ref update target is not a valid commit hash: {}", e),
                fatal: true,
            });
        }

        if let Some(ref_name) = &update.target_ref
            && let Err(e) = validate_branch_ref(ref_name)
        {
            conflicts.push(SafetyConflict {
                path: Some(ref_name.clone()),
                message: format!("Cannot move ref: {}", e),
                fatal: true,
            });
        }

        Some(update)
    } else {
        None
    };

    // ── Index update plan ──────────────────────────────────────────────
    let index_update = if request.destinations.index {
        let mut entries_to_add_or_update = Vec::new();
        let mut entries_to_remove = Vec::new();

        for (path, blob_hash) in &target_files {
            if selected(path).is_none() {
                continue;
            }
            match valid_index_entries.get(path) {
                Some(current_hash) if current_hash == blob_hash => {}
                _ => entries_to_add_or_update.push((path.clone(), blob_hash.clone())),
            }
        }

        // Removal iterates the raw index so malformed or metadata entries are
        // dropped rather than silently preserved. Removing a key from the
        // index never touches the worktree.
        for path in current_index.entries.keys() {
            if selected(path).is_none() {
                continue;
            }
            if !target_files.contains_key(path) {
                entries_to_remove.push(path.clone());
            }
        }

        Some(IndexUpdatePlan {
            entries_to_add_or_update,
            entries_to_remove,
        })
    } else {
        None
    };

    // ── Worktree plan and disk conflict detection ──────────────────────
    let mut worktree_changes = Vec::new();

    if request.destinations.worktree {
        let mut affected_paths = BTreeSet::new();
        for path in target_files.keys() {
            if selected(path).is_some() {
                affected_paths.insert(path.clone());
            }
        }
        for path in valid_index_entries.keys() {
            if selected(path).is_some() {
                affected_paths.insert(path.clone());
            }
        }

        // Paths that this plan will delete. They may legitimately be
        // directories' old file form (file `a` -> tree `a/b`).
        let deletion_paths: HashSet<String> = valid_index_entries
            .keys()
            .filter(|path| selected(path).is_some() && !target_files.contains_key(*path))
            .cloned()
            .collect();

        for path in affected_paths {
            if is_metadata_path(&path) {
                conflicts.push(SafetyConflict {
                    path: Some(path),
                    message: "Reset path targets repository metadata (.rusty or .git)".to_string(),
                    fatal: true,
                });
                continue;
            }

            let target_blob = target_files.get(&path).cloned();
            let path_state = match inspect_path(&repo.root, &path) {
                Ok(state) => state,
                Err(e) => {
                    conflicts.push(SafetyConflict {
                        path: Some(path),
                        message: format!("Unsafe or unreadable path: {}", e),
                        fatal: true,
                    });
                    continue;
                }
            };

            // If the target creates a file, no ancestor may be a non-directory
            // unless that ancestor is also being deleted by this plan.
            if target_blob.is_some() {
                let components: Vec<&str> = path.split('/').collect();
                let mut ancestor_conflict = None;
                for end in 1..components.len() {
                    let ancestor = components[..end].join("/");
                    match inspect_path(&repo.root, &ancestor) {
                        Ok(PathState::Absent) | Ok(PathState::EmptyDirectory) => {}
                        Ok(PathState::NonEmptyDirectory) => {}
                        Ok(PathState::File { .. }) if deletion_paths.contains(&ancestor) => {}
                        Ok(PathState::File { .. }) => {
                            ancestor_conflict = Some(format!(
                                "Cannot create '{}': ancestor '{}' is a file",
                                path, ancestor
                            ));
                            break;
                        }
                        Err(e) => {
                            ancestor_conflict =
                                Some(format!("Cannot inspect ancestor '{}': {}", ancestor, e));
                            break;
                        }
                    }
                }
                if let Some(message) = ancestor_conflict {
                    conflicts.push(SafetyConflict {
                        path: Some(path),
                        message,
                        fatal: true,
                    });
                    continue;
                }
            }

            match (target_blob, path_state) {
                // ── Target has a file ──────────────────────────────────
                (Some(new_blob), PathState::Absent) => {
                    worktree_changes.push(WorktreeChange {
                        path,
                        kind: ChangeKind::Create,
                        old_blob: None,
                        new_blob: Some(new_blob),
                    });
                }
                (Some(new_blob), PathState::EmptyDirectory) => {
                    worktree_changes.push(WorktreeChange {
                        path,
                        kind: ChangeKind::Create,
                        old_blob: None,
                        new_blob: Some(new_blob),
                    });
                }
                (Some(new_blob), PathState::NonEmptyDirectory) => {
                    if directory_contents_all_deleted(
                        &repo.root,
                        &path,
                        &valid_index_entries,
                        &target_files,
                        &selected,
                    ) {
                        // Child deletions prune the directory before this
                        // file is written.
                        worktree_changes.push(WorktreeChange {
                            path,
                            kind: ChangeKind::Create,
                            old_blob: None,
                            new_blob: Some(new_blob),
                        });
                    } else {
                        conflicts.push(SafetyConflict {
                            path: Some(path),
                            message: "A non-empty directory would be replaced by a file; \
                                      it contains files outside this reset's selected scope"
                                .to_string(),
                            fatal: true,
                        });
                    }
                }
                (Some(new_blob), PathState::File { hash, .. }) => {
                    if hash != new_blob {
                        if !valid_index_entries.contains_key(&path) {
                            conflicts.push(SafetyConflict {
                                path: Some(path.clone()),
                                message: "Untracked file would be overwritten by reset".to_string(),
                                fatal: true,
                            });
                        } else if valid_index_entries.get(&path) != Some(&hash) {
                            conflicts.push(SafetyConflict {
                                path: Some(path.clone()),
                                message: "Local tracked changes would be overwritten".to_string(),
                                fatal: false,
                            });
                        }

                        worktree_changes.push(WorktreeChange {
                            path,
                            kind: ChangeKind::Modify,
                            old_blob: Some(hash),
                            new_blob: Some(new_blob),
                        });
                    }
                }

                // ── Target does not have the path ──────────────────────
                (None, PathState::Absent) => {}
                (None, PathState::EmptyDirectory) => {
                    worktree_changes.push(WorktreeChange {
                        path,
                        kind: ChangeKind::Delete,
                        old_blob: None,
                        new_blob: None,
                    });
                }
                (None, PathState::NonEmptyDirectory) => {
                    if directory_contents_all_deleted(
                        &repo.root,
                        &path,
                        &valid_index_entries,
                        &target_files,
                        &selected,
                    ) {
                        worktree_changes.push(WorktreeChange {
                            path,
                            kind: ChangeKind::Delete,
                            old_blob: None,
                            new_blob: None,
                        });
                    } else {
                        conflicts.push(SafetyConflict {
                            path: Some(path),
                            message: "A non-empty directory would be removed; \
                                      it contains files outside this reset's selected scope"
                                .to_string(),
                            fatal: true,
                        });
                    }
                }
                (None, PathState::File { hash, .. }) => {
                    if !valid_index_entries.contains_key(&path) {
                        conflicts.push(SafetyConflict {
                            path: Some(path.clone()),
                            message: "Untracked file would be removed".to_string(),
                            fatal: true,
                        });
                    } else if valid_index_entries.get(&path) != Some(&hash) {
                        conflicts.push(SafetyConflict {
                            path: Some(path.clone()),
                            message: "Modified tracked file would be deleted".to_string(),
                            fatal: false,
                        });
                    }

                    worktree_changes.push(WorktreeChange {
                        path,
                        kind: ChangeKind::Delete,
                        old_blob: Some(hash),
                        new_blob: None,
                    });
                }
            }
        }
    }

    // ── Unmatched path filters ─────────────────────────────────────────
    for (index, filter) in filters.iter().enumerate() {
        if !matched_filters[index] {
            conflicts.push(SafetyConflict {
                path: Some(filter.clone()),
                message: "Path filter did not match any tracked or target file".to_string(),
                fatal: true,
            });
        }
    }

    Ok(ResetPlan {
        operation_id,
        target_commit: target.commit_id,
        destinations: request.destinations,
        filtered_paths: if filters.is_empty() {
            None
        } else {
            Some(filters)
        },
        ref_update,
        index_update,
        worktree_changes,
        conflicts,
    })
}

use std::sync::atomic::{AtomicU64, Ordering};

static OP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn generate_operation_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let counter = OP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("{}-{}-{}", now, pid, counter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::Commit;
    use crate::index::Index;
    use crate::tree::{Tree, TreeEntry};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_blob(rusty_dir: &Path, content: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash = hex::encode(Sha256::digest(content.as_bytes()));
        fs::write(rusty_dir.join("objects").join("blobs").join(&hash), content).unwrap();
        hash
    }

    fn write_tree(rusty_dir: &Path, entries: Vec<TreeEntry>) -> String {
        use sha2::{Digest, Sha256};
        let tree = Tree { entries };
        let bytes = serde_json::to_vec(&tree).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        fs::write(rusty_dir.join("objects").join("trees").join(&hash), bytes).unwrap();
        hash
    }

    fn write_commit(rusty_dir: &Path, tree_hash: &str, parents: Vec<String>, msg: &str) -> String {
        use sha2::{Digest, Sha256};
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

    #[test]
    fn test_operation_id_generation() {
        let id1 = generate_operation_id();
        let id2 = generate_operation_id();
        assert_ne!(id1, id2);
        assert!(id1.contains('-'));
    }

    #[test]
    fn test_normalize_filter_paths() {
        let paths = vec![
            "./src//lib.rs".to_string(),
            "src\\main.rs".to_string(),
            "src/lib.rs".to_string(),
        ];
        assert_eq!(
            normalize_filter_paths(&paths).unwrap(),
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );

        assert!(normalize_filter_paths(&["../escape".to_string()]).is_err());
        assert!(normalize_filter_paths(&[".rusty/config".to_string()]).is_err());
        assert!(normalize_filter_paths(&["".to_string()]).is_err());
        assert!(normalize_filter_paths(&[".".to_string()]).is_err());
    }

    #[test]
    fn test_create_plan_soft_reset() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let rusty_dir = root.join(".rusty");

        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();

        // Commit 1: file1.txt
        let b1 = write_blob(&rusty_dir, "content 1");
        let t1 = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: "file1.txt".to_string(),
                object_hash: b1.clone(),
                object_type: "blob".to_string(),
            }],
        );
        let c1 = write_commit(&rusty_dir, &t1, vec![], "Initial commit");

        // Commit 2: file1.txt modified, file2.txt added
        let b2 = write_blob(&rusty_dir, "content 2");
        let b3 = write_blob(&rusty_dir, "content 3");
        let t2 = write_tree(
            &rusty_dir,
            vec![
                TreeEntry {
                    name: "file1.txt".to_string(),
                    object_hash: b2.clone(),
                    object_type: "blob".to_string(),
                },
                TreeEntry {
                    name: "file2.txt".to_string(),
                    object_hash: b3.clone(),
                    object_type: "blob".to_string(),
                },
            ],
        );
        let c2 = write_commit(&rusty_dir, &t2, vec![c1.clone()], "Second commit");

        // Set HEAD -> refs/heads/main -> c2
        fs::write(rusty_dir.join("refs").join("heads").join("main"), &c2).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        // Index has c2 files
        let mut index = Index::new();
        index.add("file1.txt".to_string(), b2.clone());
        index.add("file2.txt".to_string(), b3.clone());
        index.save(&rusty_dir).unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();

        // Request: soft reset to HEAD^
        let req = ResetRequest {
            target: Some("HEAD^".to_string()),
            destinations: ResetDestinations {
                move_ref: true,
                index: false,
                worktree: false,
            },
            paths: Vec::new(),
            dry_run: true,
            json: false,
            yes: false,
        };

        let plan = create_plan(&repo, &req).unwrap();
        assert_eq!(plan.target_commit, c1);
        assert!(plan.is_executable());
        assert!(plan.ref_update.is_some());
        let ref_up = plan.ref_update.as_ref().unwrap();
        assert_eq!(ref_up.from_commit.as_deref(), Some(c2.as_str()));
        assert_eq!(ref_up.to_commit, c1);
        assert_eq!(ref_up.target_ref.as_deref(), Some("refs/heads/main"));

        // Index and worktree are not touched
        assert!(plan.index_update.is_none());
        assert!(plan.worktree_changes.is_empty());

        // Test preview and json rendering
        let preview = plan.render_text_preview();
        assert!(preview.contains("move-ref"));
        assert!(preview.contains(&c1));

        let json = plan.render_json().unwrap();
        assert!(json.contains(&c1));
    }

    #[test]
    fn test_create_plan_mixed_reset() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let rusty_dir = root.join(".rusty");

        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();

        let b1 = write_blob(&rusty_dir, "content 1");
        let t1 = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: "file1.txt".to_string(),
                object_hash: b1.clone(),
                object_type: "blob".to_string(),
            }],
        );
        let c1 = write_commit(&rusty_dir, &t1, vec![], "Initial commit");

        let b2 = write_blob(&rusty_dir, "content 2");
        let t2 = write_tree(
            &rusty_dir,
            vec![
                TreeEntry {
                    name: "file1.txt".to_string(),
                    object_hash: b2.clone(),
                    object_type: "blob".to_string(),
                },
                TreeEntry {
                    name: "file2.txt".to_string(),
                    object_hash: b1.clone(),
                    object_type: "blob".to_string(),
                },
            ],
        );
        let c2 = write_commit(&rusty_dir, &t2, vec![c1.clone()], "Second commit");

        fs::write(rusty_dir.join("refs").join("heads").join("main"), &c2).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let mut index = Index::new();
        index.add("file1.txt".to_string(), b2.clone());
        index.add("file2.txt".to_string(), b1.clone());
        index.save(&rusty_dir).unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();

        // Request: mixed reset to c1
        let req = ResetRequest {
            target: Some(c1.clone()),
            destinations: ResetDestinations {
                move_ref: true,
                index: true,
                worktree: false,
            },
            paths: Vec::new(),
            dry_run: true,
            json: false,
            yes: false,
        };

        let plan = create_plan(&repo, &req).unwrap();
        assert!(plan.is_executable());
        assert!(plan.ref_update.is_some());
        let idx_up = plan.index_update.as_ref().unwrap();
        // file1.txt should be updated to b1, file2.txt should be removed
        assert_eq!(
            idx_up.entries_to_add_or_update,
            vec![("file1.txt".to_string(), b1)]
        );
        assert_eq!(idx_up.entries_to_remove, vec!["file2.txt".to_string()]);
    }

    #[test]
    fn unborn_head_index_reset_empties_the_index() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let blob = write_blob(&rusty_dir, "staged");
        let mut index = Index::new();
        index.add("staged.txt".to_string(), blob);
        index.save(&rusty_dir).unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();
        let req = ResetRequest {
            target: None,
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: false,
            },
            paths: Vec::new(),
            dry_run: true,
            json: false,
            yes: false,
        };

        let plan = create_plan(&repo, &req).unwrap();
        assert!(plan.is_executable());
        assert_eq!(plan.target_commit, "<unborn>");
        let update = plan.index_update.unwrap();
        assert_eq!(update.entries_to_remove, vec!["staged.txt".to_string()]);
    }

    #[test]
    fn path_limited_index_update_leaves_other_entries_untouched() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let old_a = write_blob(&rusty_dir, "old a");
        let old_b = write_blob(&rusty_dir, "old b");
        let new_a = write_blob(&rusty_dir, "new a");
        let old_tree = write_tree(
            &rusty_dir,
            vec![
                TreeEntry {
                    name: "a.txt".to_string(),
                    object_hash: old_a.clone(),
                    object_type: "blob".to_string(),
                },
                TreeEntry {
                    name: "b.txt".to_string(),
                    object_hash: old_b.clone(),
                    object_type: "blob".to_string(),
                },
            ],
        );
        let new_tree = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: "a.txt".to_string(),
                object_hash: new_a.clone(),
                object_type: "blob".to_string(),
            }],
        );
        let old_commit = write_commit(&rusty_dir, &old_tree, vec![], "old");
        let new_commit = write_commit(&rusty_dir, &new_tree, vec![old_commit], "new");
        fs::write(rusty_dir.join("refs").join("heads").join("main"), &new_commit).unwrap();

        let mut index = Index::new();
        index.add("a.txt".to_string(), old_a);
        index.add("b.txt".to_string(), old_b);
        index.save(&rusty_dir).unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();
        let req = ResetRequest {
            target: Some(new_commit),
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: false,
            },
            paths: vec!["a.txt".to_string()],
            dry_run: true,
            json: false,
            yes: false,
        };

        let plan = create_plan(&repo, &req).unwrap();
        assert!(plan.is_executable());
        let update = plan.index_update.unwrap();
        assert_eq!(update.entries_to_add_or_update, vec![("a.txt".to_string(), new_a)]);
        assert!(update.entries_to_remove.is_empty());
    }

    #[test]
    fn target_tree_metadata_path_is_rejected() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let blob = write_blob(&rusty_dir, "evil");
        let tree = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: ".rusty".to_string(),
                object_hash: blob,
                object_type: "blob".to_string(),
            }],
        );
        let commit = write_commit(&rusty_dir, &tree, vec![], "evil");
        fs::write(rusty_dir.join("refs").join("heads").join("main"), &commit).unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();
        let req = ResetRequest {
            target: None,
            destinations: ResetDestinations {
                move_ref: false,
                index: false,
                worktree: true,
            },
            paths: Vec::new(),
            dry_run: true,
            json: false,
            yes: false,
        };

        let plan = create_plan(&repo, &req).unwrap();
        assert!(!plan.is_executable());
        assert!(plan.conflicts.iter().any(|c| c.fatal
            && c.message.contains("repository metadata")));
    }

    #[test]
    fn absolute_path_filters_are_rejected() {
        assert!(normalize_filter_paths(&["/etc/passwd".to_string()]).is_err());
        assert!(normalize_filter_paths(&["\\\\server\\share".to_string()]).is_err());
    }

    #[test]
    fn unmatched_path_filter_is_a_fatal_conflict() {
        let tmp = tempdir().unwrap();
        let rusty_dir = tmp.path().join(".rusty");
        fs::create_dir_all(rusty_dir.join("objects").join("blobs")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("trees")).unwrap();
        fs::create_dir_all(rusty_dir.join("objects").join("commits")).unwrap();
        fs::create_dir_all(rusty_dir.join("refs").join("heads")).unwrap();
        fs::write(rusty_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let blob = write_blob(&rusty_dir, "tracked");
        let tree = write_tree(
            &rusty_dir,
            vec![TreeEntry {
                name: "tracked.txt".to_string(),
                object_hash: blob.clone(),
                object_type: "blob".to_string(),
            }],
        );
        let commit = write_commit(&rusty_dir, &tree, vec![], "c");
        fs::write(rusty_dir.join("refs").join("heads").join("main"), &commit).unwrap();

        let mut index = Index::new();
        index.add("tracked.txt".to_string(), blob);
        index.save(&rusty_dir).unwrap();

        let repo = Repository::from_path(&rusty_dir).unwrap();
        let req = ResetRequest {
            target: None,
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: false,
            },
            paths: vec!["missing.txt".to_string()],
            dry_run: true,
            json: false,
            yes: false,
        };

        let plan = create_plan(&repo, &req).unwrap();
        assert!(!plan.is_executable());
        assert!(plan.conflicts.iter().any(|c| c.fatal
            && c.message.contains("did not match any tracked or target file")));
    }
}
