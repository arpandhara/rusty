use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

use crate::commit::{get_commit, Commit};
use crate::repository::get_head_commit;

/// Information about a resolved revision expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRevision {
    /// Canonical 64-hex SHA-256 commit hash
    pub commit_id: String,
    /// Symbolic reference name if resolved directly from HEAD or a branch (e.g. "refs/heads/main")
    pub ref_name: Option<String>,
    /// The parsed commit object
    pub commit: Commit,
}

/// Resolve a revision expression against the repository.
///
/// Supported formats:
/// - `HEAD`
/// - Branch names: `main`, `refs/heads/main`, `origin/main`
/// - Commit hash: full 64-character SHA-256 or unique prefix (>= 4 hex characters)
/// - Modifiers: `^`, `^n` (parent index), `~`, `~n` (first-parent ancestor depth)
///   e.g. `HEAD^`, `HEAD~2`, `main~1`, `a1b2c3d^2`
pub fn resolve_revision(repo_path: &Path, rev_str: &str) -> Result<ResolvedRevision> {
    let rev_str = rev_str.trim();
    if rev_str.is_empty() {
        bail!("Revision expression cannot be empty");
    }

    // Parse base and trailing modifiers (^, ~)
    let (base_spec, modifiers) = parse_modifiers(rev_str)?;

    // Resolve base to (commit_id, Option<ref_name>)
    let (mut current_commit_id, mut initial_ref_name) = resolve_base_commit(repo_path, &base_spec)?;

    // If modifiers were applied, initial_ref_name is no longer the exact ref target
    if !modifiers.is_empty() {
        initial_ref_name = None;
    }

    // Apply modifiers iteratively
    for modifier in modifiers {
        let commit = get_commit(repo_path, &current_commit_id)
            .with_context(|| format!("Failed to read commit object '{}'", current_commit_id))?;

        match modifier {
            Modifier::Parent(parent_idx) => {
                if parent_idx == 0 || parent_idx > commit.parents.len() {
                    bail!(
                        "Commit '{}' has {} parent(s); parent ^{} does not exist",
                        current_commit_id,
                        commit.parents.len(),
                        parent_idx
                    );
                }
                current_commit_id = commit.parents[parent_idx - 1].clone();
            }
            Modifier::Ancestor(depth) => {
                let mut ancestor_id = current_commit_id;
                for step in 1..=depth {
                    let c = get_commit(repo_path, &ancestor_id)?;
                    if c.parents.is_empty() {
                        bail!(
                            "Commit '{}' has no parents; reached root at ancestor step {}/{}",
                            ancestor_id,
                            step,
                            depth
                        );
                    }
                    ancestor_id = c.parents[0].clone();
                }
                current_commit_id = ancestor_id;
            }
        }
    }

    let final_commit = get_commit(repo_path, &current_commit_id)?;

    Ok(ResolvedRevision {
        commit_id: current_commit_id,
        ref_name: initial_ref_name,
        commit: final_commit,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Modifier {
    /// `^n`: n-th parent (1-based)
    Parent(usize),
    /// `~n`: n-th first-parent ancestor
    Ancestor(usize),
}

fn parse_modifiers(input: &str) -> Result<(String, Vec<Modifier>)> {
    let mut base_end = input.len();
    for (i, ch) in input.char_indices() {
        if ch == '^' || ch == '~' {
            base_end = i;
            break;
        }
    }

    let base = input[..base_end].trim().to_string();
    if base.is_empty() {
        bail!("Revision expression '{}' lacks a base revision before modifiers", input);
    }

    let mut modifiers = Vec::new();
    let rest = &input[base_end..];
    let mut chars = rest.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '^' {
            // Check if followed by digits
            let mut num_str = String::new();
            while let Some(&next_ch) = chars.peek() {
                if next_ch.is_ascii_digit() {
                    num_str.push(next_ch);
                    chars.next();
                } else {
                    break;
                }
            }
            let idx = if num_str.is_empty() {
                1
            } else {
                num_str.parse::<usize>().context("Invalid parent index after '^'")?
            };
            modifiers.push(Modifier::Parent(idx));
        } else if ch == '~' {
            let mut num_str = String::new();
            while let Some(&next_ch) = chars.peek() {
                if next_ch.is_ascii_digit() {
                    num_str.push(next_ch);
                    chars.next();
                } else {
                    break;
                }
            }
            let depth = if num_str.is_empty() {
                1
            } else {
                num_str.parse::<usize>().context("Invalid ancestor depth after '~'")?
            };
            modifiers.push(Modifier::Ancestor(depth));
        } else {
            bail!("Unexpected character '{}' in revision expression '{}'", ch, input);
        }
    }

    Ok((base, modifiers))
}

fn resolve_base_commit(repo_path: &Path, base: &str) -> Result<(String, Option<String>)> {
    if base.eq_ignore_ascii_case("HEAD") {
        let head_commit = get_head_commit(repo_path)?
            .ok_or_else(|| anyhow::anyhow!("HEAD does not point to any commit (unborn branch or empty repository)"))?;

        let head_path = repo_path.join("HEAD");
        let ref_name = if head_path.exists() {
            let content = fs::read_to_string(&head_path)?;
            content.trim().strip_prefix("ref: ").map(|s| s.trim().to_string())
        } else {
            None
        };

        return Ok((head_commit, ref_name));
    }

    // Check local branch refs: refs/heads/<base> or just <base>
    let local_ref_path = if base.starts_with("refs/heads/") {
        repo_path.join(base)
    } else {
        repo_path.join("refs").join("heads").join(base)
    };

    if local_ref_path.is_file() {
        let hash = fs::read_to_string(&local_ref_path)?.trim().to_string();
        if !hash.is_empty() {
            let canonical_ref = if base.starts_with("refs/heads/") {
                base.to_string()
            } else {
                format!("refs/heads/{}", base)
            };
            return Ok((hash, Some(canonical_ref)));
        }
    }

    // Check remote refs: refs/remotes/<base> or origin/<branch>
    let remote_ref_path = if base.starts_with("refs/remotes/") {
        repo_path.join(base)
    } else if let Some(stripped) = base.strip_prefix("origin/") {
        repo_path.join("refs").join("remotes").join("origin").join(stripped)
    } else {
        repo_path.join("refs").join("remotes").join(base)
    };

    if remote_ref_path.is_file() {
        let hash = fs::read_to_string(&remote_ref_path)?.trim().to_string();
        if !hash.is_empty() {
            return Ok((hash, None));
        }
    }

    // Check if it matches a commit object hash directly (full or prefix)
    if is_valid_hex(base) {
        let commit = find_commit_by_hash_or_prefix(repo_path, base)?;
        return Ok((commit, None));
    }

    bail!("Revision '{}' not found (not a valid branch name, HEAD, or commit hash)", base);
}

fn is_valid_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn find_commit_by_hash_or_prefix(repo_path: &Path, prefix: &str) -> Result<String> {
    let commits_dir = repo_path.join("objects").join("commits");
    if !commits_dir.is_dir() {
        bail!("Commit objects directory not found");
    }

    // Full 64-character SHA256 hash
    if prefix.len() == 64 {
        let full_path = commits_dir.join(prefix);
        if full_path.is_file() {
            return Ok(prefix.to_string());
        }
        bail!("Commit hash '{}' not found in repository", prefix);
    }

    if prefix.len() < 4 {
        bail!("Commit hash prefix '{}' is too short (minimum 4 characters required)", prefix);
    }

    let mut matches = Vec::new();
    for entry in fs::read_dir(&commits_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) {
            matches.push(name);
        }
    }

    match matches.len() {
        0 => bail!("No commit object found matching prefix '{}'", prefix),
        1 => Ok(matches.remove(0)),
        n => bail!("Ambiguous commit prefix '{}' (matches {} commits)", prefix, n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_modifiers() {
        let (base, mods) = parse_modifiers("HEAD").unwrap();
        assert_eq!(base, "HEAD");
        assert!(mods.is_empty());

        let (base, mods) = parse_modifiers("HEAD^").unwrap();
        assert_eq!(base, "HEAD");
        assert_eq!(mods, vec![Modifier::Parent(1)]);

        let (base, mods) = parse_modifiers("HEAD^^").unwrap();
        assert_eq!(base, "HEAD");
        assert_eq!(mods, vec![Modifier::Parent(1), Modifier::Parent(1)]);

        let (base, mods) = parse_modifiers("main~3").unwrap();
        assert_eq!(base, "main");
        assert_eq!(mods, vec![Modifier::Ancestor(3)]);

        let (base, mods) = parse_modifiers("feat^2~1").unwrap();
        assert_eq!(base, "feat");
        assert_eq!(mods, vec![Modifier::Parent(2), Modifier::Ancestor(1)]);
    }

    #[test]
    fn test_parse_modifiers_errors() {
        assert!(parse_modifiers("").is_err());
        assert!(parse_modifiers("^").is_err());
        assert!(parse_modifiers("HEAD~xyz").is_err());
    }
}
