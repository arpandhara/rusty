use anyhow::Result;
use std::fs;
use std::path::Path;

use crate::commit::get_commit;
use crate::index::Index;
use crate::merge;
use crate::tree::load_tree_files;

pub fn checkout(repo_path: &Path, branch: &str) -> Result<()> {
    // ── Block checkout during active merge ─────────────────────────────
    if merge::is_merge_in_progress(repo_path) {
        anyhow::bail!(
            "Merge in progress. Commit the merge or abort it first \
             (rusty merge --abort)."
        );
    }

    let local_branch_path = repo_path.join("refs").join("heads").join(branch);

    let remote_branch_path = repo_path
        .join("refs")
        .join("remotes")
        .join("origin")
        .join(branch);

    let branch_path;

    // ------------------------------------------------------------
    // 1. Local branch exists
    // ------------------------------------------------------------

    if local_branch_path.exists() {
        branch_path = local_branch_path;
    }
    // ------------------------------------------------------------
    // 2. Local branch doesn't exist, but remote branch exists
    // ------------------------------------------------------------
    else if remote_branch_path.exists() {
        let commit_hash = fs::read_to_string(&remote_branch_path)?.trim().to_string();

        if commit_hash.is_empty() {
            anyhow::bail!("Remote branch 'origin/{}' has no commits", branch);
        }

        if let Some(parent) = local_branch_path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&local_branch_path, &commit_hash)?;

        println!("Created local branch '{}' from 'origin/{}'", branch, branch);

        branch_path = local_branch_path;
    }
    // ------------------------------------------------------------
    // 3. Neither local nor remote branch exists
    // ------------------------------------------------------------
    else {
        anyhow::bail!("Branch '{}' does not exist locally or on origin", branch);
    }

    let commit_hash = fs::read_to_string(&branch_path)?.trim().to_string();

    if commit_hash.is_empty() {
        anyhow::bail!("Branch '{}' has no commits", branch);
    }

    let commit = get_commit(repo_path, &commit_hash)?;

    let files = load_tree_files(repo_path, &commit.tree)?;

    // ------------------------------------------------------------
    // 4. Make sure checkout will not destroy work
    // ------------------------------------------------------------

    crate::state::worktree::ensure_clean_working_tree(repo_path, &files)?;

    // ------------------------------------------------------------
    // 5. Restore target working tree
    // ------------------------------------------------------------

    crate::state::worktree::restore_working_tree(repo_path, &files)?;

    // ------------------------------------------------------------
    // 6. Rebuild index
    // ------------------------------------------------------------

    let mut index = Index::new();

    for (path, hash) in &files {
        index.add(path.clone(), hash.clone());
    }

    index.save(repo_path)?;

    // ------------------------------------------------------------
    // 7. Update HEAD
    // ------------------------------------------------------------

    fs::write(
        repo_path.join("HEAD"),
        format!("ref: refs/heads/{}\n", branch),
    )?;

    println!("Switched to branch '{}'", branch);

    Ok(())
}
