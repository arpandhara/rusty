use anyhow::{bail, Result};

use crate::reset::plan::{normalize_filter_paths, ResetPlan, ResetRequest};

/// Validate the user-supplied request before planning.
pub fn validate_request(request: &ResetRequest) -> Result<()> {
    // 1. Must specify at least one destination
    if !request.destinations.move_ref
        && !request.destinations.index
        && !request.destinations.worktree
    {
        bail!(
            "No destinations specified. Specify at least one of: \
             --move-ref (or --soft), --index, --worktree (or --mixed, --hard)"
        );
    }

    // 2. Paths cannot be combined with moving branch refs
    if !request.paths.is_empty() && request.destinations.move_ref {
        bail!(
            "Cannot specify path filters when moving branch reference (--move-ref / --soft / --mixed / --hard). \
             A branch ref can only name a complete commit, not partial paths."
        );
    }

    // 3. Validate and normalize path strings. Normalization also rejects
    //    traversal, absolute paths, and repository metadata.
    normalize_filter_paths(&request.paths)?;

    Ok(())
}

/// Reject a plan that contains any fatal safety conflict.
///
/// Non-fatal conflicts (for example, discarding locally modified tracked
/// files) are not rejected here. They are shown in the preview and are
/// explicitly accepted by the confirmation step.
pub fn validate_plan(plan: &ResetPlan) -> Result<()> {
    let fatal_conflicts: Vec<_> = plan.conflicts.iter().filter(|conflict| conflict.fatal).collect();

    if fatal_conflicts.is_empty() {
        return Ok(());
    }

    let mut message = String::from("Cannot execute reset due to safety conflicts:\n");
    for conflict in fatal_conflicts {
        if let Some(path) = &conflict.path {
            message.push_str(&format!("  - {}: {}\n", path, conflict.message));
        } else {
            message.push_str(&format!("  - {}\n", conflict.message));
        }
    }
    message.push_str("No files or index entries were changed.");

    bail!(message)
}

/// True when the plan would overwrite locally modified tracked content. These
/// conflicts are recoverable by the journal, but still need explicit consent.
pub fn plan_has_destructive_warnings(plan: &ResetPlan) -> bool {
    plan.conflicts.iter().any(|conflict| !conflict.fatal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reset::plan::{ResetDestinations, SafetyConflict};

    fn request(destinations: ResetDestinations, paths: Vec<String>) -> ResetRequest {
        ResetRequest {
            target: None,
            destinations,
            paths,
            dry_run: false,
            json: false,
            yes: false,
        }
    }

    fn plan_with_conflicts(conflicts: Vec<SafetyConflict>) -> ResetPlan {
        ResetPlan {
            operation_id: "test-op".to_string(),
            target_commit: "commit-123".to_string(),
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: true,
            },
            filtered_paths: None,
            ref_update: None,
            index_update: None,
            worktree_changes: Vec::new(),
            conflicts,
        }
    }

    #[test]
    fn test_validate_request_no_destinations() {
        let req = request(
            ResetDestinations {
                move_ref: false,
                index: false,
                worktree: false,
            },
            Vec::new(),
        );
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_validate_request_paths_with_move_ref() {
        let req = request(
            ResetDestinations {
                move_ref: true,
                index: true,
                worktree: false,
            },
            vec!["src/lib.rs".to_string()],
        );
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_validate_request_unsafe_path() {
        let req = request(
            ResetDestinations {
                move_ref: false,
                index: true,
                worktree: false,
            },
            vec![".rusty/config".to_string()],
        );
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn test_validate_request_normalizes_paths() {
        let req = request(
            ResetDestinations {
                move_ref: false,
                index: true,
                worktree: false,
            },
            vec!["./src//lib.rs".to_string()],
        );
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn test_validate_plan_fatal_conflicts() {
        let plan = plan_with_conflicts(vec![SafetyConflict {
            path: Some("file.txt".to_string()),
            message: "Untracked file would be overwritten".to_string(),
            fatal: true,
        }]);

        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn test_non_fatal_conflicts_are_warnings_not_validation_errors() {
        let plan = plan_with_conflicts(vec![SafetyConflict {
            path: Some("file.txt".to_string()),
            message: "Local tracked changes would be overwritten".to_string(),
            fatal: false,
        }]);

        assert!(validate_plan(&plan).is_ok());
        assert!(plan_has_destructive_warnings(&plan));
    }
}
